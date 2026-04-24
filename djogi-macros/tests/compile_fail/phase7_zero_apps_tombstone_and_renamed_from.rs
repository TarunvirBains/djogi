// Phase 7-Zero v3 T8 — `tombstone` and `renamed_from` are mutually
// exclusive. A tombstoned app is being retired, not renamed.

djogi::apps! {
    #[app(database = "main", tombstone, renamed_from = "prior")]
    pub struct OldThing;
}

fn main() {}
