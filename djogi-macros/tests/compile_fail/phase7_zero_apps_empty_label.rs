// Phase 7-Zero v3 T7 — explicit `#[app(label = "")]` is rejected
// (§3 ASCII-shape rule requires non-empty labels).

djogi::apps! {
    #[app(label = "", database = "main")]
    pub struct Vehicles;
}

fn main() {}
