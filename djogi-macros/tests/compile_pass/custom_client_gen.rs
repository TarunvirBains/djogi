// `djogi::primary_key!` with `generate = |…| expr`
// emits `PrimaryKeyClientGen`. The closure fires once per
// `generate_client()` call and its result is wrapped by the newtype.
//
// The client-gen path has no DB round-trip, so the fixture uses a
// counter-seeded inner value to exercise the emission without needing a
// test database.
//
// when only `generate` is supplied (no
// `bulk_sql`), `primary_key!` still emits `PrimaryKeyDbGen` — its
// `generate_many` body is a per-row `generate_client()` loop. That means
// `#[model]`'s post-T5 bulk_create binds cleanly without the adopter
// having to wire a dummy `bulk_sql` here.

use djogi::prelude::*;
use std::sync::atomic::{AtomicI64, Ordering};

static NEXT: AtomicI64 = AtomicI64::new(1);

djogi::primary_key! {
 pub struct MyClientId(i64);
 sql_type = "BIGINT";
 default_sql = "0";
 generate = || NEXT.fetch_add(1, Ordering::Relaxed);
}

#[model(table = "widgets", pk = MyClientId)]
#[derive(Debug, Clone)]
pub struct Widget {
 pub name: String,
}

fn _client_gen_surface() {
 // The emitted impl is reachable through the trait path.
 let _one = <MyClientId as ::djogi::primary_key::PrimaryKeyClientGen>::generate_client();
 let _two = <MyClientId as ::djogi::primary_key::PrimaryKeyClientGen>::generate_client();
}

fn main() {}
