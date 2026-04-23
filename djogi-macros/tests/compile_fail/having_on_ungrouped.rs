// Calling .having on a plain QuerySet (not a GroupedAnnotatedQuerySet) must
// be a compile error — .having is not a method on QuerySet<T>.
use djogi::prelude::*;

#[model(table = "txns")]
struct Txn {
    org_id: i64,
    amount: i64,
}

fn main() {
    let _ = Txn::objects().having(|_, _| unimplemented!());
}
