//! Elephant — individual elephants.
//!
//! ## What this demonstrates
//!
//! - `parent_id: Option<ForeignKey<Self>>` — a self-referential foreign key
//!   for matriarchal lineage. Djogi does not ship a tree-query API; the
//!   `lineage` demo uses a recursive CTE via `ctx.raw_rows`, the canonical
//!   escape hatch for shapes that fall outside the typed `QuerySet`
//!   surface.
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

    /// Self-FK for matriarchal lineage. `None` means the row is a
    /// matriarch — origin of the line as far as the database knows.
    pub parent_id: Option<ForeignKey<Elephant>>,

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
