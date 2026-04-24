// Phase 7-Zero v3 T7 — explicit `#[app(label = "bad char!")]` fails the
// §3 ASCII-shape rule (no spaces, no `!`).

djogi::apps! {
    #[app(label = "bad char!", database = "main")]
    pub struct Vehicles;
}

fn main() {}
