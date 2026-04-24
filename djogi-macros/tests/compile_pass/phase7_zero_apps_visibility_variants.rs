// Phase 7-Zero v3 T7 — visibility tokens round-trip into the emitted struct.
//
// `pub(crate)` and the no-visibility form are legal Rust and must be
// preserved. An underscore-leading identifier is also legal.
use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main")]
    pub(crate) struct Internal;

    #[app(database = "main")]
    struct _Hidden;
}

fn main() {
    assert_eq!(<Internal as App>::LABEL, "internal");
    assert_eq!(<_Hidden as App>::LABEL, "_hidden");
}
