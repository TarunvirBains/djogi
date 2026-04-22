//! Phase 5.5 integration tests — auth substrate.
//!
//! Task 1 scope (this file, initially): `DjogiContext::with_auth` attaches
//! an `AuthContext` that can be read back via `ctx.auth()`.
//!
//! Later Phase 5.5 tasks extend this file (Task 4 password_hash_round_trips,
//! Task 10 auto_set_tenant_from_auth, Task 11 with_auth_insecurely_emits_warn).
//!
//! # Tracing log assertions (Task 11)
//!
//! Tests that assert on warn-log output install a `tracing_test` global
//! subscriber inline (via `tracing_test::internal`) rather than via the
//! `#[traced_test]` attribute macro. The reason: `#[djogi_test]` moves the
//! test body into an inner function, so any `logs_contain` local injected by
//! `#[traced_test]` into the outer function would be out of scope inside the
//! inner body. The inline pattern avoids the double-wrapping conflict while
//! still capturing all `tracing` events emitted during the test.

use djogi::auth::AuthContext;
use djogi::prelude::*;
use djogi_macros::model;

#[djogi::djogi_test]
async fn with_auth_attaches_and_reads_back(mut ctx: djogi::DjogiContext) {
    let auth = AuthContext::new(HeerId::from_i64(42).unwrap())
        .with_tenant("org_a")
        .with_scopes(vec!["read".into(), "write".into()]);
    let ctx = ctx.with_auth(auth.clone());
    let attached = ctx.auth().expect("auth attached");
    assert_eq!(attached.user_id, auth.user_id);
    assert_eq!(attached.tenant_id, Some("org_a".into()));
    assert!(attached.has_scope("read"));
}

#[cfg(feature = "auth-argon2")]
#[djogi::djogi_test]
async fn password_hash_round_trips_through_pg(mut ctx: djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS users_password_hash_roundtrip (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            password_hash TEXT NOT NULL
        )",
    )
    .await
    .expect("create table");

    let h = djogi::auth::PasswordHash::hash("s3cret").unwrap();
    ctx.raw_execute(
        "INSERT INTO users_password_hash_roundtrip (password_hash) VALUES ($1)",
        &[&h],
    )
    .await
    .expect("insert");

    let stored: djogi::auth::PasswordHash = ctx
        .raw_scalar(
            "SELECT password_hash FROM users_password_hash_roundtrip LIMIT 1",
            &[],
        )
        .await
        .expect("select");
    assert!(stored.verify("s3cret"));
    assert!(!stored.verify("wrong"));
}

// ── Task 10: auto-set_tenant from auth ────────────────────────────────────

/// Tenant-keyed model for Task 10 tests.
///
/// Declares `org_id` as its tenant column so that `set_tenant("org_id")`
/// drives the `app.tenant_id` GUC that the RLS policy checks.
#[model(table = "phase5_5_tenant_posts", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TenantPost {
    pub org_id: String,
    pub title: String,
}

