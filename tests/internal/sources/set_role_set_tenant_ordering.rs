// .7 — D7 `set_tenant` / `set_role` ordering integration test.
//
// # D7 in plain English
//
// Inside an `atomic()` block, the canonical ordering is:
//
// 1. `ctx.set_tenant(...)` — SETs `app.tenant_id` (a GUC) for the
//    current transaction.
// 2. `ctx.set_role(...)` — SETs `LOCAL ROLE` for the current
//    transaction.
//
// Calling `set_tenant` AFTER `set_role` is the documented
// non-canonical mode. RLS policies that read `app.tenant_id` via
// `current_setting('app.tenant_id', true)` still see the value the
// second call wrote — Postgres GUCs are role-independent — so the
// observable difference is operational rather than visibility:
//
// - **Canonical (tenant first, role second).** The owner-side `SET`
//   on `app.tenant_id` runs under the original role. Every
//   subsequent statement under the new role sees the GUC AS-IF the
//   migration runner had pinned it. Tooling that audits
//   "tenant-id assignments by role" sees the assignment under the
//   trusted role, not the downgraded role.
// - **Inverted (role first, tenant second).** The `set_config(...)`
//   call lands under the downgraded role. On default Postgres,
//   `app.tenant_id` is a customised GUC with no per-role
//   restrictions, so the SET still succeeds and the value is
//   visible — but the audit shape is inverted, and any future
//   policy that restricts `set_config('app.tenant_id', ...)` to the
//   trusted role would suddenly fail.
//
// # Why this test pins the inverted-order behavior
//
// D7 (v3 §159–166) documents inverted-order as a "non-canonical
// mode" and says implementations may either reject it or accept it.
// Today's djogi implementation is non-opinionated: it does not
// reject the inversion at the framework level — Postgres decides.
// On a vanilla Postgres install with no role restrictions on the
// `app.tenant_id` GUC, both `set_role` and `set_tenant` succeed
// under the inverted ordering and both pieces of state are
// observable.
//
// **Pinned vanilla behaviour (current):** `Ok((actual_role,
// actual_tenant))` where `actual_role == role` and `actual_tenant ==
// tenant_id`. This is documented-mode-A in D7 terms.
//
// The test uses `expect(...)` on the observed result rather than a
// broad `match`-with-log-and-pass. Any `Err` outcome — regardless
// of variant — fails the test, surfacing either:
//   - a future framework-level guard that rejects role-before-tenant;
//   - a future role-scoped GUC restriction on `app.tenant_id` that
//     fails under the downgraded role; or
//   - any unrelated regression in `set_role` / `set_tenant` /
//     `atomic()`.
//
// When such a contract change lands, the maintainer must update D7
// to record the new pinned mode (e.g. mode-B with a specific
// `DjogiError` variant) and re-pin this assertion accordingly. The
// point is that the change does not silently slip through.
//
// # Spec / memory anchors
//
// - v3 plan §159–166 (D7 documented orderings).
// - v3 plan §456–462, §710–712, §729 — .7 brief.
// - The set-role / tenant-ordering design notes.

/// Idempotently create the test role on the cluster. See sibling
/// integration test `set_role_transaction_scoped.rs` for the
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

// Serialized against `role_first_then_tenant_documented_failure_mode`: both
// tests share the `ordering_role` cluster role. Without serialization the
// concurrent `CREATE ROLE` inside the DO-block exception handler can raise a
// raw `unique_violation` (SQLSTATE 23505 on `pg_authid_rolname_index`)
// instead of the expected `duplicate_object` (SQLSTATE 42710), bypassing the
// `WHEN duplicate_object THEN NULL` catch clause.
#[djogi::djogi_test]
#[serial_test::serial]
async fn tenant_first_then_role_succeeds(mut ctx: djogi::DjogiContext) {
    let role = "ordering_role";
    let tenant_id = "t1";
    ensure_test_role(&mut ctx, role).await;

    let pool = ctx.raw_pool().expect("test ctx must be pool-backed").clone();
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

// Serialized against `tenant_first_then_role_succeeds` for the same shared-role
// reason above.
#[djogi::djogi_test]
#[serial_test::serial]
async fn role_first_then_tenant_documented_failure_mode(mut ctx: djogi::DjogiContext) {
    // Inverted ordering: role first, tenant second. D7 documents
    // this as the non-canonical mode and says it may either error or
    // succeed depending on Postgres role / GUC configuration.
    //
    // Pinned vanilla behaviour: `Ok((role, tenant_id))`. Any `Err`
    // is a regression — see the module-level doc comment for the
    // re-pinning workflow.
    let role = "ordering_role";
    let tenant_id = "t1";
    ensure_test_role(&mut ctx, role).await;

    let pool = ctx.raw_pool().expect("test ctx must be pool-backed").clone();
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

    // Pinned: vanilla Postgres returns Ok for inverted ordering. Any
    // Err is a regression — either a new framework-level guard
    // rejecting the inversion, or a role-scoped GUC restriction on
    // `app.tenant_id` that fails under the downgraded role. Either
    // case is a contract change that needs an explicit D7 update +
    // re-pin, NOT a silent pass.
    //
    // We intentionally use `expect(...)` (rather than a broad
    // `if let Err(_) = ...` log-and-pass) so the wrong-error-class
    // regression cannot hide behind a generic catch-all.
    let (actual_role, actual_tenant) = observed.expect(
        "role_first_then_tenant currently succeeds on vanilla Postgres — \
         if this Err arm is reached, a future framework guard or a \
         role-scoped GUC restriction has changed the ordering contract; \
         update D7 (v3 §159–166) to document the new pinned mode and \
         re-pin this assertion accordingly",
    );

    // Pin the values so a divergence (e.g. tenant becomes invisible
    // under the downgraded role) trips the regression guard.
    assert_eq!(
        actual_role, role,
        "current_user must reflect SET LOCAL ROLE under inverted ordering",
    );
    assert_eq!(
        actual_tenant, tenant_id,
        "app.tenant_id GUC must remain visible under inverted ordering on \
         vanilla Postgres (no role-scoped GUC restriction)",
    );
}
