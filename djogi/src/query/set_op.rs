//! Typed set operations between same-model [`QuerySet`]s.
//! # What
//! [`SetOpQuerySet<T>`] is the value produced by [`QuerySet::union`],
//! [`QuerySet::union_all`], [`QuerySet::intersect`], and
//! [`QuerySet::except`]. It carries the two arms, the chosen
//! [`SetOpKind`], and an outer set of modifiers (`ORDER BY`, `LIMIT`,
//! `OFFSET`) that are emitted **outside** the parenthesised set-op
//! body. Both arms are statically required to share the same `T:
//! Model`, so the result rows decode through `T`'s existing
//! [`FromPgRow`] impl without any cross-model row reconstruction.
//! # Why a sibling type, not a `QuerySet<T>`
//! A set-op result is structurally distinct from a plain
//! `SELECT ... FROM <table>`:
//! - Further `.filter(...)` / `.exclude(...)` on the combined result
//!   is NOT the same as filtering each arm — Postgres semantics treat
//!   filters as belonging to whichever arm they appear in, and a
//!   "filter the union" requires wrapping the entire set-op as a
//!   derived table. Forcing that wrap silently into every chained
//!   builder method would surprise adopters who expect their `.filter`
//!   to compose under the same semantics they got from a plain
//!   queryset.
//! - `select_related` / `prefetch` extend the SELECT projection on the
//!   left arm in incompatible ways with the right arm (which projects
//!   only `T`'s canonical column list). Letting them ride through a
//!   set op would either silently drop the join columns or produce a
//!   row shape that does not decode as `T`.
//! - Row-level locks (`FOR UPDATE`) on a set-op subquery are rejected
//!   by Postgres at parse time.
//!   Keeping the surface narrow — a fresh [`SetOpQuerySet<T>`] with an
//!   outer `ORDER BY` / `LIMIT` / `OFFSET` slot and read terminals — is
//!   the minimum viable design that matches both PG semantics and the
//!   adopter's intuition.
//! # Postgres semantics this layer enforces
//! Each arm is **always parenthesised** in the emitted SQL so a
//! per-arm `ORDER BY` / `LIMIT` / `OFFSET` (legal Postgres when
//! parenthesised) does not bind to the outer set-op operator. Adopters
//! can therefore write:
//! ```ignore
//! let recent = Dog::objects()
//!     .filter(|f| f.status().eq(Status::Adopted))
//!     .order_by(|f| f.adopted_at().desc())
//!     .limit(10);
//! let waitlist = Dog::objects()
//!     .filter(|f| f.status().eq(Status::Fostered))
//!     .order_by(|f| f.fostered_at().desc())
//!     .limit(5);
//! let rows = recent.union(waitlist).fetch_all(&mut ctx).await?;
//! ```
//! and Postgres parses the per-arm `ORDER BY` and `LIMIT` exactly
//! as written. Outer ordering / pagination apply to the **combined**
//! result.
//! Arms with [`is_empty`](QuerySet::none) set short-circuit to a
//! `WHERE FALSE`-style emission so the set-op SQL stays semantically
//! correct: `(empty) UNION (other)` = `(other)`, `(empty) INTERSECT
//! (other)` = `(empty)`, and so on. The terminal does not fold these
//! short-circuits client-side — the database evaluates the
//! `WHERE FALSE` arm in microseconds and the row count flows naturally
//! through the set operator.
//! # What is rejected
//! ## Arm-level state
//! [`SetOpArmInvalid`](DjogiError::SetOpArmInvalid) surfaces at the
//! terminal level when an arm carries any of:
//! - `.prefetch(...)` registrations,
//! - `.select_related(...)` registrations,
//! - `.select_for_update(...)` / `.nowait()` / `.skip_locked()` locks,
//! - `.cache(...)` Punnu bindings.
//!   These leak structural shape (extra projections, locks, side
//!   effects) into a context where the type signature pretends the arm
//!   is a plain `SELECT t.* FROM t`. Silently dropping them would be a
//!   correctness bug. The rejection happens at SQL-build time so the
//!   call site reports the error before any database round trip.
//! ## Outer ordering expressions
//! [`SetOpOuterOrderingInvalid`](DjogiError::SetOpOuterOrderingInvalid)
//! surfaces when the outer
//! [`order_by`](SetOpQuerySet::order_by) carries an expression-form
//! term that Postgres rejects on set-operation outer ORDER BY. Today
//! the only producer of such a term is the spatial
//! `order_by_distance(...)` helper (feature-gated; see
//! `FieldRef::order_by_distance` under `feature = "spatial"`), which
//! lowers to `ST_Distance(...)`. Postgres's grammar restricts set-
//! operation outer ORDER BY to **output column names or position
//! numbers**, so a per-arm spatial order is legal (it lives inside
//! parenthesised arm parens) but combining sets and ordering the
//! merged result by distance is not. Djogi catches this at SQL-build
//! time so the diagnostic names the offending operation; the
//! Postgres-side message (`syntax error at or near "("`) would not.
//! Both validation failures fire *before* any GUC `SET LOCAL` from
//! tenant auto-wiring — the SQL emitter runs first, tenant scope
//! propagation runs second on the already-validated query.
//! # Why no `.fetch_one()` / `.exists()` on the set-op surface
//! Both are reachable through the equivalent shape (`.limit(2)` +
//! `.fetch_all()` for fetch_one semantics; `.first()`-then-Boolean for
//! `exists`). Phase-1 surface keeps the public API tight; adding these
//! later is additive.
//! # Why RPITIT (not `async fn`)
//! Matches the existing [`QuerySet`] terminals — every terminal
//! returns `impl Future<Output = ...> + Send` rather than using bare
//! `async fn`. The explicit `+ Send` bound matches `Model::create` /
//! `QuerySet::fetch_all` and guarantees the returned future can be
//! `.await`ed across task boundaries. `clippy::manual_async_fn` fires
//! on this pattern; the lint is allowed at the module level because
//! the explicit-bound form is the deliberate choice.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::order::OrderExpr;
use crate::query::queryset::QuerySet;
use std::future::Future;
use std::marker::PhantomData;

