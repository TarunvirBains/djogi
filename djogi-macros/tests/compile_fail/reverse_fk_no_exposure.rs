//! T9 compile-fail — reverse-FK visage accessor without exposure.
//!
//! A `djogi::reverse_one_to_many!` invocation WITHOUT any `expose(...)`
//! clause must NOT emit a visage-scoped accessor. Calling
//! `dept_public.employees(ctx)` without the exposure should fail with
//! `no method named employees found for... DeptPublic`.

use djogi::prelude::*;

#[model(table = "phase7_zero2_t9_negf_depts")]
#[derive(Debug, Clone)]
pub struct Dept {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "phase7_zero2_t9_negf_emps", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
 #[field(expose(public))]
 pub display_name: String,
 pub department: ForeignKey<Dept>,
}

// No `expose(...)` clause — only the model-scoped accessor is emitted.
djogi::reverse_one_to_many!(Dept, employees -> Emp by department);

fn _does_not_compile<'a>(
 dept_public: &'a DeptPublic,
 ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<EmpPublic>, DjogiError>> + Send + 'a {
 // Must fail: `employees` was not exposed for `DeptPublic`, so the
 // method is absent.
 dept_public.employees(ctx)
}

fn main() {}
