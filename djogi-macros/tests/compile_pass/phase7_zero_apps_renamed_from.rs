// Phase 7-Zero v3 T8 — `#[app(renamed_from = "...")]` round-trips
// to `AppDescriptor.renamed_from`. Useful when retiring an old
// label and replacing it with a new one: the differ sees
// `renamed_from` on the new entry and generates a rename migration
// instead of drop-and-create.
use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main", renamed_from = "billing_old")]
    pub struct Billing;
}

fn main() {
    assert_eq!(<Billing as App>::LABEL, "billing");
    assert_eq!(<Billing as App>::DATABASE, "main");
    assert_eq!(<Billing as App>::DESCRIPTOR.renamed_from, Some("billing_old"));
    assert!(!<Billing as App>::DESCRIPTOR.tombstone);
}