/// The four PostgreSQL set operators between two same-shape SELECTs.
/// # Semantics (Postgres)
/// | Variant | SQL keyword | Duplicates | Order |
/// |---------|-------------|------------|-------|
/// | [`Union`](SetOpKind::Union) | `UNION` | de-duplicated across both arms | unspecified |
/// | [`UnionAll`](SetOpKind::UnionAll) | `UNION ALL` | preserved | unspecified |
/// | [`Intersect`](SetOpKind::Intersect) | `INTERSECT` | de-duplicated; row must appear in both arms | unspecified |
/// | [`Except`](SetOpKind::Except) | `EXCEPT` | de-duplicated; rows in left arm not in right arm | unspecified |
/// Duplicate semantics are Postgres-native and match SQL standard
/// (`UNION` / `INTERSECT` / `EXCEPT` all imply `DISTINCT` unless `ALL`
/// is appended). Djogi exposes the `ALL` variant only for `UNION`
/// today because `INTERSECT ALL` / `EXCEPT ALL` have looser semantics
/// (preserve duplicate counts with multiset arithmetic) that are
/// less commonly useful and can be added additively if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetOpKind {
    /// `LEFT UNION RIGHT` — de-duplicated union.
    Union,
    /// `LEFT UNION ALL RIGHT` — duplicate-preserving union.
    UnionAll,
    /// `LEFT INTERSECT RIGHT` — rows in both arms (de-duplicated).
    Intersect,
    /// `LEFT EXCEPT RIGHT` — rows in left arm not in right arm
    /// (de-duplicated). Set difference; NOT symmetric.
    Except,
}

impl SetOpKind {
    /// Render the SQL keyword exactly as Postgres expects.
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            SetOpKind::Union => "UNION",
            SetOpKind::UnionAll => "UNION ALL",
            SetOpKind::Intersect => "INTERSECT",
            SetOpKind::Except => "EXCEPT",
        }
    }
}

/// One arm of a [`SetOpQuerySet<T>`].
/// Either a plain [`QuerySet<T>`] or another [`SetOpQuerySet<T>`] for
/// the nested-composition case (`a.union(b).intersect(c)` produces a
/// `SetOpQuerySet` whose left arm is itself a `SetOpQuerySet`).
/// Both variants are boxed so the enum's storage stays compact (one
/// pointer per arm) — `QuerySet<T>` itself is ~370 bytes, and a
/// `SetOpQuerySet<T>` carries two arms plus outer modifiers, so an
/// inline `QuerySet<T>` arm would push the enum past 700 bytes per
/// stack value. Boxing keeps the surface uniform and matches how
/// `Q<T>: Box`-internal nested arms are already laid out.
/// `#[doc(hidden)]` — adopters never name this type. It exists as the
/// public return shape of [`IntoSetOpArm::into_set_op_arm`] only
/// because trait methods cannot return a `pub(crate)` type from a
/// `pub` trait. The trait itself is sealed via [`sealed::Sealed`], so
/// no external impl can produce a [`SetOpArm`] — the variant
/// constructors are crate-private and the only paths in are the two
/// builtin impls below.
#[doc(hidden)]
pub enum SetOpArm<T: Model> {
    /// A plain queryset arm — emits `(<SELECT...>)` with the same
    /// canonical column list every other read terminal uses. Boxed
    /// to keep the enum's stack footprint to one pointer per arm; see
    /// the type-level note above.
    QuerySet(Box<QuerySet<T>>),
    /// A nested set-op arm — emits the inner set-op SQL inside outer
    /// parens. Boxed because [`SetOpQuerySet<T>`] is the outer type and
    /// the language requires the recursive case to be indirected.
    Nested(Box<SetOpQuerySet<T>>),
}

impl<T: Model> std::fmt::Debug for SetOpArm<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetOpArm::QuerySet(qs) => f.debug_tuple("QuerySet").field(qs).finish(),
            SetOpArm::Nested(nested) => f.debug_tuple("Nested").field(nested).finish(),
        }
    }
}

impl<T: Model> Clone for SetOpArm<T> {
    fn clone(&self) -> Self {
        match self {
            SetOpArm::QuerySet(qs) => SetOpArm::QuerySet(qs.clone()),
            SetOpArm::Nested(nested) => SetOpArm::Nested(nested.clone()),
        }
    }
}

mod sealed {
    /// Crate-private supertrait so adopters cannot construct new
    /// arm shapes outside djogi. Keeps the type set closed: only
    /// `QuerySet<T>` and `SetOpQuerySet<T>` qualify as arms today,
    /// and any future widening goes through an explicit `impl
    /// IntoSetOpArm` here.
    pub trait Sealed {}
    impl<T: super::Model> Sealed for super::QuerySet<T> {}
    impl<T: super::Model> Sealed for super::SetOpQuerySet<T> {}
}

/// Sealed conversion trait used by the set-op builder methods.
/// # What it accepts
/// - `QuerySet<T>` — a plain queryset arm.
/// - `SetOpQuerySet<T>` — a previously-built set-op result, allowing
///   chained composition (`a.union(b).intersect(c)`).
///   Adopters never name this trait directly; they pass either a
///   `QuerySet<T>` or a `SetOpQuerySet<T>` to [`QuerySet::union`] /
///   [`SetOpQuerySet::union`] and the bound is satisfied automatically.
///   The trait is sealed (no external impls) so the arm storage stays
///   closed for SQL emission.
pub trait IntoSetOpArm<T: Model>: sealed::Sealed {
    /// Lift `self` into the internal arm representation. Public only
    /// because the trait itself is — the produced [`SetOpArm`] stays
    /// `pub(crate)` and adopters cannot pattern-match on it.
    #[doc(hidden)]
    fn into_set_op_arm(self) -> SetOpArm<T>;
}