async fn setup_tenant_posts(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS phase5_5_tenant_posts (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            org_id TEXT NOT NULL,
            title TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .await
    .expect("create phase5_5_tenant_posts table");

    ctx.raw_ddl("ALTER TABLE phase5_5_tenant_posts ENABLE ROW LEVEL SECURITY")
        .await
        .expect("enable RLS on phase5_5_tenant_posts");

    // Drop any previously created policy so the setup is idempotent across
    // test runs (Postgres has no `CREATE POLICY IF NOT EXISTS`).
    let _ = ctx
        .raw_ddl("DROP POLICY IF EXISTS phase5_5_tenant_posts_isolation ON phase5_5_tenant_posts")
        .await;

    ctx.raw_ddl(
        "CREATE POLICY phase5_5_tenant_posts_isolation
           ON phase5_5_tenant_posts
           USING (org_id = current_setting('app.tenant_id', true))",
    )
    .await
    .expect("create RLS policy on phase5_5_tenant_posts");
}

/// When `ctx.auth()` carries a `tenant_id`, the first CRUD/QuerySet
/// operation on a tenant-keyed model automatically calls
/// `ctx.__ensure_tenant_set_for_macros(tenant_id)` — the caller does not need to call
/// `ctx.set_tenant(...)` explicitly.
///
/// This test does NOT call `set_tenant` anywhere. It attaches an
/// `AuthContext` with `tenant_id = "org_a"` via `ctx.set_auth(...)` and
/// then calls `TenantPost::objects().fetch_all(ctx)`. The auto-wiring in
/// Task 10 should issue `ensure_tenant_set("org_a")` transparently before
/// the SQL executes, and `ctx.tenant_set` should be `true` afterwards.
#[djogi::djogi_test]
async fn auto_set_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            let auth = AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a");
            tx.set_auth(auth);
            // No explicit set_tenant call — auto-wiring must issue it.
            let posts = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(posts.len(), 0);
            // Auto-wiring must have set tenant_set = true.
            assert!(tx.tenant_set, "tenant_set must be true after auto-wiring");
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

/// When `ctx.auth()` is set but carries no `tenant_id`, the auto-wiring
/// must be a no-op. `ctx.tenant_set` stays `false` and no error is raised.
///
/// The RLS policy uses `current_setting('app.tenant_id', true)` with
/// `missing_ok = true` so an unset GUC returns NULL — the `=` predicate
/// treats NULL as unmatched, so the query succeeds but returns zero rows.
#[djogi::djogi_test]
async fn auto_set_tenant_no_op_when_tenant_id_none(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Auth present, but no tenant_id — auto-wiring must be a no-op.
            let auth = AuthContext::new(HeerId::from_i64(1).unwrap());
            tx.set_auth(auth);
            // Auto-wiring should not call set_tenant — no tenant_id to apply.
            let posts = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(posts.len(), 0);
            // tenant_set must remain false — nothing was auto-applied.
            assert!(
                !tx.tenant_set,
                "tenant_set must stay false when auth has no tenant_id"
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

// ── Task 11: warn-log emission tests ─────────────────────────────────────────

/// Helper: ensure the `tracing_test` global subscriber is installed and return
/// a closure that checks whether any log line emitted after this call contains
/// the given substring.
///
/// `tracing_test::internal::INITIALIZED` is a `Once` — safe to call from
/// multiple test threads; subsequent calls are no-ops. The global buffer
/// (`tracing_test::internal::global_buf()`) is append-only across the test
/// binary run, so we snapshot its current length before the action under test
/// and check only lines appended after the snapshot.
fn init_log_capture() -> usize {
    tracing_test::internal::INITIALIZED.call_once(|| {
        let buf = tracing_test::internal::global_buf();
        let mock_writer = tracing_test::internal::MockWriter::new(buf);
        let subscriber = tracing_test::internal::get_subscriber(mock_writer, "trace");
        // `set_global_default` on the `Dispatch` type sets it as the global
        // default dispatcher. This may silently no-op if already installed.
        tracing::dispatcher::set_global_default(subscriber).unwrap_or(()); // ignore if a default is already set
    });
    // Return the byte length of the buffer before the action under test so
    // `logs_since` can scope the search to new output only.
    tracing_test::internal::global_buf().lock().unwrap().len()
}

/// Return `true` if any line appended to the global log buffer since byte
/// offset `since` contains `needle`.
fn logs_since_contain(since: usize, needle: &str) -> bool {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    let text = std::str::from_utf8(&buf[since..]).unwrap_or("");
    text.lines().any(|line| line.contains(needle))
}

/// `with_auth_insecurely` must emit a warn-level log that includes the string
/// "auth guard bypassed via with_auth_insecurely".
#[djogi::djogi_test]
async fn with_auth_insecurely_emits_warn(mut ctx: djogi::DjogiContext) {
    let since = init_log_capture();
    let auth = AuthContext::new(HeerId::from_i64(1).unwrap());
    let _ctx = ctx.with_auth_insecurely(auth);
    assert!(
        logs_since_contain(since, "auth guard bypassed via with_auth_insecurely"),
        "expected warn log from with_auth_insecurely"
    );
}

/// `set_auth_insecurely` must emit a warn-level log that includes the string
/// "auth guard bypassed via set_auth_insecurely".
#[djogi::djogi_test]
async fn set_auth_insecurely_emits_warn(mut ctx: djogi::DjogiContext) {
    let since = init_log_capture();
    let auth = AuthContext::new(HeerId::from_i64(1).unwrap());
    ctx.set_auth_insecurely(auth);
    assert!(
        logs_since_contain(since, "auth guard bypassed via set_auth_insecurely"),
        "expected warn log from set_auth_insecurely"
    );
}

/// When auth is present but `tenant_id` is `None` on a tenant-keyed model,
/// `auto_set_tenant` must emit a warn-level log containing "auth attached but
/// tenant_id is None". The tenant_set flag must remain `false`.
#[djogi::djogi_test]
async fn auto_set_tenant_warns_when_tenant_id_missing_on_tenant_keyed_model(
    mut ctx: djogi::DjogiContext,
) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    let since = init_log_capture();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Auth with no tenant_id on a tenant-keyed model — warn must fire.
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()));
            let _ = TenantPost::objects().fetch_all(tx).await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
    assert!(
        logs_since_contain(since, "auth attached but tenant_id is None"),
        "expected cross-tenant warn log when tenant_id is None"
    );
}

