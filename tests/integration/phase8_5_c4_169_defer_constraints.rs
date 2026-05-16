// Phase 8.5 Cluster 4 issue #169 — typed `SET CONSTRAINTS DEFERRED /
// IMMEDIATE` surface (`DjogiContext::defer_constraints` +
// `set_constraints_immediate` + `DeferScope` enum).
//
// # Scope
//
// Cover the five live-PG behaviours of the typed defer-constraints
// surface:
//
// 1. **`DeferScope::All` defers every deferrable constraint in the
//    open transaction.** Emits `SET CONSTRAINTS ALL DEFERRED` and
//    runs to completion against a model with one deferrable FK.
//
// 2. **`DeferScope::Named(&["..."])` defers a single named
//    constraint** after validating the name against the model
//    descriptor inventory.
//
// 3. **Unknown name validation** — `Named` with a name that does
//    not match any registered FK raises
//    `DjogiError::UnknownConstraintName` BEFORE the SQL flies.
//
// 4. **`set_constraints_immediate` reverses the flip mid-transaction.**
//    Mirror of `defer_constraints`; same pool-rejection invariant.
//
// 5. **Pool-backed rejection** — calling either helper on a pool-
//    backed context raises
//    `DjogiError::ConstraintModeOutsideTransaction`.
//
// The compile-time pin for `DeferScope::All` against a model with
// no FKs lives in
// `tests/integration/phase8_5_c3_110_dogfood_round2.rs::cat3_c_defer_constraints_typed_surface`;
// this file proves the typed surface against a model that ACTUALLY
// declares a deferrable FK.
//
// # Spec / memory anchors
//
// - djogi#169 issue body (closing-condition checklist).
// - `docs/guide/transactions.md` §"Deferred constraints —
//   `defer_constraints`".
// - `feedback_djogi_local_postgres.md` — `#[djogi_test]` provisions a
//   fresh DB per test.

use djogi::prelude::*;
use djogi::transaction::{DeferScope, atomic};

// `DeferNode` declares a self-FK marked
// `#[field(deferrable, initially_deferred)]` so the constraint is
// `DEFERRABLE INITIALLY DEFERRED` at DDL time — the canonical
// shape for the circular-FK use case. The framework names the
// constraint via the standard convention:
// `<table>_<column>_fkey = djogi_defer_nodes_peer_id_fkey`.
#[model(table = "djogi_defer_nodes", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct DeferNode {
    pub label: String,
    /// Self-FK declared deferrable + initially deferred. Inserts
    /// with a peer_id pointing at a not-yet-existing row are
    /// accepted; Postgres checks the FK at COMMIT.
    #[field(deferrable, initially_deferred)]
    pub peer_id: Option<ForeignKey<DeferNode>>,
}

#[djogi::djogi_test(sync_models = [DeferNode])]
async fn defer_constraints_all_with_circular_cycle(mut ctx: djogi::DjogiContext) {
    // The canonical use case: pre-allocate two IDs, insert two rows
    // whose `peer_id` columns point at each other (forming a cycle),
    // and commit. The `initially_deferred` flag pushes the FK check
    // to COMMIT — at that moment both peers exist and the cycle
    // resolves cleanly. `defer_constraints(All)` is called inside
    // the transaction to exercise the typed surface end-to-end;
    // with `initially_deferred = true` it composes a SET CONSTRAINTS
    // ALL DEFERRED that is technically a no-op (the FK is already
    // deferred) but still pins the SQL emission path.
    atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Exercise the typed surface — emits
            // `SET CONSTRAINTS ALL DEFERRED`.
            ctx.defer_constraints(DeferScope::All).await?;

            // Pre-allocate IDs so we can construct a cycle: each
            // row's peer_id references the OTHER row's id, which
            // wouldn't exist at insert time without ID pre-
            // generation.
            let id_a = djogi::HeerId::generate(ctx).await?;
            let id_b = djogi::HeerId::generate(ctx).await?;

            // Insert node-a — its peer_id points at id_b, which
            // does NOT yet exist as a row. Postgres would normally
            // FK-violate; the INITIALLY DEFERRED + SET CONSTRAINTS
            // ALL DEFERRED combination pushes the check to COMMIT.
            DeferNode::create_with_id(
                ctx,
                id_a,
                DeferNode {
                    label: "node-a".to_string(),
                    peer_id: Some(ForeignKey::new(id_b)),
                    ..Default::default()
                },
            )
            .await?;

            // Insert node-b — its peer_id points at id_a, which
            // DOES now exist. No FK violation at this statement;
            // the deferred check at COMMIT will see both peers.
            DeferNode::create_with_id(
                ctx,
                id_b,
                DeferNode {
                    label: "node-b".to_string(),
                    peer_id: Some(ForeignKey::new(id_a)),
                    ..Default::default()
                },
            )
            .await?;

            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("defer_constraints(All) + cycle inserts must commit");

    let count = DeferNode::objects()
        .count(&mut ctx)
        .await
        .expect("post-commit count");
    assert_eq!(count, 2, "both rows must persist after the cycle commits");
}

