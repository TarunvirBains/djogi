// Phase 7-Zero v3 T7 — `#[app(...)]` without `database = "…"` is rejected.

djogi::apps! {
    #[app(label = "vehicles")]
    pub struct Vehicles;
}

fn main() {}