/// When `ctx.set_no_tenant_scope()` is called, the cross-tenant warn must be
/// suppressed even when auth is present and `tenant_id` is `None`.
#[djogi::djogi_test]
async fn auto_set_tenant_no_warn_with_no_tenant_scope_opt_out(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    let since = init_log_capture();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()));
            tx.set_no_tenant_scope();
            let _ = TenantPost::objects().fetch_all(tx).await?;
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
    assert!(
        !logs_since_contain(since, "auth attached but tenant_id is None"),
        "cross-tenant warn must be suppressed when set_no_tenant_scope() is called"
    );
}

/// Regression guard for the Task 10 stale-tenant bug surfaced by the Codex
/// stop-gate review of `f393a87`: when auth changes inside one `atomic()`
/// scope from `org_a` to `org_b`, the second CRUD must run under `org_b` —
/// not leak through under the sticky `SET LOCAL app.tenant_id = 'org_a'`
/// from the first set.
///
/// The fix tracks `applied_tenant_id: Option<String>` on `DjogiContext` and
/// re-issues `SET LOCAL` whenever the auto-wiring requests a different tid.
/// Before the fix, `ensure_tenant_set` short-circuited on a plain
/// `tenant_set: bool`, causing silent cross-tenant reads.
/// Regression guard for the Task 10 stale-tenant bug surfaced by the Codex
/// stop-gate review of `f393a87`. Proves the `applied_tenant_id` field
/// transitions correctly through three scenarios:
///
/// 1. First `set_tenant("org_a")` populates `applied_tenant_id`.
/// 2. A second `set_tenant("org_b")` updates it to `"org_b"` — so any
///    subsequent `ensure_tenant_set("org_a")` would detect the mismatch
///    and re-issue SET LOCAL.
/// 3. `ensure_tenant_set(tid)` is a no-op when `tid == applied_tenant_id`
///    (same-tenant short-circuit preserved after the fix).
/// 4. `ensure_tenant_set(tid)` re-issues when `tid != applied_tenant_id`
///    (the actual bug fix).
///
/// Before the fix, `ensure_tenant_set` short-circuited on a plain
/// `tenant_set: bool`, causing step 4 to silently no-op and leaving
/// `SET LOCAL app.tenant_id = 'org_a'` in force even after auth had
/// changed to `org_b` — a cross-tenant read bug.
///
/// This is a mechanism-level assertion rather than an end-to-end RLS
/// filtering assertion. Phase 5's `set_tenant_rls_isolates_tenants`
/// test (`tests/integration/phase5_postgres_native.rs`) covers the
/// RLS filtering path via a restricted service-account role; the fix
/// here is at the `DjogiContext` state level, and the per-field
/// inspection test captures it with zero extra setup.
#[djogi::djogi_test]
async fn ensure_tenant_set_switches_on_tid_change(mut ctx: djogi::DjogiContext) {
    let pool = ctx.pool().expect("pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // (1) First set.
            tx.set_tenant("org_a").await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_a"),
                "applied_tenant_id must be Some(\"org_a\") after first set_tenant",
            );

            // (2) Switch tenants via set_tenant directly.
            tx.set_tenant("org_b").await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_b"),
                "applied_tenant_id must update to \"org_b\" after a second set_tenant",
            );

            // (3) Same-tenant ensure is a no-op (preserves the short-circuit).
            tx.__ensure_tenant_set_for_macros("org_b").await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_b"),
                "ensure_tenant_set with the same tid must no-op",
            );

            // (4) Different-tenant ensure re-issues (THE bug fix). Before
            // the fix, a `tenant_set: bool` short-circuit would leave
            // applied_tenant stuck on \"org_b\" here.
            tx.__ensure_tenant_set_for_macros("org_c").await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_c"),
                "ensure_tenant_set must re-issue SET LOCAL when tid differs \
                 (Task 10 fixup — stale-tenant leak regression)",
            );

            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

