//! full-peer embed via `expose(scope -> Department)`.
//!
//! When the path on the right of `->` matches the relation's target model
//! ident exactly (rather than a `<Model><Scope>` narrow visage), the
//! emitter embeds the FULL peer model — cloned out of the resolved
//! relation — instead of dispatching through a peer-visage `TryFrom`.
//!
//! This pins the full-peer-vs-narrow heuristic: last-segment ident match
//! against the relation target's ident.
use djogi::prelude::*;
use djogi::__private::serde::{Deserialize, Serialize};

// Full-peer embed routes the resolved `Department` through the visage's
// serde derives, so the model itself must be (de)serialisable. Models
// always carry framework-injected `id` / `created_at` / `updated_at`,
// which derive serde via `::djogi::__private::serde`.
#[model(table = "departments_t6_full_peer")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "::djogi::__private::serde")]
pub struct Department {
    #[field(expose(public))]
    pub name: String,
    #[field(expose(public))]
    pub budget: i64,
}

#[model(table = "employees_t6_full_peer", no_default)]
#[derive(Debug, Clone)]
pub struct Employee {
    #[field(expose(public))]
    pub display_name: String,

    // `-> Department` (full peer model) — emitter clones the resolved
    // target instead of routing through DepartmentAdmin.
    #[field(expose(admin -> Department))]
    pub department: ForeignKey<Department>,
}

fn main() {
    // EmployeeAdmin must carry a Department (the full model) under
    // `department`, not a `DepartmentAdmin` narrow visage.
    let _build = |emp: &Employee| -> Result<EmployeeAdmin, djogi::VisageError> {
        EmployeeAdmin::try_from(emp)
    };
}
