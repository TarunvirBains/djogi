//! Herd — a named family group of elephants.
//!
//! ## What this demonstrates
//!
//! - The `Herd` side of a many-to-many relationship to `Country` through
//! the explicit `HerdRange` model. Djogi does not provide implicit M2M
//! fields — every M2M is an explicit through model with whatever
//! payload the relationship needs. The macro invocation lives in
//! `mod.rs` because `many_to_many!` takes bare type identifiers.
//!
//! - A materialised `territory` polygon column — the convex hull of all
//! sightings observed for this herd, cached on the row so the
//! `mating-pairs` demo's `PairAreaOverlapRatio` pair-tuple annotation
//! can compute per-pair territory overlap in one SQL pass instead of
//! pre-aggregating from `Sighting` per query. Populated post-seed by
//! `seed::populate_herd_territories` and refreshed by adopter code on
//! the same cadence as the `Sighting` write stream.
//!
//! Adopters write `herd.countries(ctx).await` for the M2M side; they
//! construct `HerdSummary::from(&herd)` to get a hand-rolled projection
//! that exposes a `herd_size` side-query (see `crate::visages`).

use djogi::prelude::*;

#[model(table = "herds", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Herd {
    /// Display name. Unique within an organization in a real app;
    /// the example keeps the schema laxer.
    #[field(unique)]
    pub name: String,

    /// Estimated population at last census. The `HerdSummary` visage's
    /// `herd_size` side-query method surfaces the live count from
    /// `Elephant` without denormalising it onto this row.
    pub estimated_population: i32,

    /// Materialised convex-hull territory polygon over every sighting
    /// observed for this herd, in EPSG:4326 (lon/lat geography).
    /// `None` for herds with fewer than three sightings (the minimum
    /// `ST_ConvexHull(ST_Collect(...))` needs to return a non-degenerate
    /// polygon — fewer points yield a `LINESTRING` or `POINT` and the
    /// column stays NULL).
    ///
    /// The `mating-pairs` demo's `PairAreaOverlapRatio` annotation
    /// reads this column on both sides of a `Herd::self_pairs()` join
    /// to compute per-pair territory overlap in a single SQL pass. A
    /// `NULL` territory on either side makes the overlap ratio
    /// `NULL`, which decodes to `0.0` via the `NULLIF` guard in the
    /// annotation slot — no special-casing required on the adopter side.
    pub territory: Option<Polygon>,
}
