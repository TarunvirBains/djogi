// Phase 7-Zero v3 T7 — two `djogi::apps!` invocations in the same
// crate collide on the hidden sentinel module.

djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles;
}

djogi::apps! {
    #[app(database = "main")]
    pub struct Users;
}

fn main() {}
