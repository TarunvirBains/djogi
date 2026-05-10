// Phase 8.5 Cluster 2 issue #129 — non-superuser test pool for RLS-backed
// integration tests.
//
// # What this file pins
//
// **`non_superuser_rls_filters_typed_fetch_and_refresh`** —
// the cluster-exit RLS proof that closes the `phase8_t8_10_refresh_e2e`
// deferred Option-B gap (see GH #129). Two tenants are seeded by an
// admin (superuser) connection that bypasses RLS; the same per-test
// database is then re-opened through
// [`djogi::testing::connect_test_db_as_non_superuser`], giving us a
// `DjogiContext` whose physical connections authenticate as the
// `djogi_test_user` role (`LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
// NOREPLICATION NOBYPASSRLS`). With that context:
//
// 1. A typed `QuerySet` fetch wrapped in `transaction::atomic` returns
//    only tenant-1000 rows after `set_auth(...).with_tenant("1000")`.
// 2. The same model's `refresh_into` handle, constructed against the
//    non-superuser pool with the same tenant-locked `AuthContext`,
//    populates the bound `Punnu<T>` with tenant-1000 ids and excludes
//    every tenant-2000 id — exercising the cluster-8δ delta-sync path
//    that `phase8_t8_10_refresh_e2e::refresh_into_auth_locked_to_subscription`
//    can only prove structurally because it runs as a superuser.
//
// # Why a manual `#[test]` and not `#[djogi::djogi_test]`
//
// The harness `#[djogi::djogi_test]` macro hides the `TestDbCleanup`
// token behind its generated wrapper — there is no way to thread it
// through to a test body that also needs to call
// [`djogi::testing::connect_test_db_as_non_superuser`]. The hand-rolled
// shape below opens the per-test database explicitly via
// [`djogi::testing::setup_test_db`], runs the body inside a
// `current_thread` Tokio runtime, and uses
// `futures::FutureExt::catch_unwind` to capture panics so
// [`djogi::testing::teardown_test_db`] always runs (matching the panic
// containment the macro provides).
//
// # Spec anchors
//
// - GH #129 — "Test infra: non-superuser test pool for RLS-backed
//   integration tests".
// - `tests/integration/phase8_t8_10_refresh_e2e.rs` —
//   `refresh_into_auth_locked_to_subscription` documents the Option-C
//   structural proof and refers to GH #129 for the full row-count
//   isolation.
// - `docs/guide/tenancy.md` — note on superuser bypass and the two safe
//   paths for RLS-backed tests (this file's helper closes the
//   "non-owner, non-superuser role" path for the test harness).
//
// # Fixture model
//
// `Phase85C2129RlsRow` carries a BIGINT `tenant_id` (so the macro routes
// the policy cast to `::bigint`) and an opaque `label` for assertion
// readability. `pk = Serial` keeps the PK independent of HeeRanjID
// generator naming churn — the row id is a plain `i32` from a sequence,
// which the non-superuser role consumes via the `GRANT USAGE, SELECT,
// UPDATE ON ALL SEQUENCES` clause `connect_test_db_as_non_superuser`
// emits.