#[djogi::djogi_test(sync_models = [DeferNode])]
async fn defer_constraints_named_validates_and_emits(mut ctx: djogi::DjogiContext) {
    // `DeferScope::Named` targets specific constraints. The
    // descriptor-inventory validator must accept the conventional
    // name `<table>_<column>_fkey` for the declared deferrable FK.
    atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // The constraint name follows the standard FK convention:
            //   `<table>_<column>_fkey` = `djogi_defer_nodes_peer_id_fkey`
            // The validator routes through
            // `crate::migrate::sql::fk_constraint_name`, so this
            // exact name must round-trip the inventory lookup
            // successfully.
            ctx.defer_constraints(DeferScope::Named(&["djogi_defer_nodes_peer_id_fkey"]))
                .await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("defer_constraints(Named) on a registered deferrable FK must succeed");
}

#[djogi::djogi_test(sync_models = [DeferNode])]
async fn defer_constraints_named_rejects_unknown_name(mut ctx: djogi::DjogiContext) {
    // The validator must surface `UnknownConstraintName` for a
    // name that does not match any registered FK. The check runs
    // BEFORE any SQL flies — the error must NOT carry a SQL-side
    // SQLSTATE.
    let result = atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            ctx.defer_constraints(DeferScope::Named(&["nonexistent_typo_fkey"]))
                .await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await;

    match result {
        Err(DjogiError::UnknownConstraintName(name)) => {
            assert_eq!(
                name, "nonexistent_typo_fkey",
                "rejection must carry the offending name verbatim"
            );
        }
        other => panic!("expected UnknownConstraintName, got {other:?}"),
    }
}

#[djogi::djogi_test(sync_models = [DeferNode])]
async fn set_constraints_immediate_reverses_defer(mut ctx: djogi::DjogiContext) {
    // The mirror surface must compose end-to-end: defer to DEFERRED,
    // then flip back to IMMEDIATE for the remainder of the
    // transaction.
    atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            ctx.defer_constraints(DeferScope::All).await?;
            // Insert one row in the deferred window with peer_id = None
            // so the IMMEDIATE flip below has no pending FK violations
            // to surface.
            DeferNode::create(
                ctx,
                DeferNode {
                    label: "deferred-then-immediate".to_string(),
                    peer_id: None,
                    ..Default::default()
                },
            )
            .await?;
            // Flip back to IMMEDIATE — Postgres checks any pending
            // deferred constraints right now. With no cycle in
            // flight, the IMMEDIATE flip succeeds.
            ctx.set_constraints_immediate(DeferScope::All).await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("defer_constraints + set_constraints_immediate must compose");
}

#[djogi::djogi_test(sync_models = [DeferNode])]
async fn defer_constraints_rejects_pool_backed_context(mut ctx: djogi::DjogiContext) {
    // Outside an `atomic()` scope, the helper must surface
    // `ConstraintModeOutsideTransaction` synchronously — same
    // discipline as `SetRoleOutsideTransaction`.
    let err = ctx
        .defer_constraints(DeferScope::All)
        .await
        .expect_err("pool-backed defer_constraints must surface a typed terminal error");
    assert!(
        matches!(err, DjogiError::ConstraintModeOutsideTransaction),
        "expected ConstraintModeOutsideTransaction, got {err:?}",
    );

    let err = ctx
        .set_constraints_immediate(DeferScope::All)
        .await
        .expect_err("pool-backed set_constraints_immediate must surface a typed terminal error");
    assert!(
        matches!(err, DjogiError::ConstraintModeOutsideTransaction),
        "expected ConstraintModeOutsideTransaction, got {err:?}",
    );
}
