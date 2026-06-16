//! Typed cross-model set operations (UNION / INTERSECT / EXCEPT) between
//! different `Model` types.
//! # What
//! [`CrossModelSetOpQuerySet<R>`] combines rows from two querysets whose
//! `Model` types differ but whose column shapes are compatible with a common
//! decode target `R: FromPgRow`. The free constructors
//! ([`union_as`], [`union_all_as`], [`intersect_as`], [`except_as`]) accept
//! any arm implementing [`IntoCrossArm<R>`] — today that means
//! [`QuerySet<M>`] and [`VisageQuerySet<V>`].
//! # Why cross-model, not same-model
//! The same-model set ops (`QuerySet::union`, `intersect`, `except`) enforce
//! that both arms share the same `T: Model` at compile time. Cross-model set
//! ops remove that constraint: you can union rows from `LoginEvent` and
//! `ContentEdit` into a single `Vec<Activity>` when both tables expose
//! columns that decode as the `Activity` row type. The decode target `R` is a
//! type parameter on the free constructors (e.g. `union_as::<Activity>(...)`)
//! rather than inferred from the arm models.
//! # Arm types
//! - [`QuerySet<M>`] arms: any model's queryset, subject to the same arm
//!   restrictions as same-model set ops (no prefetch, select_related, lock,
//!   or cache). Validated at SQL-build time; see the [same-module set-op docs](set_op) for details.
//! - [`VisageQuerySet<V>`] arms: visage querysets allow cross-schema
//!   projection — different tables can share a public `DjogiVisage` type, and
//!   the visage's narrowed SELECT projection becomes the arm shape. This is
//!   useful when two backend models project through the same audience-facing
//!   visage (see the [visages guide](../../docs/guide/visages.md)).
//! # Postgres semantics this layer enforces
//! Each arm is parenthesised in the emitted SQL so per-arm `ORDER BY` /
//! `LIMIT` / `OFFSET` stay scoped to that arm. Outer modifiers
//! ([`order_by`](CrossModelSetOpQuerySet::order_by),
//! [`limit`](CrossModelSetOpQuerySet::limit),
//! [`offset`](CrossModelSetOpQuerySet::offset)) apply to the combined result.
//! # Restrictions
//! ## Arm-level state
//! [`DjogiError::SetOpArmInvalid`] surfaces at the terminal level when a
//! `QuerySet` arm carries any of: `.prefetch(...)`, `.select_related(...)`,
//! `.select_for_update(...)` / `.nowait()` / `.skip_locked()`, or
//! `.cache(...)`. Visage arms bypass this check (visages are already narrowed).
//! ## Outer ordering column names
//! [`DjogiError::SetOpOuterOrderingInvalid`] surfaces when an outer
//! `order_by` column is not a valid ASCII identifier or uses the framework-
//! reserved `__djogi_` namespace. Postgres set-operation outer ORDER BY
//! accepts only output column names; this validation catches invalid columns
//! before the SQL round trip.
//! ## Tenant reconciliation
//! Terminal execution (`fetch_all`, `first`, `count`) reconciles arm tenants
//! before issuing any `SET LOCAL`: both tenant-keyed arms must resolve to the
//! same intended tenant, or at most one may carry a concrete tenant. A
//! conflict returns [`DjogiError::CrossModelSetOpTenantConflict`] without
//! modifying the connection's GUC state.
//! # Lazy
//! Nothing hits the database until a terminal (`fetch_all`, `first`, `count`)
//! is awaited. The struct is cheap to construct; arms are boxed behind
//! [`CrossArm<R>`] trait objects so the set-op value stays compact on the
//! stack.
//! # Why RPITIT (not `async fn`)
//! Matches the existing terminal pattern — every terminal returns
//! `impl Future<Output = ...> + Send` rather than bare `async fn`. The
//! explicit `+ Send` bound guarantees the returned future can be `.await`ed
//! across task boundaries. `clippy::manual_async_fn` fires on this pattern;
//! the lint is allowed at the module level because the explicit-bound form is
//! the deliberate choice.

#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::queryset::QuerySet;
use crate::query::set_op::SetOpKind;
use crate::query::visage_queryset::VisageQuerySet;
use crate::visage::DjogiVisage;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;

