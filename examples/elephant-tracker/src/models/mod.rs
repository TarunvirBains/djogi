//! Domain models for the elephant-tracker example.
//!
//! Each module's docstring explains which Djogi feature it demonstrates.
//! Models are listed in dependency order — `country` and `researcher` have
//! no FK dependencies; `herd` depends on `country` via `herd_range`;
//! `elephant` depends on `herd` and self-references for lineage;
//! `sighting` depends on `elephant` and `researcher`.

pub mod country;
pub mod researcher;
pub mod herd;
pub mod herd_range;
pub mod elephant;
pub mod sighting;

pub use country::Country;
pub use researcher::Researcher;
pub use herd::Herd;
pub use herd_range::HerdRange;
pub use elephant::{Elephant, ElephantTags};
pub use sighting::Sighting;
