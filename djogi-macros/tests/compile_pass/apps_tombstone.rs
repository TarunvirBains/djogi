// `#[app(tombstone)]` surfaces as
// `AppDescriptor.tombstone = true` and `App::TOMBSTONE = true`.
// No active models reference `OldBilling` in this fixture; active
// references are a compile error (see the compile_fail fixture).
use djogi::prelude::*;

djogi::apps! {
 #[app(database = "main", tombstone)]
 pub struct OldBilling;
}

fn main() {
 assert_eq!(<OldBilling as App>::LABEL, "oldbilling");
 assert!(<OldBilling as App>::TOMBSTONE);
 assert!(<OldBilling as App>::DESCRIPTOR.tombstone);
}
