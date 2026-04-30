//! Elephant — individual elephants.
//!
//! Demonstrates:
//! - `parent: Option<ForeignKey<Self>>` — self-referential FK for
//!   matriarchal lineage. Djogi does NOT ship a tree-query API; the
//!   `lineage` demo uses a raw recursive CTE via the `sqlx::QueryBuilder`
//!   escape hatch (intentional honesty about scope).
//! - `Jsonb<ElephantTags>` — typed JSONB. Unknown fields (rows with
//!   keys absent from `ElephantTags`) are preserved across `save()`,
//!   not silently dropped.
//! - `version: i32` for optimistic locking. `Elephant::tags` updates
//!   bump the version; concurrent updates fail with `GoneAggregate`.
//! - `Tracked<String>` on `name` — name changes are audited.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};
use crate::models::Herd;

#[djogi::model(table = "elephants")]
#[derive(Debug, Clone)]
pub struct Elephant {
    pub name: Tracked<String>,

    pub herd: ForeignKey<Herd>,

    /// Self-FK for matriarchal lineage. `None` means "matriarch — origin
    /// of the line as far as the database knows."
    pub parent: Option<ForeignKey<Elephant>>,

    pub estimated_birth_year: Option<i16>,

    /// Typed JSONB. Unknown fields preserved on round-trip.
    pub tags: Jsonb<ElephantTags>,

    /// Optimistic-locking column. Concurrent updates fail loudly rather
    /// than last-write-wins.
    #[field(version)]
    pub version: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ElephantTags {
    /// Distinguishing physical marks reported by field staff.
    #[serde(default)]
    pub physical_marks: Vec<String>,

    /// `f` or `m`. Optional because not all sightings can determine sex.
    #[serde(default)]
    pub sex: Option<String>,

    /// Free-form notes researchers attach to a specific elephant.
    #[serde(default)]
    pub field_notes: Option<String>,
}
