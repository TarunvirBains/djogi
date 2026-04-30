//! Domain models for the elephant-tracker example.
//!
//! Each module's docstring explains which Djogi feature it demonstrates.
//! Models are listed in dependency order — `country` and `researcher`
//! have no FK dependencies; `herd` depends on `country` via `herd_range`;
//! `elephant` depends on `herd` and self-references for lineage;
//! `sighting` depends on `elephant` and `researcher`.

pub mod country;
pub mod elephant;
pub mod herd;
pub mod herd_range;
pub mod researcher;
pub mod sighting;

pub use country::Country;
pub use elephant::{Elephant, ElephantTags};
pub use herd::Herd;
pub use herd_range::HerdRange;
pub use researcher::Researcher;
pub use sighting::Sighting;

// Many-to-many declarations.
//
// Djogi's `many_to_many!` macro takes bare type identifiers (not paths),
// so it must be invoked from a module where every referenced type is
// directly in scope. `mod.rs` is that module — `Herd`, `Country`, and
// `HerdRange` are all re-exported above.
//
// One direction per invocation: this stamp out the `Herd ↔ Country` half.
// The example does not need the reverse `Country -> Herd` direction;
// adopters who want it would add a second `many_to_many!` block.
djogi::many_to_many!(
    Herd,
    Country,
    through = HerdRange,
    this_fk = herd_id,
    that_fk = country_id,
    relation = "countries",
);
