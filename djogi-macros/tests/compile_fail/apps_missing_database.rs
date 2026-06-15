// `#[app(...)]` without `database = "…"` is rejected.

djogi::apps! {
 #[app(label = "vehicles")]
 pub struct Vehicles;
}

fn main() {}