// ── Phase-boundary fixups (2026-04-22 Codex stop-gate of phase-5-5-auth) ──

/// Regression guard for **Blocker 2** from the Codex phase-boundary review
/// of the full `phase-5-5-auth` branch.
///
/// Scenario: auth carrying `tenant_id = Some("org_a")` is swapped mid-
/// transaction for auth with `tenant_id = None` (or, equivalently, the
/// user calls `set_auth(auth_without_tenant)`). Before the fix, the
/// previously-applied `SET LOCAL app.tenant_id = 'org_a'` stayed in force
/// and subsequent tenant-keyed queries silently leaked across tenants.
///
/// After the fix, the auto-tenant wiring calls `ctx.clear_tenant()` in the
/// `None` arm when `applied_tenant_id.is_some()`, issuing
/// `SELECT set_config('app.tenant_id', '', true)` and resetting the
/// in-memory tracker. This test observes `applied_tenant_id()` transitions
/// through `None → Some("org_a") → None` to verify the clear fires.
#[djogi::djogi_test]
async fn auto_set_tenant_clears_applied_tid_when_auth_loses_tenant(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // (1) Auth with tenant → first fetch applies `org_a`.
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
            let _ = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(tx.applied_tenant_id(), Some("org_a"));

            // (2) Swap to auth without tenant → next fetch must CLEAR
            // `applied_tenant_id` (and issue SET LOCAL to reset the GUC).
            // Opt into the no-tenant-scope suppress flag to keep the
            // warn path quiet for this targeted assertion.
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()));
            tx.set_no_tenant_scope();
            let _ = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(
                tx.applied_tenant_id(),
                None,
                "applied_tenant_id must be cleared when auth loses tenant_id \
                 on a tenant-keyed model",
            );
            assert!(
                !tx.tenant_set,
                "tenant_set must be false after clear_tenant reset",
            );

            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

/// Regression guard for **Blocker 3** from the Codex phase-boundary review.
///
/// `bulk_create` builds `INSERT ... RETURNING ...` SQL directly and executes
/// via `ctx.__query_all_for_macros` — before the fix, the macro did NOT
/// prepend the `auto_set_tenant` snippet, so auth-bound bulk inserts on a
/// tenant-keyed model hit RLS-backed tables without establishing the
/// `app.tenant_id` GUC.
///
/// After the fix, the macro emission for `bulk_create` (and `bulk_upsert`)
/// prepends the snippet just like `get` / `create` / `save` / `delete` /
/// `refresh` do. This test verifies `applied_tenant_id()` gets set when
/// a tenant-bound auth context runs `bulk_create`.
#[djogi::djogi_test]
async fn bulk_create_auto_sets_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
            // Bulk-insert one row under org_a. Before the fix the bulk
            // path would execute with no SET LOCAL, so applied_tenant_id
            // would stay None.
            let _ = TenantPost::bulk_create(
                tx,
                vec![TenantPost {
                    org_id: "org_a".to_string(),
                    title: "bulk-one".to_string(),
                    ..Default::default()
                }],
            )
            .await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_a"),
                "bulk_create on a tenant-keyed model must auto-set tenant \
                 when auth.tenant_id is Some",
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

