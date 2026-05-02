//! Elephant — individual elephants.
//!
//! ## What this demonstrates
//!
//! - **Multi-edge self-FKs** — `mother_id` + `father_id` both point at
//!   `Elephant`, mirroring biological pedigree where every individual
//!   has at most one mother and one father (each potentially unknown).
//!   `Model::full_ancestors(id)` walks both edges in a single recursive
//!   CTE preserving path multiplicity (Wright kinship requires summing
//!   independent connecting paths). The single-edge `tree_ancestors`
//!   /`tree_descendants` builders walk one named edge — typically the
//!   matrilineal `mother_id` for herd-society semantics; the `lineage`
//!   demo's raw recursive CTE uses the same matrilineal edge.
//! - **Macro-generated relation accessors**: `mother_id` → `ElephantRelated::mother()`,
//!   `father_id` → `ElephantRelated::father()` (the `_id` suffix is
//!   stripped by the framework's relation-name convention).
//! - `Jsonb<ElephantTags>` — typed JSONB with unknown-field preservation.
//!   A row whose JSON contains keys not present on `ElephantTags` (added
//!   by an older or newer version of the schema, hand-edited in psql,
//!   etc.) round-trips those keys through every `save()` instead of
//!   silently dropping them.
//! - `#[field(version)]` — optimistic locking. Every `save()` bumps the
//!   `version` column inside the same UPDATE that touches user fields.
//!   A `save()` whose pre-image version no longer matches the row's
//!   current version returns [`DjogiError::GoneAggregate`] rather than
//!   silently overwriting a concurrent edit.
//! - `Tracked<String>` on `name` — name changes write to the structural
//!   CRUD audit log without explicit calls.
//! - `no_default` — `ForeignKey<Herd>` does not implement `Default`, so
//!   the macro's `Default` impl is suppressed; callers populate every
//!   field explicitly.

use crate::models::Herd;
use djogi::prelude::*;

#[model(table = "elephants", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Elephant {
    pub name: Tracked<String>,

    pub herd_id: ForeignKey<Herd>,

    /// Maternal self-FK. `None` means the mother is unknown / not in
    /// the dataset — common for older matriarchs and members of
    /// unmonitored herds. Realistic seed data populates this for
    /// roughly 70% of individuals.
    pub mother_id: Option<ForeignKey<Elephant>>,

    /// Paternal self-FK. `None` means the father is unknown — much
    /// more common than unknown mothers in elephant-society research
    /// because females are observed nursing while males are
    /// peripheral. Realistic seed data populates this for roughly
    /// 40% of individuals.
    pub father_id: Option<ForeignKey<Elephant>>,

    pub estimated_birth_year: Option<i16>,

    /// Typed JSONB. Unknown fields are preserved across every `save()`.
    pub tags: Jsonb<ElephantTags>,

    /// Optimistic-locking column. Concurrent updates fail loudly with
    /// `GoneAggregate` instead of last-write-wins.
    #[field(version)]
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ElephantTags {
    /// Distinguishing physical marks reported by field staff
    /// (`"left tusk chipped"`, `"notch in right ear"`, etc.).
    #[serde(default)]
    pub physical_marks: Vec<String>,

    /// `"f"` or `"m"`. Optional because not all sightings can determine
    /// sex.
    #[serde(default)]
    pub sex: Option<String>,

    /// Free-form notes researchers attach to a specific elephant.
    #[serde(default)]
    pub field_notes: Option<String>,
}
