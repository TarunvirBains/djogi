//! Portable predicate SQL emission — error type, emit context, walker, and
//! the hidden `emit::*` helper surface PR2d's macro override consumes.
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
//! # PR2b scope
//!
//! - Add `emit_portable_predicate` — the borrow-walker that drives
//!   `Q::Portable` SQL emission for the direct walker in `query::sql`.
//! - Add the hidden `emit::*` helper module that PR2d's macro override
//!   consumes for value / pair / list / null / option / pattern lowering.
//!   The helpers are crate-public (`pub mod emit`) so the macro-emitted
//!   impl can name them through `::djogi::__private::query::portable_emit::*`.
//!
//! # The Model hook contract
//!
//! Every model gets a default `__djogi_emit_field_predicate` (defined in
//! `crate::model`) that returns `PortablePredicateError::UnsupportedModel`.
//! PR2d will override this on macro-emitted `impl Model for {Model}` blocks
//! to dispatch on `(field_name, LookupOp)` and call into the hidden
//! `emit::*` helpers below. Hand-written `Model` impls (used by some
//! tests) keep the default and surface a typed error rather than panicking.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::predicate::PortablePredicate;
use sassi::BasicPredicate;

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

/// Walk a [`PortablePredicate<T>`] by reference and emit SQL into `acc`.
///
/// Phase 8eta PR2b — the direct-`Q<T>` SQL walker (`query::sql::emit_q`)
/// delegates `Q::Portable(_)` arms here so trusted-portable predicates
/// emit through `Model::__djogi_emit_field_predicate` (PR2d's macro
/// override) without first lowering to `Condition`.
///
/// # Vacuous identities
///
/// `BasicPredicate::True` and `BasicPredicate::False` emit literal
/// `TRUE` / `FALSE` directly — no model hook call. Phase 8eta PR2b's
/// `Q::always_true()` / `Q::always_false()` rely on this so unfiltered
/// querysets stay SQL-emittable on hand-written `Model` test fixtures
/// where `__djogi_emit_field_predicate`'s default returns
/// `UnsupportedModel`.
///
/// # Compound nodes
///
/// `And(parts)` / `Or(parts)` emit `(p1 AND p2 AND ...)` / `(p1 OR p2
/// OR ...)`. Empty `And(vec![])` is the vacuous-truth identity (renders
/// `TRUE`); empty `Or(vec![])` is vacuous-falsehood (renders `FALSE`).
/// `Not(inner)` emits `NOT (...)` and `Xor(a, b)` emits the general
/// truth-table identity `((NOT a) AND b) OR (a AND (NOT b))`.
///
/// # Future Sassi variants
///
/// `BasicPredicate<T>` is `#[non_exhaustive]`; the wildcard arm returns
/// `PortablePredicateError::UnsupportedPredicateKind` rather than
/// panicking so a future Sassi variant added between Djogi releases
/// surfaces as a typed error from the SQL emitter.
#[doc(hidden)]
pub(crate) fn emit_portable_predicate<T: Model>(
    acc: &mut SqlAccumulator,
    predicate: &PortablePredicate<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    emit_basic_predicate::<T>(acc, predicate.inner_ref(), ctx)
}

/// Emit a `BasicPredicate<T>` borrow into `acc`. Recursive helper for
/// `emit_portable_predicate` — `BasicPredicate::And` / `Or` / `Not` /
/// `Xor` walk through this without going back through `PortablePredicate`
/// (which carries provenance metadata that does not change the SQL
/// shape).
fn emit_basic_predicate<T: Model>(
    acc: &mut SqlAccumulator,
    bp: &BasicPredicate<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match bp {
        BasicPredicate::True => {
            acc.push_sql("TRUE");
            Ok(())
        }
        BasicPredicate::False => {
            acc.push_sql("FALSE");
            Ok(())
        }
        BasicPredicate::Field(fp) => T::__djogi_emit_field_predicate(acc, fp, ctx),
        BasicPredicate::And(parts) => {
            if parts.is_empty() {
                acc.push_sql("TRUE");
                return Ok(());
            }
            acc.push_sql("(");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" AND ");
                }
                emit_basic_predicate::<T>(acc, p, ctx)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        BasicPredicate::Or(parts) => {
            if parts.is_empty() {
                acc.push_sql("FALSE");
                return Ok(());
            }
            acc.push_sql("(");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" OR ");
                }
                emit_basic_predicate::<T>(acc, p, ctx)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        BasicPredicate::Not(inner) => {
            acc.push_sql("NOT (");
            emit_basic_predicate::<T>(acc, inner, ctx)?;
            acc.push_sql(")");
            Ok(())
        }
        BasicPredicate::Xor(a, b) => {
            // General XOR identity: `((NOT a) AND b) OR (a AND (NOT b))`.
            // Same shape `query::q::xor_to_condition_basic` produces in
            // the legacy bridge.
            acc.push_sql("(((NOT (");
            emit_basic_predicate::<T>(acc, a, ctx)?;
            acc.push_sql(")) AND (");
            emit_basic_predicate::<T>(acc, b, ctx)?;
            acc.push_sql(")) OR ((");
            emit_basic_predicate::<T>(acc, a, ctx)?;
            acc.push_sql(") AND (NOT (");
            emit_basic_predicate::<T>(acc, b, ctx)?;
            acc.push_sql("))))");
            Ok(())
        }
        // `BasicPredicate<T>` is `#[non_exhaustive]`. A future Sassi
        // variant lands here as a typed error rather than `unreachable!`,
        // `todo!`, or a silent fallback. The macro / SQL builder layers
        // surface this as `DjogiError::Predicate(_)`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "BasicPredicate::<unknown>",
        }),
    }
}

