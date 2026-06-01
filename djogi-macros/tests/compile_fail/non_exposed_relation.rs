//! non-exposed relation field is ABSENT from the
//! `{Visage}Fields` accessor surface.
//!
//! `department` is declared `expose(admin -> DeptPublic)` only — it is
//! therefore reachable on `EmpAdminFields`, but `EmpPublicFields` does
//! not see it. Calling `EmpPublicFields::department()` fails with "no
//! function or associated item named …".
use djogi::prelude::*;

#[model(table = "depts_t7_non_exposed_relation")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "emps_t7_non_exposed_relation", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    // Not exposed in `public` — only in `admin`.
    #[field(expose(admin -> DeptPublic))]
    pub department: ForeignKey<Dept>,
}

fn main() {
    // EmpPublic does NOT expose the department relation — accessor is
    // not generated, even though EmpAdminFields has it.
    let _bad = EmpPublicFields::department();
}
