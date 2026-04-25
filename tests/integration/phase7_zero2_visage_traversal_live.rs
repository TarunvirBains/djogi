//! Phase 7-Zero-2 T8 live coverage for visage-scoped forward traversal.
//!
//! # What this test does
//!
//! 1. Creates two tables with an FK relationship (`emps → depts`).
//! 2. Inserts fixtures so one department has a known name and the
//!    employees that point at it form a witness set.
//! 3. Builds the T8 traversal chain `fields.department().name().eq(…)`
//!    and asserts the emitted `Condition` carries column path
//!    `"department.name"` — the composed SQL-alias path the T8
//!    accessor is supposed to thread through.
//! 4. Runs a hand-written JOIN query that applies the same predicate
//!    (`dept.name = 'Engineering'`) to confirm the SQL the eventual
//!    T10 query planner will emit does return the expected rows.
//!
//! # Why the Condition-level assertion
//!
//! T10 wires `{Visage}::filter(|f| …)` to `QuerySet::filter` with an
//! automatic FK join. Until that lands, a full end-to-end filter
//! closure can't reach the live DB through the visage surface. The
//! Condition-level assertion (Step 3 above) is the narrowest proof
//! that the T8 chain composes correctly: if the peer's scalar
//! accessor produces a `FieldRef` whose column is anything other
//! than `"department.name"`, the Rust-level assertion fails loudly.
//! Once T10 lands, this test can lift Step 4 into a real visage
//! filter closure.

use djogi::prelude::*;
use djogi::query::Condition;
use djogi::query::internal::{Leaf, LookupOp};

#[model(table = "phase7_zero2_t8_live_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t8_live_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(public -> DeptPublic))]
    pub department: ForeignKey<Dept>,
}

#[model(table = "phase7_zero2_t8_live_opt_users")]
#[derive(Debug, Clone)]
pub struct OptUser {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "phase7_zero2_t8_live_opt_posts", no_default)]
#[derive(Debug, Clone)]
pub struct OptPost {
    #[field(expose(public))]
    pub title: String,
    #[field(expose(public -> OptUserPublic))]
    pub author: Option<ForeignKey<OptUser>>,
}

async fn setup(ctx: &mut DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t8_live_depts (
            id           BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name         TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE depts");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t8_live_emps (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            display_name  TEXT        NOT NULL,
            department    BIGINT      NOT NULL    REFERENCES phase7_zero2_t8_live_depts(id)
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE emps");
}

/// Walk the emitted `Condition` and return the leaf's column string.
fn leaf_column_of(cond: &Condition) -> &str {
    match cond {
        Condition::Leaf(Leaf { column, .. }) => column,
        other => panic!("expected a Condition::Leaf; got {other:?}"),
    }
}

#[djogi::djogi_test]
async fn visage_traversal_composes_dot_qualified_path(mut ctx: DjogiContext) {
    setup(&mut ctx).await;

    // T8 chain: build the traversal Condition through the visage's
    // `{Visage}Fields` state-carrying struct and its path-threaded
    // peer accessor.
    let fields = EmpPublicFields::default();
    let cond: Condition = fields.department().name().eq("Engineering".to_string());

    // The leaf's column must be the dot-qualified traversal path —
    // that is the T8 acceptance shape, the piece the eventual T10
    // query planner will lift into a JOIN + `dept.name` reference.
    assert_eq!(
        leaf_column_of(&cond),
        "department.name",
        "T8 traversal must thread the FK column name as SQL-alias prefix"
    );

    // Hand-written JOIN + predicate that mirrors what the eventual
    // T10 emitter will produce. Alias the depts table as `department`
    // so the dot-qualified column resolves.
    ctx.raw_execute(
        "INSERT INTO phase7_zero2_t8_live_depts (name) VALUES ('Engineering'), ('Marketing')",
        &[],
    )
    .await
    .expect("insert depts");
    ctx.raw_execute(
        "INSERT INTO phase7_zero2_t8_live_emps (display_name, department)
         SELECT 'Ada', id FROM phase7_zero2_t8_live_depts WHERE name = 'Engineering'",
        &[],
    )
    .await
    .expect("insert Ada");
    ctx.raw_execute(
        "INSERT INTO phase7_zero2_t8_live_emps (display_name, department)
         SELECT 'Grace', id FROM phase7_zero2_t8_live_depts WHERE name = 'Engineering'",
        &[],
    )
    .await
    .expect("insert Grace");
    ctx.raw_execute(
        "INSERT INTO phase7_zero2_t8_live_emps (display_name, department)
         SELECT 'Mia', id FROM phase7_zero2_t8_live_depts WHERE name = 'Marketing'",
        &[],
    )
    .await
    .expect("insert Mia");

    // Count rows where the joined department's name matches the filter
    // — this is the SQL shape the T10 emitter will produce from the T8
    // chain. Two employees (Ada, Grace) are in Engineering.
    let count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM phase7_zero2_t8_live_emps e
               JOIN phase7_zero2_t8_live_depts department
                 ON department.id = e.department
              WHERE department.name = $1",
            &[&"Engineering".to_string()],
        )
        .await
        .expect("count join must succeed");
    assert_eq!(
        count, 2,
        "exactly Ada + Grace are in the Engineering department"
    );
}

#[djogi::djogi_test]
async fn optional_relation_ref_emits_is_not_null_guard(mut ctx: DjogiContext) {
    // No tables needed — this test is purely Condition-shape.
    let _ = &mut ctx;

    // An `OptionalRelationRef<V>::map_filter` composition must emit a
    // `Condition::And(IS NOT NULL, inner)` tree where the inner leaf
    // carries the dot-qualified traversal path.
    let fields = OptPostPublicFields::default();
    let cond: Condition = fields
        .author()
        .map_filter(|a| a.display_name().eq("Ada".to_string()));

    let Condition::And(children) = &cond else {
        panic!("map_filter must produce a top-level And; got {cond:?}");
    };
    assert_eq!(
        children.len(),
        2,
        "map_filter emits exactly two children (guard + inner)"
    );
    match &children[0] {
        Condition::Leaf(Leaf { column, op, .. }) => {
            assert_eq!(*column, "author", "guard leaf targets the FK column");
            assert!(
                matches!(op, LookupOp::IsNotNull),
                "guard leaf must be IsNotNull; got {op:?}"
            );
        }
        other => panic!("first child must be the IS NOT NULL guard; got {other:?}"),
    }
    match &children[1] {
        Condition::Leaf(Leaf { column, .. }) => {
            assert_eq!(
                *column, "author.display_name",
                "inner leaf must carry the dot-qualified traversal path"
            );
        }
        other => panic!("second child must be the inner leaf; got {other:?}"),
    }

    // Standalone `is_some` / `is_none` shortcut predicates on the
    // OptionalRelationRef — emits a single leaf with the FK column
    // name and the appropriate NULL-check op.
    let some_only: Condition = fields.author().is_some();
    match &some_only {
        Condition::Leaf(Leaf { column, op, .. }) => {
            assert_eq!(*column, "author");
            assert!(matches!(op, LookupOp::IsNotNull));
        }
        other => panic!("is_some must emit a single Leaf; got {other:?}"),
    }
    let none_only: Condition = fields.author().is_none();
    match &none_only {
        Condition::Leaf(Leaf { column, op, .. }) => {
            assert_eq!(*column, "author");
            assert!(matches!(op, LookupOp::IsNull));
        }
        other => panic!("is_none must emit a single Leaf; got {other:?}"),
    }
}