#[doc(hidden)]
pub trait CrossArm<R: FromPgRow>: Send + Sync {
    fn emit(&self, acc: &mut SqlAccumulator, side: &'static str) -> Result<(), DjogiError>;
    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>);
    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>>;
    /// Column names this arm emits, in SELECT ordinal position.
    /// Used at SQL-build time for shape compatibility validation against decode target `R`.
    fn arm_columns(&self) -> &'static [&'static str];
}

struct QuerySetArm<M, R>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    qs: QuerySet<M>,
    _row: PhantomData<fn() -> R>,
}

impl<M, R> CrossArm<R> for QuerySetArm<M, R>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    fn emit(&self, acc: &mut SqlAccumulator, side: &'static str) -> Result<(), DjogiError> {
        acc.push_sql("(");
        crate::query::set_op::validate_arm::<M>(&self.qs, side)?;
        if self.qs.is_empty() {
            acc.push_sql("SELECT ");
            acc.push_sql(<M as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
            acc.push_sql(M::table_name());
            acc.push_sql(" WHERE FALSE");
        } else {
            let inner = crate::query::sql::build_select(&self.qs)?;
            acc.extend_with(inner);
        }
        acc.push_sql(")");
        Ok(())
    }

    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>) {
        if M::descriptor().tenant_key.is_none() {
            return (std::any::type_name::<M>(), None);
        }
        let intended = ctx.auth().and_then(|a| a.tenant_id.clone());
        (std::any::type_name::<M>(), intended)
    }

    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>> {
        Box::pin(async move { crate::query::terminal::auto_set_tenant::<M>(ctx).await })
    }

    fn arm_columns(&self) -> &'static [&'static str] {
        <M as FromPgRow>::COLUMNS
    }
}

struct VisageArm<V, R>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    qs: VisageQuerySet<V>,
    _row: PhantomData<fn() -> R>,
}

impl<V, R> CrossArm<R> for VisageArm<V, R>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow,
{
    fn emit(&self, acc: &mut SqlAccumulator, _side: &'static str) -> Result<(), DjogiError> {
        acc.push_sql("(");
        let inner = crate::query::visage_queryset::build_visage_select(&self.qs)
            .map_err(DjogiError::from)?;
        acc.extend_with(inner);
        acc.push_sql(")");
        Ok(())
    }

    fn intended_tenant(&self, ctx: &DjogiContext) -> (&'static str, Option<String>) {
        if <V::Model as Model>::descriptor().tenant_key.is_none() {
            return (std::any::type_name::<V::Model>(), None);
        }
        let intended = ctx.auth().and_then(|a| a.tenant_id.clone());
        (std::any::type_name::<V::Model>(), intended)
    }

    fn fire_tenant<'a>(
        &'a self,
        ctx: &'a mut DjogiContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), DjogiError>> + Send + 'a>> {
        Box::pin(async move { crate::query::terminal::auto_set_tenant::<V::Model>(ctx).await })
    }

    fn arm_columns(&self) -> &'static [&'static str] {
        <V as DjogiVisage>::COLUMNS
    }
}

mod sealed {
    pub trait Sealed {}
    impl<M> Sealed for super::QuerySet<M> where
        M: super::Model + super::FromPgRow + Send + Unpin + 'static
    {
    }
    impl<V> Sealed for super::VisageQuerySet<V> where
        V: super::DjogiVisage + super::FromPgRow + Send + Unpin + 'static
    {
    }
}

/// Sealed conversion trait for cross-model set-op arms.
/// # What it accepts
/// - [`QuerySet<M>`] — a plain queryset arm from any `M: Model`.
/// - [`VisageQuerySet<V>`] — a visage queryset arm from any `V: DjogiVisage`.
///   Adopters never name this trait directly; they pass either a `QuerySet` or
///   a `VisageQuerySet` to [`union_as`] / [`intersect_as`] / etc., and the
///   bound is satisfied automatically. The trait is sealed via
///   [`sealed::Sealed`] so no external impl can produce a new arm shape.
pub trait IntoCrossArm<R: FromPgRow>: sealed::Sealed {
    #[doc(hidden)]
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>>;
}

impl<M, R> IntoCrossArm<R> for QuerySet<M>
where
    M: Model + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow + 'static,
{
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>> {
        Box::new(QuerySetArm::<M, R> {
            qs: self,
            _row: PhantomData,
        })
    }
}