/// Hidden helper module consumed by PR2d's macro-emitted
/// `Model::__djogi_emit_field_predicate` override. Adopter code never
/// names anything in here directly; the macro routes calls through
/// `::djogi::__private::query::portable_emit::*` (see `lib.rs`).
///
/// Each helper writes the column reference via
/// [`SqlEmitContext::push_column`], dispatches the operator token, and
/// calls `acc.push_bind(_)` on cloned operand values pulled out of the
/// type-erased Sassi `FieldPredicate::value_as<V>()` payload. Type
/// mismatches return `PortablePredicateError::ValueTypeMismatch` instead
/// of panicking, so a future macro emission bug surfaces as a typed
/// error rather than a runtime crash.
#[doc(hidden)]
pub mod emit {
    use super::{PortablePredicateError, SqlEmitContext};
    use crate::model::Model;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::types::{FieldPredicate, LookupOp};

    /// Djogi's portable string-pattern lowering operator. Mirrors the
    /// pattern half of Sassi's `LookupOp` (the SQL-only `Regex` /
    /// `IRegex` variants are excluded — they ride `Q::Regex(_)`
    /// directly).
    #[derive(Clone, Copy, Debug)]
    #[doc(hidden)]
    pub enum PatternOp {
        Contains,
        IContains,
        StartsWith,
        IStartsWith,
        EndsWith,
        IEndsWith,
        IExact,
    }

