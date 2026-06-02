// explicit `label = "…"` overrides must
// satisfy the §3 length cap of 63 bytes. This fixture supplies a
// 64-byte label; the macro rejects it with a length-specific error
// pointing at the offending literal.

djogi::apps! {
    #[app(label = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", database = "main")]
    pub struct Vehicles;
}

fn main() {}