impl<V, R> IntoCrossArm<R> for VisageQuerySet<V>
where
    V: DjogiVisage + FromPgRow + Send + Unpin + 'static,
    R: FromPgRow + 'static,
{
    fn into_cross_arm(self) -> Box<dyn CrossArm<R>> {
        Box::new(VisageArm::<V, R> {
            qs: self,
            _row: PhantomData,
        })
    }
}

/// A typed cross-model set operation (`UNION` / `UNION ALL` / `INTERSECT` /
/// `EXCEPT`) between two querysets whose `Model` types may differ.
/// Constructed via [`union_as`] / [`union_all_as`] / [`intersect_as`] /
/// [`except_as`]. Outer `ORDER BY` / `LIMIT` / `OFFSET` are applied to the
/// combined result.
/// # Lazy
/// Nothing hits the database until a terminal (`fetch_all`, `first`, `count`)
/// is awaited. The struct is cheap to construct; arms are boxed behind
/// [`CrossArm<R>`] trait objects so the value stays compact on the stack.
/// # Type parameter
/// The `R: FromPgRow` type parameter is the decode target for all result
/// rows. Both arms must produce columns positionally compatible with `R`, but
/// the arms' own `Model` types can differ. There is no compile-time guarantee
/// that the arm columns match `R`; column count mismatches are validated at
/// SQL-build time and return `DjogiError::CrossModelSetOpColumnMismatch`.
/// Column type mismatches (same count, incompatible OIDs) still surface as
/// Postgres decode errors.
pub struct CrossModelSetOpQuerySet<R: FromPgRow> {
    pub(crate) left: Box<dyn CrossArm<R>>,
    pub(crate) op: SetOpKind,
    pub(crate) right: Box<dyn CrossArm<R>>,
    pub(crate) ordering: Vec<OuterColumnOrder>,
    pub(crate) limit: Option<i64>,
    pub(crate) offset: Option<i64>,
    _row: PhantomData<fn() -> R>,
}

impl<R: FromPgRow> std::fmt::Debug for CrossModelSetOpQuerySet<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossModelSetOpQuerySet")
            .field("op", &self.op)
            .field("ordering", &self.ordering)
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

/// Combine two querysets via Postgres `UNION` (de-duplicated).
/// Both arms can be different `Model` types or visage types; the decode
/// target `R` is specified as a turbofish type parameter.
/// # Semantics
/// `(LEFT) UNION (RIGHT)` — Postgres de-duplicates by the implicit full-row
/// tuple. A row appearing in both arms shows up once in the result. Per-arm
/// `ORDER BY` / `LIMIT` / `OFFSET` apply inside each parenthesised arm; outer
/// modifiers apply to the combined result.
/// # Restrictions on arms
/// `QuerySet` arms with `.prefetch(...)`, `.select_related(...)`,
/// `.select_for_update(...)`, or `.cache(...)` are rejected at the terminal
/// with [`DjogiError::SetOpArmInvalid`]. Visage arms bypass this check.
/// # Example
/// ```ignore
/// use djogi::prelude::*;
/// // Combine login events and content edits into a unified activity feed.
/// let logins = LoginEvent::objects().filter(|f| f.created_at().gt(last_hour()));
/// let edits  = ContentEdit::objects().filter(|f| f.created_at().gt(last_hour()));
/// let activities: Vec<Activity> = union_as::<Activity, _, _>(logins, edits)
///     .order_by("created_at", OuterOrder::Desc)
///     .limit(50)
///     .fetch_all(&mut ctx)
///     .await?;
/// ```
#[must_use = "cross-model set ops are lazy"]
pub fn union_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow,
    A: IntoCrossArm<R>,
    B: IntoCrossArm<R>,
{
    CrossModelSetOpQuerySet {
        left: left.into_cross_arm(),
        op: SetOpKind::Union,
        right: right.into_cross_arm(),
        ordering: Vec::new(),
        limit: None,
        offset: None,
        _row: PhantomData,
    }
}

