//! A relation-embed visage has no queryset entry point, so `::filter` is not
//! emitted.
use djogi::prelude::*;

#[model(table = "vsq_re_relation_departments")]
#[derive(Debug, Clone)]
pub struct Department {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "vsq_re_relation_employees")]
#[derive(Debug, Clone)]
pub struct Employee {
 #[field(expose(public))]
 pub display_name: String,

 #[field(expose(public -> DepartmentPublic))]
 pub department: ForeignKey<Department>,
}

fn main() {
 let employee = Employee {
  display_name: "Ada".to_string(),
 ..Default::default()
 };

 let employee_public =
  <EmployeePublic as std::convert::TryFrom<&Employee>>::try_from(&employee).unwrap();
 let _ = &employee_public.department;

 let _ = EmployeePublic::filter(|f| f.display_name.eq("Ada".to_string()));
}
