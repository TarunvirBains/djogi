//! `JoinedRow<T>` — the post-`select_related` row wrapper.
//!
//! # What
//!
//! [`JoinedRow<T>`] pairs a parent-query row with the child rows the
//! `LEFT JOIN` materialised for it in the same round trip. User code
//! obtains a `Vec<JoinedRow<T>>` from
//! [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined)
//! and reads joined relations via [`JoinedRow::get`], typed by the same
//! `RelationPath<Source, Target>` that was passed to `.select_related(...)`.
//!
//! The wrapper shape mirrors [`PrefetchedRow<T>`](crate::relation::PrefetchedRow)
//! — same `row` field, same `get(path)` accessor, same `&'static str`
//! source-column key on the resolved-relations map. The *decoding path*
//! differs: prefetch issues a follow-up query and decodes the child's
//! columns from their own result set, while select_related decodes the
//! child from the **same** row as the parent, under aliased column names.
//!
//! # Why a new wrapper (not reuse `PrefetchedRow`)
//!
//! The two terminals produce structurally identical maps from
//! `source_column -> Box<dyn Any>`, but they ship on opt-in terminals
//! (`fetch_all_prefetched` / `fetch_all_joined`) with distinct generic
//! bounds — prefetch needs `T::Pk: Encode + Type + ...` for the
//! follow-up `IN (...)` bind, whereas select_related needs `T:
//! FromJoinedPgRow` for single-row decoding. Giving each path its own
//! wrapper type keeps those bounds from leaking into the other
//! terminal's signature and makes the one-query-vs-two-query split
//! legible in the types: "I have a `JoinedRow<Vehicle>`" tells the
//! reader select_related ran; "I have a `PrefetchedRow<Vehicle>`" tells
//! the reader prefetch ran. A future amendment could collapse them
//! behind a shared `RelatedRow<T>` trait, but the phase-3 priority is
//! clarity of mechanism at the API boundary.
//!
//! # How the SQL carries both sides through one row
//!
//! The parent table's columns come through the result set unqualified
//! (`id`, `make`, `owner_id`, …). Each joined child's columns are
//! aliased in the SELECT list under a prefix derived from the
//! [`RelationPath::source_column`]:
//!
//! ```sql
//! SELECT vehicles_p3.*,
//!        rel_owner_id.id   AS "rel_owner_id.id",
//!        rel_owner_id.name AS "rel_owner_id.name",
//!        …
//! FROM vehicles_p3
//! LEFT JOIN owners_p3 rel_owner_id ON vehicles_p3.owner_id = rel_owner_id.id
//! ```
//!
//! The quoted alias `"rel_owner_id.id"` embeds a literal dot, matching
//! the table-qualified shape Postgres would emit for a `.select(t.*)`
//! cross-table projection. `row.try_get("rel_owner_id.id")` returns
//! the child's `id` — and `try_get_raw("rel_owner_id.id").is_null()`
//! distinguishes LEFT JOIN misses (all child columns NULL) from live
//! child rows, matching the probe Task 4's prefetch loader established.
//!
//! # Where
//!
//! Consumed by [`crate::relation::select_related::apply_select_related`]
//! and emitted from [`crate::query::terminal::QuerySet::fetch_all_joined`].
//! The post-fetch wrapper is returned as-is to user code — there is no
//! terminal-free "join this and forget about prefetch" access path in
//! Phase 3; callers consume the typed handle or reach for the raw
//! `ctx.raw_execute` / `ctx.raw_scalar` escape hatch.

use crate::model::Model;
use crate::relation::path::RelationPath;
use std::any::Any;
use std::collections::HashMap;

