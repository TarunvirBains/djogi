// Phase 7-Zero v3 T7 — unknown keys inside `#[app(...)]` are rejected.
// Only `label` and `database` are valid today; lifecycle markers land
// in T8.

djogi::apps! {
    #[app(database = "main", ghost = "no")]
    pub struct Vehicles;
}

fn main() {}
