// explicit `#[app(label = "…")]` override.
//
// `BillingAccounts` would default-derive to `"billingaccounts"` which
// is awkward; users can override with an explicit label.
use djogi::prelude::*;

djogi::apps! {
 #[app(label = "fleet_vehicles", database = "main")]
 pub struct Vehicles;

 #[app(database = "main", label = "billing_accounts")]
 pub struct BillingAccounts;
}

fn main() {
 assert_eq!(<Vehicles as App>::LABEL, "fleet_vehicles");
 assert_eq!(<BillingAccounts as App>::LABEL, "billing_accounts");
}