impl<T: Model> IntoSetOpArm<T> for QuerySet<T> {
    fn into_set_op_arm(self) -> SetOpArm<T> {
        SetOpArm::QuerySet(Box::new(self))
    }
}

impl<T: Model> IntoSetOpArm<T> for SetOpQuerySet<T> {
    fn into_set_op_arm(self) -> SetOpArm<T> {
        SetOpArm::Nested(Box::new(self))
    }
}

/// A typed set operation (`UNION` / `UNION ALL` / `INTERSECT` /
/// `EXCEPT`) between two same-model [`QuerySet`]s.
/// Constructed via [`QuerySet::union`] / [`QuerySet::union_all`] /
/// [`QuerySet::intersect`] / [`QuerySet::except`] or the sibling
/// methods on [`SetOpQuerySet`] for chained composition. Outer
/// `ORDER BY` / `LIMIT` / `OFFSET` are applied to the combined result.
/// # Lazy
/// Nothing hits the database until a terminal (`fetch_all`, `first`,
/// `count`) is awaited. The struct is cheap to clone (`Arc` semantics
/// inside arms; no row materialisation).
/// # Type parameter
/// Both arms carry the same `T: Model`, so the combined result
/// decodes positionally through `T`'s [`FromPgRow`] impl. There is no
/// public constructor that accepts heterogeneous-model arms; the
/// existing typed-row machinery for multi-model shapes (
/// issue #99 / #84 pair-tuple work) is the right surface for that
/// case, not this one.
pub struct SetOpQuerySet<T: Model> {
    /// Left-hand arm. Always parenthesised at emit time.
    pub(crate) left: SetOpArm<T>,
    /// The set operator joining the two arms.
    pub(crate) op: SetOpKind,
    /// Right-hand arm. Always parenthesised at emit time.
    pub(crate) right: SetOpArm<T>,
    /// Outer `ORDER BY` — applied to the combined result, **after**
    /// the set operator. Per-arm orderings live on each arm's own
    /// `QuerySet::ordering` and are emitted inside the arm parens.
    pub(crate) ordering: Vec<OrderExpr>,
    /// Outer `LIMIT` — applied to the combined result. `None` means
    /// no limit. `i64` to match Postgres `BIGINT`.
    pub(crate) limit: Option<i64>,
    /// Outer `OFFSET` — applied to the combined result. `None` means
    /// no offset. `i64` to match Postgres `BIGINT`.
    pub(crate) offset: Option<i64>,
    /// Covariant `T` tag — see [`QuerySet`]'s `_model` field for the
    /// rationale (`fn() -> T` makes the wrapper covariant in `T` and
    /// `Send + Sync` regardless of `T`'s own markers).
    _model: PhantomData<fn() -> T>,
}

impl<T: Model> std::fmt::Debug for SetOpQuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetOpQuerySet")
            .field("table", &T::table_name())
            .field("op", &self.op)
            .field("left", &self.left)
            .field("right", &self.right)
            .field("ordering", &self.ordering)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .finish()
    }
}

