//! Phase 8.5 #225/#231 — derived fields cannot share a visage scope
//! with relation-form exposure yet.
//!
//! The relation-form projector does not emit derived SQL expressions.
//! A derived projection in the same scope as `expose(scope -> Peer)`
//! must therefore reject at macro time with E_DJG_VDF_010.

use djogi::prelude::*;

#[model(table = "phase85_derived_overlap_departments")]
#[derive(Model, Debug, Clone)]
pub struct Department {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase85_derived_overlap_employees", no_default)]
#[derive(Model, Debug, Clone)]
#[derived(
    name = department,
    ty = String,
    scopes = [public],
    sql = "department_id::text",
    rust = "model.department_id.to_string()",
)]
pub struct Employee {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(public -> DepartmentPublic))]
    pub department: ForeignKey<Department>,
}

fn main() {}
