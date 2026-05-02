//! `ElephantAncestry` — materialized transitive-closure of every
//! Elephant's pedigree.
//!
//! ## What this demonstrates
//!
//! - **`ClosureModel` trait + `Model::materialize_closure`** (Phase 8-Zero
//!   Cluster B4 substrate). One framework call repopulates the entire
//!   closure table from the live `elephants` graph; adopters never write
//!   the recursive walker themselves.
//! - **Multi-edge multiplicity preservation.** The `Elephant` model has
//!   two self-FK edges (`mother_id`, `father_id`). `materialize_closure`
//!   walks both edges in a single recursive CTE; an ancestor reachable
//!   from a source row through two distinct edge sequences (e.g. a
//!   common matrilineal + patrilineal ancestor in a linebred lineage)
//!   surfaces as `path_count = 2`. Wright kinship sums these
//!   independent paths; without multiplicity preservation the
//!   `mating-pairs` demo would silently under-estimate inbreeding.
//! - **Indexed lookup at runtime.** Once seeded, every kinship query
//!   becomes a single indexed JOIN against this closure table —
//!   orders of magnitude faster than re-walking the recursive CTE per
//!   query at scale (the production-scale answer per the v3 plan's
//!   scalability lens).
//!
//! ## Schema
//!
//! Each row records one (source, ancestor, depth, path_count) triple:
//!
//! - `elephant_id` — the source Elephant whose ancestry this row
//!   describes.
//! - `ancestor_id` — an ancestor reachable from the source via the
//!   pedigree graph. Self-pairs at `depth = 0` are included
//!   (`elephant_id = ancestor_id`, `path_count = 1`).
//! - `depth` — number of edges traversed from source to ancestor.
//!   `0` for the source itself; `1` for direct mother / father; `2`
//!   for grandparents; etc. Capped at the v3 plan's depth-5 budget
//!   when the helper is called with `with_max_depth(5)`.
//! - `path_count` — number of distinct edge sequences from source
//!   to ancestor. Equals `1` for source rows reachable through
//!   exactly one path; greater than `1` only when the ancestor sits
//!   on multiple ancestral lines (linebreeding).
//!
//! ## How it's populated
//!
//! `Elephant::materialize_closure::<ElephantAncestry>(ctx, opts)`
//! issues a single statement: a `WITH RECURSIVE` CTE that walks every
//! self-FK edge declared on `Elephant` (i.e. `mother_id` and
//! `father_id`), grouped by `(source, ancestor, depth)` with
//! `path_count = COUNT(*)`, then upserted into the closure table via
//! `ON CONFLICT … DO UPDATE SET path_count = EXCLUDED.path_count`.
//! The seed flow runs this after the elephants table is fully
//! populated so the closure aligns with the deterministic graph.

use crate::models::Elephant;
use djogi::prelude::*;

#[model(table = "elephant_ancestries", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct ElephantAncestry {
    /// Source Elephant. `ON DELETE CASCADE` — when an elephant is
    /// removed from the source table, every closure row keyed on it
    /// (whether as source or as ancestor) is removed too. Re-running
    /// `materialize_closure` after source mutations is the canonical
    /// way to keep the closure aligned; cascade just bounds the
    /// inconsistency window.
    #[field(on_delete = "cascade")]
    pub elephant_id: ForeignKey<Elephant>,

    /// Ancestor of `elephant_id`. Same cascade rationale as above.
    #[field(on_delete = "cascade")]
    pub ancestor_id: ForeignKey<Elephant>,

    pub depth: i32,
    pub path_count: i64,
}

impl djogi::query::ClosureModel for ElephantAncestry {
    type Source = Elephant;

    fn source_column() -> &'static str {
        "elephant_id"
    }
    fn ancestor_column() -> &'static str {
        "ancestor_id"
    }
    fn depth_column() -> &'static str {
        "depth"
    }
    fn path_count_column() -> &'static str {
        "path_count"
    }
}
