//! Adopter-linked `djogi` CLI for elephant-tracker.
//!
//! This keeps descriptor-dependent commands (`compose`, `schema`, `docs`,
//! etc.) out of the app CLI and routed through `cargo djogi` while
//! still keeping migration internals and seed behavior in the example.

djogi_cli::djogi_main!(
    elephant_tracker::models::Country,
    elephant_tracker::models::Researcher,
    elephant_tracker::models::Herd,
    elephant_tracker::models::HerdRange,
    elephant_tracker::models::Elephant,
    elephant_tracker::models::ElephantAncestry,
    elephant_tracker::models::Sighting,
);
