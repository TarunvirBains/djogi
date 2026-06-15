// The `renamed_from`-targets-a-live-label rule is scoped to the same
// database. The migration contract's full identity is `(database,
// label)`, so a tombstoned `audit` rename on `main/` while a live
// `audit` exists in `crud_log/` is legitimate at the macro-emission
// boundary — these expand to different `<database>/<label>/`
// migration directories.
//
// Note: as with `phase7_zero_apps_same_label_different_db`, v1 lifts
// `(database, label)` to workspace-wide label uniqueness — the
// renamed-from `MainAudit` and the live `CrudAudit` here both
// carry the label `"audit"`, so calling `AppRegistry::all()`
// panics at startup. This fixture pins the macro emission
// shape; runtime registry use of these descriptors would panic.
use djogi::prelude::*;

djogi::apps! {
 // Live `audit` in crud_log.
 #[app(database = "crud_log", label = "audit")]
 pub struct CrudAudit;

 // Rename-from-`"audit"` on main — accepted by macro expansion;
 // see the prose above for the runtime-registry panic.
 #[app(database = "main", renamed_from = "audit")]
 pub struct MainAudit;
}

fn main() {
 // Inspect the const descriptors only — calling
 // `AppRegistry::all()` here would panic per F2.
 assert_eq!(<MainAudit as App>::DATABASE, "main");
 assert_eq!(<MainAudit as App>::DESCRIPTOR.renamed_from, Some("audit"));
 assert_eq!(<CrudAudit as App>::DATABASE, "crud_log");
 assert_eq!(<CrudAudit as App>::DESCRIPTOR.renamed_from, None);
}
