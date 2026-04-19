//! `JoinedRow<T>` — the post-`select_related` row wrapper + its decoder trait.
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
//! FromJoinedRow` for single-row decoding. Giving each path its own
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
//! # The `FromJoinedRow` trait — decoder contract
//!
//! Every `#[model]`-emitted struct implements
//! [`FromJoinedRow`]. The implementation iterates the struct's fields
//! and calls `row.try_get::<Type, _>(&format!("{prefix}{column}"))` for
//! each, exactly mirroring the unprefixed lookups [`FromRow`] already
//! performs. Parent decoding passes an empty prefix (`""`); child
//! decoding passes `"rel_{source_column}."`. Both cases share one
//! method, so the macro only emits one extra impl per model.
//!
//! `FromJoinedRow` is macro-emitted rather than blanket-implemented via
//! `FromRow` because sqlx's `FromRow` reads columns by their bare name
//! — there is no "prefix" knob to thread through, and intercepting
//! every lookup with a newtype wrapper around `PgRow` would be both
//! slower (indirection per column) and more invasive than the
//! sibling impl the macro already emits for `FromRow`.
//!
//! # Where
//!
//! Consumed by [`crate::relation::select_related::apply_select_related`]
//! and emitted from [`crate::query::terminal::QuerySet::fetch_all_joined`].
//! The post-fetch wrapper is returned as-is to user code — there is no
//! terminal-free "join this and forget about prefetch" access path in
//! Phase 3; callers consume the typed handle or reach for the raw
//! `sqlx::QueryBuilder` escape hatch.

use crate::model::Model;
use crate::relation::path::RelationPath;
use sqlx::postgres::PgRow;
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
    /// [`FromJoinedRow::from_prefixed_row`] with an empty prefix.
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

/// Prefix-aware row decoder emitted per model by `#[model]`.
///
/// # Contract
///
/// For every field `name: Ty` on the struct (including the framework-
/// injected `id` / `created_at` / `updated_at`), the generated impl
/// decodes `row.try_get::<Ty, _>(&format!("{prefix}{name}"))`. Parent
/// decoding passes `""`; child decoding passes `"rel_{source_column}."`
/// — matching the alias shape [`crate::relation::select_related`]
/// emits in its `SELECT` list.
///
/// # Why a bespoke trait (not a `FromRow` adapter)
///
/// sqlx's [`FromRow`](sqlx::FromRow) looks columns up by bare name —
/// there is no hook to rename them at decode time. A wrapper newtype
/// around `PgRow` that intercepted every lookup would work but adds an
/// extra indirection per column; the proc macro already generates
/// field-name-aware decode blocks for `FromRow`, so emitting a sibling
/// prefix-aware version is both simpler and closer to the existing
/// implementation shape.
///
/// # Phase 3 scope
///
/// The trait signature is deliberately minimal — a single `prefix`
/// parameter, one `try_get` per field, no hook for per-field codecs or
/// computed columns. Future amendments that need joined decoding for
/// expression columns (Phase 4) or projection aliases (Phase 4.5) can
/// extend the trait with default-methods without breaking existing
/// impls.
pub trait FromJoinedRow: Sized {
    /// Decode `Self` from `row`, reading each field under
    /// `"{prefix}{field_name}"` via
    /// [`try_get`](sqlx::Row::try_get).
    ///
    /// An empty prefix (`""`) degenerates to the same column lookups
    /// [`FromRow`](sqlx::FromRow) would perform — the parent side of
    /// a `select_related` join uses this spelling. A non-empty prefix
    /// of the form `"rel_{source_column}."` matches the aliased
    /// columns the select_related SQL emitter produces for the
    /// child side.
    fn from_prefixed_row(row: &PgRow, prefix: &str) -> Result<Self, sqlx::Error>;
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
                fn get<'a>(
                    _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    _id: HeerId,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn create<'a>(
                    _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    _v: Self,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn save<'a>(
                    &self,
                    _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn delete<'a>(
                    self,
                    _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                ) -> impl Future<Output = Result<(), crate::DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn refresh_from_db<'a>(
                    &self,
                    _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                ) -> impl Future<Output = Result<Self, crate::DjogiError>> + Send {
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
            RelationPath::__new("child_id", "children", RelationKind::ForeignKey);
        assert!(jr.get(path).is_some(), "joined child should be present");
    }

    #[test]
    fn joined_row_get_returns_none_when_missing() {
        // No entry for the path's source column -> None. Covers the
        // LEFT JOIN miss (NULL FK / orphan) at the lookup layer.
        let jr: JoinedRow<Parent> = JoinedRow::new(Parent, HashMap::new());
        let path: RelationPath<Parent, Child> =
            RelationPath::__new("child_id", "children", RelationKind::ForeignKey);
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
            RelationPath::__new("child_id", "children", RelationKind::ForeignKey);
        assert!(
            jr.get(path).is_some(),
            "relations_mut insert should be observable via get"
        );
    }
}