    /// Emit a `column op $n` predicate for a value pulled out of the
    /// `FieldPredicate`'s type-erased payload.
    ///
    /// `op_sql` is the binary operator token (with surrounding spaces)
    /// — e.g. `" = "`, `" <> "`, `" > "`. The helper clones the captured
    /// value once into `push_bind`; the macro arm guarantees `V`
    /// matches the field's Rust type.
    #[doc(hidden)]
    pub fn emit_value<M, V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        op_sql: &'static str,
        field: &FieldPredicate<M>,
    ) -> Result<(), PortablePredicateError>
    where
        M: Model,
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        let Some(value) = field.value_as::<V>() else {
            return Err(PortablePredicateError::ValueTypeMismatch {
                field: field.field_name(),
                op: field.op(),
            });
        };
        ctx.push_column(acc, column);
        acc.push_sql(op_sql);
        acc.push_bind(value.clone());
        Ok(())
    }

    /// Same as [`emit_value`] but takes the operand as an explicit
    /// `&V` reference — used by the option-aware arms which already
    /// downcast at the macro layer.
    #[doc(hidden)]
    pub fn emit_value_ref<V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        op_sql: &'static str,
        value: &V,
    ) -> Result<(), PortablePredicateError>
    where
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        ctx.push_column(acc, column);
        acc.push_sql(op_sql);
        acc.push_bind(value.clone());
        Ok(())
    }

    /// Emit `column BETWEEN $a AND $b`. Sassi's `Between` payload shape
    /// is `(V, V)`.
    #[doc(hidden)]
    pub fn emit_pair<M, V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        field: &FieldPredicate<M>,
    ) -> Result<(), PortablePredicateError>
    where
        M: Model,
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        let Some(pair) = field.value_as::<(V, V)>() else {
            return Err(PortablePredicateError::ValueTypeMismatch {
                field: field.field_name(),
                op: field.op(),
            });
        };
        ctx.push_column(acc, column);
        acc.push_sql(" BETWEEN ");
        acc.push_bind(pair.0.clone());
        acc.push_sql(" AND ");
        acc.push_bind(pair.1.clone());
        Ok(())
    }

    /// Emit `column IN ($a, $b, ...)` (or `NOT IN ...`) for non-optional
    /// `Vec<V>` payloads. Empty list short-circuits to the same
    /// `FALSE` / `TRUE` identities Djogi's legacy emitter uses.
    #[doc(hidden)]
    pub fn emit_list<M, V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        field: &FieldPredicate<M>,
        negated: bool,
    ) -> Result<(), PortablePredicateError>
    where
        M: Model,
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        let Some(values) = field.value_as::<Vec<V>>() else {
            return Err(PortablePredicateError::ValueTypeMismatch {
                field: field.field_name(),
                op: field.op(),
            });
        };
        if values.is_empty() {
            acc.push_sql(if negated { "TRUE" } else { "FALSE" });
            return Ok(());
        }
        ctx.push_column(acc, column);
        acc.push_sql(if negated { " NOT IN (" } else { " IN (" });
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            acc.push_bind(v.clone());
        }
        acc.push_sql(")");
        Ok(())
    }

    /// Emit a portable string-pattern predicate. Uses Postgres
    /// `ILIKE` / `LIKE` with `ESCAPE '\\'`; `IExact` lowers to
    /// `COLLATE "C" ILIKE` to match the ASCII-stable case-insensitive
    /// equality semantics PR1's Sassi evaluator implements.
    ///
    /// The captured `String` value is escaped via [`escape_like`] so
    /// literal `%`, `_`, and `\\` in user input do not act as
    /// wildcards. Substring / prefix / suffix wrappers (`%foo%`,
    /// `foo%`, `%foo`) are added after escaping.
    #[doc(hidden)]
    pub fn emit_string_pattern<M>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        op: PatternOp,
        field: &FieldPredicate<M>,
    ) -> Result<(), PortablePredicateError>
    where
        M: Model,
    {
        let Some(value) = field.value_as::<String>() else {
            return Err(PortablePredicateError::ValueTypeMismatch {
                field: field.field_name(),
                op: field.op(),
            });
        };
        let escaped = escape_like(value);
        match op {
            PatternOp::Contains => {
                ctx.push_column(acc, column);
                acc.push_sql(" LIKE ");
                acc.push_bind(format!("%{escaped}%"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::IContains => {
                ctx.push_column(acc, column);
                acc.push_sql(" COLLATE \"C\" ILIKE ");
                acc.push_bind(format!("%{escaped}%"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::StartsWith => {
                ctx.push_column(acc, column);
                acc.push_sql(" LIKE ");
                acc.push_bind(format!("{escaped}%"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::IStartsWith => {
                ctx.push_column(acc, column);
                acc.push_sql(" COLLATE \"C\" ILIKE ");
                acc.push_bind(format!("{escaped}%"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::EndsWith => {
                ctx.push_column(acc, column);
                acc.push_sql(" LIKE ");
                acc.push_bind(format!("%{escaped}"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::IEndsWith => {
                ctx.push_column(acc, column);
                acc.push_sql(" COLLATE \"C\" ILIKE ");
                acc.push_bind(format!("%{escaped}"));
                acc.push_sql(" ESCAPE '\\'");
            }
            PatternOp::IExact => {
                // No wildcard wrapping — `IExact` is exact equality
                // with ASCII case folding. `COLLATE "C"` pins
                // collation so the SQL-side semantics match Sassi's
                // byte-level ASCII case insensitivity.
                ctx.push_column(acc, column);
                acc.push_sql(" COLLATE \"C\" ILIKE ");
                acc.push_bind(escaped);
                acc.push_sql(" ESCAPE '\\'");
            }
        }
        Ok(())
    }

    /// Emit `column IS NULL` or `column IS NOT NULL`. No `FieldPredicate`
    /// payload is consumed — Sassi's null-check ops carry an inert
    /// `()` operand.
    #[doc(hidden)]
    pub fn emit_null(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        is_null: bool,
    ) -> Result<(), PortablePredicateError> {
        ctx.push_column(acc, column);
        if is_null {
            acc.push_sql(" IS NULL");
        } else {
            acc.push_sql(" IS NOT NULL");
        }
        Ok(())
    }

    /// Direct `Option<V>` equality. Mirrors Rust's `Option` semantics:
    /// `Some(v)` lowers to `column = $n`; `None` lowers to `column IS
    /// NULL`.
    #[doc(hidden)]
    pub fn emit_option_eq<V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        value: &Option<V>,
    ) -> Result<(), PortablePredicateError>
    where
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        match value {
            Some(v) => {
                ctx.push_column(acc, column);
                acc.push_sql(" = ");
                acc.push_bind(v.clone());
            }
            None => {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NULL");
            }
        }
        Ok(())
    }

    /// Direct `Option<V>` inequality.
    /// `neq(Some(v))` lowers to `(column IS NULL OR column <> $n)` —
    /// matching Rust's `Some(_) != None`.
    /// `neq(None)` lowers to `column IS NOT NULL`.
    #[doc(hidden)]
    pub fn emit_option_neq<V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        value: &Option<V>,
    ) -> Result<(), PortablePredicateError>
    where
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        match value {
            Some(v) => {
                acc.push_sql("(");
                ctx.push_column(acc, column);
                acc.push_sql(" IS NULL OR ");
                ctx.push_column(acc, column);
                acc.push_sql(" <> ");
                acc.push_bind(v.clone());
                acc.push_sql(")");
            }
            None => {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NOT NULL");
            }
        }
        Ok(())
    }

    /// Direct `Option<V>` list membership. Splits the input into
    /// `Some(_)` and `None` partitions and emits the full-shape SQL
    /// from the v3 plan PR2 Step 6 table:
    ///
    /// - `in_([])` -> `FALSE`
    /// - `in_([None])` -> `column IS NULL`
    /// - `in_([Some(v1), Some(v2)])` -> `column IN ($n, $m)`
    /// - `in_([None, Some(v)])` -> `(column IS NULL OR column IN ($n))`
    ///
    /// And the negated dual:
    ///
    /// - `not_in([])` -> `TRUE`
    /// - `not_in([None])` -> `column IS NOT NULL`
    /// - `not_in([Some(v1), Some(v2)])`
    ///   -> `(column IS NULL OR column NOT IN ($n, $m))`
    /// - `not_in([None, Some(v)])`
    ///   -> `(column IS NOT NULL AND column NOT IN ($n))`
    #[doc(hidden)]
    pub fn emit_option_in<V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        values: &[Option<V>],
        negated: bool,
    ) -> Result<(), PortablePredicateError>
    where
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        if values.is_empty() {
            acc.push_sql(if negated { "TRUE" } else { "FALSE" });
            return Ok(());
        }
        let has_none = values.iter().any(Option::is_none);
        let some_values: Vec<&V> = values.iter().filter_map(|v| v.as_ref()).collect();

        if !negated {
            match (has_none, some_values.is_empty()) {
                // Only None values.
                (true, true) => {
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL");
                }
                // Only Some values.
                (false, false) => {
                    ctx.push_column(acc, column);
                    acc.push_sql(" IN (");
                    for (i, v) in some_values.iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        acc.push_bind((*v).clone());
                    }
                    acc.push_sql(")");
                }
                // Mix of None + Some.
                (true, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL OR ");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IN (");
                    for (i, v) in some_values.iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        acc.push_bind((*v).clone());
                    }
                    acc.push_sql("))");
                }
                // Empty list — handled above.
                (false, true) => unreachable!("non-empty values with no None and no Some"),
            }
        } else {
            match (has_none, some_values.is_empty()) {
                // Only None values: NOT IN ([None]) -> `column IS NOT NULL`.
                (true, true) => {
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NOT NULL");
                }
                // Only Some values: `(column IS NULL OR column NOT IN (...))`.
                (false, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL OR ");
                    ctx.push_column(acc, column);
                    acc.push_sql(" NOT IN (");
                    for (i, v) in some_values.iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        acc.push_bind((*v).clone());
                    }
                    acc.push_sql("))");
                }
                // Mix: `(column IS NOT NULL AND column NOT IN (...))`.
                (true, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NOT NULL AND ");
                    ctx.push_column(acc, column);
                    acc.push_sql(" NOT IN (");
                    for (i, v) in some_values.iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        acc.push_bind((*v).clone());
                    }
                    acc.push_sql("))");
                }
                (false, true) => unreachable!("non-empty values with no None and no Some"),
            }
        }
        Ok(())
    }

    /// Escape `%`, `_`, and `\\` in user-supplied LIKE / ILIKE input
    /// so they are matched literally instead of as wildcards. Mirrors
    /// `query::sql::escape_like` (kept private there to lock the
    /// pre-PR2b emit path); the helper is duplicated here so PR2d's
    /// macro override does not have to reach into the SQL module's
    /// crate-private surface.
    fn escape_like(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' | '%' | '_' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }

    // Re-export `LookupOp` for macro-emitted callers. PR2d's generated
    // code constructs `PortablePredicateError::UnsupportedLookup { op,
    // .. }` from the wildcard arm; routing through `crate::types`
    // means the macro never names `::sassi::*` directly.
    pub use crate::types::LookupOp as _LookupOp;
    // Silence unused-import lint when no test imports it.
    #[allow(dead_code)]
    fn _ensure_lookup_op_visible(_op: LookupOp) {}
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
