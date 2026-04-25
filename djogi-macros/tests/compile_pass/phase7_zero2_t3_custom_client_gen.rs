// Phase 7-Zero-2 T3 — `djogi::primary_key!` with `generate = |…| expr`
// emits `PrimaryKeyClientGen`. The closure fires once per
// `generate_client()` call and its result is wrapped by the newtype.
//
// The client-gen path has no DB round-trip, so the fixture uses a
// counter-seeded inner value to exercise the emission without needing a
// test database.
//
// Phase 7-Zero-2 T5 update: `#[model]`'s post-T5 `bulk_create` emission
// binds every non-Serial PK on `PrimaryKeyDbGen`. Adopting a client-gen
// PK on a model therefore also requires wiring `bulk_sql` — without it
// the generated model body fails to type-check. The fixture adds a
// sequence-backed `bulk_sql` purely so `#[model]` compiles; the T3
// behaviour under test (the emitted `PrimaryKeyClientGen` surface) is
// unchanged.

use djogi::prelude::*;
use std::sync::atomic::{AtomicI64, Ordering};

static NEXT: AtomicI64 = AtomicI64::new(1);

djogi::primary_key! {
    pub struct MyClientId(i64);
    sql_type = "BIGINT";
    default_sql = "0";
    bulk_sql = "SELECT 0::bigint AS id FROM generate_series(1, $1)";
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
