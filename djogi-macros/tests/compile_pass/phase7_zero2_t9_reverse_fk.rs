//! Phase 7-Zero-2 T9 — reverse-FK visage-scoped accessor emission.
//!
//! A reverse accessor declared via `djogi::reverse_one_to_many!` with
//! an `expose(scope -> PeerVisage)` clause must emit TWO inherent methods:
//!
//! - The unchanged model-scoped accessor on the receiver model:
//!   `impl Dept { pub fn employees(...) -> ... Vec<Emp> }`
//! - A NEW visage-scoped accessor on the receiver's visage:
//!   `impl DeptPublic { pub fn employees(...) -> ... Vec<EmpPublic> }`
//!
//! The visage-scoped variant converts every fetched row through the peer's
//! `TryFrom<&Emp>` impl before returning, so `fetch_all` yields
//! `Vec<EmpPublic>`, not `Vec<Emp>`.
//!
//! ## Compile-only probes
//!
//! We assert the method shapes with function-coercion probes rather than
//! `.await`-ing them, because trybuild has no live Postgres pool. A future
//! function pointer that matches the emitted signature will only typecheck
//! if the macro emitted the method with exactly that signature.

use djogi::prelude::*;

#[model(table = "phase7_zero2_t9_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
    #[field(expose(public))]
    pub display_name: String,
    pub department: ForeignKey<Dept>,
}

// Declare the reverse accessor WITH a visage-exposure clause.
//
// The `expose(public -> EmpPublic)` clause asks the macro to emit an
// additional inherent method on `DeptPublic` (the receiver's scope-
// `public` visage) that returns `Vec<EmpPublic>`. Both the receiver's
// `{scope}` visage AND the peer's named visage must exist; if either
// is missing, the emitted code fails to compile.
djogi::reverse_one_to_many!(
    Dept, employees -> Emp by department,
    expose(public -> EmpPublic)
);

// Model-scoped accessor still compiles — the visage-exposure clause is
// additive, it never removes the baseline reverse accessor.
#[allow(dead_code)]
fn _model_scoped_accessor<'a>(
    dept: &'a Dept,
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Emp>, DjogiError>> + Send + 'a {
    dept.employees(ctx)
}

// Visage-scoped accessor emits on `DeptPublic` and returns a
// SELECT-narrowed `VisageQuerySet<EmpPublic>` (Phase 7-Zero-2 T13a).
// The caller chains `.fetch_all(ctx)` for `Vec<EmpPublic>`. Typechecking
// this function pointer pins the method signature at compile time
// without requiring a pool.
#[allow(dead_code)]
fn _visage_scoped_accessor(
    dept_public: &DeptPublic,
) -> djogi::query::VisageQuerySet<EmpPublic> {
    dept_public.employees()
}

fn main() {
    // Compile-only — the function probes above do the real work. A
    // runtime call would need a pool; trybuild doesn't provide one.
}
