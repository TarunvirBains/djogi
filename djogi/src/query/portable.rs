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

    /// A Sassi `LookupOp::Json` predicate reached the SQL emitter without
    /// trusted Djogi provenance.
    ///
    /// Djogi requires every JSON predicate to be constructed through
    /// [`DjogiField<M, MirJzSON>::jsahibon`] (or its `Option<MirJzSON>`
    /// sibling) so the column name routes through Djogi's identifier
    /// validator and the predicate carries a [`DjogiFieldProvenance`]
    /// stamp. Raw `sassi::Field::new("payload", _).jsahibon()...`
    /// predicates lack the stamp and would smuggle a caller-supplied
    /// `&'static str` past Djogi's identifier gate; lowering them is a
    /// hard refusal.
    ///
    /// Adopters who hit this error are reaching into Sassi directly for
    /// JSON predicate construction — re-route through
    /// `DjogiField<M, MirJzSON>::jsahibon()` to fix.
    ///
    /// [`DjogiField<M, MirJzSON>::jsahibon`]:
    ///     crate::query::mirjzson::DjogiJSahibONFieldRef
    /// [`DjogiFieldProvenance`]: crate::query::field::DjogiFieldProvenance
    #[error(
        "field {field} JSON predicate lacks Djogi trusted provenance — \
         construct through `DjogiField<M, MirJzSON>::jsahibon()` instead of \
         raw `sassi::Field::new(...).jsahibon()`"
    )]
    UntrustedJsonPredicate {
        /// The Sassi `field_name` reported on the predicate leaf.
        field: &'static str,
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

/// Trust marker threaded through SQL emission to gate `LookupOp::Json`
/// lowering against forged Sassi JSON predicates.
///
/// # Why this exists
///
/// JSON predicates are unusual among `BasicPredicate<T>` leaves: their
/// SQL emission bypasses the macro-emitted `Model::__djogi_emit_field_predicate`
/// dispatcher (whose generated arms statically validate column names
/// against the model's field table) and routes directly to
/// [`emit_jsahibon_predicate`]. That helper reads `fp.field_name()` —
/// the Sassi-side caller-supplied `&'static str` — and emits it as a
/// SQL column reference. If a raw `sassi::Field::<T, JSahibON>::new("col", _)`
/// predicate reached the helper, the column name would never have
/// passed Djogi's identifier validator and the SQL would target a
/// column the Djogi field surface never blessed.
///
/// `PortablePredicate<T>` is the sealed wrapper that establishes
/// trusted Djogi provenance: its crate-private constructors
/// ([`PortablePredicate::from_djogi_field`],
/// [`PortablePredicate::always_true`], [`PortablePredicate::always_false`])
/// and the operator overloads in [`crate::query::predicate`] only mint
/// JSON leaves through [`crate::query::mirjzson::wrap_predicate`], which
/// captures the column from Djogi's identifier-validated
/// `DjogiField::__sql_field()` route.
///
/// # How trust propagates
///
/// - [`emit_portable_predicate`] passes [`JsonTrust::Trusted`]
///   unconditionally — `PortablePredicate<T>` is the trusted-construction
///   boundary; every JSON leaf inside has Djogi provenance.
/// - Callers that extracted a bare [`BasicPredicate<T>`] from a
///   [`PortablePredicate<T>`]-rooted path (e.g.
///   [`crate::query::QuerySet::is_portable`] /
///   [`crate::query::QuerySet::try_portable`] reduce a `Q::Portable`-only
///   tree via `try_reduce_q_ref_to_basic`; the refresh fetcher stores
///   the reduced `Option<BasicPredicate<T>>` and rebroadcasts it on
///   full-baseline ticks) also pass [`JsonTrust::Trusted`].
/// - Recursive [`emit_basic_predicate`] calls (under `And` / `Or` /
///   `Not` / `Xor`) propagate the caller's trust unchanged. The
///   recursive walker never gains trust mid-walk — a forged JSON leaf
///   nested inside an `And` still surfaces `UntrustedJsonPredicate`.
/// - Any other entry point — and in particular the unit-test path that
///   constructs a raw `sassi::Field::new("forged", _).jsahibon()...`
///   predicate directly — passes [`JsonTrust::Untrusted`]. The first
///   JSON leaf returns
///   [`PortablePredicateError::UntrustedJsonPredicate`] instead of
///   dispatching to [`emit_jsahibon_predicate`].
///
/// Non-JSON field leaves are unaffected by this flag — their dispatch
/// routes through `Model::__djogi_emit_field_predicate`, whose
/// macro-emitted arm matrix is gated by statically-validated column
/// names independently of the trust marker.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonTrust {
    /// Predicate originated at a Djogi-trusted boundary
    /// ([`PortablePredicate<T>`] wrapper or a `BasicPredicate<T>` reduced
    /// from one). `LookupOp::Json` leaves dispatch to
    /// [`emit_jsahibon_predicate`].
    Trusted,
    /// Predicate has unknown provenance — raw Sassi or otherwise.
    /// `LookupOp::Json` leaves surface
    /// [`PortablePredicateError::UntrustedJsonPredicate`].
    ///
    /// No non-test production code path constructs this variant —
    /// every caller of [`emit_basic_predicate`] originates at a
    /// `PortablePredicate<T>` boundary (`emit_portable_predicate`) or
    /// extracts from a `try_portable`-gated `Q::Portable` tree
    /// (`refresh.rs` filter pushdown, `queryset.rs::validate_portable_sql_emit`).
    /// The variant exists so the trust marker is type-system complete
    /// and the rejection path is unit-testable from
    /// `query::portable::tests` against a forged raw Sassi predicate.
    /// `#[allow(dead_code)]` is therefore correct without papering over
    /// a real bug — see `phase85_195_forged_*` tests in this module
    /// for the exercising callers.
    #[allow(dead_code)]
    Untrusted,
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
    // `PortablePredicate<T>` is the sealed trusted-construction boundary
    // (see [`JsonTrust`] for the threat model). Its crate-private
    // constructors mint provenance through `DjogiField` /
    // `DjogiPresentField` (non-JSON) or the MirJzSON builder in
    // `query::mirjzson::wrap_predicate` (JSON). Every JSON leaf inside
    // `inner` is therefore Djogi-trusted and the trust flag is
    // unconditional here.
    emit_basic_predicate::<T>(acc, predicate.inner_ref(), ctx, JsonTrust::Trusted)
}