/// Regression guard for **Blocker 1** from the Codex phase-boundary review.
///
/// A nested `atomic()` savepoint that sets a different tenant (or changes
/// `auth` state) and then rolls back must leave the outer scope's
/// auth-related trackers pristine. Before the fix, the nested path shared
/// `&mut DjogiContext` with the closure and snapshotted only the
/// `on_commit` queue — so an inner `set_tenant("org_b")` would leave
/// `self.applied_tenant_id = Some("org_b")` after `ROLLBACK TO SAVEPOINT`
/// reverted the inner `SET LOCAL`, breaking the `ensure_tenant_set`
/// re-issue contract on subsequent outer-scope queries.
///
/// The fix snapshots `auth` / `applied_tenant_id` / `tenant_set` /
/// `tenant_scope_suppressed` at entry and restores them on Err + panic
/// paths in `djogi/src/transaction.rs`'s nested impl.
#[djogi::djogi_test]
async fn nested_atomic_rollback_restores_auth_state(mut ctx: djogi::DjogiContext) {
    let pool = ctx.pool().expect("pool-backed").clone();
    djogi::transaction::atomic(&pool, |outer| {
        Box::pin(async move {
            // Outer scope: set tenant to org_a AND attach an outer AuthContext
            // so we can verify auth restoration as well as tenant state
            // restoration (Codex re-review noted B1 tested only tenant, not
            // auth — expand the scenario to prove both pieces of the
            // snapshot/restore contract).
            outer.set_tenant("org_a").await?;
            outer.set_auth(AuthContext::new(HeerId::from_i64(42).unwrap()).with_tenant("org_a"));
            assert_eq!(outer.applied_tenant_id(), Some("org_a"));
            assert_eq!(
                outer.auth().map(|a| a.user_id),
                Some(HeerId::from_i64(42).unwrap())
            );

            // Nested scope that changes tenant AND replaces auth with a
            // different user, then returns Err to trigger ROLLBACK TO
            // SAVEPOINT. The outer scope should see both applied_tenant_id
            // AND auth restored to their pre-nested values.
            let nested_res: Result<(), djogi::DjogiError> =
                djogi::transaction::atomic(&mut *outer, |inner| {
                    Box::pin(async move {
                        inner.set_tenant("org_b").await?;
                        inner.set_auth(
                            AuthContext::new(HeerId::from_i64(99).unwrap()).with_tenant("org_b"),
                        );
                        assert_eq!(inner.applied_tenant_id(), Some("org_b"));
                        assert_eq!(
                            inner.auth().map(|a| a.user_id),
                            Some(HeerId::from_i64(99).unwrap()),
                        );
                        // Force a rollback by returning an error. Use a
                        // Validation error so it's classified as non-
                        // transient (no retry).
                        Err::<(), _>(djogi::DjogiError::Validation(
                            "intentional rollback".to_string(),
                        ))
                    })
                })
                .await;

            assert!(nested_res.is_err(), "inner atomic must have rolled back");

            // After ROLLBACK TO SAVEPOINT the Postgres GUC reverted to
            // 'org_a'. applied_tenant_id must match — otherwise a follow-
            // up `ensure_tenant_set("org_a")` would short-circuit on the
            // stale `Some("org_b")` tracker while the real GUC is 'org_a'.
            // That's the exact bug Codex flagged; the snapshot/restore
            // dance in transaction.rs is what keeps the invariant.
            assert_eq!(
                outer.applied_tenant_id(),
                Some("org_a"),
                "nested atomic rollback must restore outer applied_tenant_id",
            );
            assert!(
                outer.tenant_set,
                "nested atomic rollback must restore outer tenant_set",
            );
            assert_eq!(
                outer.auth().map(|a| a.user_id),
                Some(HeerId::from_i64(42).unwrap()),
                "nested atomic rollback must restore outer auth (user_id)",
            );
            assert_eq!(
                outer.auth().and_then(|a| a.tenant_id.clone()),
                Some("org_a".to_string()),
                "nested atomic rollback must restore outer auth.tenant_id",
            );

            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

/// Additional coverage for **Blocker 3** surfaced in the Codex re-review of
/// `3083c3b`: `create_with_id` also builds SQL directly and must carry the
/// `#auto_set_tenant` snippet. Mirror of
/// `bulk_create_auto_sets_tenant_from_auth` for the single-row
/// pre-allocated-id path. `create_with_id` is emitted for every HeerId-PK
/// model (TenantPost qualifies by default).
#[djogi::djogi_test]
async fn create_with_id_auto_sets_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
            // Pre-allocate an id and use create_with_id. Before the fix,
            // this path emitted SQL with no auth→tenant hookup and would
            // leave applied_tenant_id == None for auth-bound callers.
            // Deterministic id is fine — #[djogi_test] gives each test a
            // fresh DB so there's no collision with other tests.
            let preallocated = HeerId::from_i64(987654321).unwrap();
            let _ = TenantPost::create_with_id(
                tx,
                preallocated,
                TenantPost {
                    org_id: "org_a".to_string(),
                    title: "create-with-id-one".to_string(),
                    ..Default::default()
                },
            )
            .await?;
            assert_eq!(
                tx.applied_tenant_id(),
                Some("org_a"),
                "create_with_id on a tenant-keyed model must auto-set tenant \
                 when auth.tenant_id is Some",
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}
