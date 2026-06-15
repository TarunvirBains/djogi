// `#[model(app = Vehicles)]` + descriptor
// projection. The macro lowers `app = Vehicles` to
// `Some(<Vehicles as App>::LABEL)` at const-eval, and
// `moved_from_app = OldBilling` works the same way for historical
// metadata. A tombstoned app is legal as a `moved_from_app` target
// — that's the point.
use djogi::prelude::*;

djogi::apps! {
 #[app(database = "main")]
 pub struct Vehicles;

 #[app(database = "main", tombstone)]
 pub struct OldBilling;
}

#[model(table = "cars", app = Vehicles)]
pub struct Car {
 pub make: String,
}

#[model(table = "invoices", app = Vehicles, moved_from_app = OldBilling)]
pub struct Invoice {
 pub amount_cents: i64,
}

fn main() {
 let car_desc = Car::descriptor();
 assert_eq!(car_desc.app, Some("vehicles"));
 assert_eq!(car_desc.moved_from_app, None);

 let invoice_desc = Invoice::descriptor();
 assert_eq!(invoice_desc.app, Some("vehicles"));
 assert_eq!(invoice_desc.moved_from_app, Some("oldbilling"));
}