/// Combine two querysets via Postgres `UNION ALL` (duplicate-preserving).
/// Behaves like [`union_as`] but every row from both arms appears in the
/// output, including duplicates. Cheaper than `UNION` when the caller knows
/// the arms are already disjoint — Postgres can skip the de-duplication pass.
/// # Example
/// ```ignore
/// use djogi::prelude::*;
/// let logins = LoginEvent::objects();
/// let edits  = ContentEdit::objects();
/// let feed: Vec<Activity> = union_all_as::<Activity, _, _>(logins, edits)
///     .fetch_all(&mut ctx)
///     .await?;
/// ```
#[must_use = "cross-model set ops are lazy"]
pub fn union_all_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow,
    A: IntoCrossArm<R>,
    B: IntoCrossArm<R>,
{
    CrossModelSetOpQuerySet {
        left: left.into_cross_arm(),
        op: SetOpKind::UnionAll,
        right: right.into_cross_arm(),
        ordering: Vec::new(),
        limit: None,
        offset: None,
        _row: PhantomData,
    }
}

/// Combine two querysets via Postgres `INTERSECT` (de-duplicated).
/// Returns only rows whose full-row tuple appears in **both** arms.
/// # Semantics
/// `(LEFT) INTERSECT (RIGHT)` — the result is implicitly de-duplicated;
/// `INTERSECT ALL` (multiset arithmetic) is not exposed today.
/// # Example
/// ```ignore
/// use djogi::prelude::*;
/// // Items that appear in both warehouse tables.
/// let wh_a = WarehouseA::objects().filter(|f| f.stocked().eq(true));
/// let wh_b = WarehouseB::objects().filter(|f| f.stocked().eq(true));
/// let in_both: Vec<Inventory> = intersect_as::<Inventory, _, _>(wh_a, wh_b)
///     .fetch_all(&mut ctx)
///     .await?;
/// ```
#[must_use = "cross-model set ops are lazy"]
pub fn intersect_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow,
    A: IntoCrossArm<R>,
    B: IntoCrossArm<R>,
{
    CrossModelSetOpQuerySet {
        left: left.into_cross_arm(),
        op: SetOpKind::Intersect,
        right: right.into_cross_arm(),
        ordering: Vec::new(),
        limit: None,
        offset: None,
        _row: PhantomData,
    }
}

/// Combine two querysets via Postgres `EXCEPT` (de-duplicated).
/// Returns rows in the left arm that are **not** in the right arm.
/// # Semantics
/// `(LEFT) EXCEPT (RIGHT)` — set difference. NOT symmetric:
/// `except_as(a, b) != except_as(b, a)`. The result is implicitly de-
/// duplicated; `EXCEPT ALL` is not exposed today.
/// # Example
/// ```ignore
/// use djogi::prelude::*;
/// // Users who have profiles but have never logged in.
/// let with_profile = UserProfile::objects();
/// let logged_in    = LoginRecord::objects();
/// let no_login: Vec<UserSummary> = except_as::<UserSummary, _, _>(with_profile, logged_in)
///     .fetch_all(&mut ctx)
///     .await?;
/// ```
#[must_use = "cross-model set ops are lazy"]
pub fn except_as<R, A, B>(left: A, right: B) -> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow,
    A: IntoCrossArm<R>,
    B: IntoCrossArm<R>,
{
    CrossModelSetOpQuerySet {
        left: left.into_cross_arm(),
        op: SetOpKind::Except,
        right: right.into_cross_arm(),
        ordering: Vec::new(),
        limit: None,
        offset: None,
        _row: PhantomData,
    }
}

/// Sort direction for outer `ORDER BY` on a [`CrossModelSetOpQuerySet`].
/// Used with [`CrossModelSetOpQuerySet::order_by`] to control whether a
/// column is sorted ascending or descending in the combined result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OuterOrder {
    Asc,
    Desc,
}

