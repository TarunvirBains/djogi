//! Portable predicate SQL emission — error type + emit context shell.
//!
//! # Why a hidden public module?
//!
//! Phase 8eta PR2b installs a direct-`Q<T>` SQL walker that emits portable
//! predicates without first lowering them to `Condition`. The default
//! [`Model::__djogi_emit_field_predicate`](crate::model::Model::__djogi_emit_field_predicate)
//! hook lives on the public `Model` trait, and PR2d's macro override expands
//! into adopter crates. Both call sites have to name `SqlEmitContext` and
//! `PortablePredicateError` from a path that is reachable cross-crate but
//! does not pollute `cargo doc`. `#[doc(hidden)] pub mod portable;` plus the
//! `::djogi::__private::query::*` macro routing path satisfies both
//! constraints.
//!
//! # PR2a scope
//!
//! - Define `PortablePredicateError` (the typed lowering error).
//! - Define `SqlEmitContext` (the walker's parent-table threading channel).
//! - Provide the hidden `emit::*` shell that PR2b's SQL walker and PR2d's
//!   macro override consume.
//!
//! PR2a does **not** wire any builder fallibility, override the model hook,
//! or change the SQL-emit path. The current `q_to_condition` bridge keeps
//! handling all WHERE emission until PR2b lands.
//!
//! # The Model hook contract
//!
//! Every model gets a default `__djogi_emit_field_predicate` (added in
//! `crate::model`) that returns `PortablePredicateError::UnsupportedModel`.
//! PR2d will override this on macro-emitted `impl Model for {Model}` blocks
//! to dispatch on `(field_name, LookupOp)` and call into the hidden
//! `emit::*` helpers below. Hand-written `Model` impls (used by some
//! tests) keep the default and surface a typed error rather than panicking.

use crate::pg::accumulator::SqlAccumulator;

/// Typed error returned by the portable SQL lowering pipeline.
///
/// PR2a defines the variants; PR2b's direct-`Q<T>` walker plumbs them into
/// `DjogiError` through `query/terminal.rs` / `query/update.rs`. PR2c adds
/// regression coverage for cache-invalid manual-`Condition` ingress.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PortablePredicateError {
    /// The receiver model did not opt into portable SQL lowering. Hand-
    /// written `impl Model` blocks (test fixtures, internal stubs) hit this
    /// path because they keep the default
    /// `__djogi_emit_field_predicate` hook.
    #[error("model does not support portable SQL lowering: {model}")]
    UnsupportedModel {
        /// `core::any::type_name::<Self>()` of the receiver model.
        model: &'static str,
    },

    /// The `(field_name, LookupOp)` pair did not match any generated arm.
    /// Either the field is unknown to portable lowering (relation/visage
    /// path, JSONB, computed-FTS, etc.) or the operator is not portable on
    /// that field.
    #[error("field {field} does not support portable SQL lowering")]
    UnsupportedField {
        /// The Sassi `field_name` reported on the predicate leaf.
        field: &'static str,
    },

    /// The field is portable but the supplied operator is not. PR2d's
    /// generated wildcard-arm dispatch produces this when the macro saw a
    /// `LookupOp` variant the support matrix does not cover.
    #[error("field {field} lookup {op:?} is not portable to SQL")]
    UnsupportedLookup {
        /// The Sassi `field_name` reported on the predicate leaf.
        field: &'static str,
        /// The Sassi `LookupOp` the leaf carried.
        op: crate::types::LookupOp,
    },

    /// The captured operand value's runtime type did not match any payload
    /// shape the macro arm knew about. PR2d's generated arms emit this
    /// instead of panicking when `FieldPredicate::value_as::<V>()` returns
    /// `None`.
    #[error("field {field} lookup {op:?} had an unexpected payload type")]
    ValueTypeMismatch {
        /// The Sassi `field_name` reported on the predicate leaf.
        field: &'static str,
        /// The Sassi `LookupOp` the leaf carried.
        op: crate::types::LookupOp,
    },

    /// The field is a Djogi root field, but its Rust value type cannot be
    /// bound through `postgres_types::ToSql + Clone + Send + Sync + 'static`.
    /// User enums, codecs, and custom newtypes that do not satisfy the bind
    /// surface land here.
    #[error("field {field} type is not supported by portable SQL lowering")]
    UnsupportedFieldType {
        /// The Sassi `field_name` reported on the predicate leaf.
        field: &'static str,
    },

    /// A `Q<T>` node reached the portable cache/refresh boundary but is not
    /// reducible to a portable predicate. PR4 surfaces this through
    /// [`crate::query::QuerySet::try_portable`] / `cache(...)` /
    /// `refresh_into(...)` to keep cached values from drifting against
    /// SQL-only filters.
    #[error("query node {kind} cannot be used as a portable cache predicate")]
    CacheInvalidNode {
        /// Display name of the offending `Q<T>` variant or sub-node.
        kind: &'static str,
    },

    /// A future Sassi `BasicPredicate<T>` variant reached the portable SQL
    /// walker. Sassi marks the enum `#[non_exhaustive]`, so PR2b's
    /// `emit_portable_predicate` includes a wildcard arm that produces this
    /// error rather than panicking.
    #[error("Sassi predicate variant {kind} is not supported by Djogi SQL lowering")]
    UnsupportedPredicateKind {
        /// `&'static str` describing the unrecognised Sassi variant.
        kind: &'static str,
    },
}