impl<T: Model> Clone for SetOpQuerySet<T> {
    fn clone(&self) -> Self {
        SetOpQuerySet {
            left: self.left.clone(),
            op: self.op,
            right: self.right.clone(),
            ordering: self.ordering.clone(),
            limit: self.limit,
            offset: self.offset,
            _model: PhantomData,
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────

/// Validate that this set-op's outer `ORDER BY` only contains terms that
/// Postgres accepts on a set-operation outer ordering — i.e. output
/// column names (rendered as bare identifiers by
/// [`OrderExpr::Column`]). Expression-form terms (e.g. spatial
/// `ST_Distance(...)` from `FieldRef::order_by_distance` under
/// `feature = "spatial"`) are rejected with
/// [`DjogiError::SetOpOuterOrderingInvalid`].
/// # Why
/// Postgres's grammar restricts set-operation `ORDER BY` to the output
/// projection — column names or `$N` position numbers — because the
/// expression context (which arm's columns?) is ambiguous on the
/// combined result. Letting an `ST_Distance(...)` term ride through
/// would surface a parser error from Postgres without naming the
/// offending operation. Catching it here keeps the diagnostic
/// actionable.
/// # Workaround for adopters
/// Spatial distance ordering still works on a single arm (per-arm
/// `.order_by(|f| f.location().order_by_distance(center))` is legal,
/// because the arm is parenthesised and Postgres allows expression
/// `ORDER BY` inside the parens). For combined-result spatial ordering,
/// wrap the entire set-op in a subquery and apply the spatial order
/// there — not supported by this surface today.
// `T` is consumed by the spatial-error path (`T::table_name()`) when
// the `spatial` feature is enabled. Without that feature, the only
// reachable arm is `OrderExpr::Column { .. }` (which does not need
// `T`), so clippy correctly flags the parameter as unused. We keep
// `T` in the signature for API consistency — adding a new
// expression-form `OrderExpr` variant in the future is much easier
// with the type parameter already in place — and silence the lint
// only when the spatial branch is compiled out.
#[cfg_attr(not(feature = "spatial"), allow(clippy::extra_unused_type_parameters))]
fn validate_outer_ordering<T: Model>(ordering: &[OrderExpr]) -> Result<(), DjogiError> {
    for o in ordering {
        match o {
            OrderExpr::Column { .. } => {
                // Bare-column outer ORDER BY is exactly what Postgres
                // set-op outer accepts. No further check needed
                // `OrderExpr::Column` carries macro-validated column
                // names from `FieldRef::asc` / `desc`.
            }
            #[cfg(feature = "spatial")]
            OrderExpr::SpatialDistance { .. } => {
                return Err(DjogiError::SetOpOuterOrderingInvalid {
                    table: T::table_name(),
                    reason: "spatial distance ordering (ST_Distance(...) from \
                             `order_by_distance`) is an expression, but Postgres \
                             set-operation outer ORDER BY accepts only output \
                             column names. Apply spatial ordering on a per-arm \
                             basis instead, or wrap the set-op result in a \
                             subquery before ordering by distance",
                });
            }
        }
    }
    Ok(())
}

/// Validate that `qs` carries no state the set-op surface cannot
/// represent. Returns a typed [`DjogiError::SetOpArmInvalid`] when the
/// arm has prefetch / select_related / lock / cache bindings.
/// Centralised here so both arms in [`build_set_op_select`] and the
/// nested case in [`emit_arm`] report identical diagnostics.
pub(crate) fn validate_arm<T: Model>(
    qs: &QuerySet<T>,
    side: &'static str,
) -> Result<(), DjogiError> {
    if !qs.prefetch_paths.is_empty() {
        return Err(DjogiError::SetOpArmInvalid {
            table: T::table_name(),
            side,
            reason: "arm has registered .prefetch(...) paths; \
                     prefetch is not supported on set-op arms — \
                     run the set op then prefetch on the combined result",
        });
    }
    if !qs.select_related_paths.is_empty() {
        return Err(DjogiError::SetOpArmInvalid {
            table: T::table_name(),
            side,
            reason: "arm has registered .select_related(...) paths; \
                     select_related is not supported on set-op arms because \
                     the joined projection would not match the other arm's column list",
        });
    }
    if !matches!(qs.lock, crate::query::lock::LockMode::None) {
        return Err(DjogiError::SetOpArmInvalid {
            table: T::table_name(),
            side,
            reason: "arm has a row-level lock (FOR UPDATE / FOR SHARE / NOWAIT \
                     / SKIP LOCKED); Postgres rejects FOR UPDATE and FOR \
                     SHARE inside a set-op subquery",
        });
    }
    if qs.cache_target.is_some() {
        return Err(DjogiError::SetOpArmInvalid {
            table: T::table_name(),
            side,
            reason: "arm has a .cache(&punnu) binding; cache hooks are not yet \
                     supported on set-op terminals — bind the cache on a plain \
                     fetch instead",
        });
    }
    Ok(())
}

/// Emit one arm into `acc`, wrapped in parens. Handles both plain
/// queryset arms (delegates to a small inline builder mirroring
/// [`crate::query::sql::build_select`]) and nested set-op arms
/// (recurses into [`build_set_op_select_inner`]).
/// The `is_empty` short-circuit emits `SELECT <cols> FROM <table>
/// WHERE FALSE` so that
/// [`QuerySet::none`](crate::query::QuerySet::none)-derived arms
/// produce zero rows from this side without inheriting the parent
/// queryset's accumulated condition tree.
fn emit_arm<T: Model + FromPgRow>(
    acc: &mut SqlAccumulator,
    arm: &SetOpArm<T>,
    side: &'static str,
) -> Result<(), DjogiError> {
    acc.push_sql("(");
    match arm {
        SetOpArm::QuerySet(qs) => {
            validate_arm(qs, side)?;
            if qs.is_empty() {
                // Short-circuit: a structural-none arm emits a guaranteed
                // zero-row SELECT so the set operator's algebra stays
                // correct. We bypass `build_select` entirely because the
                // queryset's accumulated `condition` tree is irrelevant
                // for an arm that the caller explicitly marked empty.
                acc.push_sql("SELECT ");
                acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
                acc.push_sql(" FROM ");
                acc.push_sql(T::table_name());
                acc.push_sql(" WHERE FALSE");
            } else {
                // Build the arm SELECT through the same accumulator
                // `build_select` builds into a fresh accumulator and we
                // splice with `extend_with`, which renumbers `$N`
                // placeholders to continue the outer bind sequence.
                let inner = crate::query::sql::build_select(qs)?;
                acc.extend_with(inner);
            }
        }
        SetOpArm::Nested(nested) => {
            // Recurse — the inner set-op also wraps its own arms in
            // parens, so the resulting SQL is well-nested.
            build_set_op_select_inner(acc, nested)?;
        }
    }
    acc.push_sql(")");
    Ok(())
}

/// Inner emitter for [`SetOpQuerySet<T>`] — assumes the outer
/// `SELECT FROM (...) AS sub` wrap (for `count`) or no wrap at all
/// (for `fetch_all` / `first`) is the caller's concern. Emits:
/// `(<left>) <OP> (<right>) [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
/// Validates the outer `ORDER BY` shape before emission via
/// [`validate_outer_ordering`] — spatial `ST_Distance(...)` ordering on
/// the combined result is rejected as a typed
/// [`DjogiError::SetOpOuterOrderingInvalid`] so the constraint surfaces
/// before the SQL ever reaches Postgres. Arm validation
/// ([`validate_arm`]) runs inside [`emit_arm`] for each arm.
fn build_set_op_select_inner<T: Model + FromPgRow>(
    acc: &mut SqlAccumulator,
    sop: &SetOpQuerySet<T>,
) -> Result<(), DjogiError> {
    // Front-load the outer-ordering validation so the error is reported
    // without us having emitted the arm SQL into `acc` first. Same
    // before-the-round-trip contract `validate_arm` already follows.
    validate_outer_ordering::<T>(&sop.ordering)?;

    emit_arm(acc, &sop.left, "left")?;
    acc.push_sql(" ");
    acc.push_sql(sop.op.keyword());
    acc.push_sql(" ");
    emit_arm(acc, &sop.right, "right")?;

    // Outer modifiers — Postgres binds these to the combined result
    // because each arm is parenthesised. Order matters: ORDER BY before
    // LIMIT before OFFSET, mirroring `query::sql::push_tail`.
    if !sop.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in sop.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            // `table_qualifier = None` — set-op outer ORDER BY
            // references projection-level column names, which are
            // unqualified in the combined result. The single-table
            // queryset path always uses `None` here too.
            // `validate_outer_ordering` above guarantees every entry
            // here is an `OrderExpr::Column` variant; expression-form
            // terms (spatial `ST_Distance(...)`) would have errored out
            // already.
            o.emit(acc, None);
        }
    }
    if let Some(n) = sop.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = sop.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(())
}

/// Public-ish (crate-internal) entry for `fetch_all` / `first` style
/// terminals: returns a populated accumulator whose SQL is the full
/// set-op SELECT, ready for execution.
pub(crate) fn build_set_op_select<T: Model + FromPgRow>(
    sop: &SetOpQuerySet<T>,
) -> Result<SqlAccumulator, DjogiError> {
    let mut acc = SqlAccumulator::new("");
    build_set_op_select_inner(&mut acc, sop)?;
    Ok(acc)
}

/// Build `SELECT COUNT(*) FROM (<set-op SQL with no LIMIT/OFFSET>) AS sub`.
/// Outer `LIMIT` / `OFFSET` are intentionally **dropped** from the
/// count: they cap the row count the user sees in `fetch_all`, but the
/// authoritative cardinality of the set-op result is what `count`
/// reports. This matches the plain `QuerySet::count` behaviour where
/// `LIMIT`/`OFFSET` are stripped from the count emitter (see
/// [`crate::query::sql::build_count`]).
/// Outer `ORDER BY` is also dropped — it does not affect cardinality
/// and would force a sort the count path never reads.
pub(crate) fn build_set_op_count<T: Model + FromPgRow>(
    sop: &SetOpQuerySet<T>,
) -> Result<SqlAccumulator, DjogiError> {
    // Validate the user-supplied outer ordering before we strip it. The
    // count path drops outer `ORDER BY` for emission, but if the user
    // built the queryset with an invalid expression-form ordering we
    // surface that consistently with `fetch_all` / `first` — same
    // shape, same diagnostic, regardless of which terminal runs first.
    // Otherwise a queryset whose outer ordering is invalid would
    // `.count(...)` cleanly and only error when the caller later
    // `.fetch_all(...)`s, which is a confusing UX.
    validate_outer_ordering::<T>(&sop.ordering)?;

    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (");
    // Clone the set-op shape but strip the outer ORDER BY / LIMIT /
    // OFFSET so the COUNT(*) reflects the full set-op cardinality.
    // The arms keep their own ORDER BY / LIMIT — those are part of the
    // semantic set the caller wants counted (e.g. `(LIMIT 10) UNION
    // (LIMIT 5)` has at most 15 distinct rows; that's the count).
    let stripped = SetOpQuerySet {
        left: sop.left.clone(),
        op: sop.op,
        right: sop.right.clone(),
        ordering: Vec::new(),
        limit: None,
        offset: None,
        _model: PhantomData,
    };
    build_set_op_select_inner(&mut acc, &stripped)?;
    acc.push_sql(") AS sub");
    Ok(acc)
}

// ── Public API: builder methods on QuerySet ─────────────────────────────

impl<T: Model> QuerySet<T> {
    /// Combine this queryset with another via Postgres `UNION`
    /// **de-duplicated** union of the two row sets.
    /// Both arms must share the same `T: Model`, enforced by the type
    /// signature. The returned [`SetOpQuerySet<T>`] is lazy: no SQL is
    /// emitted until a terminal (`fetch_all`, `first`, `count`) is
    /// awaited.
    /// # Semantics
    /// `(LEFT) UNION (RIGHT)` — Postgres de-duplicates by the implicit
    /// full-row tuple. A row that appears in both arms shows up once
    /// in the result. Per-arm `ORDER BY` / `LIMIT` / `OFFSET` apply
    /// inside each parenthesised arm; outer modifiers
    /// ([`SetOpQuerySet::order_by`] / [`limit`](SetOpQuerySet::limit) /
    /// [`offset`](SetOpQuerySet::offset)) apply to the combined
    /// result.
    /// # Restrictions on arms
    /// Either arm with `.prefetch(...)`, `.select_related(...)`,
    /// `.select_for_update(...)`, or `.cache(...)` is rejected at the
    /// terminal with [`DjogiError::SetOpArmInvalid`]. See the module
    /// docs for the rationale.
    /// # Example
    /// ```ignore
    /// use djogi::prelude::*;
    /// let adopted = Dog::objects().filter(|f| f.status().eq(Status::Adopted));
    /// let fostered = Dog::objects().filter(|f| f.status().eq(Status::Fostered));
    /// let rows = adopted.union(fostered).fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn union<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::QuerySet(Box::new(self)),
            op: SetOpKind::Union,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Combine this queryset with another via Postgres `UNION ALL`
    /// **duplicate-preserving** union of the two row sets.
    /// Behaves like [`union`](QuerySet::union) but every row from both
    /// arms appears in the output, including duplicates. Cheaper than
    /// `UNION` when the caller knows the arms are already disjoint
    /// Postgres can skip the de-duplication pass entirely.
    /// # Example
    /// ```ignore
    /// // Recent activity feed — duplicates across arms are meaningful.
    /// let logins = Activity::objects().filter(|f| f.kind().eq(Kind::Login));
    /// let edits = Activity::objects().filter(|f| f.kind().eq(Kind::Edit));
    /// let feed = logins.union_all(edits).order_by(|f| f.created_at().desc()).limit(50);
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn union_all<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::QuerySet(Box::new(self)),
            op: SetOpKind::UnionAll,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Combine this queryset with another via Postgres `INTERSECT`
    /// rows that appear in **both** arms (de-duplicated).
    /// # Semantics
    /// `(LEFT) INTERSECT (RIGHT)` — Postgres returns only rows whose
    /// full-row tuple appears in both `LEFT` and `RIGHT`. The result
    /// is implicitly de-duplicated; `INTERSECT ALL` (multiset
    /// arithmetic) is not exposed today.
    /// # Example
    /// ```ignore
    /// let recent = Post::objects().filter(|f| f.published_at().gt(last_week()));
    /// let popular = Post::objects().filter(|f| f.view_count().gt(1000));
    /// let recent_and_popular = recent.intersect(popular);
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn intersect<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::QuerySet(Box::new(self)),
            op: SetOpKind::Intersect,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Combine this queryset with another via Postgres `EXCEPT`
    /// rows in `self` that are **not** in `other` (de-duplicated).
    /// # Semantics
    /// `(LEFT) EXCEPT (RIGHT)` — set difference. NOT symmetric:
    /// `a.except(b) != b.except(a)`. The result is implicitly
    /// de-duplicated; `EXCEPT ALL` is not exposed today.
    /// # Example
    /// ```ignore
    /// // Users who own a vehicle but have never logged in via SSO.
    /// let with_vehicle = User::objects().filter(|f| f.vehicle_count().gt(0));
    /// let sso_users = User::objects().filter(|f| f.auth_provider().eq("sso"));
    /// let drivers_without_sso = with_vehicle.except(sso_users);
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn except<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::QuerySet(Box::new(self)),
            op: SetOpKind::Except,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }
}

// ── Public API: methods on SetOpQuerySet ────────────────────────────────

impl<T: Model> SetOpQuerySet<T> {
    /// Read-only access to the operator. Useful for tests asserting
    /// SQL structure, and for downstream tooling that needs to know
    /// which set op was chosen without re-emitting SQL.
    pub fn op(&self) -> SetOpKind {
        self.op
    }