/// Emit a `BasicPredicate<T>` borrow into `acc`. Recursive helper for
/// `emit_portable_predicate` — `BasicPredicate::And` / `Or` / `Not` /
/// `Xor` walk through this without going back through `PortablePredicate`
/// (which carries provenance metadata that does not change the SQL
/// shape).
///
/// `trust` is the caller's [`JsonTrust`] marker (see that type for the
/// threat model and propagation rules). Recursive calls under
/// composite arms forward the same value unchanged — a forged JSON
/// leaf nested inside an `And` still surfaces
/// [`PortablePredicateError::UntrustedJsonPredicate`].
pub(crate) fn emit_basic_predicate<T: Model>(
    acc: &mut SqlAccumulator,
    bp: &BasicPredicate<T>,
    ctx: SqlEmitContext,
    trust: JsonTrust,
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
        BasicPredicate::Field(fp) => {
            // `LookupOp::Json` leaves bypass the macro-emitted
            // `__djogi_emit_field_predicate` override because their SQL
            // shape (guarded, two-valued, path-extracted) does not match
            // any of the per-(field, op) arms the macro emits for regular
            // scalar / string / bool columns. The Djogi-owned JSON SQL
            // emitter under `emit_jsahibon_predicate` is the only valid
            // lowering route — see `query::mirjzson` for the
            // construction-side contract.
            //
            // Trusted provenance is enforced here: an untrusted caller
            // (raw `sassi::Field::new(...).jsahibon()` predicate that did
            // not transit `PortablePredicate<T>`) surfaces
            // `UntrustedJsonPredicate` rather than emitting SQL that
            // would target a never-validated column. See `JsonTrust`
            // above for the propagation rules.
            if matches!(fp.op(), crate::types::LookupOp::Json) {
                if !matches!(trust, JsonTrust::Trusted) {
                    return Err(PortablePredicateError::UntrustedJsonPredicate {
                        field: fp.field_name(),
                    });
                }
                emit_jsahibon_predicate::<T>(acc, fp, ctx)
            } else {
                T::__djogi_emit_field_predicate(acc, fp, ctx)
            }
        }
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
                emit_basic_predicate::<T>(acc, p, ctx, trust)?;
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
                emit_basic_predicate::<T>(acc, p, ctx, trust)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        BasicPredicate::Not(inner) => {
            acc.push_sql("NOT (");
            emit_basic_predicate::<T>(acc, inner, ctx, trust)?;
            acc.push_sql(")");
            Ok(())
        }
        BasicPredicate::Xor(a, b) => {
            // General XOR identity: `((NOT a) AND b) OR (a AND (NOT b))`.
            // Same shape `query::q::xor_to_condition_basic` produces in
            // the legacy bridge.
            acc.push_sql("(((NOT (");
            emit_basic_predicate::<T>(acc, a, ctx, trust)?;
            acc.push_sql(")) AND (");
            emit_basic_predicate::<T>(acc, b, ctx, trust)?;
            acc.push_sql(")) OR ((");
            emit_basic_predicate::<T>(acc, a, ctx, trust)?;
            acc.push_sql(") AND (NOT (");
            emit_basic_predicate::<T>(acc, b, ctx, trust)?;
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

// ── MirJzSON / JSahibON predicate SQL emission ────────────────────────────
//
// Djogi-owned SQL lowering for `LookupOp::Json` leaves. Called from
// `emit_basic_predicate` when the dispatched `FieldPredicate<T>` carries
// `op == LookupOp::Json`. The contract:
//
// 1. Every leaf emits a two-valued SQL boolean (`TRUE` or `FALSE`) before
//    composition under `NOT`, `XOR`, `AND`, `OR`. SQL `NULL` never leaks
//    out of a leaf — `COALESCE(_, FALSE)` and `CASE WHEN ... ELSE FALSE
//    END` wrappers are mandatory.
// 2. Missing path / type mismatch / SQL NULL → `FALSE` (except `missing()`
//    which is `TRUE`).
// 3. JSON `null` and SQL `NULL` are kept distinct.
// 4. Key predicates guard `jsonb_typeof(j) = 'object'`.
// 5. Array predicates guard `jsonb_typeof(j) = 'array'`.
// 6. Numeric comparisons preflight `jsonb_typeof(j) = 'number'` and bind
//    `u64` through `rust_decimal::Decimal` (never through `as i64`).
// 7. All paths / keys / scalar operands / JSON operands / lengths are
//    bound parameters. No path/key interpolation.

use crate::types::FieldPredicate;
use sassi::JSahibON;
use sassi::predicate::{
    JCompareOp, JInPolarity, JPath, JSahibONPredicateBody, JScalarKind, JScalarValue, JTypeKind,
};

/// Emit a `LookupOp::Json` leaf as SQL.
///
/// The function is the **single** SQL-lowering entry point for JSON
/// predicates. The caller (always [`emit_basic_predicate`]) has
/// already enforced the [`JsonTrust::Trusted`] precondition; this
/// helper assumes its `fp` originated from a Djogi-trusted
/// `PortablePredicate<T>`-rooted path. It:
///
/// 1. Downcasts `fp.value_as::<JSahibONPredicateBody>()`. A `None`
///    return indicates either a future Sassi schema change that
///    invalidated the `Arc<JSahibONPredicateBody>` payload contract
///    or an internal Djogi bug — surfaces as
///    [`PortablePredicateError::UntrustedJsonPredicate`] rather than a
///    panic for the same defense-in-depth reason the body trust check
///    lives upstream.
/// 2. Dispatches on the body variant, walking the
///    [`JSahibONPredicateBody`] tree from `sassi::predicate::jsahibon`.
///    Each arm emits the guarded two-valued SQL shape documented in
///    [`docs/spec/mirjzson-jsonb-integration.md`][spec] under the
///    "SQL Mapping" section.
///
/// [spec]: ../../docs/spec/mirjzson-jsonb-integration.md
fn emit_jsahibon_predicate<T: Model>(
    acc: &mut SqlAccumulator,
    fp: &FieldPredicate<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    let body: &JSahibONPredicateBody = match fp.value_as::<JSahibONPredicateBody>() {
        Some(body) => body,
        None => {
            // Sassi's `JSahibONPathRef::predicate` always stores an
            // `Arc<JSahibONPredicateBody>` under the type-erased
            // `FieldPredicate::value` payload. A `LookupOp::Json` leaf
            // whose payload does NOT downcast is therefore a forgery
            // (or a future Sassi schema change that broke the
            // contract). Both surface as a typed error rather than a
            // panic.
            return Err(PortablePredicateError::UntrustedJsonPredicate {
                field: fp.field_name(),
            });
        }
    };
    emit_jsahibon_body(acc, fp.field_name(), body, ctx)
}

/// Emit the JSON expression `(column #> $path_text_array)` into the
/// accumulator. This is the uniform `j` expression for both root and
/// path predicates per the spec — `path = []` binds an empty
/// `text[]::text[]` and Postgres's `#>` operator returns the column
/// itself.
///
/// `path` is bound as a `Vec<String>` parameter — every segment is a
/// bound value, never interpolated into SQL. This is the path-
/// smuggling defence: even if a caller somehow constructed a
/// `JPath::from_segments([malicious_segment])`, the segments only
/// appear in the bound `$n` slot.
fn push_j_expression(
    acc: &mut SqlAccumulator,
    column: &'static str,
    path: &JPath,
    ctx: SqlEmitContext,
) {
    acc.push_sql("(");
    ctx.push_column(acc, column);
    acc.push_sql(" #> ");
    // `path.segments()` returns `&[String]`; cloning into an owned `Vec` is
    // the canonical bind shape for postgres-types' `Vec<String>` -> `text[]`
    // codec. `to_vec()` is the idiomatic spelling per clippy's
    // `iter_cloned_collect` lint.
    let segments: Vec<String> = path.segments().to_vec();
    acc.push_bind(segments);
    acc.push_sql(")");
}

/// Push a `text[]` array bind for a key list. Used by `HasAnyKey` /
/// `HasAllKeys` per the spec.
fn push_key_array_bind(acc: &mut SqlAccumulator, keys: &[String]) {
    let owned: Vec<String> = keys.to_vec();
    acc.push_bind(owned);
}

/// Emit a `JSahibON`-bound parameter (the full JSON value) as a
/// `jsonb` bind. The value is serialised to `serde_json::Value` so
/// postgres-types' `serde_json::Value` `ToSql` codec handles the
/// JSONB framing — this is the same path `Jsonb<T>` already uses.
fn push_jsonb_value_bind(acc: &mut SqlAccumulator, value: &JSahibON) {
    let json: serde_json::Value = value.clone().into();
    acc.push_bind(json);
}

/// Emit a `numeric` bind for a JSON scalar operand. `i64` / `u64` /
/// `f64` all bind through `rust_decimal::Decimal` for unlimited-
/// precision comparison against `(j #>> '{}')::numeric` — the spec's
/// safe numeric preflight shape. Strings and booleans surface as a
/// `ValueTypeMismatch` because numeric arms must guard on
/// `jsonb_typeof(j) = 'number'` first; the caller is responsible for
/// matching the scalar kind against the arm.
fn push_numeric_bind(
    acc: &mut SqlAccumulator,
    operand: &JScalarValue,
) -> Result<(), PortablePredicateError> {
    match operand {
        JScalarValue::I64(value) => {
            acc.push_bind(rust_decimal::Decimal::from(*value));
            Ok(())
        }
        JScalarValue::U64(value) => {
            // `Decimal::from(u64)` is infallible (u64::MAX < Decimal::MAX).
            // This is the spec's required "bind u64 through Decimal,
            // never through as i64" path; the test fixtures include
            // `u64::MAX` to lock it in.
            acc.push_bind(rust_decimal::Decimal::from(*value));
            Ok(())
        }
        JScalarValue::F64(value) => {
            // `JFiniteF64` enforces the finite-only invariant at
            // construction time, so `try_from(f64)` cannot fail here.
            // The conversion through `Decimal::from_f64` may still
            // return `None` for unrepresentable values (very large
            // magnitudes outside Decimal's mantissa); we forward that
            // as a typed error rather than panicking.
            match rust_decimal::Decimal::try_from(value.get()) {
                Ok(d) => {
                    acc.push_bind(d);
                    Ok(())
                }
                Err(_) => Err(PortablePredicateError::UnsupportedPredicateKind {
                    kind: "JSahibON::F64 operand exceeds Decimal range",
                }),
            }
        }
        JScalarValue::String(_) | JScalarValue::Bool(_) => {
            // Reachable only if the caller paired a non-numeric scalar
            // kind with a numeric arm — Sassi's typed builders prevent
            // this on the construction side, but we surface a typed
            // error rather than emitting wrong SQL.
            Err(PortablePredicateError::UnsupportedPredicateKind {
                kind: "non-numeric operand in numeric JSON comparison",
            })
        }
        // `JScalarValue` is `#[non_exhaustive]`; a future Sassi variant
        // lands here as a typed error rather than `unreachable!`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "JSahibON scalar operand variant unknown to Djogi SQL emission",
        }),
    }
}

