// Calling .fetch_all on GroupedQuerySet (before .annotate) must be a compile
// error — the type-state contract mandates that terminals are only available on
// GroupedAnnotatedQuerySet, not on the unannotated GroupedQuerySet.
use djogi::prelude::*;

#[model(table = "txns")]
struct Txn {
    org_id: i64,
    amount: i64,
}

fn main() {
    let mut ctx: djogi::context::DjogiContext = unimplemented!();
    let _ = Txn::objects().group_by(|f| f.org_id()).fetch_all(&mut ctx);
}
