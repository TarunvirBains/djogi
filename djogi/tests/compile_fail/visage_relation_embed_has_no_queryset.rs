use djogi::prelude::*;

#[model(table = "vsq_rel_embed_departments")]
#[derive(Debug, Clone)]
pub struct Department {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "vsq_rel_embed_employees", no_default)]
#[derive(Debug, Clone)]
pub struct Employee {
    #[field(expose(public -> DepartmentPublic))]
    pub department: ForeignKey<Department>,
}

fn _no_queryset_for_relation_embed_visage() {
    let _ = EmployeePublic::filter(|_| true);
}

fn main() {}