/// Emit a text bind for a string operand. Used by string scalar
/// comparison arms.
fn push_text_bind(
    acc: &mut SqlAccumulator,
    operand: &JScalarValue,
) -> Result<(), PortablePredicateError> {
    match operand {
        JScalarValue::String(value) => {
            acc.push_bind(value.clone());
            Ok(())
        }
        // Wildcard arm covers every non-String variant — including the
        // future-Sassi-variant case (`JScalarValue` is `#[non_exhaustive]`).
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "non-string operand in string JSON comparison",
        }),
    }
}

/// Emit a boolean bind for a boolean operand.
fn push_bool_bind(
    acc: &mut SqlAccumulator,
    operand: &JScalarValue,
) -> Result<(), PortablePredicateError> {
    match operand {
        JScalarValue::Bool(value) => {
            acc.push_bind(*value);
            Ok(())
        }
        // Wildcard arm covers every non-Bool variant — including the
        // future-Sassi-variant case (`JScalarValue` is `#[non_exhaustive]`).
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "non-boolean operand in boolean JSON comparison",
        }),
    }
}

/// Map a Sassi `JCompareOp` to its SQL token (with surrounding spaces).
///
/// `JCompareOp` is `#[non_exhaustive]`. The match below covers every
/// v0.1 variant; future additions land in the wildcard arm with `=`
/// (rather than `unreachable!`) and surface as a SQL miscompilation —
/// which gets caught by integration tests immediately. The defensive
/// wildcard is preferable to a panic because the caller (a Djogi-
/// trusted builder) cannot have routed an unknown op through this
/// path without going through Sassi's own constructors.
fn compare_op_token(op: JCompareOp) -> &'static str {
    match op {
        JCompareOp::Eq => " = ",
        JCompareOp::Neq => " <> ",
        JCompareOp::Gt => " > ",
        JCompareOp::Gte => " >= ",
        JCompareOp::Lt => " < ",
        JCompareOp::Lte => " <= ",
        // Wildcard — `JCompareOp` is `#[non_exhaustive]`. Returning `=`
        // is a deliberate fallback (rather than an `unreachable!`)
        // because the caller is a Djogi-trusted builder; a future
        // Sassi variant adoption is the only realistic route here and
        // the SQL parity tests will catch any miscompilation.
        _ => " = ",
    }
}

/// Map a [`JTypeKind`] to the literal `jsonb_typeof` result text.
///
/// `jsonb_typeof(jsonb)` returns one of `'object'`, `'array'`,
/// `'string'`, `'number'`, `'boolean'`, `'null'`. Sassi's `JTypeKind`
/// collapses the three numeric carriers under `Number`; the mapping
/// is exact otherwise.
fn jsonb_typeof_literal(kind: JTypeKind) -> &'static str {
    match kind {
        JTypeKind::Null => "'null'",
        JTypeKind::Bool => "'boolean'",
        JTypeKind::Number => "'number'",
        JTypeKind::String => "'string'",
        JTypeKind::Array => "'array'",
        JTypeKind::Object => "'object'",
        // `JTypeKind` is `#[non_exhaustive]`. Future variants
        // miscompile to `'null'`; SQL parity tests catch any drift.
        _ => "'null'",
    }
}

/// Walk a [`JSahibONPredicateBody`] and emit its guarded two-valued
/// SQL shape. The function returns one SQL fragment that evaluates to
/// `TRUE` or `FALSE` — never `NULL`. Composition under `NOT`/`XOR`/
/// `AND`/`OR` therefore stays well-defined.
fn emit_jsahibon_body(
    acc: &mut SqlAccumulator,
    column: &'static str,
    body: &JSahibONPredicateBody,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match body {
        // `Exists` — `(column #> $path) IS NOT NULL`. SQL NULL on a
        // missing path naturally projects to `FALSE` through
        // `IS NOT NULL`, so no `COALESCE` wrapper is needed here.
        JSahibONPredicateBody::Exists { path } => {
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" IS NOT NULL");
            Ok(())
        }
        // `Missing` — the dual of `Exists`. The only predicate variant
        // that returns `TRUE` on a missing path.
        JSahibONPredicateBody::Missing { path } => {
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" IS NULL");
            Ok(())
        }
        // `IsJsonNull` — `COALESCE(j = 'null'::jsonb, FALSE)`. The
        // `COALESCE` is mandatory because `j IS NULL` (the SQL `NULL`
        // arising from a missing path) would propagate to NULL
        // through the `=` operator.
        JSahibONPredicateBody::IsJsonNull { path } => {
            acc.push_sql("COALESCE(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" = 'null'::jsonb, FALSE)");
            Ok(())
        }
        JSahibONPredicateBody::IsNotJsonNull { path } => {
            acc.push_sql("COALESCE(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" <> 'null'::jsonb, FALSE)");
            Ok(())
        }
        // `Type(kind)` — `COALESCE(jsonb_typeof(j) = '<kind>', FALSE)`.
        JSahibONPredicateBody::Type { path, kind } => {
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = ");
            acc.push_sql(jsonb_typeof_literal(*kind));
            acc.push_sql(", FALSE)");
            Ok(())
        }
        // `HasKey` — guards `jsonb_typeof = 'object'` so the predicate
        // matches Sassi's portable "key is an object key" semantics.
        // Postgres `?` (jsonb-existence) would also match string-typed
        // arrays element-wise — we do NOT want that.
        JSahibONPredicateBody::HasKey { path, key } => {
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'object' AND ");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" ? ");
            acc.push_bind(key.as_str().to_owned());
            acc.push_sql(", FALSE)");
            Ok(())
        }
        JSahibONPredicateBody::HasAnyKey { path, keys } => {
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'object' AND ");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" ?| ");
            push_key_array_bind(acc, keys);
            acc.push_sql(", FALSE)");
            Ok(())
        }
        JSahibONPredicateBody::HasAllKeys { path, keys } => {
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'object' AND ");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" ?& ");
            push_key_array_bind(acc, keys);
            acc.push_sql(", FALSE)");
            Ok(())
        }
        // `ScalarCompare` — guarded two-valued SQL. Numeric arms go
        // through `CASE WHEN ... THEN ... ELSE FALSE END` so the cast
        // is preflighted. String and boolean arms restrict the
        // operator set to equality (Sassi's contract — string/bool
        // ordering is excluded from `JOrderedScalar`); we surface
        // `UnsupportedPredicateKind` if a non-equality op arrives.
        JSahibONPredicateBody::ScalarCompare {
            path,
            op,
            scalar_kind,
            operand,
        } => emit_scalar_compare(acc, column, path, *op, *scalar_kind, operand, ctx),
        // `ScalarIn` — guarded two-valued list membership. Empty list
        // short-circuits to `FALSE` (or `TRUE` for `NotIn`) per Sassi
        // semantics, *but only after* the type-mismatch / missing-path
        // guard returns `FALSE` so an empty `not_in` on a string field
        // still returns `FALSE` (not `TRUE`).
        JSahibONPredicateBody::ScalarIn {
            path,
            scalar_kind,
            operands,
            polarity,
        } => emit_scalar_in(acc, column, path, *scalar_kind, operands, *polarity, ctx),
        // `ScalarBetween` — numeric only per Sassi. Emits the safe
        // `CASE` shape with `numeric BETWEEN $low AND $high`.
        JSahibONPredicateBody::ScalarBetween {
            path,
            scalar_kind,
            low,
            high,
        } => emit_scalar_between(acc, column, path, *scalar_kind, low, high, ctx),
        // `JsonEq` — `COALESCE(j = $jsonb, FALSE)`. Postgres `jsonb =
        // jsonb` is order-insensitive on objects (treats `{"a":1,
        // "b":2}` == `{"b":2, "a":1}`) and numeric-strict (does NOT
        // soften `1` vs `1.0`). The spec calls out object equality as
        // order-insensitive matching Sassi; numeric softening parity
        // between Postgres and Sassi is a known divergence that
        // surfaces only when comparing two integer-shaped numbers
        // with different scale text representations (`1.0` vs `1`).
        // The shipped behaviour matches the spec contract literally.
        JSahibONPredicateBody::JsonEq { path, value } => {
            acc.push_sql("COALESCE(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" = ");
            push_jsonb_value_bind(acc, value);
            acc.push_sql(", FALSE)");
            Ok(())
        }
        JSahibONPredicateBody::JsonNeq { path, value } => {
            acc.push_sql("COALESCE(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" <> ");
            push_jsonb_value_bind(acc, value);
            acc.push_sql(", FALSE)");
            Ok(())
        }
        // `ArrayContains` — guarded `@>` against a single-element
        // jsonb array. The element is bound as a JSONB array of one
        // element so Postgres's `@>` finds the element by structural
        // equality.
        JSahibONPredicateBody::ArrayContains { path, element } => {
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'array' AND ");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" @> ");
            // Wrap the element in a single-element JSON array so
            // Postgres's `jsonb @> jsonb` operator does the right
            // thing for "array contains element".
            let array = serde_json::Value::Array(vec![element.clone().into()]);
            acc.push_bind(array);
            acc.push_sql(", FALSE)");
            Ok(())
        }
        // `ArrayLen` — `CASE WHEN jsonb_typeof(j) = 'array' THEN
        // jsonb_array_length(j) <op> $len ELSE FALSE END`. The
        // `jsonb_array_length` call ONLY runs inside the
        // `jsonb_typeof = 'array'` arm so non-arrays return `FALSE`
        // without erroring on the array-length call (per the spec's
        // "non-arrays return false and never call
        // jsonb_array_length" requirement).
        JSahibONPredicateBody::ArrayLen { path, op, len } => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'array' THEN jsonb_array_length(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(")");
            acc.push_sql(compare_op_token(*op));
            // Lengths are `u64` from Sassi; bind through Decimal for
            // the same reason scalar `u64` operands do (the column
            // is i64-sized, but the comparison target may exceed
            // i64). Realistically `jsonb_array_length` returns `int`,
            // so any `len > i32::MAX` will simply never match — but
            // the bind path stays consistent with the rest of the
            // numeric surface.
            acc.push_bind(rust_decimal::Decimal::from(*len));
            acc.push_sql(" ELSE FALSE END");
            Ok(())
        }
        // `#[non_exhaustive]` — a future Sassi variant lands here as
        // a typed error rather than `unreachable!`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "JSahibONPredicateBody::<unknown>",
        }),
    }
}