/// Post-`select_related` wrapper pairing a main-query row with joined
/// relations.
///
/// Produced by
/// [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined).
/// Access the underlying row via the public [`JoinedRow::row`] field and
/// joined relations via [`JoinedRow::get`].
///
/// # Why the relations map keys on `&'static str`
///
/// Same reasoning as [`PrefetchedRow`](crate::relation::PrefetchedRow):
/// two paths can legitimately point at the same target type (e.g.
/// `author: ForeignKey<User>` and `editor: ForeignKey<User>` on a
/// `Post` — both `User`), so a `TypeId` key would collapse them. The
/// source column name is the natural discriminator — unique per
/// relation by macro-emission rules and directly available on every
/// [`RelationPath`].
///
/// # Ownership
///
/// `Box<dyn Any + Send + Sync>` lets the map carry heterogeneous child
/// types without a variadic generic; `Send + Sync` propagates so
/// `Vec<JoinedRow<T>>` can cross async task boundaries. When a path is
/// registered via `.select_related(...)` and the corresponding LEFT JOIN
/// missed (NULL FK or orphan target), the map carries no entry for that
/// source column — [`get`](JoinedRow::get) returns `None`, matching the
/// prefetch wrapper's contract.
pub struct JoinedRow<T: Model> {
    /// The parent-query row, decoded via
    /// [`FromJoinedPgRow::from_joined_pg_row`] with an empty prefix.
    /// Relation columns on this row remain in their raw (unresolved)
    /// shape — the `ForeignKey<T>` wrapper is just a PK newtype; the
    /// joined child lives in [`JoinedRow::relations`], not here.
    pub row: T,
    /// Joined child rows, keyed by
    /// [`RelationPath::source_column`](crate::relation::RelationPath::source_column).
    /// Entries are present only for paths whose LEFT JOIN resolved to
    /// a live child row; missed joins (NULL FK or orphan target) are
    /// absent here and [`get`](JoinedRow::get) returns `None` for them.
    relations: HashMap<&'static str, Box<dyn Any + Send + Sync>>,
}

impl<T: Model + std::fmt::Debug> std::fmt::Debug for JoinedRow<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Match `PrefetchedRow::Debug` — render the resolved-relation
        // map as its key set. The map values cannot be formatted
        // generically but the source-column keys are strings and
        // useful on their own for debugging which joins attached.
        let keys: Vec<&&'static str> = self.relations.keys().collect();
        f.debug_struct("JoinedRow")
            .field("row", &self.row)
            .field("joined_relations", &keys)
            .finish()
    }
}

