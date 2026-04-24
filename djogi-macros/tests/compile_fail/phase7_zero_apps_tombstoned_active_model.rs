// Phase 7-Zero v3 T8 — `#[model(app = OldBilling)]` on a
// tombstoned app is a compile error. Active models must point at
// live apps; historical metadata uses `moved_from_app` instead.
use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main", tombstone)]
    pub struct OldBilling;
}

#[model(table = "invoices", app = OldBilling)]
pub struct Invoice {
    pub amount_cents: i64,
}

fn main() {}