    /// Chain another `UNION` arm onto this set-op result.
    /// `self` becomes the left arm of a fresh `SetOpQuerySet` with
    /// the new arm on the right. Per Postgres semantics, set operators
    /// of the same precedence are left-associative; chaining
    /// `a.union(b).union(c)` evaluates as `(a UNION b) UNION c`.
    /// Unlike the bare-`QuerySet` methods, this also resets the outer
    /// modifiers (`ORDER BY` / `LIMIT` / `OFFSET`) to the default
    /// they belong to the previous combination, not the new one.
    /// Reapply via [`order_by`](Self::order_by) / [`limit`](Self::limit) /
    /// [`offset`](Self::offset) on the returned `SetOpQuerySet`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn union<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::Nested(Box::new(self)),
            op: SetOpKind::Union,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Chain another `UNION ALL` arm onto this set-op result. See
    /// [`SetOpQuerySet::union`] for chaining semantics.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn union_all<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::Nested(Box::new(self)),
            op: SetOpKind::UnionAll,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Chain another `INTERSECT` arm onto this set-op result.
    /// # Postgres precedence note
    /// Postgres binds `INTERSECT` tighter than `UNION` / `EXCEPT`. The
    /// djogi surface is left-associative regardless of operator,
    /// so `a.union(b).intersect(c)` produces `(a UNION b) INTERSECT
    /// c`. If you want `a UNION (b INTERSECT c)`, build the inner
    /// `b.intersect(c)` first and then pass it as the right arm of
    /// `a.union(...)`.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn intersect<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::Nested(Box::new(self)),
            op: SetOpKind::Intersect,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Chain another `EXCEPT` arm onto this set-op result. See
    /// [`SetOpQuerySet::union`] for chaining semantics.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn except<A: IntoSetOpArm<T>>(self, other: A) -> SetOpQuerySet<T> {
        SetOpQuerySet {
            left: SetOpArm::Nested(Box::new(self)),
            op: SetOpKind::Except,
            right: other.into_set_op_arm(),
            ordering: Vec::new(),
            limit: None,
            offset: None,
            _model: PhantomData,
        }
    }

