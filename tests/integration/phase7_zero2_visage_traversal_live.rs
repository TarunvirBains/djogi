//! Phase 7-Zero-2 T8 + T9 live coverage for visage-scoped traversal and
//! reverse-FK / M2M boundary enforcement.
//!
//! # What this test does
//!
//! 1. (T8) Creates two tables with an FK relationship (`emps → depts`).
//! 2. (T8) Inserts fixtures so one department has a known name and the
//!    employees that point at it form a witness set.
//! 3. (T8) Builds the traversal chain `fields.department().name().eq(…)`
//!    and asserts the emitted `Condition` carries column path
//!    `"department.name"` — the composed SQL-alias path the T8
//!    accessor is supposed to thread through.
//! 4. (T8) Runs a hand-written JOIN query that applies the same predicate
//!    (`dept.name = 'Engineering'`) to confirm the SQL the eventual
//!    T10 query planner will emit does return the expected rows.
//! 5. (T9) Exercises the visage-scoped reverse-FK accessor — a
//!    `DeptPublic::employees(ctx)` call that returns `Vec<EmpPublic>`
//!    end-to-end against the live Postgres database.
//! 6. (T9) Exercises the visage-scoped M2M accessor — a
//!    `PersonPublic::groups(ctx)` call that returns `Vec<GroupPublic>`
//!    walking through a junction table.
//!
//! # Why the Condition-level assertion (T8)
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
//!
//! # Why the reverse/M2M live tests (T9)
//!
//! T9 is exactly the surface where `{Visage}::fetch` DOES work today
//! — the visage-scoped method goes model-scoped query → TryFrom
//! projection → `Vec<PeerVisage>`. Testing against a live DB pins
//! that the conversion cycle survives real row decoding, real PK
//! round-trips, and real closure captures.

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

// --- Phase 7-Zero-2 T9 — reverse-FK visage boundary live coverage ---

#[model(table = "phase7_zero2_t9_live_depts")]
#[derive(Debug, Clone)]
pub struct RevDept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_live_emps", no_default)]
#[derive(Debug, Clone)]
pub struct RevEmp {
    #[field(expose(public))]
    pub display_name: String,
    pub department: ForeignKey<RevDept>,
}

djogi::reverse_one_to_many!(
    RevDept, employees -> RevEmp by department,
    expose(public -> RevEmpPublic)
);

async fn setup_reverse_live(ctx: &mut DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t9_live_depts (
            id           BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name         TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE rev_depts");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t9_live_emps (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            display_name  TEXT        NOT NULL,
            department    BIGINT      NOT NULL    REFERENCES phase7_zero2_t9_live_depts(id)
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE rev_emps");
}

#[djogi::djogi_test]
async fn reverse_fk_visage_accessor_projects_to_peer_visage(mut ctx: DjogiContext) {
    setup_reverse_live(&mut ctx).await;

    // Create a department + two employees pointing at it through the
    // model CRUD surface so RETURNING id cycles through the
    // heerid_next_desc() default and the PK lifecycle is exercised
    // end-to-end.
    let dept = RevDept::create(
        &mut ctx,
        RevDept {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Engineering".to_string(),
        },
    )
    .await
    .expect("create dept");
    let _ = RevEmp::create(
        &mut ctx,
        RevEmp {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Ada".to_string(),
            department: ForeignKey::new(dept.id),
        },
    )
    .await
    .expect("create Ada");
    let _ = RevEmp::create(
        &mut ctx,
        RevEmp {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            display_name: "Grace".to_string(),
            department: ForeignKey::new(dept.id),
        },
    )
    .await
    .expect("create Grace");

    // Drive the visage-scoped reverse accessor through the DeptPublic
    // visage. The return type is `Vec<RevEmpPublic>`, proving the
    // macro-emitted body fetched `RevEmp` rows and projected each one
    // through `<RevEmpPublic as TryFrom<&RevEmp>>::try_from`.
    let dept_public = RevDeptPublic::from(&dept);
    let employees: Vec<RevEmpPublic> = dept_public
        .employees(&mut ctx)
        .await
        .expect("visage reverse accessor must succeed");

    assert_eq!(employees.len(), 2, "both employees roll up");
    let mut names: Vec<&str> = employees.iter().map(|e| e.display_name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["Ada", "Grace"]);
}

// --- Phase 7-Zero-2 T9 — M2M visage boundary live coverage ---

#[model(table = "phase7_zero2_t9_live_m2m_persons")]
#[derive(Debug, Clone)]
pub struct M2mPerson {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_live_m2m_groups")]
#[derive(Debug, Clone)]
pub struct M2mGroup {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_live_m2m_person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct M2mPersonGroup {
    pub person_id: ForeignKey<M2mPerson>,
    pub group_id: ForeignKey<M2mGroup>,
    #[field(expose(public))]
    pub role: String,
}

djogi::many_to_many!(
    M2mPerson, M2mGroup,
    through = M2mPersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups",
    expose(public -> M2mGroupPublic)
);

async fn setup_m2m_live(ctx: &mut DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t9_live_m2m_persons (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name          TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE m2m_persons");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t9_live_m2m_groups (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name          TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE m2m_groups");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t9_live_m2m_person_groups (
            id            BIGINT      PRIMARY KEY DEFAULT heerid_next_desc(),
            created_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at    TIMESTAMPTZ NOT NULL    DEFAULT now(),
            person_id     BIGINT      NOT NULL    REFERENCES phase7_zero2_t9_live_m2m_persons(id),
            group_id      BIGINT      NOT NULL    REFERENCES phase7_zero2_t9_live_m2m_groups(id),
            role          TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE m2m_person_groups");
}

#[djogi::djogi_test]
async fn m2m_visage_accessor_returns_peer_visage(mut ctx: DjogiContext) {
    setup_m2m_live(&mut ctx).await;

    let ada = M2mPerson::create(
        &mut ctx,
        M2mPerson {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Ada".to_string(),
        },
    )
    .await
    .expect("create Ada");
    let engineers = M2mGroup::create(
        &mut ctx,
        M2mGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Engineers".to_string(),
        },
    )
    .await
    .expect("create Engineers");
    let reviewers = M2mGroup::create(
        &mut ctx,
        M2mGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            name: "Reviewers".to_string(),
        },
    )
    .await
    .expect("create Reviewers");
    let _ = M2mPersonGroup::create(
        &mut ctx,
        M2mPersonGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            person_id: ForeignKey::new(ada.id),
            group_id: ForeignKey::new(engineers.id),
            role: "member".to_string(),
        },
    )
    .await
    .expect("join Ada → Engineers");
    let _ = M2mPersonGroup::create(
        &mut ctx,
        M2mPersonGroup {
            id: <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel(),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            person_id: ForeignKey::new(ada.id),
            group_id: ForeignKey::new(reviewers.id),
            role: "lead".to_string(),
        },
    )
    .await
    .expect("join Ada → Reviewers");

    // Drive the visage-scoped M2M accessor. The return type is
    // `Vec<M2mGroupPublic>`, proving the emitted body walked the
    // through table and converted each resolved peer row through
    // `<M2mGroupPublic as TryFrom<&M2mGroup>>::try_from`.
    let ada_public = M2mPersonPublic::from(&ada);
    let mut groups: Vec<M2mGroupPublic> = ada_public
        .groups(&mut ctx)
        .await
        .expect("m2m visage accessor");
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].name, "Engineers");
    assert_eq!(groups[1].name, "Reviewers");
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
