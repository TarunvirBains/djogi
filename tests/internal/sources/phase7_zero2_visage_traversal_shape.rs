// Internal shape assertions for Phase 7-Zero-2 T8/T9 visage traversal.
//
// The ordinary live test exercises the public typed surface against Postgres.
// These assertions intentionally inspect Djogi's lowered condition tree and
// emitted test SQL, so they live outside ordinary integration roots.

use djogi::prelude::*;
use djogi::query::internal::{Condition, LookupOp};

fn sentinel_id() -> djogi::types::HeerIdDesc {
    <djogi::types::HeerIdDesc as djogi::PrimaryKey>::sentinel()
}

fn sentinel_dt() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

#[model(table = "phase7_zero2_t8_shape_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t8_shape_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(public -> DeptPublic))]
    pub department: ForeignKey<Dept>,
}

#[model(table = "phase7_zero2_t8_shape_opt_users")]
#[derive(Debug, Clone)]
pub struct OptUser {
    #[field(expose(public))]
    pub display_name: String,
}

#[model(table = "phase7_zero2_t8_shape_opt_posts", no_default)]
#[derive(Debug, Clone)]
pub struct OptPost {
    #[field(expose(public))]
    pub title: String,
    #[field(expose(public -> OptUserPublic))]
    pub author: Option<ForeignKey<OptUser>>,
}

#[model(table = "phase7_zero2_t9_shape_depts")]
#[derive(Debug, Clone)]
pub struct RevDept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_shape_emps", no_default)]
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

#[model(table = "phase7_zero2_t9_shape_m2m_persons")]
#[derive(Debug, Clone)]
pub struct M2mPerson {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_shape_m2m_groups")]
#[derive(Debug, Clone)]
pub struct M2mGroup {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_shape_m2m_person_groups", through, no_default)]
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

fn leaf_column_of(cond: &Condition) -> &str {
    match cond {
        Condition::Leaf(leaf) => leaf.column(),
        other => panic!("expected a Condition::Leaf; got {other:?}"),
    }
}

#[test]
fn visage_traversal_composes_dot_qualified_path_shape() {
    let fields = EmpPublicFields::default();
    let cond: Condition = fields.department().name().eq("Engineering".to_string());

    assert_eq!(
        leaf_column_of(&cond),
        "department.name",
        "T8 traversal must thread the FK column name as SQL-alias prefix"
    );
}

#[test]
fn reverse_fk_visage_accessor_selects_only_peer_visage_columns_shape() {
    let dept = RevDept {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        name: "Engineering".to_string(),
    };
    let dept_public = RevDeptPublic::from(&dept);
    let sql = dept_public.employees().__sql_for_test();

    assert!(
        sql.contains("display_name"),
        "narrowed SELECT must include exposed `display_name`; got: {sql}",
    );
    assert!(
        !sql.contains("SELECT department,")
            && !sql.contains(", department,")
            && !sql.contains(", department FROM"),
        "narrowed SELECT must NOT project `department`; got: {sql}",
    );
}

#[test]
fn m2m_visage_accessor_uses_exists_without_widening_projection_shape() {
    let ada = M2mPerson {
        id: sentinel_id(),
        created_at: sentinel_dt(),
        updated_at: sentinel_dt(),
        name: "Ada".to_string(),
    };
    let ada_public = M2mPersonPublic::from(&ada);
    let sql = ada_public.groups().__sql_for_test();

    assert!(
        sql.contains("name") && sql.starts_with("SELECT id"),
        "outer SELECT must list M2mGroupPublic's exposed columns; got: {sql}",
    );
    assert!(
        sql.contains("EXISTS"),
        "M2M predicate must lower to an EXISTS correlated subquery; got: {sql}",
    );
    assert!(
        sql.contains("phase7_zero2_t9_shape_m2m_groups.id"),
        "EXISTS predicate must qualify the outer table reference; got: {sql}",
    );
}

#[test]
fn optional_relation_ref_emits_is_not_null_guard_shape() {
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
        Condition::Leaf(leaf) => {
            assert_eq!(leaf.column(), "author", "guard leaf targets the FK column");
            let op = leaf.op();
            assert!(
                matches!(op, LookupOp::IsNotNull),
                "guard leaf must be IsNotNull; got {op:?}"
            );
        }
        other => panic!("first child must be the IS NOT NULL guard; got {other:?}"),
    }
    match &children[1] {
        Condition::Leaf(leaf) => {
            assert_eq!(
                leaf.column(),
                "author.display_name",
                "inner leaf must carry the dot-qualified traversal path"
            );
        }
        other => panic!("second child must be the inner leaf; got {other:?}"),
    }

    let some_only: Condition = fields.author().is_some();
    match &some_only {
        Condition::Leaf(leaf) => {
            assert_eq!(leaf.column(), "author");
            assert!(matches!(leaf.op(), LookupOp::IsNotNull));
        }
        other => panic!("is_some must emit a single Leaf; got {other:?}"),
    }
    let none_only: Condition = fields.author().is_none();
    match &none_only {
        Condition::Leaf(leaf) => {
            assert_eq!(leaf.column(), "author");
            assert!(matches!(leaf.op(), LookupOp::IsNull));
        }
        other => panic!("is_none must emit a single Leaf; got {other:?}"),
    }
}