/// Emit a `ScalarCompare` JSON predicate. Per the spec:
///
/// - Numeric kinds emit `CASE WHEN jsonb_typeof = 'number' THEN
///   (j #>> '{}')::numeric <op> $operand ELSE FALSE END` so the cast
///   is preflighted and the operand is bound through `Decimal` (never
///   `as i64`).
/// - String kind emits `COALESCE(jsonb_typeof = 'string' AND
///   (j #>> '{}')::text <op> $operand, FALSE)`. The op set Sassi
///   permits for strings is `Eq` / `Neq` — ordering is excluded by
///   `JOrderedScalar`.
/// - Boolean kind emits the analogous shape with
///   `jsonb_typeof = 'boolean'` and a `boolean` bind.
fn emit_scalar_compare(
    acc: &mut SqlAccumulator,
    column: &'static str,
    path: &JPath,
    op: JCompareOp,
    scalar_kind: JScalarKind,
    operand: &JScalarValue,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match scalar_kind {
        JScalarKind::I64 | JScalarKind::U64 | JScalarKind::F64 => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'number' THEN (");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" #>> '{}'::text[])::numeric");
            acc.push_sql(compare_op_token(op));
            push_numeric_bind(acc, operand)?;
            acc.push_sql(" ELSE FALSE END");
            Ok(())
        }
        JScalarKind::String => {
            // Sassi's `JScalar for String` does not implement
            // `JOrderedScalar`, so the builder side never produces
            // `Gt` / `Gte` / `Lt` / `Lte` over strings. Guard against
            // a hypothetical forged input.
            if matches!(
                op,
                JCompareOp::Gt | JCompareOp::Gte | JCompareOp::Lt | JCompareOp::Lte
            ) {
                return Err(PortablePredicateError::UnsupportedPredicateKind {
                    kind: "ordering operator on JSON string operand",
                });
            }
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'string' AND (");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" #>> '{}'::text[])");
            acc.push_sql(compare_op_token(op));
            push_text_bind(acc, operand)?;
            acc.push_sql(", FALSE)");
            Ok(())
        }
        JScalarKind::Bool => {
            // Sassi excludes ordering on booleans, same as strings.
            if matches!(
                op,
                JCompareOp::Gt | JCompareOp::Gte | JCompareOp::Lt | JCompareOp::Lte
            ) {
                return Err(PortablePredicateError::UnsupportedPredicateKind {
                    kind: "ordering operator on JSON boolean operand",
                });
            }
            acc.push_sql("COALESCE(jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'boolean' AND ((");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" #>> '{}'::text[])::boolean");
            acc.push_sql(compare_op_token(op));
            push_bool_bind(acc, operand)?;
            acc.push_sql("), FALSE)");
            Ok(())
        }
        // `JScalarKind` is `#[non_exhaustive]`. Future Sassi additions
        // (e.g. a hypothetical decimal-typed scalar kind) land here as
        // a typed error rather than `unreachable!`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "JSahibON scalar kind unknown to Djogi SQL emission",
        }),
    }
}

