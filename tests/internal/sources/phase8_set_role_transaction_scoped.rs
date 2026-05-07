// Phase 8ε T9.7 — `DjogiContext::set_role` transaction-scoping integration tests.
//
// # Scope
//
// These tests cover the two transaction-scoping arms of
// `DjogiContext::set_role` end-to-end against a real Postgres
// database, complementing the unit-level validation tests in
// `djogi/src/context.rs::tests` (which run without a live DB and
// cover the byte-level identifier validator).
//
// 1. `set_role_inside_atomic_succeeds` — the canonical happy path.
//    Inside an `atomic()` block, `set_role` issues `SET LOCAL ROLE
//    "<role>"` and `current_user` reflects the new role for the
//    remainder of the transaction.
// 2. `set_role_outside_atomic_returns_error` — a pool-backed context
//    is not transaction-scoped, so `set_role` MUST refuse with
//    `DjogiError::SetRoleOutsideTransaction` BEFORE touching SQL.
//
// # Test-DB role provisioning
//
// Postgres requires the role to exist before `SET LOCAL ROLE` will
// accept it. We CREATE the role inside the per-test database before
// opening the `atomic()` scope. Postgres does NOT support
// `CREATE ROLE IF NOT EXISTS`, so we wrap the DDL in a `DO $$ ...
// EXCEPTION WHEN duplicate_object THEN NULL; END $$` block — that
// handles parallel test runs without flaking on "role already
// exists".
//
// Roles are cluster-scoped on Postgres, so the role outlives the
// per-test database. That is acceptable: the role name is unique
// enough (`djogi_t9_7_test_role`) that parallel CI runs don't
// collide on each other, and a leftover role on the cluster is
// benign — it grants no privileges by default.
//
// # Spec / memory anchors
//
// - v3 plan §456–462, §710–712, §729 — T9.7 brief.
// - Plan §T9.7 (`docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`).
// - `feedback_djogi_local_postgres.md` — `#[djogi_test]` provisions a
//   fresh DB per test.

use djogi::DjogiError;

/// Idempotently create the test role on the cluster. The `DO` block
/// wraps `CREATE ROLE` in an exception handler so a duplicate-object
/// error from a peer test (or a leftover from a previous run) is
/// silently ignored. `CREATE ROLE IF NOT EXISTS` does not exist in
/// Postgres for the ROLE object kind — only for tables, types, etc.
async fn ensure_test_role(ctx: &mut djogi::DjogiContext, role: &str) {
    // The role name is interpolated into a quoted SQL identifier
    // below. The validator that protects production
    // `DjogiContext::set_role` runs the same byte-level check we
    // would otherwise reproduce here; we keep the test fixture role
    // name a plain identifier so neither side sees an injection
    // shape. The identifier is also embedded inside a SQL string
    // literal that closes a `$$`-delimited dollar-quoted block —
    // any embedded `$$` would terminate the block early. The
    // hard-coded role name (`djogi_t9_7_test_role`) cannot contain
    // `$$`, so this is safe.
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
async fn set_role_inside_atomic_succeeds(mut ctx: djogi::DjogiContext) {
    let role = "djogi_t9_7_test_role";
    ensure_test_role(&mut ctx, role).await;

    let pool = ctx.raw_pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // The validator and the SQL gate must both succeed.
            tx.set_role(role).await?;

            // Round-trip check — `current_user` reflects the role
            // for the remainder of this transaction. We use
            // `raw_scalar` rather than the typed query path because
            // we are asserting on a Postgres GUC, not a model row.
            let actual: String = tx
                .raw_scalar("SELECT current_user::text", &[])
                .await
                .expect("SELECT current_user");
            assert_eq!(
                actual, role,
                "current_user should reflect SET LOCAL ROLE inside atomic()",
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("atomic block with set_role must succeed");

    // After `atomic()` commits, the connection returns to the pool
    // with the role cleared (`SET LOCAL` is bound to the
    // transaction). A fresh raw query against the same pool should
    // observe the original `djogi` role, not the test role. We
    // assert on the inequality rather than the literal pre-role so
    // the test stays robust against future changes to the harness's
    // base role.
    let after: String = ctx
        .raw_scalar("SELECT current_user::text", &[])
        .await
        .expect("SELECT current_user post-atomic");
    assert_ne!(
        after, role,
        "current_user must NOT leak the per-transaction role onto a pooled connection",
    );
}

#[djogi::djogi_test]
async fn set_role_outside_atomic_returns_error(mut ctx: djogi::DjogiContext) {
    // Pool-backed context — no transaction scope. The
    // `_execute_savepoint_unchecked`-style discriminant check inside
    // `set_role` MUST refuse before any SQL is issued.
    //
    // The role name is well-formed (it would pass the validator),
    // and it exists on the cluster (we provision it from a
    // transaction in a sibling test if cluster state is shared).
    // Both facts let us isolate the discriminant arm — we are
    // testing the pool-vs-transaction guard, not validation, and
    // not a missing-role error.
    match ctx.set_role("djogi_t9_7_test_role").await {
        Err(DjogiError::SetRoleOutsideTransaction) => {}
        other => panic!(
            "expected DjogiError::SetRoleOutsideTransaction on pool-backed ctx, got {other:?}",
        ),
    }
}
