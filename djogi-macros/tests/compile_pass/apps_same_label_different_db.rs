// Same-label / different-database apps. The migration contract's full
// identity is `(database, label)`, but v1 lifts that to a workspace-
// wide label-uniqueness invariant — `ModelDescriptor` carries only
// `label`, so cross-app FK resolution would silently route to the
// wrong database without it. `AppRegistry::all()` panics on the
// configuration this fixture declares (see
// `validate_app_identity_uniqueness` in `djogi/src/apps.rs`).
//
// The macro emission itself still type-checks — the panic is at
// registry-resolution time, not at macro expansion. This fixture is
// kept as a compile-pass to pin that boundary: declaration is
// allowed, but using the registry on these descriptors would panic.
// The deferred descriptor-shape upgrade (`(label, database)` keying)
// in `docs/spec/apps-and-database-domains.md` would unlock the
// looser identity contract.
use djogi::prelude::*;

djogi::apps! {
 #[app(database = "main", label = "audit")]
 pub struct MainAudit;

 #[app(database = "crud_log", label = "audit")]
 pub struct CrudAudit;
}

fn main() {
 // Inspect the const associated descriptors only — calling
 // `AppRegistry::all()` here would panic per F2 (workspace-wide
 // label-uniqueness invariant).
 assert_eq!(<MainAudit as App>::LABEL, "audit");
 assert_eq!(<MainAudit as App>::DATABASE, "main");
 assert_eq!(<CrudAudit as App>::LABEL, "audit");
 assert_eq!(<CrudAudit as App>::DATABASE, "crud_log");
}
