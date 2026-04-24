// Phase 7-Zero v3 T8 — the `renamed_from`-targets-a-live-label rule
// is scoped to the same database. App identity is `(database, label)`,
// so `main/audit/` renamed from `"audit"` while `crud_log/audit/`
// still exists is legitimate — the two apps are separate migration
// directories.
use djogi::prelude::*;

djogi::apps! {
    // Live `audit` in crud_log.
    #[app(database = "crud_log", label = "audit")]
    pub struct CrudAudit;

    // Rename-from-`"audit"` on main — fine, because the live `audit`
    // lives in a different database.
    #[app(database = "main", renamed_from = "audit")]
    pub struct MainAudit;
}

fn main() {
    assert_eq!(<MainAudit as App>::DATABASE, "main");
    assert_eq!(<MainAudit as App>::DESCRIPTOR.renamed_from, Some("audit"));
    assert_eq!(<CrudAudit as App>::DATABASE, "crud_log");
    assert_eq!(<CrudAudit as App>::DESCRIPTOR.renamed_from, None);
}
