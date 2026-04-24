// Phase 7-Zero v3 T7 fixup — app identity is `(database, label)`,
// not label alone. Two apps with the same label are legitimate as
// long as their database targets differ: they map to separate
// `<database>/<label>/` migration directories on disk
// (`main/audit/` vs `crud_log/audit/`).
use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main", label = "audit")]
    pub struct MainAudit;

    #[app(database = "crud_log", label = "audit")]
    pub struct CrudAudit;
}

fn main() {
    assert_eq!(<MainAudit as App>::LABEL, "audit");
    assert_eq!(<MainAudit as App>::DATABASE, "main");
    assert_eq!(<CrudAudit as App>::LABEL, "audit");
    assert_eq!(<CrudAudit as App>::DATABASE, "crud_log");
}