impl<T: Model> JoinedRow<T> {
    /// Look up a joined relation by its [`RelationPath`].
    ///
    /// Returns `Some(&Target)` when the main-query row carried a
    /// non-null FK **and** the child row existed at join time. Returns
    /// `None` for nullable FKs whose column was `NULL`, for FKs pointing
    /// at child rows that were missing (LEFT JOIN miss), and for any
    /// path that was never registered via
    /// [`QuerySet::select_related`](crate::query::QuerySet::select_related).
    ///
    /// The typed `RelationPath<T, Target>` argument means mismatched
    /// target types fail at the type level — a
    /// `RelationPath<Vehicle, Owner>` can only be read back as
    /// `&Owner`, never as `&FuelType`.
    pub fn get<Target: Model + 'static>(&self, path: RelationPath<T, Target>) -> Option<&Target> {
        self.relations
            .get(path.source_column())
            .and_then(|b| b.downcast_ref::<Target>())
    }

    /// Crate-private constructor used by
    /// [`apply_select_related`](crate::relation::select_related::apply_select_related).
    /// Not part of the public API — rows originate from the terminal
    /// method, never from user code.
    pub(crate) fn new(
        row: T,
        relations: HashMap<&'static str, Box<dyn Any + Send + Sync>>,
    ) -> Self {
        Self { row, relations }
    }

    /// Crate-private mutable access to the resolved-relations map.
    /// Used by the terminal to stitch **prefetch** entries (follow-up
    /// query path) into a `JoinedRow` that originated from a
    /// `select_related`-driven main query. Both code paths key on
    /// `RelationPath::source_column`, so stitching from either side is
    /// a straightforward HashMap insert.
    pub(crate) fn relations_mut(
        &mut self,
    ) -> &mut HashMap<&'static str, Box<dyn Any + Send + Sync>> {
        &mut self.relations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::relation::path::{RelationKind, RelationPath};
    use crate::types::HeerId;
    use std::future::Future;

    // Same stub-Model pattern as the sibling modules in `relation/`.
    #[derive(Debug, Clone)]
    struct Parent;
    #[derive(Debug, Clone)]
    struct Child;

    macro_rules! dummy_model {
        ($ty:ty, $table:literal) => {
            impl crate::model::__sealed::Sealed for $ty {}
            #[allow(clippy::manual_async_fn)]
            impl crate::model::Model for $ty {
                type Pk = HeerId;
                type Fields = ();
                fn table_name() -> &'static str {
                    $table
                }
                fn pk_value(&self) -> &HeerId {
                    unreachable!()
                }
                fn descriptor() -> &'static ModelDescriptor {
                    unreachable!()
                }
                fn get(
                    _ctx: &mut crate::context::DjogiContext,
                    _id: HeerId,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn create(
                    _ctx: &mut crate::context::DjogiContext,
                    _v: Self,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn save<'ctx>(
                    &'ctx mut self,
                    _ctx: &'ctx mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
                fn delete(
                    self,
                    _ctx: &mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn refresh_from_db<'ctx>(
                    &'ctx self,
                    _ctx: &'ctx mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
            }
        };
    }
    dummy_model!(Parent, "parents");
    dummy_model!(Child, "children");

    #[test]
    fn joined_row_get_returns_child_when_present() {
        // Manually construct a JoinedRow with a pre-resolved Child.
        // Confirms the source-column-keyed lookup + downcast round-trip
        // works without touching a real DB or emitting SQL.
        let row = Parent;
        let child = Child;
        let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> = HashMap::new();
        relations.insert("child_id", Box::new(child) as Box<dyn Any + Send + Sync>);
        let jr: JoinedRow<Parent> = JoinedRow::new(row, relations);

        let path: RelationPath<Parent, Child> =
            RelationPath::new("child_id", "children", RelationKind::ForeignKey);
        assert!(jr.get(path).is_some(), "joined child should be present");
    }

    #[test]
    fn joined_row_get_returns_none_when_missing() {
        // No entry for the path's source column -> None. Covers the
        // LEFT JOIN miss (NULL FK / orphan) at the lookup layer.
        let jr: JoinedRow<Parent> = JoinedRow::new(Parent, HashMap::new());
        let path: RelationPath<Parent, Child> =
            RelationPath::new("child_id", "children", RelationKind::ForeignKey);
        assert!(jr.get(path).is_none());
    }

    #[test]
    fn joined_row_debug_lists_relation_keys() {
        // Debug output should name the joined-relation columns so
        // operators can see which relations attached at a glance —
        // parity with PrefetchedRow's Debug shape.
        let mut relations: HashMap<&'static str, Box<dyn Any + Send + Sync>> = HashMap::new();
        relations.insert("child_id", Box::new(Child) as Box<dyn Any + Send + Sync>);
        let jr: JoinedRow<Parent> = JoinedRow::new(Parent, relations);
        let debug = format!("{jr:?}");
        assert!(
            debug.contains("child_id"),
            "Debug should list the joined relation keys, got: {debug}"
        );
    }

    #[test]
    fn joined_row_relations_mut_allows_stitching() {
        // The terminal uses `relations_mut` to fold prefetched entries
        // into a JoinedRow after the main JOIN decode finishes. Pins
        // that the crate-private hook exists and mutates the map.
        let mut jr: JoinedRow<Parent> = JoinedRow::new(Parent, HashMap::new());
        jr.relations_mut()
            .insert("child_id", Box::new(Child) as Box<dyn Any + Send + Sync>);
        let path: RelationPath<Parent, Child> =
            RelationPath::new("child_id", "children", RelationKind::ForeignKey);
        assert!(
            jr.get(path).is_some(),
            "relations_mut insert should be observable via get"
        );
    }
}
