//! Phase 7-Zero-2 T8 — required-FK forward traversal on visage-scoped Fields.
//!
//! A filter closure written like `|e: &EmpPublicFields| e.department().name().eq("…")`
//! must type-check: the `.department()` accessor returns `DeptPublicFields` with a
//! SQL-alias path threaded through, so the peer's `.name()` emits a `FieldRef`
//! whose column path is `department.name`.
//!
//! ## Deferred: `EmpPublic::filter(|f| …)` entry point
//!
//! The plan's v3 sketch for this fixture used
//! `EmpPublic::filter(|e: &EmpPublicFields| …)` to prove the chain composes.
//! That entry point is T10's concern (wiring `{Visage}::filter` to
//! `QuerySet::filter` with visage-scope enforcement). T8's proof obligation is
//! narrower: the traversal method chain itself must type-check and compose
//! into a `Condition`. We prove that here with a standalone helper that takes
//! a `&EmpPublicFields` and returns `Condition` — no dependence on `::filter`.
//!
//! The T10 fixture will lift this helper into the real closure position.

use djogi::prelude::*;
use djogi::query::internal::Condition;

#[model(table = "phase7_zero2_t8_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t8_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    #[field(expose(public -> DeptPublic))]
    pub department: ForeignKey<Dept>,
}

/// Proves `.department().name().eq(…)` composes through visage-scoped Fields.
/// The helper's body is the T8 acceptance criterion — the traversal's
/// return type must chain into the peer's scalar accessor and then into a
/// leaf comparison, all under the source-model `FieldRef<Dept, String>`.
#[allow(dead_code)]
fn traversal_composes(e: &EmpPublicFields) -> Condition {
    e.department().name().eq("Engineering".to_string())
}

fn main() {
    // Root-level scalar accessor still composes (sanity check).
    let fields = EmpPublicFields::default();
    let _own: Condition = fields.display_name().eq("Ada".to_string());

    // Required-FK traversal composes via the helper. Reaching this line
    // means rustc resolved every method on the chain — that is the
    // compile-pass gate T8 needs.
    let _traversed: Condition = traversal_composes(&fields);

}
