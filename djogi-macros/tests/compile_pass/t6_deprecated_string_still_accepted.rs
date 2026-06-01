//! backward-compat for the pre-T6
//! `expose(scope = "Peer")` string-literal relation form.
//!
//! T6 introduces `expose(scope -> Peer)` as the canonical form. The prior
//! `expose(scope = "...")` shape continues to compile so existing user
//! code does not break; an internal `from_string_form` flag on the parsed
//! `RelationExposure` reserves a hook for future `#[deprecated]` advisory
//! wiring. This fixture asserts the form still emits a working visage —
//! the warning text itself is not pinned (lihaaf does not snapshot
//! warnings), only that compilation succeeds.
use djogi::prelude::*;

#[model(table = "departments_t6_dep_str")]
#[derive(Debug, Clone)]
pub struct Department {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "employees_t6_dep_str", no_default)]
#[derive(Debug, Clone)]
pub struct Employee {
    #[field(expose(public))]
    pub display_name: String,

    // Deprecated string-literal form — equivalent to
    // `expose(public -> DepartmentPublic)`. Still accepted.
    #[field(expose(public = "DepartmentPublic"))]
    pub department: ForeignKey<Department>,
}

fn main() {
    let _build = |emp: &Employee| -> Result<EmployeePublic, djogi::VisageError> {
        EmployeePublic::try_from(emp)
    };
}