use djogi::auth::AuthContext;
use djogi::prelude::*;
use djogi::testing::{
    TEST_NON_SUPERUSER_ROLE, connect_test_db_as_non_superuser, setup_test_db, teardown_test_db,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

#[model(table = "phase8_5_c2_129_rls_rows", pk = Serial, tenant_key = "tenant_id")]
#[derive(Debug, Clone)]
pub struct Phase85C2129RlsRow {
    pub tenant_id: i64,
    pub label: String,
}

/// Attach RLS to the fixture table from the admin context.
///
/// `ENABLE` turns the policy machinery on; `FORCE` would make the policy
/// apply to a non-superuser table owner. It still does not override
/// superuser bypass, which is why the assertions below reconnect as
/// `djogi_test_user` rather than relying on the admin setup context.
/// The `USING` clause carries the `::bigint` cast that matches the
/// `i64`-typed `tenant_id` column. Adopters typically rely on the
/// macro-emitted side-channel SQL at `target/djogi_rls/{table}_rls.sql`
/// (Phase 5 T9), but the migration differ that consumes those files
/// is still phase-gated; emitting the policy directly here keeps the
/// test self-contained.
async fn install_rls_policy(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl("ALTER TABLE phase8_5_c2_129_rls_rows ENABLE ROW LEVEL SECURITY")
        .await
        .expect("ENABLE RLS must succeed against a freshly-created table");
    ctx.raw_ddl("ALTER TABLE phase8_5_c2_129_rls_rows FORCE ROW LEVEL SECURITY")
        .await
        .expect("FORCE RLS must succeed against a freshly-created table");
    ctx.raw_ddl(
        "CREATE POLICY phase8_5_c2_129_rls_rows_tenant_isolation \
         ON phase8_5_c2_129_rls_rows \
         USING (tenant_id = current_setting('app.tenant_id', true)::bigint)",
    )
    .await
    .expect("CREATE POLICY must succeed against the freshly-enabled table");
}

/// Seed the fixture table with three rows for tenant 1000 and two for
/// tenant 2000. Runs as the admin (superuser) connection, which
/// bypasses RLS — this is the legitimate "trusted setup" path.
async fn seed_two_tenants(ctx: &mut djogi::DjogiContext) -> SeededRows {
    let mut tenant_1000_ids = Vec::with_capacity(3);
    for i in 1i64..=3 {
        let row = Phase85C2129RlsRow::create(
            ctx,
            Phase85C2129RlsRow {
                tenant_id: 1000,
                label: format!("tenant1000-row{i}"),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("seed tenant 1000 row {i}: {e:?}"));
        tenant_1000_ids.push(row.id);
    }

    let mut tenant_2000_ids = Vec::with_capacity(2);
    for i in 1i64..=2 {
        let row = Phase85C2129RlsRow::create(
            ctx,
            Phase85C2129RlsRow {
                tenant_id: 2000,
                label: format!("tenant2000-row{i}"),
                ..Default::default()
            },
        )
        .await
        .unwrap_or_else(|e| panic!("seed tenant 2000 row {i}: {e:?}"));
        tenant_2000_ids.push(row.id);
    }

    SeededRows {
        tenant_1000_ids,
        tenant_2000_ids,
    }
}

struct SeededRows {
    tenant_1000_ids: Vec<i32>,
    tenant_2000_ids: Vec<i32>,
}

#[test]
fn non_superuser_rls_filters_typed_fetch_and_refresh() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread Tokio runtime");

    runtime.block_on(async {
        // ── Admin path: provision DB, sync the model, install RLS, seed ──
        let (cleanup, mut admin_ctx) = setup_test_db()
            .await
            .expect("setup_test_db must succeed against DATABASE_URL");

        // Capture any panic from the test body so teardown still runs and
        // the `djogi_test_*` orphan cleanup ledger stays clean.
        let outcome = AssertUnwindSafe(async {
            djogi::testing::sync_models(
                &mut admin_ctx,
                &[<Phase85C2129RlsRow as Model>::descriptor()],
            )
            .await
            .expect("sync_models must materialise the fixture table");

            install_rls_policy(&mut admin_ctx).await;
            let seeded = seed_two_tenants(&mut admin_ctx).await;

            // ── Non-superuser path: re-open the same DB without privilege ──
            // We never call a &mut-self method on `non_super_ctx`; it
            // lives only to vend `share_pool()` and `punnu::<T>()` for
            // the typed fetch and refresh paths below.
            let non_super_ctx = connect_test_db_as_non_superuser(&cleanup)
                .await
                .expect("connect_test_db_as_non_superuser must succeed");

            // 1. Typed QuerySet fetch under tenant 1000.
            //
            // The fetch must run inside `transaction::atomic` because
            // `set_tenant` issues `set_config(..., is_local = true)`,
            // which scopes the GUC to the current transaction. On a
            // pool-backed context, every CRUD call would otherwise
            // check out a fresh connection and lose the GUC.
            let pool_for_atomic = non_super_ctx
                .share_pool()
                .expect("non-superuser ctx must be pool-backed");
            let visible_titles_for_1000: Vec<String> = djogi::transaction::atomic(
                &pool_for_atomic,
                |tx| {
                    Box::pin(async move {
                        tx.set_auth(
                            AuthContext::new(
                                djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"),
                            )
                            .with_tenant("1000"),
                        );
                        // auto_set_tenant fires inside fetch_all and applies
                        // `app.tenant_id = '1000'` for the duration of the
                        // transaction.
                        let rows = Phase85C2129RlsRow::objects()
                            .fetch_all(tx)
                            .await?;
                        Ok::<_, djogi::DjogiError>(
                            rows.into_iter().map(|r| r.label).collect(),
                        )
                    })
                },
            )
            .await
            .expect("typed fetch under tenant 1000 must succeed");

            assert_eq!(
                visible_titles_for_1000.len(),
                3,
                "tenant 1000 must see exactly its own 3 rows under non-superuser \
                 RLS-enforced fetch; got {visible_titles_for_1000:?}",
            );
            for title in &visible_titles_for_1000 {
                assert!(
                    title.starts_with("tenant1000-"),
                    "every visible label must belong to tenant 1000; got {title:?} \
                     in {visible_titles_for_1000:?}",
                );
            }

            // 2. Same fetch under tenant 2000 — exactly the disjoint subset.
            let visible_titles_for_2000: Vec<String> = djogi::transaction::atomic(
                &pool_for_atomic,
                |tx| {
                    Box::pin(async move {
                        tx.set_auth(
                            AuthContext::new(
                                djogi::HeerId::from_i64(2).expect("HeerId(2) is valid"),
                            )
                            .with_tenant("2000"),
                        );
                        let rows = Phase85C2129RlsRow::objects()
                            .fetch_all(tx)
                            .await?;
                        Ok::<_, djogi::DjogiError>(
                            rows.into_iter().map(|r| r.label).collect(),
                        )
                    })
                },
            )
            .await
            .expect("typed fetch under tenant 2000 must succeed");

            assert_eq!(
                visible_titles_for_2000.len(),
                2,
                "tenant 2000 must see exactly its own 2 rows under non-superuser \
                 RLS-enforced fetch; got {visible_titles_for_2000:?}",
            );
            for title in &visible_titles_for_2000 {
                assert!(
                    title.starts_with("tenant2000-"),
                    "every visible label must belong to tenant 2000; got {title:?} \
                     in {visible_titles_for_2000:?}",
                );
            }

            // 3. Sanity: a tenant id that matches no row hides everything.
            // Pre-fix the empty-cast policy (#37 family) returned 0 rows for
            // every tenant — including the legitimate ones above. The
            // tenant-1000/2000 assertions disprove that, but pinning the
            // empty-set tenant pins the policy is exercising a real cast
            // rather than failing closed.
            let visible_titles_for_9999: Vec<String> = djogi::transaction::atomic(
                &pool_for_atomic,
                |tx| {
                    Box::pin(async move {
                        tx.set_auth(
                            AuthContext::new(
                                djogi::HeerId::from_i64(99).expect("HeerId(99) is valid"),
                            )
                            .with_tenant("9999"),
                        );
                        let rows = Phase85C2129RlsRow::objects()
                            .fetch_all(tx)
                            .await?;
                        Ok::<_, djogi::DjogiError>(
                            rows.into_iter().map(|r| r.label).collect(),
                        )
                    })
                },
            )
            .await
            .expect("typed fetch under tenant 9999 must succeed");
            assert!(
                visible_titles_for_9999.is_empty(),
                "tenant 9999 has no rows; the policy must hide all 5 rows; \
                 got {visible_titles_for_9999:?}",
            );

            // 4. Refresh path under non-superuser + tenant-locked auth.
            //
            // This is the cluster-8δ proof that
            // `phase8_t8_10_refresh_e2e::refresh_into_auth_locked_to_subscription`
            // can only do structurally. The fetcher opens its own
            // `transaction::atomic` per tick, calls `auto_set_tenant::<T>`,
            // and runs the SELECT under the tenant-1000 GUC. With the
            // non-superuser role, RLS filters the rows server-side.
            let punnu = non_super_ctx
                .punnu::<Phase85C2129RlsRow>()
                .expect("Punnu must be registered for Phase85C2129RlsRow");
            let pool_for_refresh = non_super_ctx
                .share_pool()
                .expect("non-superuser ctx must be pool-backed");
            let refresh_auth = AuthContext::new(
                djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"),
            )
            .with_tenant("1000");

            let handle = Phase85C2129RlsRow::objects()
                .refresh_into(&punnu, pool_for_refresh, refresh_auth)
                .expect("unfiltered queryset must satisfy the portable refresh gate");

            let tick = handle
                .update()
                .await
                .expect("refresh tick must succeed under non-superuser RLS");

            // The first tick is a full scan (`since = None`). With RLS
            // active, only the 3 tenant-1000 rows are visible to the
            // fetcher's SELECT — anything else here means the auth is not
            // being applied to the fetcher's transaction.
            assert_eq!(
                tick.applied,
                3,
                "first refresh tick must apply exactly the 3 tenant-1000 rows; \
                 got {applied}",
                applied = tick.applied,
            );

            // Tenant-1000 ids must be resident; tenant-2000 ids must NOT
            // appear. The Punnu is keyed by `T::Id`, which for `pk = Serial`
            // is `i32`; the seeded ids are returned by `Model::create` via
            // `RETURNING id`.
            for id in &seeded.tenant_1000_ids {
                assert!(
                    punnu.get(id).is_some(),
                    "tenant-1000 row id={id} must be resident in the Punnu after \
                     the first tick; punnu len = {}",
                    punnu.len(),
                );
            }
            for id in &seeded.tenant_2000_ids {
                assert!(
                    punnu.get(id).is_none(),
                    "tenant-2000 row id={id} must NOT be resident in the Punnu \
                     under tenant-1000 auth — RLS leak indicates the fetcher's \
                     auto_set_tenant did not scope the GUC, or the connection \
                     was unexpectedly authenticated as the superuser",
                );
            }

            // ── Defensive: stop the periodic loop. (Prevents the helper
            // from leaving a background task alive after the test body
            // returns and the runtime is dropped.)
            handle.cancel();
        })
        .catch_unwind()
        .await;

        // Always drop the per-test DB so the orphan-cleanup helper does
        // not have to sweep it later. Run AFTER the test body so a panic
        // mid-body still reaches teardown.
        teardown_test_db(cleanup).await;

        // Surface any panic from the test body to the harness so the
        // failure is reported with the original payload.
        if let Err(panic_payload) = outcome {
            std::panic::resume_unwind(panic_payload);
        }
    });

    // Suppress the unused-const warn — referencing the role name proves
    // it remains in scope for adopters reading this test alongside
    // `connect_test_db_as_non_superuser`.
    let _role_name: &str = TEST_NON_SUPERUSER_ROLE;
}