    /// Append one or more outer `ORDER BY` expressions to the combined
    /// set-op result.
    /// Outer ordering applies **after** the set operator, to the
    /// merged row set. Per-arm orderings (set on each `QuerySet`
    /// before passing it as an arm) still apply inside each
    /// parenthesised arm; both can coexist. Postgres binds outer
    /// ORDER BY columns to the combined projection — i.e. it
    /// references the column names from `T`'s canonical column list.
    /// Like [`QuerySet::order_by`], later calls **append** to the
    /// existing ordering rather than replacing it.
    /// # Example
    /// ```ignore
    /// let recent = Dog::objects().filter(|f| f.status().eq(Status::Adopted));
    /// let waitlist = Dog::objects().filter(|f| f.status().eq(Status::Fostered));
    /// let rows = recent
    ///     .union(waitlist)
    ///     .order_by(|f| f.name().asc())
    ///     .limit(20)
    ///     .fetch_all(&mut ctx).await?;
    /// ```
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn order_by<F, O>(mut self, f: F) -> Self
    where
        F: FnOnce(T::Fields) -> O,
        O: Into<Vec<OrderExpr>>,
    {
        let exprs: Vec<OrderExpr> = f(T::Fields::default()).into();
        self.ordering.extend(exprs);
        self
    }

