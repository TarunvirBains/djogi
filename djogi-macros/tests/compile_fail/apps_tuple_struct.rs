// tuple structs inside `djogi::apps!` are rejected.
// Apps are zero-sized unit structs.

djogi::apps! {
    #[app(database = "main")]
    pub struct Vehicles(u32);
}

fn main() {}
