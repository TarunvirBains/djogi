//! Query API — lazy `QuerySet<T>`, typed filters, SQL emission.
//!
//! The public surface is re-exported at crate root and in `prelude`:
//! users write `use djogi::prelude::*;` and get `QuerySet`, `FieldRef`,
//! `Lookup`, etc. without a second import.
//!
//! Internally: `queryset` holds the builder state, `condition` the filter
//! tree, `field` the typed column handles, `order` ordering expressions,
//! `filter` the programmatic-builder types, `update` bulk-update assignments,
//! `sql` the `ConditionBuilder` + SQL emitters, and `terminal` the `fetch_*`
//! methods. Splitting by responsibility keeps each file auditable.
//!
//! # Public vs internal surface
//!
//! User code composes filters through `Condition` + `FieldRef` lookup
//! methods — **never** by constructing `Leaf`/`FilterValue`/`LookupOp`
//! variants directly. The raw AST types remain reachable under
//! [`internal`] for in-tree consumers (the Task 6 SQL emitter, migration
//! differ, shell bindings) and for integration tests that assert on the
//! tree shape, but they are not peer public API with `Condition` /
//! `FieldRef`. Treat paths inside `internal` as unstable — variant names,
//! payload shapes, and the module layout can shift across phases without
//! a semver bump.

pub mod aggregate;
pub mod annotate;
pub mod condition;
pub mod field;
pub mod filter;
pub mod grouped;
pub(crate) mod lock;
pub mod order;
pub mod queryset;
pub mod sql;
pub mod stream;
pub mod terminal;
pub mod update;

pub use aggregate::AggregateQuery;
pub use annotate::{AnnotatedQuerySet, IntoAggregateTuple};
pub use condition::Condition;
pub use field::{FieldRef, IntoFilterValue};
pub use filter::{FilterClause, Lookup, ModelFilter};
pub use order::{Direction, NullsOrder, OrderExpr};
pub use queryset::{DistinctMode, IntoDistinctColumns, QuerySet};
pub use stream::{ModelCursorStream, RawCursorStream};
pub use update::{IntoAssignments, UpdateAssignment, UpdateStmt};

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