    /// Apply outer `LIMIT n` to the combined set-op result. Replaces
    /// any prior outer limit.
    /// Per-arm `.limit(...)` calls (set on each `QuerySet` before
    /// passing it as an arm) are independent — they cap each arm's
    /// pre-merge row count. The outer limit caps the post-merge total.
    /// Takes `u64` at the API boundary so negative values are not
    /// representable. Stored internally as `Option<i64>` to match
    /// `tokio_postgres`'s `BIGINT` bind type.
    /// # Panics
    /// Panics if `n > i64::MAX`. Postgres's `LIMIT` bind type is
    /// `BIGINT`, so values above `i64::MAX` cannot round-trip; such a
    /// value is a programming error, not a runtime condition. The check
    /// uses `i64::try_from` (not `debug_assert!`) so release builds also
    /// panic rather than silently truncate.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn limit(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("SetOpQuerySet::limit(n = {n}) overflows i64"));
        self.limit = Some(n);
        self
    }

    /// Apply outer `OFFSET n` to the combined set-op result. Replaces
    /// any prior outer offset.
    /// Takes `u64` for the same reason as
    /// [`SetOpQuerySet::limit`] — negative offsets are meaningless
    /// and impossible to construct.
    /// # Panics
    /// Panics if `n > i64::MAX` — see [`SetOpQuerySet::limit`] for the
    /// rationale.
    #[must_use = "querysets are lazy — dropping one silently omits the query"]
    pub fn offset(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("SetOpQuerySet::offset(n = {n}) overflows i64"));
        self.offset = Some(n);
        self
    }

    /// Build the set-op SQL this queryset would execute, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    /// Mirrors [`QuerySet::__sql_for_test`] — returns just the SQL
    /// string. Tests that pin the textual SQL contract reach for this
    /// hook instead of `pg_stat_statements` server-side.
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String, DjogiError>
    where
        T: FromPgRow,
    {
        let acc = build_set_op_select(self)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }

    /// Build the `SELECT COUNT(*) FROM (set-op) AS sub` SQL this
    /// queryset would emit for its `.count()` terminal, without
    /// touching a database. **Internal-test plumbing — never call
    /// this from adopter code.**
    /// Returns `(sql, bind_count)` so tests can assert both the SQL
    /// shape and the post-strip bind cardinality (outer `LIMIT` /
    /// `OFFSET` bind slots are removed from the count emission).
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<(String, u32), DjogiError>
    where
        T: FromPgRow,
    {
        let acc = build_set_op_count(self)?;
        let bind_count = acc.bind_count();
        let (sql, _binds) = acc.into_parts();
        Ok((sql, bind_count))
    }
}

// ── Terminals ────────────────────────────────────────────────────────────

impl<T: Model> SetOpQuerySet<T>
where
    T: FromPgRow + Send + Unpin,
{
    /// Execute the set operation and collect every matching row into
    /// a `Vec<T>`.
    /// All four operators (UNION / UNION ALL / INTERSECT / EXCEPT)
    /// flow through the same terminal — the SQL emitter selects the
    /// keyword based on the stored [`SetOpKind`]. Outer
    /// `ORDER BY` / `LIMIT` / `OFFSET` apply to the combined result.
    /// # Empty arm short-circuit
    /// Arms marked [`is_empty`](QuerySet::none) emit `WHERE FALSE` so
    /// the set operator evaluates them as zero-row inputs. No
    /// client-side fold of the algebra happens; the database
    /// computes the semantically correct result either way.
    /// # Errors
    /// Returns [`DjogiError::SetOpArmInvalid`] if either arm carries
    /// a `.prefetch(...)`, `.select_related(...)`, lock, or `.cache(...)`
    /// binding. See the module docs for the rationale.
    /// # Tenant auto-wiring
    /// Tenant GUC propagation fires once before the query runs, keyed
    /// on `T`'s `tenant_key`. Both arms share the same `T`, so a
    /// single tenant scope covers the entire query (no per-arm tenant
    /// set / clear traffic).
    /// # Ordering of validation vs tenant setup
    /// Arm validation (`SetOpArmInvalid`) and outer-ordering validation
    /// (`SetOpOuterOrderingInvalid`) run **inside** `build_set_op_select`,
    /// which is invoked **before** `auto_set_tenant`. This preserves
    /// the "rejection before any DB round trip" contract for both
    /// non-tenant-keyed and tenant-keyed models: an invalid arm cannot
    /// trigger a `SET LOCAL` tenant statement that the caller never
    /// asked for. The cost of the swap is negligible — SQL build is a
    /// pure-CPU traversal of an already-built condition tree.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // Build + validate first so invalid-arm / invalid-ordering
            // queries error out cleanly without issuing the tenant GUC
            // `SET LOCAL` statement that `auto_set_tenant` may run.
            // Tenant setup is reserved for queries that will actually
            // execute against the database.
            let acc = build_set_op_select(&self)?;
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            let result: Vec<T> = rows
                .iter()
                .map(|r| T::from_pg_row(r))
                .collect::<Result<Vec<T>, _>>()?;
            Ok(result)
        }
    }

    /// Execute the set operation with `LIMIT 1` (overriding any outer
    /// limit) and return the first matching row, or `None` if the
    /// combined result is empty.
    /// Outer `ORDER BY` controls which row is chosen. Without an outer
    /// `ORDER BY`, Postgres returns an unspecified row — pair this
    /// terminal with [`order_by`](Self::order_by) for a deterministic
    /// pick.
    /// Per-arm `.order_by(...)` / `.limit(...)` still apply inside
    /// each arm; the outer `LIMIT 1` caps the post-merge count.
    /// # Ordering of validation vs tenant setup
    /// Same contract as [`fetch_all`](Self::fetch_all) — arm /
    /// outer-ordering validation runs through `build_set_op_select`
    /// before `auto_set_tenant`, so an invalid set-op never issues a
    /// tenant GUC `SET LOCAL` statement.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<T>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            let mut sop = self;
            // Outer LIMIT is the right knob — Postgres still evaluates
            // both arms but stops emitting after one combined row.
            sop.limit = Some(1);
            // Build + validate first; only then propagate tenant GUC
            // (see `fetch_all` for the rationale).
            let acc = build_set_op_select(&sop)?;
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let opt = ctx.query_opt(&sql, &params).await?;
            opt.as_ref().map(|r| T::from_pg_row(r)).transpose()
        }
    }
}

