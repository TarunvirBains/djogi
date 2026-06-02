// two apps landing on the same label inside a
// single `djogi::apps!` invocation are rejected at macro-expansion time.
// Here `Vehicles` (default → "vehicles") collides with an explicit
// `label = "vehicles"` on the second struct.

djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;

    #[app(label = "vehicles", database = "main")]
    pub struct Cars;
}

fn main() {}
