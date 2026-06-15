// `djogi::apps!` happy path.
//
// Two apps, each with an explicit database target; labels default to
// the struct identifier lowercased. Proves:
//
// - the macro emits each unit struct verbatim so user code can name it,
// - `App::LABEL` defaults to the lowercased struct identifier,
// - `App::DATABASE` surfaces the `#[app(database = "…")]` target,
// - `App::DESCRIPTOR` is const-accessible,
// - `AppRegistry::all()` sees both entries plus the synthetic global bucket.
use djogi::prelude::*;

djogi::apps! {
 #[app(database = "main")]
 pub struct Vehicles;

 #[app(database = "crud_log")]
 pub struct Billing;
}

fn main() {
 // Const assertions on the per-app macro-generated App impls.
 const _: &str = <Vehicles as App>::LABEL;
 const _: &str = <Vehicles as App>::DATABASE;
 const _: AppDescriptor = <Vehicles as App>::DESCRIPTOR;
 const _: &str = <Billing as App>::LABEL;
 const _: &str = <Billing as App>::DATABASE;

 assert_eq!(<Vehicles as App>::LABEL, "vehicles");
 assert_eq!(<Vehicles as App>::DATABASE, "main");
 assert_eq!(<Billing as App>::LABEL, "billing");
 assert_eq!(<Billing as App>::DATABASE, "crud_log");

 let all = AppRegistry::all();
 // Global bucket + vehicles + billing. Registry sorts by label,
 // empty-string first, so the exact slice is pinned.
 let labels: Vec<&str> = all.iter().map(|d| d.label).collect();
 assert_eq!(labels, vec!["", "billing", "vehicles"]);
 let databases: Vec<&str> = all.iter().map(|d| d.database).collect();
 assert_eq!(databases, vec!["main", "crud_log", "main"]);
 // Forward-declared T8 fields stay empty in T7.
 assert!(all.iter().all(|d| d.renamed_from.is_none()));
 assert!(all.iter().all(|d| !d.tombstone));
}