impl<T: Model> SetOpQuerySet<T>
where
    T: FromPgRow,
{
    /// `SELECT COUNT(*) FROM (<set-op SQL>) AS sub`.
    /// Counts the **combined** set-op result, not either individual
    /// arm. Outer `ORDER BY` / `LIMIT` / `OFFSET` are stripped from the
    /// emitted count SQL (they do not change cardinality and forcing a
    /// sort would slow the query). Per-arm `LIMIT` / `ORDER BY` are
    /// preserved because they constrain each arm's pre-merge row set,
    /// which is part of what the caller wants counted.
    /// Returns `i64` to match Postgres `COUNT(*)`'s `BIGINT` return
    /// type and to leave headroom for huge tables.
    /// # Ordering of validation vs tenant setup
    /// Same contract as [`fetch_all`](Self::fetch_all) — arm /
    /// outer-ordering validation runs through `build_set_op_count`
    /// before `auto_set_tenant`, so an invalid set-op never issues a
    /// tenant GUC `SET LOCAL` statement. Outer ordering is validated
    /// even though `count` strips it for emission: the user's queryset
    /// shape is the same one they may have passed to `fetch_all`, and
    /// a queryset that errors on `fetch_all` should error on `count`
    /// too — otherwise the same value behaves differently at
    /// different terminals.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
    {
        async move {
            // Build + validate first; only then propagate tenant GUC
            // (see `fetch_all` for the rationale).
            let acc = build_set_op_count(&self)?;
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let n: i64 = crate::pg::decode::try_get_scalar(&row, 0)?;
            Ok(n)
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl<T> SetOpQuerySet<T>
where
    T: Model + FromPgRow,
{
    /// Render the full set-op SQL string for test assertions
    /// `testing`-feature mirror of the hidden `__sql_for_test` hook,
    /// matching [`QuerySet::render_select_sql_for_testing`].
    pub fn render_set_op_sql_for_testing(&self) -> Result<String, DjogiError> {
        let acc = build_set_op_select(self)?;
        let (sql, _binds) = acc.into_parts();
        Ok(sql)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the set-op surface that do not need the full
    //! `#[djogi_test]` harness — keyword rendering, classification of
    //! [`DjogiError::SetOpArmInvalid`], and the variant-coverage
    //! check that every [`SetOpKind`] arm produces a non-empty SQL
    //! keyword.

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::DjogiError;

    // Minimal DB-free `Model` impl for set-op builder panic tests. Mirrors
    // the `Fake` model in `queryset`/`sql` unit tests so these stay
    // independent of `#[model]` macro expansion. Only the methods the
    // builder path touches are reachable; the rest are `unreachable!`.
    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &Self::Pk {
            unreachable!("not called in set-op panic tests")
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!("not called in set-op panic tests")
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: Self::Pk,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    #[test]
    fn set_op_kind_keyword_renders_postgres_tokens() {
        assert_eq!(SetOpKind::Union.keyword(), "UNION");
        assert_eq!(SetOpKind::UnionAll.keyword(), "UNION ALL");
        assert_eq!(SetOpKind::Intersect.keyword(), "INTERSECT");
        assert_eq!(SetOpKind::Except.keyword(), "EXCEPT");
    }

    #[test]
    fn set_op_arm_invalid_is_terminal() {
        // `SetOpArmInvalid` is a programming-error class; retrying
        // the same call cannot turn a `.cache(...)`-bound arm into a
        // cache-free one. Classification falls through the catch-all
        // `_ => false` of `is_transient`, which mirrors the
        // `Validation` / `Decode` / `RelationUnloaded` precedent.
        let err = DjogiError::SetOpArmInvalid {
            table: "phase8_5_c4b_test",
            side: "left",
            reason: "test fixture",
        };
        assert!(err.is_terminal(), "SetOpArmInvalid must be terminal");
        assert!(!err.is_transient(), "SetOpArmInvalid must not be transient");
    }

    #[test]
    fn set_op_arm_invalid_display_names_table_and_side() {
        let err = DjogiError::SetOpArmInvalid {
            table: "phase8_5_c4b_dogs",
            side: "right",
            reason: "arm has a row-level lock",
        };
        let msg = format!("{err}");
        assert!(msg.contains("phase8_5_c4b_dogs"), "{msg}");
        assert!(msg.contains("right"), "{msg}");
        assert!(msg.contains("row-level lock"), "{msg}");
    }

    #[test]
    #[should_panic(expected = "SetOpQuerySet::limit(n = 18446744073709551615) overflows i64")]
    fn set_op_limit_over_i64_max_panics() {
        // u64::MAX is 2^64-1, far above i64::MAX (2^63-1). A LIMIT that
        // large is nonsensical and indicates a programming error; the
        // guard must panic in every build profile, not silently truncate.
        let _ = QuerySet::<Fake>::new()
            .union(QuerySet::<Fake>::new())
            .limit(u64::MAX);
    }

    #[test]
    #[should_panic(expected = "SetOpQuerySet::offset(n = 18446744073709551615) overflows i64")]
    fn set_op_offset_over_i64_max_panics() {
        let _ = QuerySet::<Fake>::new()
            .union(QuerySet::<Fake>::new())
            .offset(u64::MAX);
    }
}