/// SQL-emission context threaded through the direct-`Q<T>` walker.
///
/// `SqlEmitContext` carries the parent-table qualifier so portable
/// root-field predicates emitted under `build_select_joined` qualify their
/// columns as `<table>.<column>` while the same predicate emitted under
/// `build_select` stays unqualified. Generated portable arms always pass a
/// bare physical column name; the qualifier is added by
/// [`SqlEmitContext::push_column`].
///
/// The struct is `#[doc(hidden)]` because it appears in the public `Model`
/// trait signature for the `__djogi_emit_field_predicate` hook (so the
/// default impl in `crate::model` and PR2d's macro override both spell it
/// out), but adopter code never constructs one directly.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SqlEmitContext {
    parent_table: Option<&'static str>,
}

impl SqlEmitContext {
    /// Root context — no parent table, columns emit unqualified.
    /// Used by `build_select`, `build_count`, `build_update`, `build_delete`,
    /// and similar non-joined builders in PR2b.
    #[doc(hidden)]
    pub const fn root() -> Self {
        Self { parent_table: None }
    }

    /// Joined context — columns emit qualified as `<parent_table>.<column>`.
    /// Used by `build_select_joined` and visage-aware builders so portable
    /// predicates qualify root fields the same way `emit_condition`'s
    /// `parent_table` parameter does today.
    #[doc(hidden)]
    pub const fn joined(parent_table: &'static str) -> Self {
        Self {
            parent_table: Some(parent_table),
        }
    }

    /// Push a column reference into the accumulator with the appropriate
    /// qualifier.
    ///
    /// - Plain column names (no `.`): pre-qualified as
    ///   `<parent_table>.<column>` when the context is joined; emitted bare
    ///   otherwise.
    /// - Dotted/path columns (`rel.field`): emitted as-is. This is a
    ///   defensive guard for SQL-only condition paths that already carry a
    ///   qualified column; portable root-field arms must never pass a
    ///   dotted column because relation traversal is not portable.
    #[doc(hidden)]
    pub fn push_column(self, acc: &mut SqlAccumulator, column: &'static str) {
        if column.contains('.') {
            acc.push_sql(column);
            return;
        }
        if let Some(table) = self.parent_table {
            acc.push_sql(table);
            acc.push_sql(".");
        }
        acc.push_sql(column);
    }

    /// Crate-internal accessor — PR2b's `emit_q` walker reads this to
    /// thread the joined-query qualification channel through legacy
    /// `emit_condition(..., parent_table)` call sites.
    #[doc(hidden)]
    #[allow(dead_code)] // PR2b uses this to thread parent_table through emit_condition
    pub(crate) const fn parent_table(self) -> Option<&'static str> {
        self.parent_table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_context_emits_bare_column() {
        let mut acc = SqlAccumulator::new("");
        SqlEmitContext::root().push_column(&mut acc, "title");
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "title");
    }

    #[test]
    fn joined_context_qualifies_bare_column() {
        let mut acc = SqlAccumulator::new("");
        SqlEmitContext::joined("posts").push_column(&mut acc, "title");
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "posts.title");
    }

    #[test]
    fn dotted_column_is_emitted_as_is_under_root() {
        let mut acc = SqlAccumulator::new("");
        SqlEmitContext::root().push_column(&mut acc, "author.name");
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "author.name");
    }

    #[test]
    fn dotted_column_is_emitted_as_is_under_joined() {
        let mut acc = SqlAccumulator::new("");
        SqlEmitContext::joined("posts").push_column(&mut acc, "author.name");
        let (sql, _) = acc.into_parts();
        // Joined context does NOT prepend `posts.` to a dotted column —
        // the column already carries its own qualifier (legacy/SQL-only).
        assert_eq!(sql, "author.name");
    }

    #[test]
    fn parent_table_accessor_returns_stored_value() {
        assert_eq!(SqlEmitContext::root().parent_table(), None);
        assert_eq!(SqlEmitContext::joined("t").parent_table(), Some("t"));
    }
}
