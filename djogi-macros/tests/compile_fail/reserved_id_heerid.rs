// Under the default `pk = HeerIdRecencyBiased` strategy (T2
// flip), the macro injects `id` as HeerIdDesc, so a user `id` field
// collides and must be rejected with a targeted macro diagnostic that
// points at the offending field. The same error also fires under
// `pk = HeerId` / `pk = RanjId` etc. — any strategy that injects `id`.
use djogi::prelude::*;

#[model(table = "posts")]
struct Bad {
    pub id: String,
    pub title: String,
}

fn main() {}
