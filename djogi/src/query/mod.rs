//! Query API — lazy `QuerySet<T>`, typed filters, SQL emission.
//!
//! The public surface is re-exported at crate root and in `prelude`:
//! users write `use djogi::prelude::*;` and get `QuerySet`, `FieldRef`,
//! `Lookup`, `Q`, etc. without a second import.
//!
//! Internally: `queryset` holds the builder state, `condition` the filter
//! tree (the legacy substrate, retired by Cluster 8γ T6.9), `q` the
//! public Q-algebra (the post-8γ substrate), `field` the typed column
//! handles, `order` ordering expressions, `filter` the programmatic-builder
//! types, `update` bulk-update assignments, `sql` the `ConditionBuilder` +
//! SQL emitters, and `terminal` the `fetch_*` methods. Splitting by
//! responsibility keeps each file auditable.
//!
//! # Public vs internal surface
//!
//! User code composes filters through `Q<T>` (the public algebra
//! introduced by Cluster 8γ) and `FieldRef` lookup methods — **never** by
//! constructing `Leaf`/`FilterValue`/`LookupOp` variants directly. The
//! raw AST types remain reachable under [`internal`] for in-tree
//! consumers (the SQL emitter, migration differ, shell bindings) and
//! for integration tests that assert on the tree shape, but they are
//! not peer public API with `Q` / `FieldRef`. Treat paths inside
//! `internal` as unstable — variant names, payload shapes, and the
//! module layout can shift across phases without a semver bump.
//!
//! # Substrate — Q<T> alongside Condition during the 8γ transition
//!
//! `Condition` is the pre-8γ filter tree; `Q<T>` is the post-8γ
//! public algebra. The Cluster 8γ refactor introduces `Q<T>` as an
//! additive surface first (T6.1–T6.5 + T6.10–T6.13) so adopters and
//! sister clusters (8β / 8δ / 8ε) can compose against the new shape
//! without waiting for the substrate swap. T6.6–T6.9 then retire the
//! internal `Condition` enum and route every `QuerySet<T>::filter`
//! through `Q<T>`. Both types remain reachable through this stage of
//! the work; `pub use condition::Condition` stays in place so existing
//! `FieldRef::eq` / `gt` / `ilike` etc. callers continue compiling.

pub mod aggregate;
pub mod annotate;
pub mod closure;
pub mod condition;
pub mod field;
pub mod filter;
pub mod grouped;
pub(crate) mod lock;
pub mod order;
pub mod q;
pub mod queryset;
pub mod recursive;
#[cfg(feature = "spatial")]
pub mod spatial_grouping;
pub(crate) mod sql;
pub mod stream;
pub mod terminal;
pub mod update;
pub mod visage_queryset;

pub use aggregate::AggregateQuery;
pub use annotate::{AnnotatedQuerySet, IntoAggregateTuple};
pub use closure::{ClosureModel, MaterializeClosureOptions, MaterializeClosureReport};
pub use condition::Condition;
pub use field::{FieldRef, IntoFilterValue, OptionalRelationRef};
pub use filter::{FilterClause, Lookup, ModelFilter};
pub use order::{Direction, NullsOrder, OrderExpr};
pub use q::{ArrayPredicate, IntoQ, Q};
pub use queryset::{DistinctMode, IntoDistinctColumns, QuerySet};
pub use recursive::{RecursiveDirection, RecursiveQuerySet};
// `BasicPredicate<T>` is sassi's universal Rust-evaluable predicate algebra.
// Re-exported here so adopters reach it as `djogi::query::BasicPredicate`
// without depending on sassi directly. The Cluster 8γ refactor (T6) lifts
// the 15 Rust-evaluable `LookupOp` variants into `sassi::BasicPredicate`
// while keeping the 2 SQL-only ops (`Regex`, `IRegex`) on the djogi side
// (`Q::Regex`) — see spec §8e bullet 6 and `decisions.md` row 107 + 108.
pub use sassi::BasicPredicate;
#[cfg(feature = "spatial")]
pub use spatial_grouping::{ClusterId, ClusterRadius, GeohashKey, GeohashPrecision, RegionKey};
pub use stream::{ModelCursorStream, RawCursorStream};
pub use update::{IntoAssignments, UpdateAssignment, UpdateStmt};
pub use visage_queryset::VisageQuerySet;

/// Raw Condition-AST surface — not peer public API with `Condition`.
///
/// Holds `Leaf`, `FilterValue`, and `LookupOp` for framework-internal
/// consumers (SQL emitter, differ, shell). User code that finds itself
/// reaching for these is a sign the `FieldRef` API is missing a lookup
/// method — please file an issue rather than building leaves by hand.
///
/// # Stability
///
/// Items re-exported here follow the same variant-level `#[non_exhaustive]`
/// guarantees as `FilterValue` / `LookupOp` themselves, but the **set of
/// items** in this module, and its path, may change across phases. Pin
/// your own type aliases if you depend on them.
pub mod internal {
    pub use super::condition::{FilterValue, Leaf, LookupOp};
}

#[cfg(test)]
mod tests {
    /// Compile-only sanity: `BasicPredicate<T>` is reachable both as
    /// `sassi::BasicPredicate` (the originating crate path) and as
    /// `crate::query::BasicPredicate` (the re-export adopters depend on).
    /// This locks the re-export contract: removing the `pub use` line
    /// would silently break adopter call-sites; this test catches it
    /// at the compilation step.
    #[test]
    fn basic_predicate_reachable_from_djogi_query() {
        let _: sassi::BasicPredicate<()> = sassi::BasicPredicate::True;
        let _: crate::query::BasicPredicate<()> = crate::query::BasicPredicate::True;
    }
}
