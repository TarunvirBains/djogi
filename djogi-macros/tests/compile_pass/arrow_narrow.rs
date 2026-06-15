//! narrow `->` peer-visage form.
//!
//! `expose(public -> DepartmentPublic)` replaces the deprecated string-literal
//! relation form (`expose(public = "DepartmentPublic")`). The new form takes
//! a bare `syn::Path` after `->` so peer-visage names compose with module
//! prefixes without needing a quoted-string round-trip.
//!
//! `DepartmentPublic` is the macro-emitted narrow visage for `Department` —
//! recognising the `<ModelIdent><Scope>` shape selects the narrow embed.
use djogi::prelude::*;

#[model(table = "departments_t6_narrow")]
#[derive(Debug, Clone)]
pub struct Department {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "employees_t6_narrow", no_default)]
#[derive(Debug, Clone)]
pub struct Employee {
 #[field(expose(public))]
 pub display_name: String,

 #[field(expose(public -> DepartmentPublic))]
 pub department: ForeignKey<Department>,
}

fn main() {
 // Compile-time only: the parser must accept the `->` form, the emitter
 // must produce an `EmployeePublic` carrying a `DepartmentPublic` peer
 // populated via `<DepartmentPublic as TryFrom<&Department>>::try_from(...)`.
 let _build = |emp: &Employee| -> Result<EmployeePublic, djogi::VisageError> {
  EmployeePublic::try_from(emp)
 };
}