/// Emit a `ScalarIn` JSON predicate. Per the spec, missing/type-
/// mismatch returns `FALSE` BEFORE the empty-list identity fires —
/// so `not_in([])` on a non-string column returns `FALSE`, not `TRUE`.
fn emit_scalar_in(
    acc: &mut SqlAccumulator,
    column: &'static str,
    path: &JPath,
    scalar_kind: JScalarKind,
    operands: &[JScalarValue],
    polarity: JInPolarity,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    let in_token = match polarity {
        JInPolarity::In => " IN (",
        JInPolarity::NotIn => " NOT IN (",
        // `#[non_exhaustive]` — future Sassi additions default to `IN`
        // (the conservative cache-safe shape).
        _ => " IN (",
    };

    // The kind guard is the same as `ScalarCompare`. We hold the
    // guard token outside the `IN (...)` shape so an empty operand
    // list still emits the guard — Sassi's evaluator returns `FALSE`
    // on empty `in_` after the kind guard, and `FALSE` after the
    // kind guard for empty `not_in`. The shape:
    //
    //   CASE WHEN <kind guard> THEN
    //     <extracted scalar> [NOT] IN ($1, $2, ...)
    //   ELSE FALSE END
    //
    // ...where the empty `(?, ?, ?)` arm short-circuits to `FALSE`
    // (or in the `NotIn` case, to `TRUE` inside the kind-guard).

    match scalar_kind {
        JScalarKind::I64 | JScalarKind::U64 | JScalarKind::F64 => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'number' THEN ");
            if operands.is_empty() {
                // `JInPolarity` is `#[non_exhaustive]`. Future variants
                // default to `FALSE` (the conservative cache-safe
                // shape); SQL parity tests catch any miscompilation.
                acc.push_sql(match polarity {
                    JInPolarity::In => "FALSE",
                    JInPolarity::NotIn => "TRUE",
                    _ => "FALSE",
                });
            } else {
                acc.push_sql("((");
                push_j_expression(acc, column, path, ctx);
                acc.push_sql(" #>> '{}'::text[])::numeric");
                acc.push_sql(in_token);
                for (i, operand) in operands.iter().enumerate() {
                    if i > 0 {
                        acc.push_sql(", ");
                    }
                    push_numeric_bind(acc, operand)?;
                }
                acc.push_sql("))");
            }
            acc.push_sql(" ELSE FALSE END");
            Ok(())
        }
        JScalarKind::String => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'string' THEN ");
            if operands.is_empty() {
                // `JInPolarity` is `#[non_exhaustive]`. Future variants
                // default to `FALSE` (the conservative cache-safe
                // shape); SQL parity tests catch any miscompilation.
                acc.push_sql(match polarity {
                    JInPolarity::In => "FALSE",
                    JInPolarity::NotIn => "TRUE",
                    _ => "FALSE",
                });
            } else {
                acc.push_sql("((");
                push_j_expression(acc, column, path, ctx);
                acc.push_sql(" #>> '{}'::text[])");
                acc.push_sql(in_token);
                for (i, operand) in operands.iter().enumerate() {
                    if i > 0 {
                        acc.push_sql(", ");
                    }
                    push_text_bind(acc, operand)?;
                }
                acc.push_sql("))");
            }
            acc.push_sql(" ELSE FALSE END");
            Ok(())
        }
        JScalarKind::Bool => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'boolean' THEN ");
            if operands.is_empty() {
                // `JInPolarity` is `#[non_exhaustive]`. Future variants
                // default to `FALSE` (the conservative cache-safe
                // shape); SQL parity tests catch any miscompilation.
                acc.push_sql(match polarity {
                    JInPolarity::In => "FALSE",
                    JInPolarity::NotIn => "TRUE",
                    _ => "FALSE",
                });
            } else {
                acc.push_sql("(((");
                push_j_expression(acc, column, path, ctx);
                acc.push_sql(" #>> '{}'::text[])::boolean)");
                acc.push_sql(in_token);
                for (i, operand) in operands.iter().enumerate() {
                    if i > 0 {
                        acc.push_sql(", ");
                    }
                    push_bool_bind(acc, operand)?;
                }
                acc.push_sql("))");
            }
            acc.push_sql(" ELSE FALSE END");
            Ok(())
        }
        // `JScalarKind` is `#[non_exhaustive]` — see analogous comment
        // in `emit_scalar_compare`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "JSahibON scalar kind unknown to Djogi SQL emission",
        }),
    }
}

