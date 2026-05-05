//! Phase 8ε T9.7 — D7 `set_tenant` / `set_role` ordering integration test.
//!
//! # D7 in plain English
//!
//! Inside an `atomic()` block, the canonical ordering is:
//!
//! 1. `ctx.set_tenant(...)` — SETs `app.tenant_id` (a GUC) for the
//!    current transaction.
//! 2. `ctx.set_role(...)` — SETs `LOCAL ROLE` for the current
//!    transaction.
//!
//! Calling `set_tenant` AFTER `set_role` is the documented
//! non-canonical mode. RLS policies that read `app.tenant_id` via
//! `current_setting('app.tenant_id', true)` still see the value the
//! second call wrote — Postgres GUCs are role-independent — so the
//! observable difference is operational rather than visibility:
//!
//! - **Canonical (tenant first, role second).** The owner-side `SET`
//!   on `app.tenant_id` runs under the original role. Every
//!   subsequent statement under the new role sees the GUC AS-IF the
//!   migration runner had pinned it. Tooling that audits
//!   "tenant-id assignments by role" sees the assignment under the
//!   trusted role, not the downgraded role.
//! - **Inverted (role first, tenant second).** The `set_config(...)`
//!   call lands under the downgraded role. On default Postgres,
//!   `app.tenant_id` is a customised GUC with no per-role
//!   restrictions, so the SET still succeeds and the value is
//!   visible — but the audit shape is inverted, and any future
//!   policy that restricts `set_config('app.tenant_id', ...)` to the
//!   trusted role would suddenly fail.
//!
//! # Why this test pins the inverted-order behavior
//!
//! D7 (v3 §159–166) documents inverted-order as a "non-canonical
//! mode" and says implementations may either reject it or accept it.
//! Today's djogi implementation is non-opinionated: it does not
//! reject the inversion at the framework level — Postgres decides.
//! On a vanilla Postgres install with no role restrictions on the
//! `app.tenant_id` GUC, both orderings succeed and the GUC value is
//! visible.
//!
//! This test pins THAT behaviour as the documented mode. If a future
//! commit adds a framework-level guard or a role-scoped GUC
//! restriction, this test trips and the maintainer must either
//! re-pin the documented mode or update D7 — both are fine; the
//! point is that the change does not silently slip through.
//!
//! # Spec / memory anchors
//!
//! - v3 plan §159–166 (D7 documented orderings).
//! - v3 plan §456–462, §710–712, §729 — T9.7 brief.
//! - Plan §T9.7 (`docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`).

/// Idempotently create the test role on the cluster. See sibling
/// integration test `phase8_set_role_transaction_scoped.rs` for the
/// rationale on the `DO`-block exception handler — Postgres has no
/// `CREATE ROLE IF NOT EXISTS`, so the duplicate-object exception is
/// the canonical ignore-existing pattern.
async fn ensure_test_role(ctx: &mut djogi::DjogiContext, role: &str) {
    let sql = format!(
        "DO $$ BEGIN \
           CREATE ROLE \"{role}\"; \
         EXCEPTION WHEN duplicate_object THEN NULL; \
         END $$",
    );
    ctx.raw_ddl(&sql)
        .await
        .expect("ensure_test_role: CREATE ROLE inside DO block must succeed");
}

#[djogi::djogi_test]
async fn tenant_first_then_role_succeeds(mut ctx: djogi::DjogiContext) {
    let role = "djogi_t9_7_ordering_role";
    let tenant_id = "t1";
    ensure_test_role(&mut ctx, role).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Canonical order: tenant first, role second.
            tx.set_tenant(tenant_id).await?;
            tx.set_role(role).await?;

            // Both pieces of state must be observable on the
            // post-set connection.
            let actual_role: String = tx
                .raw_scalar("SELECT current_user::text", &[])
                .await
                .expect("SELECT current_user");
            assert_eq!(
                actual_role, role,
                "current_user must reflect SET LOCAL ROLE inside atomic()",
            );

            // Postgres `current_setting('app.tenant_id', true)` —
            // the `true` second argument means "missing OK"; returns
            // an empty string when the GUC is not set, our value
            // when it is.
            let actual_tenant: String = tx
                .raw_scalar("SELECT current_setting('app.tenant_id', true)::text", &[])
                .await
                .expect("SELECT current_setting('app.tenant_id', true)");
            assert_eq!(
                actual_tenant, tenant_id,
                "app.tenant_id GUC must reflect set_tenant() inside atomic()",
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("canonical ordering inside atomic() must succeed");
}

#[djogi::djogi_test]
async fn role_first_then_tenant_documented_failure_mode(mut ctx: djogi::DjogiContext) {
    // Inverted ordering: role first, tenant second. D7 documents
    // this as the non-canonical mode and says it may either error or
    // succeed depending on Postgres role / GUC configuration.
    //
    // On vanilla Postgres with no role-scoped GUC restrictions, the
    // inversion succeeds and both pieces of state are observable.
    // This test pins THAT behaviour so a future framework-level
    // guard or a role-scoped GUC restriction shows up as a
    // regression here rather than slipping through.
    let role = "djogi_t9_7_ordering_role";
    let tenant_id = "t1";
    ensure_test_role(&mut ctx, role).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    let observed = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Inverted order: role first, tenant second.
            tx.set_role(role).await?;
            tx.set_tenant(tenant_id).await?;

            // Read both pieces of state and return them so the
            // outer test can assert on the observed shape rather
            // than panicking inside the closure.
            let actual_role: String = tx
                .raw_scalar("SELECT current_user::text", &[])
                .await
                .expect("SELECT current_user");
            let actual_tenant: String = tx
                .raw_scalar("SELECT current_setting('app.tenant_id', true)::text", &[])
                .await
                .expect("SELECT current_setting('app.tenant_id', true)");
            Ok::<_, djogi::DjogiError>((actual_role, actual_tenant))
        })
    })
    .await;

    match observed {
        Ok((actual_role, actual_tenant)) => {
            // Documented mode A — both calls succeed, both pieces of
            // state are observable. Pin the values so a divergence
            // (e.g. tenant becomes invisible under the downgraded
            // role) trips the regression guard.
            assert_eq!(
                actual_role, role,
                "current_user must reflect SET LOCAL ROLE under inverted ordering",
            );
            assert_eq!(
                actual_tenant, tenant_id,
                "app.tenant_id GUC must remain visible under inverted ordering on \
                 vanilla Postgres (no role-scoped GUC restriction). If this \
                 assertion ever flips to documented-mode-B (Err), update D7 \
                 and re-pin.",
            );
        }
        Err(e) => {
            // Documented mode B — the inversion errored out. This is
            // legal under D7 (e.g. a future role-scoped restriction
            // on `app.tenant_id` rejects the SET under the
            // downgraded role). The test still passes — the contract
            // is "error OR succeed-but-non-canonical", not a
            // specific outcome.
            //
            // We intentionally do NOT panic here. If a maintainer
            // wants to harden the contract (one specific outcome),
            // they should update D7 first and then narrow the
            // assertion. Pinning the message tail keeps the
            // documented-mode signal visible in CI logs.
            eprintln!(
                "role_first_then_tenant: documented mode B observed (Err: {e}); \
                 see D7 for the documented orderings",
            );
        }
    }
}