impl OuterOrder {
    fn keyword(self) -> &'static str {
        match self {
            OuterOrder::Asc => "ASC",
            OuterOrder::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OuterColumnOrder {
    column: String,
    direction: OuterOrder,
}

impl OuterColumnOrder {
    fn emit(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(&self.column);
        acc.push_sql(" ");
        acc.push_sql(self.direction.keyword());
    }
    fn validate(&self) -> Result<(), DjogiError> {
        if self.column.starts_with("__djogi_") {
            return Err(DjogiError::SetOpOuterOrderingInvalid {
                table: "cross-model set op",
                reason: "outer ORDER BY column names the framework-reserved `__djogi_` namespace",
            });
        }
        let bytes = self.column.as_bytes();
        let ok = !bytes.is_empty()
            && bytes.len() <= 63
            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            && bytes[1..]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if ok {
            Ok(())
        } else {
            Err(DjogiError::SetOpOuterOrderingInvalid {
                table: "cross-model set op",
                reason: "outer ORDER BY column must be an ASCII identifier",
            })
        }
    }
}

impl<R: FromPgRow> CrossModelSetOpQuerySet<R> {
    /// Read-only access to the operator. Useful for tests asserting SQL
    /// structure and for downstream tooling that inspects which set op was
    /// chosen without re-emitting SQL.
    pub fn op(&self) -> SetOpKind {
        self.op
    }

    /// Add an outer `ORDER BY` clause applied to the combined result.
    /// The `column` must be a valid ASCII identifier (letters, digits,
    /// underscore; starting with a letter or underscore). Columns using
    /// the framework-reserved `__djogi_` namespace are rejected with
    /// [`DjogiError::SetOpOuterOrderingInvalid`].
    /// Multiple calls chain additional columns into the ORDER BY list.
    #[must_use = "cross-model set ops are lazy"]
    pub fn order_by(mut self, column: impl Into<String>, direction: OuterOrder) -> Self {
        self.ordering.push(OuterColumnOrder {
            column: column.into(),
            direction,
        });
        self
    }

    /// Set an outer `LIMIT` applied to the combined result.
    /// `None` means no limit; setting this replaces any previous limit.
    /// Panics if `n > i64::MAX` — Postgres bind type for `LIMIT` is `BIGINT`,
    /// so values above `i64::MAX` cannot round-trip. The check uses
    /// `try_from` (not `debug_assert!`) so release builds also panic
    /// rather than silently truncate.
    #[must_use = "cross-model set ops are lazy"]
    pub fn limit(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("CrossModelSetOpQuerySet::limit(n = {n}) overflows i64"));
        self.limit = Some(n);
        self
    }

    /// Set an outer `OFFSET` applied to the combined result.
    /// `None` means no offset; setting this replaces any previous offset.
    /// Panics if `n > i64::MAX` — see [`Self::limit`] for the rationale.
    #[must_use = "cross-model set ops are lazy"]
    pub fn offset(mut self, n: u64) -> Self {
        let n = i64::try_from(n)
            .unwrap_or_else(|_| panic!("CrossModelSetOpQuerySet::offset(n = {n}) overflows i64"));
        self.offset = Some(n);
        self
    }
}

fn build_cross_set_op_select_inner<R: FromPgRow>(
    acc: &mut SqlAccumulator,
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<(), DjogiError> {
    validate_arm_columns(&*sop.left, &*sop.right)?;
    for o in &sop.ordering {
        o.validate()?;
    }
    sop.left.emit(acc, "left")?;
    acc.push_sql(" ");
    acc.push_sql(sop.op.keyword());
    acc.push_sql(" ");
    sop.right.emit(acc, "right")?;
    if !sop.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in sop.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(acc);
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

pub(crate) fn build_cross_set_op_select<R: FromPgRow>(
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<SqlAccumulator, DjogiError> {
    let mut acc = SqlAccumulator::new("");
    build_cross_set_op_select_inner(&mut acc, sop)?;
    Ok(acc)
}

pub(crate) fn build_cross_set_op_count<R: FromPgRow>(
    sop: &CrossModelSetOpQuerySet<R>,
) -> Result<SqlAccumulator, DjogiError> {
    validate_arm_columns(&*sop.left, &*sop.right)?;
    for o in &sop.ordering {
        o.validate()?;
    }
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (");
    sop.left.emit(&mut acc, "left")?;
    acc.push_sql(" ");
    acc.push_sql(sop.op.keyword());
    acc.push_sql(" ");
    sop.right.emit(&mut acc, "right")?;
    acc.push_sql(") AS sub");
    Ok(acc)
}

/// Decide whether two arm-models' **intended** tenant scopes are
/// compatible. Compatible when at most one arm carries a concrete intended
/// tenant, or both carry the same one. Incompatible (two *different*
/// intended tenants) surfaces [`DjogiError::CrossModelSetOpTenantConflict`].
/// Each tuple is `(model_type_name, intended_tenant)` as produced by an
/// arm's [`CrossArm::intended_tenant`] **pure** read. Reconciliation runs
/// against these intent values BEFORE any `SET LOCAL` is issued, so the
/// conflict path returns with the connection's GUC unchanged — this is the
/// fix for the GUC-poisoning defect where firing both arms' tenant wiring
/// first would overwrite the GUC before the conflict was detected.
fn reconcile_arm_tenants(
    left: (&'static str, Option<String>),
    right: (&'static str, Option<String>),
) -> Result<(), DjogiError> {
    if let (Some(lt), Some(rt)) = (&left.1, &right.1)
        && lt != rt
    {
        return Err(DjogiError::CrossModelSetOpTenantConflict {
            left_model: left.0,
            right_model: right.0,
            left_tenant: lt.clone(),
            right_tenant: rt.clone(),
        });
    }
    Ok(())
}

/// Validate that each arm's column count matches the decode target `R`.
/// Postgres compares set-op arms positionally, so a mismatch produces
/// a silent misdecode in release mode. This check catches it at
/// SQL-build time before any tenant wiring or SQL emission.
fn validate_arm_columns<R: FromPgRow>(
    left: &dyn CrossArm<R>,
    right: &dyn CrossArm<R>,
) -> Result<(), DjogiError> {
    let target = R::COLUMNS;

    let l_cols = left.arm_columns();
    if l_cols.len() != target.len() {
        return Err(DjogiError::CrossModelSetOpColumnMismatch {
            side: "left",
            arm_columns: l_cols.into(),
            expected_columns: target.into(),
        });
    }

    let r_cols = right.arm_columns();
    if r_cols.len() != target.len() {
        return Err(DjogiError::CrossModelSetOpColumnMismatch {
            side: "right",
            arm_columns: r_cols.into(),
            expected_columns: target.into(),
        });
    }

    Ok(())
}

// ── Terminals: fetch_all / first (Send + Unpin) ──────────────────────────

impl<R> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow + Send + Unpin,
{
    /// Execute the cross-model set operation and collect every row into a `Vec<R>`.
    /// # Errors
    /// - [`DjogiError::SetOpArmInvalid`] if either arm carries prefetch/lock/cache bindings.
    /// - [`DjogiError::SetOpOuterOrderingInvalid`] if outer ORDER BY column is invalid.
    /// - [`DjogiError::CrossModelSetOpTenantConflict`] if arms resolve to different tenants.
    /// - [`DjogiError::CrossModelSetOpColumnMismatch`] if an arm's column count differs from `R`.
    /// - Postgres decode error if arm columns have the same count but incompatible types.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<R>, DjogiError>> + Send + 'ctx
    where
        R: 'ctx,
    {
        async move {
            let acc = build_cross_set_op_select(&self)?;
            let left_intended = self.left.intended_tenant(ctx);
            let right_intended = self.right.intended_tenant(ctx);
            reconcile_arm_tenants(left_intended, right_intended)?;
            self.left.fire_tenant(ctx).await?;
            self.right.fire_tenant(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            rows.iter().map(|r| R::from_pg_row(r)).collect()
        }
    }

    /// Execute with `LIMIT 1` and return the first row, or `None`.
    /// Same validation / tenant ordering as [`fetch_all`](Self::fetch_all).
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<R>, DjogiError>> + Send + 'ctx
    where
        R: 'ctx,
    {
        async move {
            let mut sop = self;
            sop.limit = Some(1);
            let acc = build_cross_set_op_select(&sop)?;
            let left_intended = sop.left.intended_tenant(ctx);
            let right_intended = sop.right.intended_tenant(ctx);
            reconcile_arm_tenants(left_intended, right_intended)?;
            sop.left.fire_tenant(ctx).await?;
            sop.right.fire_tenant(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let opt = ctx.query_opt(&sql, &params).await?;
            opt.as_ref().map(|r| R::from_pg_row(r)).transpose()
        }
    }
}

// ── Terminal: count (no Send + Unpin bound) ──────────────────────────────

impl<R> CrossModelSetOpQuerySet<R>
where
    R: FromPgRow,
{
    /// `SELECT COUNT(*) FROM (<cross-model set op>) AS sub`.
    /// Outer ORDER BY / LIMIT / OFFSET stripped from emission; per-arm state preserved.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        R: 'ctx,
    {
        async move {
            let acc = build_cross_set_op_count(&self)?;
            let left_intended = self.left.intended_tenant(ctx);
            let right_intended = self.right.intended_tenant(ctx);
            reconcile_arm_tenants(left_intended, right_intended)?;
            self.left.fire_tenant(ctx).await?;
            self.right.fire_tenant(ctx).await?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let n: i64 = crate::pg::decode::try_get_scalar(&row, 0)?;
            Ok(n)
        }
    }

    /// Render the full cross-model set-op SQL for test assertions.
    /// **Internal-test plumbing — never call from adopter code.**
    #[doc(hidden)]
    pub fn __sql_for_test(&self) -> Result<String, DjogiError> {
        let acc = build_cross_set_op_select(self)?;
        Ok(acc.into_parts().0)
    }

    /// Render the count SQL + post-strip bind count for test assertions.
    /// **Internal-test plumbing — never call from adopter code.**
    #[doc(hidden)]
    pub fn __count_sql_for_test(&self) -> Result<(String, u32), DjogiError> {
        let acc = build_cross_set_op_count(self)?;
        let bind_count = acc.bind_count();
        Ok((acc.into_parts().0, bind_count))
    }
}

// ── Testing-feature mirrors ───────────────────────────────────────────────

#[cfg(any(test, feature = "testing"))]
impl<R: FromPgRow> CrossModelSetOpQuerySet<R> {
    /// `testing`-feature mirror of [`__sql_for_test`](Self::__sql_for_test).
    pub fn render_cross_set_op_sql_for_testing(&self) -> Result<String, DjogiError> {
        let acc = build_cross_set_op_select(self)?;
        Ok(acc.into_parts().0)
    }

    /// `testing`-feature mirror of [`__count_sql_for_test`](Self::__count_sql_for_test).
    pub fn render_cross_count_sql_for_testing(&self) -> Result<(String, u32), DjogiError> {
        let acc = build_cross_set_op_count(self)?;
        let bind_count = acc.bind_count();
        Ok((acc.into_parts().0, bind_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::model(table = "x462_left_widgets", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct LeftWidget {
        name: String,
    }

    #[crate::model(table = "x462_right_gadgets", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct RightGadget {
        name: String,
    }

    #[crate::model(table = "x462_combined_rows", pk = crate::HeerId)]
    #[derive(Debug, Clone)]
    pub struct CombinedRow {
        name: String,
    }

    #[crate::model(table = "x462_tenant_widgets", pk = crate::HeerId, tenant_key = "org_id")]
    #[derive(Debug, Clone)]
    pub struct TenantWidget {
        org_id: String,
        name: String,
    }

    #[test]
    fn cross_union_emits_parenthesised_arms_with_keyword_and_renumbered_binds() {
        let left = LeftWidget::objects().filter(|f| f.name().eq("a".to_string()));
        let right = RightGadget::objects().filter(|f| f.name().eq("b".to_string()));
        let sop = union_as::<CombinedRow, _, _>(left, right);

        let acc = build_cross_set_op_select(&sop).unwrap();
        let sql = acc.sql();
        assert!(sql.contains("UNION"), "{sql}");
        assert!(sql.contains("$1"), "left arm bind: {sql}");
        assert!(sql.contains("$2"), "right arm bind renumbered: {sql}");
        assert!(sql.contains("x462_left_widgets"), "{sql}");
        assert!(sql.contains("x462_right_gadgets"), "{sql}");
    }

    #[test]
    fn cross_count_wraps_in_subquery_and_strips_outer_modifiers() {
        let left = LeftWidget::objects();
        let right = RightGadget::objects();
        let sop = union_as::<CombinedRow, _, _>(left, right)
            .limit(5)
            .offset(2);

        let acc = build_cross_set_op_count(&sop).unwrap();
        let sql = acc.sql();
        assert!(sql.starts_with("SELECT COUNT(*) FROM ("), "{sql}");
        assert!(sql.trim_end().ends_with(") AS sub"), "{sql}");
        assert!(
            !sql.contains("LIMIT"),
            "count must strip outer LIMIT: {sql}"
        );
        assert!(
            !sql.contains("OFFSET"),
            "count must strip outer OFFSET: {sql}"
        );
    }

    #[test]
    fn cross_outer_order_by_limit_offset_emit_after_arms() {
        let left = LeftWidget::objects();
        let right = RightGadget::objects();
        let sop = union_as::<CombinedRow, _, _>(left, right)
            .order_by("name", OuterOrder::Asc)
            .order_by("created_at", OuterOrder::Desc)
            .limit(10)
            .offset(3);

        let acc = build_cross_set_op_select(&sop).unwrap();
        let sql = acc.sql();
        let order_idx = sql.find("ORDER BY").expect("order by present");
        let union_idx = sql.find("UNION").expect("union present");
        assert!(
            order_idx > union_idx,
            "ORDER BY must follow the operator: {sql}"
        );
        assert!(sql.contains("ORDER BY name ASC, created_at DESC"), "{sql}");
        assert!(sql.contains("LIMIT"), "{sql}");
        assert!(sql.contains("OFFSET"), "{sql}");
    }

    #[test]
    fn cross_order_by_rejects_non_identifier_column() {
        let sop = union_as::<CombinedRow, _, _>(LeftWidget::objects(), RightGadget::objects())
            .order_by("name; DROP TABLE x", OuterOrder::Asc);
        let err = match build_cross_set_op_select(&sop) {
            Ok(_) => panic!("expected error for non-identifier column"),
            Err(e) => e,
        };
        assert!(
            matches!(err, DjogiError::SetOpOuterOrderingInvalid { .. }),
            "non-identifier outer order column must be rejected: {err:?}"
        );
    }

    #[test]
    fn cross_order_by_rejects_framework_reserved_column() {
        let sop = union_as::<CombinedRow, _, _>(LeftWidget::objects(), RightGadget::objects())
            .order_by("__djogi_tenant_id", OuterOrder::Asc);
        let err = match build_cross_set_op_select(&sop) {
            Ok(_) => panic!("expected error for reserved column"),
            Err(e) => e,
        };
        assert!(
            matches!(err, DjogiError::SetOpOuterOrderingInvalid { reason, .. }
                if reason.contains("__djogi_")),
            "framework-reserved outer order column must be rejected: {err:?}"
        );
    }

    #[test]
    fn tenant_conflict_detected_when_arms_resolve_to_different_tenants() {
        let res = reconcile_arm_tenants(
            ("LeftWidget", Some("org_a".to_string())),
            ("RightGadget", Some("org_b".to_string())),
        );
        assert!(matches!(
            res.unwrap_err(),
            DjogiError::CrossModelSetOpTenantConflict { .. }
        ),);
    }

    #[test]
    fn tenant_no_conflict_when_one_arm_untenanted() {
        assert!(
            reconcile_arm_tenants(
                ("LeftWidget", Some("org_a".to_string())),
                ("RightGadget", None)
            )
            .is_ok()
        );
        assert!(reconcile_arm_tenants(("LeftWidget", None), ("RightGadget", None)).is_ok());
        assert!(
            reconcile_arm_tenants(
                ("LeftWidget", Some("org_a".to_string())),
                ("RightGadget", Some("org_a".to_string()))
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn untenanted_arm_intent_is_none_despite_stale_applied_tenant() {
        use crate::auth::AuthContext;

        let pool = crate::pg::pool::DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(1)
            .build()
            .await
            .expect("pool build should not connect until checkout");
        let mut ctx = crate::context::DjogiContext::from_pool(pool);

        ctx.set_auth(AuthContext::new(crate::HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
        ctx.applied_tenant_id = Some("org_stale".to_string());

        let arm = IntoCrossArm::<CombinedRow>::into_cross_arm(LeftWidget::objects());
        let (model_name, intent) = arm.intended_tenant(&ctx);

        assert_eq!(
            intent, None,
            "untenanted arm must report None regardless of auth or stale applied_tenant_id: got {intent:?} for {model_name}"
        );
    }

    #[tokio::test]
    async fn tenant_keyed_arm_intent_is_auth_tenant_not_stale_applied() {
        use crate::auth::AuthContext;

        let pool = crate::pg::pool::DjogiPool::builder("postgres://localhost/_djogi_unreachable")
            .max_size(1)
            .build()
            .await
            .expect("pool build should not connect until checkout");
        let mut ctx = crate::context::DjogiContext::from_pool(pool);
        ctx.set_auth(AuthContext::new(crate::HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
        ctx.applied_tenant_id = Some("org_stale".to_string());

        let arm = IntoCrossArm::<CombinedRow>::into_cross_arm(TenantWidget::objects());
        let (_model_name, intent) = arm.intended_tenant(&ctx);

        assert_eq!(
            intent,
            Some("org_a".to_string()),
            "tenant-keyed arm must report AUTH tenant (org_a), not stale applied_tenant_id"
        );
    }
}