/// Emit a `ScalarBetween` JSON predicate. Sassi restricts `between`
/// to `JOrderedScalar` (numeric kinds only), so the SQL emission
/// branches on `JScalarKind::I64 | U64 | F64` and surfaces a typed
/// error for any other kind reaching here through a forged input.
fn emit_scalar_between(
    acc: &mut SqlAccumulator,
    column: &'static str,
    path: &JPath,
    scalar_kind: JScalarKind,
    low: &JScalarValue,
    high: &JScalarValue,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match scalar_kind {
        JScalarKind::I64 | JScalarKind::U64 | JScalarKind::F64 => {
            acc.push_sql("CASE WHEN jsonb_typeof(");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(") = 'number' THEN ((");
            push_j_expression(acc, column, path, ctx);
            acc.push_sql(" #>> '{}'::text[])::numeric BETWEEN ");
            push_numeric_bind(acc, low)?;
            acc.push_sql(" AND ");
            push_numeric_bind(acc, high)?;
            acc.push_sql(") ELSE FALSE END");
            Ok(())
        }
        // `String` / `Bool` and any future non-numeric variants.
        // `JScalarKind` is `#[non_exhaustive]`.
        _ => Err(PortablePredicateError::UnsupportedPredicateKind {
            kind: "BETWEEN on non-numeric JSON operand",
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
    use crate::descriptor::{BoxedSqlBind, EnumPredicateCodec, FieldSqlType};
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

    /// Attempt runtime-registered custom scalar lowering for fields the
    /// model macro could not classify statically.
    ///
    /// Today this is intentionally limited to `#[derive(DjogiEnum)]` codecs.
    /// Unknown adopter newtypes still return `UnsupportedFieldType`; the
    /// fallback only succeeds when the field descriptor's custom SQL type and
    /// the type-erased Sassi payload both match a registered enum codec.
    #[doc(hidden)]
    pub fn emit_registered_custom<M>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        field: &FieldPredicate<M>,
    ) -> Result<(), PortablePredicateError>
    where
        M: Model,
    {
        let Some(field_descriptor) = M::descriptor()
            .fields
            .iter()
            .find(|field_descriptor| field_descriptor.name == column)
        else {
            return Err(PortablePredicateError::UnsupportedFieldType { field: column });
        };
        let postgres_type = match &field_descriptor.sql_type {
            FieldSqlType::Custom(postgres_type) => *postgres_type,
            _ => return Err(PortablePredicateError::UnsupportedFieldType { field: column }),
        };

        let mut saw_matching_sql_type = false;
        for codec in inventory::iter::<EnumPredicateCodec> {
            if codec.postgres_type != postgres_type {
                continue;
            }
            saw_matching_sql_type = true;

            match field.op() {
                LookupOp::Eq => {
                    if field_descriptor.nullable
                        && let Some(value) = (codec.bind_option_value)(field.value())
                    {
                        return emit_boxed_option_eq(acc, ctx, column, value);
                    }
                    if let Some(value) = (codec.bind_value)(field.value()) {
                        return emit_boxed_value(acc, ctx, column, " = ", value);
                    }
                }
                LookupOp::Neq => {
                    if field_descriptor.nullable
                        && let Some(value) = (codec.bind_option_value)(field.value())
                    {
                        return emit_boxed_option_neq(acc, ctx, column, value);
                    }
                    if let Some(value) = (codec.bind_value)(field.value()) {
                        return emit_boxed_value(acc, ctx, column, " <> ", value);
                    }
                }
                LookupOp::In => {
                    if field_descriptor.nullable
                        && let Some(values) = (codec.bind_option_list)(field.value())
                    {
                        return emit_boxed_option_list(acc, ctx, column, values, false);
                    }
                    if let Some(values) = (codec.bind_list)(field.value()) {
                        if field_descriptor.nullable {
                            return emit_boxed_present_list(acc, ctx, column, values, false);
                        }
                        return emit_boxed_list(acc, ctx, column, values, false);
                    }
                }
                LookupOp::NotIn => {
                    if field_descriptor.nullable
                        && let Some(values) = (codec.bind_option_list)(field.value())
                    {
                        return emit_boxed_option_list(acc, ctx, column, values, true);
                    }
                    if let Some(values) = (codec.bind_list)(field.value()) {
                        if field_descriptor.nullable {
                            return emit_boxed_present_list(acc, ctx, column, values, true);
                        }
                        return emit_boxed_list(acc, ctx, column, values, true);
                    }
                }
                op => {
                    return Err(PortablePredicateError::UnsupportedLookup { field: column, op });
                }
            }
        }

        if saw_matching_sql_type {
            Err(PortablePredicateError::ValueTypeMismatch {
                field: column,
                op: field.op(),
            })
        } else {
            Err(PortablePredicateError::UnsupportedFieldType { field: column })
        }
    }

    fn emit_boxed_value(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        op_sql: &'static str,
        value: BoxedSqlBind,
    ) -> Result<(), PortablePredicateError> {
        ctx.push_column(acc, column);
        acc.push_sql(op_sql);
        acc.push_boxed_bind(value);
        Ok(())
    }

    fn emit_boxed_list(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        values: Vec<BoxedSqlBind>,
        negated: bool,
    ) -> Result<(), PortablePredicateError> {
        if values.is_empty() {
            acc.push_sql(if negated { "TRUE" } else { "FALSE" });
            return Ok(());
        }
        ctx.push_column(acc, column);
        acc.push_sql(if negated { " NOT IN (" } else { " IN (" });
        for (i, value) in values.into_iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            acc.push_boxed_bind(value);
        }
        acc.push_sql(")");
        Ok(())
    }

    fn emit_boxed_present_list(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        values: Vec<BoxedSqlBind>,
        negated: bool,
    ) -> Result<(), PortablePredicateError> {
        if values.is_empty() {
            if negated {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NOT NULL");
            } else {
                acc.push_sql("FALSE");
            }
            return Ok(());
        }

        if negated {
            acc.push_sql("(");
            ctx.push_column(acc, column);
            acc.push_sql(" IS NOT NULL AND ");
            ctx.push_column(acc, column);
            acc.push_sql(" NOT IN (");
        } else {
            ctx.push_column(acc, column);
            acc.push_sql(" IN (");
        }

        for (i, value) in values.into_iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            acc.push_boxed_bind(value);
        }

        if negated {
            acc.push_sql("))");
        } else {
            acc.push_sql(")");
        }
        Ok(())
    }

    fn emit_boxed_option_eq(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        value: Option<BoxedSqlBind>,
    ) -> Result<(), PortablePredicateError> {
        match value {
            Some(value) => emit_boxed_value(acc, ctx, column, " = ", value),
            None => {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NULL");
                Ok(())
            }
        }
    }

    fn emit_boxed_option_neq(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        value: Option<BoxedSqlBind>,
    ) -> Result<(), PortablePredicateError> {
        match value {
            Some(value) => {
                acc.push_sql("(");
                ctx.push_column(acc, column);
                acc.push_sql(" IS NULL OR ");
                ctx.push_column(acc, column);
                acc.push_sql(" <> ");
                acc.push_boxed_bind(value);
                acc.push_sql(")");
                Ok(())
            }
            None => {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NOT NULL");
                Ok(())
            }
        }
    }

    fn emit_boxed_option_list(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        values: Vec<Option<BoxedSqlBind>>,
        negated: bool,
    ) -> Result<(), PortablePredicateError> {
        if values.is_empty() {
            acc.push_sql(if negated { "TRUE" } else { "FALSE" });
            return Ok(());
        }

        let has_none = values.iter().any(Option::is_none);
        let some_values: Vec<BoxedSqlBind> = values.into_iter().flatten().collect();

        if !negated {
            match (has_none, some_values.is_empty()) {
                (true, true) => {
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL");
                }
                (false, false) => emit_boxed_list(acc, ctx, column, some_values, false)?,
                (true, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL OR ");
                    emit_boxed_list(acc, ctx, column, some_values, false)?;
                    acc.push_sql(")");
                }
                (false, true) => unreachable!("non-empty values with no None and no Some"),
            }
        } else {
            match (has_none, some_values.is_empty()) {
                (true, true) => {
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NOT NULL");
                }
                (false, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NULL OR ");
                    emit_boxed_list(acc, ctx, column, some_values, true)?;
                    acc.push_sql(")");
                }
                (true, false) => {
                    acc.push_sql("(");
                    ctx.push_column(acc, column);
                    acc.push_sql(" IS NOT NULL AND ");
                    ctx.push_column(acc, column);
                    acc.push_sql(" NOT IN (");
                    for (i, value) in some_values.into_iter().enumerate() {
                        if i > 0 {
                            acc.push_sql(", ");
                        }
                        acc.push_boxed_bind(value);
                    }
                    acc.push_sql("))");
                }
                (false, true) => unreachable!("non-empty values with no None and no Some"),
            }
        }
        Ok(())
    }

    /// Emit list membership for `Option<V>::some()` predicates.
    ///
    /// This is intentionally distinct from [`emit_list`]: Sassi's
    /// `PresentField<T, V>` treats `None` as `false` for every comparison,
    /// so `some().not_in([])` is `column IS NOT NULL`, not the scalar-list
    /// identity `TRUE`.
    #[doc(hidden)]
    pub fn emit_present_list<V>(
        acc: &mut SqlAccumulator,
        ctx: SqlEmitContext,
        column: &'static str,
        values: &[V],
        negated: bool,
    ) -> Result<(), PortablePredicateError>
    where
        V: postgres_types::ToSql + Clone + Send + Sync + 'static,
    {
        if values.is_empty() {
            if negated {
                ctx.push_column(acc, column);
                acc.push_sql(" IS NOT NULL");
            } else {
                acc.push_sql("FALSE");
            }
            return Ok(());
        }

        if negated {
            acc.push_sql("(");
            ctx.push_column(acc, column);
            acc.push_sql(" IS NOT NULL AND ");
            ctx.push_column(acc, column);
            acc.push_sql(" NOT IN (");
        } else {
            ctx.push_column(acc, column);
            acc.push_sql(" IN (");
        }

        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            acc.push_bind(v.clone());
        }

        if negated {
            acc.push_sql("))");
        } else {
            acc.push_sql(")");
        }
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

    // ── PR2d helper-level SQL lowering tests ──────────────────────────────
    //
    // The helpers under `emit::*` are called by macro-emitted
    // `Model::__djogi_emit_field_predicate` arms. The unit tests here
    // exercise the helper signatures directly so PR2d's macro emission
    // and the helpers stay in lock-step on:
    //
    // - Optional field equality / inequality / list shapes (the v3
    //   plan PR2 Step 6 truth table).
    // - Empty / non-empty list emission for non-Option scalars.
    // - String-pattern LIKE escape + ASCII-stable case-folding parity.
    // - Joined-select parent-table qualification through
    //   `SqlEmitContext::joined`.
    //
    // The tests construct `FieldPredicate<TestModel>` instances via
    // sassi's `Field<T, V>` builder methods, mirroring what
    // `DjogiField` does internally. No live database is required.

    use crate::model::Model;
    use sassi::BasicPredicate;
    use sassi::Field as SassiField;

    // Hand-written model — the SQL helpers in `emit::*` only need
    // `M: Model` for type-binding the `FieldPredicate<M>` payload, not
    // for descriptor lookup. The trait default's unsupported-model
    // error never fires because the helpers never call back through
    // `__djogi_emit_field_predicate`.
    //
    // `active` / `maybe_year` are referenced through the typed Sassi
    // extractors below but the struct's borrow path is not exercised in
    // every test arm; suppress the dead-code lint to keep the test
    // surface stable across helper additions.
    #[allow(dead_code)]
    #[derive(Debug)]
    struct TestModel {
        id: i64,
        score: i32,
        name: String,
        active: bool,
        maybe_year: Option<i32>,
        /// JSahibON-typed payload. Tests never construct a `TestModel`
        /// instance — the field exists so `sassi::Field<TestModel,
        /// sassi::JSahibON>::new(...)` can take a `fn(&TestModel) ->
        /// &sassi::JSahibON` extractor for the forged-raw-Sassi JSON
        /// predicate rejection tests below.
        payload: sassi::JSahibON,
    }

    impl crate::model::__sealed::Sealed for TestModel {}
    #[allow(clippy::manual_async_fn)]
    impl Model for TestModel {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "test_models"
        }
        fn pk_value(&self) -> &i64 {
            &self.id
        }
        fn descriptor() -> &'static crate::descriptor::ModelDescriptor {
            unimplemented!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unimplemented!() }
        }
    }

    /// Helper: extract the `FieldPredicate<M>` out of a
    /// `BasicPredicate::Field(_)` leaf, panicking on any other variant.
    /// `M: std::fmt::Debug` so the panic path can format the unexpected
    /// variant; `BasicPredicate<M>` derives `Debug` requiring the bound.
    fn unwrap_field_pred<M: std::fmt::Debug>(
        bp: BasicPredicate<M>,
    ) -> sassi::predicate::FieldPredicate<M> {
        match bp {
            BasicPredicate::Field(fp) => fp,
            other => panic!("expected Field predicate, got {other:?}"),
        }
    }

    // ── Optional-field SQL lowering (Eq) ──────────────────────────────────

    #[test]
    fn emit_option_eq_some_value_uses_equals_bind() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_eq::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &Some(2020),
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year = $1");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_option_eq_none_uses_is_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_eq::<i32>(&mut acc, SqlEmitContext::root(), "estimated_year", &None)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NULL");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_option_neq_some_value_uses_null_or_neq() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_neq::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &Some(2020),
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "(estimated_year IS NULL OR estimated_year <> $1)");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_option_neq_none_uses_is_not_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_neq::<i32>(&mut acc, SqlEmitContext::root(), "estimated_year", &None)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NOT NULL");
        assert!(binds.is_empty());
    }

    // ── Optional-field SQL lowering (In / NotIn) ──────────────────────────

    #[test]
    fn emit_option_in_empty_returns_false_literal() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[],
            false,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "FALSE");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_option_not_in_empty_returns_true_literal() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[],
            true,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "TRUE");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_option_in_only_none_uses_is_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[None],
            false,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NULL");
    }

    #[test]
    fn emit_option_in_only_some_uses_in_list() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[Some(2019), Some(2020)],
            false,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year IN ($1, $2)");
        assert_eq!(binds.len(), 2);
    }

    #[test]
    fn emit_option_in_mixed_none_and_some_unions_predicates() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[None, Some(2020)],
            false,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "(estimated_year IS NULL OR estimated_year IN ($1))");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_option_not_in_mixed_intersects_predicates() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[None, Some(2020)],
            true,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(
            sql,
            "(estimated_year IS NOT NULL AND estimated_year NOT IN ($1))"
        );
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_option_not_in_only_none_uses_is_not_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[None],
            true,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NOT NULL");
    }

    #[test]
    fn emit_option_not_in_only_some_unions_null_branch() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_in::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[Some(2019), Some(2020)],
            true,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(
            sql,
            "(estimated_year IS NULL OR estimated_year NOT IN ($1, $2))"
        );
        assert_eq!(binds.len(), 2);
    }

    // ── Scalar list SQL lowering ──────────────────────────────────────────

    #[test]
    fn emit_list_empty_in_returns_false() {
        // `field_predicate` with an empty `Vec<i32>` payload — the
        // bound `Field<TestModel, i32>::in_(vec![])` produces this.
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.in_(vec![]));

        let mut acc = SqlAccumulator::new("");
        emit::emit_list::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", &pred, false)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "FALSE");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_list_empty_not_in_returns_true() {
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.not_in(vec![]));

        let mut acc = SqlAccumulator::new("");
        emit::emit_list::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", &pred, true)
            .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "TRUE");
    }

    #[test]
    fn emit_list_non_empty_in_emits_inlist() {
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.in_(vec![1, 2, 3]));

        let mut acc = SqlAccumulator::new("");
        emit::emit_list::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", &pred, false)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "score IN ($1, $2, $3)");
        assert_eq!(binds.len(), 3);
    }

    #[test]
    fn emit_list_non_empty_not_in_emits_not_inlist() {
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.not_in(vec![1, 2]));

        let mut acc = SqlAccumulator::new("");
        emit::emit_list::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", &pred, true)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "score NOT IN ($1, $2)");
        assert_eq!(binds.len(), 2);
    }

    #[test]
    fn emit_list_value_type_mismatch_returns_typed_error() {
        // Predicate's payload is `Vec<i32>`; helper requested `Vec<String>`.
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.in_(vec![1]));

        let mut acc = SqlAccumulator::new("");
        let result = emit::emit_list::<TestModel, String>(
            &mut acc,
            SqlEmitContext::root(),
            "score",
            &pred,
            false,
        );
        match result {
            Err(PortablePredicateError::ValueTypeMismatch { field, .. }) => {
                assert_eq!(field, "score");
            }
            other => panic!("expected ValueTypeMismatch, got {other:?}"),
        }
    }

    // ── Present optional-field list SQL lowering ─────────────────────────

    #[test]
    fn emit_present_list_empty_in_returns_false() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_present_list::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[],
            false,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "FALSE");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_present_list_empty_not_in_requires_present_value() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_present_list::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[],
            true,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NOT NULL");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_present_list_non_empty_not_in_excludes_nulls() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_present_list::<i32>(
            &mut acc,
            SqlEmitContext::root(),
            "estimated_year",
            &[2019, 2020],
            true,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(
            sql,
            "(estimated_year IS NOT NULL AND estimated_year NOT IN ($1, $2))"
        );
        assert_eq!(binds.len(), 2);
    }

    // ── String pattern SQL lowering ───────────────────────────────────────

    #[test]
    fn emit_string_pattern_contains_wraps_bind_with_percent_signs() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.contains("rust"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::root(),
            "name",
            emit::PatternOp::Contains,
            &pred,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        // `Contains` is the case-sensitive form — Postgres `LIKE` with
        // explicit `ESCAPE '\'`.
        assert_eq!(sql, "name LIKE $1 ESCAPE '\\'");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_string_pattern_icontains_uses_collate_c_ilike() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.icontains("rust"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::root(),
            "name",
            emit::PatternOp::IContains,
            &pred,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        // ASCII-stable case-insensitive form: `COLLATE "C" ILIKE`.
        // Mirrors `query/sql.rs::escape_like` semantics so portable
        // and SQL evaluators agree.
        assert_eq!(sql, "name COLLATE \"C\" ILIKE $1 ESCAPE '\\'");
    }

    #[test]
    fn emit_string_pattern_iexact_uses_no_wildcard_collate_ilike() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.iexact("Rust"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::root(),
            "name",
            emit::PatternOp::IExact,
            &pred,
        )
        .unwrap();
        let (sql, binds) = acc.into_parts();
        // No wildcard wrapping — `IExact` is exact equality with
        // ASCII case folding. Exact user input must not become a
        // wildcard match.
        assert_eq!(sql, "name COLLATE \"C\" ILIKE $1 ESCAPE '\\'");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_string_pattern_starts_with_appends_percent() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.starts_with("ru"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::root(),
            "name",
            emit::PatternOp::StartsWith,
            &pred,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "name LIKE $1 ESCAPE '\\'");
    }

    #[test]
    fn emit_string_pattern_ends_with_prepends_percent() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.ends_with("st"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::root(),
            "name",
            emit::PatternOp::EndsWith,
            &pred,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "name LIKE $1 ESCAPE '\\'");
    }

    // ── Scalar value bind shape ───────────────────────────────────────────

    #[test]
    fn emit_value_emits_op_and_bind() {
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.eq(42));

        let mut acc = SqlAccumulator::new("");
        emit::emit_value::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", " = ", &pred)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "score = $1");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn emit_pair_emits_between_with_two_binds() {
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.between(0, 100));

        let mut acc = SqlAccumulator::new("");
        emit::emit_pair::<TestModel, i32>(&mut acc, SqlEmitContext::root(), "score", &pred)
            .unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "score BETWEEN $1 AND $2");
        assert_eq!(binds.len(), 2);
    }

    // ── Null-test SQL lowering ────────────────────────────────────────────

    #[test]
    fn emit_null_true_uses_is_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_null(&mut acc, SqlEmitContext::root(), "estimated_year", true).unwrap();
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NULL");
        assert!(binds.is_empty());
    }

    #[test]
    fn emit_null_false_uses_is_not_null() {
        let mut acc = SqlAccumulator::new("");
        emit::emit_null(&mut acc, SqlEmitContext::root(), "estimated_year", false).unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "estimated_year IS NOT NULL");
    }

    // ── Joined-select parent-table qualification ──────────────────────────

    #[test]
    fn emit_value_under_joined_context_qualifies_column() {
        // Mirrors what `build_select_joined` will do once PR2b's
        // direct-Q walker threads `SqlEmitContext::joined(T::table_name())`
        // into expression subqueries / joined-select WHERE emission.
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred = unwrap_field_pred(f.eq(42));

        let mut acc = SqlAccumulator::new("");
        emit::emit_value::<TestModel, i32>(
            &mut acc,
            SqlEmitContext::joined("test_models"),
            "score",
            " = ",
            &pred,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "test_models.score = $1");
    }

    #[test]
    fn emit_string_pattern_under_joined_context_qualifies_column() {
        let f = SassiField::<TestModel, String>::new("name", |m| &m.name);
        let pred = unwrap_field_pred(f.icontains("rust"));

        let mut acc = SqlAccumulator::new("");
        emit::emit_string_pattern(
            &mut acc,
            SqlEmitContext::joined("test_models"),
            "name",
            emit::PatternOp::IContains,
            &pred,
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(sql, "test_models.name COLLATE \"C\" ILIKE $1 ESCAPE '\\'");
    }

    #[test]
    fn emit_option_eq_under_joined_context_qualifies_both_sides() {
        // The mixed `(col IS NULL OR col <> $1)` shape uses
        // `push_column` twice — both sides must qualify.
        let mut acc = SqlAccumulator::new("");
        emit::emit_option_neq::<i32>(
            &mut acc,
            SqlEmitContext::joined("test_models"),
            "estimated_year",
            &Some(2020),
        )
        .unwrap();
        let (sql, _) = acc.into_parts();
        assert_eq!(
            sql,
            "(test_models.estimated_year IS NULL OR test_models.estimated_year <> $1)"
        );
    }

    // ── Defensive SqlEmitContext::joined("parent").push_column(rel.field) ──
    //
    // Already covered by `dotted_column_is_emitted_as_is_under_joined`
    // above. This test repeats the exact assertion the dispatch
    // requires so the named test is easy to find by phase prefix:
    // the joined context MUST emit a dotted column unchanged, even
    // though that means the SQL-only relation field is NOT a portable
    // predicate leaf.
    #[test]
    fn phase8eta_pr2d_joined_push_column_with_dotted_path_emits_as_is() {
        let mut acc = SqlAccumulator::new("");
        SqlEmitContext::joined("posts").push_column(&mut acc, "rel.field");
        let (sql, _) = acc.into_parts();
        // Expectation from the v3 plan: "asserts it emits `rel.field`,
        // not `parent.rel.field`; this test does not make `rel.field`
        // portable through generated root metadata."
        assert_eq!(sql, "rel.field");
    }

    // ── #195: forged raw Sassi `LookupOp::Json` rejection ─────────────────
    //
    // Spec at `docs/spec/mirjzson-jsonb-integration.md` §"Trusted Portable
    // Construction" and §"Tests": "Forged standalone Sassi `LookupOp::Json`
    // predicates without Djogi provenance are rejected by Djogi lowering."
    //
    // These tests construct a `BasicPredicate<TestModel>` directly via the
    // raw Sassi `sassi::Field<T, JSahibON>::new(...).jsahibon()` builder —
    // bypassing Djogi's [`crate::query::mirjzson::DjogiField<M, MirJzSON>::
    // jsahibon`] trusted-construction surface. The body downcasts
    // correctly to a real `JSahibONPredicateBody` (because Sassi's own
    // builder produced it), so the upstream "downcast None ⇒ forgery"
    // branch in `emit_jsahibon_predicate` does NOT trigger. The defense
    // is the [`JsonTrust::Untrusted`] check at the walker dispatch.

    /// Forged `Field<TestModel, JSahibON>` extractor. Sassi requires a
    /// `fn(&T) -> &V` pointer (non-capturing). `TestModel::payload` is a
    /// real field on the test struct so the lifetime relation
    /// `&'a TestModel -> &'a JSahibON` holds without static gymnastics.
    /// SQL emission never invokes the extractor — the predicate body
    /// alone determines the SQL shape — so a never-constructed
    /// `TestModel` is fine.
    fn forged_jsahibon_extractor(m: &TestModel) -> &sassi::JSahibON {
        &m.payload
    }

    #[test]
    fn phase85_195_forged_raw_sassi_json_predicate_is_rejected_when_untrusted() {
        // Raw Sassi field builder — no Djogi-provenance stamp. The
        // `field_name` "forged_payload" never went through Djogi's
        // identifier validator.
        let forged: BasicPredicate<TestModel> = SassiField::<TestModel, sassi::JSahibON>::new(
            "forged_payload",
            forged_jsahibon_extractor,
        )
        .jsahibon()
        .exists();

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &forged,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        match result {
            Err(PortablePredicateError::UntrustedJsonPredicate { field }) => {
                assert_eq!(field, "forged_payload");
            }
            other => panic!("expected UntrustedJsonPredicate, got {other:?}"),
        }
    }

    #[test]
    fn phase85_195_forged_json_predicate_nested_in_and_is_rejected() {
        // Trust does not get "promoted" by nesting. The recursive
        // walker propagates the caller's `JsonTrust` into every
        // sub-tree, so a forged JSON leaf wrapped inside an
        // `And([True, forged_json])` still surfaces
        // `UntrustedJsonPredicate`. Without this propagation, an
        // attacker could "launder" a forged leaf by composing it with a
        // trivial trusted operand.
        let forged_json: BasicPredicate<TestModel> = SassiField::<TestModel, sassi::JSahibON>::new(
            "forged_payload",
            forged_jsahibon_extractor,
        )
        .jsahibon()
        .exists();
        let nested = BasicPredicate::And(vec![BasicPredicate::True, forged_json]);

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &nested,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        match result {
            Err(PortablePredicateError::UntrustedJsonPredicate { field }) => {
                assert_eq!(field, "forged_payload");
            }
            other => panic!("expected UntrustedJsonPredicate, got {other:?}"),
        }
    }

    #[test]
    fn phase85_195_forged_json_predicate_nested_in_or_is_rejected() {
        // Mirror of the `And` case for `Or`. Same propagation rule.
        let forged_json: BasicPredicate<TestModel> = SassiField::<TestModel, sassi::JSahibON>::new(
            "forged_payload",
            forged_jsahibon_extractor,
        )
        .jsahibon()
        .exists();
        let nested = BasicPredicate::Or(vec![BasicPredicate::False, forged_json]);

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &nested,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        assert!(
            matches!(
                result,
                Err(PortablePredicateError::UntrustedJsonPredicate { field }) if field == "forged_payload"
            ),
            "expected UntrustedJsonPredicate(forged_payload), got {result:?}",
        );
    }

    #[test]
    fn phase85_195_forged_json_predicate_nested_in_not_is_rejected() {
        // Negation does not flip trust. `NOT (forged_json)` is still
        // a forged JSON leaf walk; the walker rejects it.
        let forged_json: BasicPredicate<TestModel> = SassiField::<TestModel, sassi::JSahibON>::new(
            "forged_payload",
            forged_jsahibon_extractor,
        )
        .jsahibon()
        .exists();
        let nested = BasicPredicate::Not(Box::new(forged_json));

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &nested,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        assert!(matches!(
            result,
            Err(PortablePredicateError::UntrustedJsonPredicate { .. })
        ));
    }

    #[test]
    fn phase85_195_forged_json_predicate_nested_in_xor_is_rejected() {
        // XOR's binary shape composes both sides through the recursive
        // walker (the SQL identity `((NOT a) AND b) OR (a AND (NOT b))`
        // visits each operand twice). A forged JSON leaf on either side
        // surfaces `UntrustedJsonPredicate` from the first visit.
        let forged_json: BasicPredicate<TestModel> = SassiField::<TestModel, sassi::JSahibON>::new(
            "forged_payload",
            forged_jsahibon_extractor,
        )
        .jsahibon()
        .exists();
        let nested = BasicPredicate::Xor(Box::new(BasicPredicate::True), Box::new(forged_json));

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &nested,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        assert!(matches!(
            result,
            Err(PortablePredicateError::UntrustedJsonPredicate { .. })
        ));
    }

    #[test]
    fn phase85_195_untrusted_non_json_field_predicate_dispatches_normally() {
        // Trust gating only applies to `LookupOp::Json` leaves. A
        // forged non-JSON leaf (`f.score.eq(42)` via raw Sassi) still
        // routes through `Model::__djogi_emit_field_predicate`. Hand-
        // written `impl Model for TestModel` keeps the trait default
        // (returns `UnsupportedModel`), so the walker surfaces that
        // typed error rather than `UntrustedJsonPredicate`. Confirms
        // the trust check does NOT short-circuit non-JSON dispatch.
        let f = SassiField::<TestModel, i32>::new("score", |m| &m.score);
        let pred: BasicPredicate<TestModel> = f.eq(42);

        let mut acc = SqlAccumulator::new("");
        let result = emit_basic_predicate::<TestModel>(
            &mut acc,
            &pred,
            SqlEmitContext::root(),
            JsonTrust::Untrusted,
        );

        // `TestModel`'s default `__djogi_emit_field_predicate` returns
        // `UnsupportedModel`; the trust flag does not intercept this
        // dispatch.
        match result {
            Err(PortablePredicateError::UnsupportedModel { .. }) => {}
            other => panic!("expected UnsupportedModel from non-JSON dispatch, got {other:?}"),
        }
    }

    #[test]
    fn phase85_195_jsontrust_variants_are_distinct() {
        // Smoke test — exercises the enum's PartialEq / Eq derives so
        // a future accidental `#[derive]` removal trips compilation
        // here rather than silently breaking the trust comparison in
        // `emit_basic_predicate`.
        assert_ne!(JsonTrust::Trusted, JsonTrust::Untrusted);
        assert_eq!(JsonTrust::Trusted, JsonTrust::Trusted);
        assert_eq!(JsonTrust::Untrusted, JsonTrust::Untrusted);
    }
}
