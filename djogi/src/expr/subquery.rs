//! Correlated subqueries, EXISTS predicates, and typed outer-scope
//! column references — the remaining expression-IR surface the plan's
//! Task 5 brief calls out.
//!
//! # What this module ships
//!
//! Three typed wrappers, one macro seal:
//!
//! - [`Subquery<T, V>`] — `(SELECT <col> FROM T::table_name() WHERE ...)`
//!   as a scalar [`Expr<V>`]. Built from a [`QuerySet<T>`] plus a
//!   [`FieldRef<T, V>`] picking the column to project.
//! - [`Exists`] — `EXISTS (SELECT 1 FROM T::table_name() WHERE ...)` as
//!   an [`Expr<bool>`]. Built from a [`QuerySet<T>`] alone; the emitter
//!   always renders `SELECT 1` because EXISTS only cares about row
//!   presence.
//! - [`OuterRef<M, V>`] — correlated reference to a column in the
//!   enclosing query scope. Shape mirrors [`FieldRef<M, V>`] so the
//!   compiler catches value-type mismatches at `eq` / `lt` / arithmetic
//!   composition sites.
//! - [`__macro_support::__make_outer_ref`] — the sealed macro entry
//!   point `{Model}OuterRef::column()` accessors route through (same
//!   identifier-validation pattern as
//!   [`crate::query::field::__macro_support::__make_field_ref`]).
//!
//! # Design: store `Condition`, not a lowered `ExprNode` tree
//!
//! The subquery's `WHERE` predicate is the parent queryset's
//! accumulated [`Condition`] tree, cloned in at `Subquery::new` /
//! `Exists::new` construction time and carried verbatim on the
//! [`crate::expr::node::SubqueryNode`] payload. Emission reuses the
//! shipped [`crate::query::sql::emit_condition`] walk.
//!
//! **Why not lower to `ExprNode`?** The condition tree carries a full
//! [`crate::query::condition::LookupOp`] vocabulary — ILIKE, BETWEEN,
//! IN, IS NULL, regex — every one of which already has a matching
//! emitter arm. Lowering into `ExprNode` would mean duplicating every
//! op arm on the expression side, which in turn would drift over time
//! as [`LookupOp`](crate::query::condition::LookupOp) grows new
//! variants. Storing the `Condition` directly lets `emit_condition` stay
//! the single source of truth for every filter-side lookup, inside or
//! outside a subquery. The tradeoff — one structural `Clone` at
//! subquery-build time — is negligible compared to the duplicated-emitter
//! maintenance burden.
//!
//! # Why typed `OuterRef<M, V>` (plan Q12 = B)
//!
//! Bare `&'static str` would let the user write
//! `Expr::outer_ref_raw("id").as_expr().eq(other_col.as_expr())` with no
//! check that "id" actually exists on the outer-scope model or that the
//! value type matches. The typed shape makes the compiler catch both:
//! `OuterRef<Account, HeerId>::id()` is constructive evidence that
//! column "id" exists on `Account` as `HeerId`, and `.as_expr()` returns
//! an `Expr<HeerId>` that can only `.eq` another `Expr<HeerId>`.
//!
//! # Macro integration
//!
//! `#[derive(Model)]` emits a `{Model}OuterRef` ZST with one accessor per
//! column, mirroring the `{Model}Fields` pattern. The accessors route
//! through [`__macro_support::__make_outer_ref`] so the column string
//! gets the same validation as [`FieldRef::new`].
//!
//! # Column qualification
//!
//! `OuterRef` exposes two emission modes:
//!
//! - [`OuterRef::as_expr`] — emits the column name **unqualified**. Postgres
//!   resolves an unqualified name against the enclosing query scope when
//!   the inner `FROM` list has no matching column, which covers the common
//!   case. When both tables expose a same-named column (every Djogi model
//!   has `id`, `created_at`, `updated_at`, so this is real), the emission
//!   is ambiguous and Postgres raises `42702`. Use this form when you've
//!   verified the inner / outer scopes share no column names.
//! - [`OuterRef::as_qualified_expr`] — emits `<M::table_name()>.<column>`
//!   so the reference disambiguates to the outer scope unconditionally.
//!   Phase 7-Zero-2 T13b's macro-emitted M2M `EXISTS` predicates use this
//!   form because the through-table and target-table always collide on
//!   framework-column names. Adopters writing correlated subqueries by
//!   hand should reach for this whenever the inner / outer column-name
//!   sets are not provably disjoint.
//!
//! Generalised `parent_table` threading for `select_related + filter_expr`
//! composition (where the outer FROM may be aliased rather than literal
//! `M::table_name()`) is the next step on this surface and remains a
//! Phase 8+ enhancement. See `docs/roadmap/future-work.md` §4.7.

use crate::expr::Expr;
use crate::expr::node::{ExprNode, SubqueryNode};
use crate::model::Model;
use crate::query::field::FieldRef;
use crate::query::queryset::QuerySet;
use std::marker::PhantomData;

// ── Subquery<T, V> ────────────────────────────────────────────────────
//
// Scalar subquery — (SELECT <col> FROM T::table_name() WHERE ...).
// The SQL is parenthesised at emission time so the subquery can appear
// in any `Expr<V>` position.

/// Typed scalar subquery — lowers to `(SELECT <col> FROM <table>
/// WHERE <cond>)` when emitted.
///
/// `T` pins the source model (so the emitter knows which table to
/// SELECT from) and `V` pins the column's Rust type (so the scalar
/// returned by the subquery slots into `Expr<V>` positions without an
/// explicit cast). Both are phantom-typed; the node itself carries only
/// the untyped [`SubqueryNode`] payload.
///
/// `#[must_use]` — a dropped `Subquery` is usually a mistake; the user
/// likely meant to feed it to `.as_expr()` and thence to
/// [`crate::query::QuerySet::filter_expr`] / an arithmetic composition.
///
/// Construction: [`Subquery::new`] takes the source queryset and the
/// projected column. No other public constructor — the `SubqueryNode`
/// inside is crate-private so downstream code cannot fabricate a
/// subquery with a mismatched column / type pair.
#[must_use = "subqueries are lazy — drop one and the predicate is silently omitted"]
pub struct Subquery<T: Model, V> {
    // Untyped payload — the emitter walks this directly. Typed `T`/`V`
    // live only in the phantom markers.
    node: SubqueryNode,
    // Covariant phantom tags — mirror the pattern on
    // [`crate::expr::Expr`] / [`FieldRef`] so the wrapper is
    // `Send + Sync` regardless of `T`/`V`'s own markers.
    _phantom: PhantomData<fn() -> (T, V)>,
}

impl<T: Model, V> Subquery<T, V> {
    /// Build a scalar subquery from a source queryset and the column to
    /// project.
    ///
    /// The `qs` parameter supplies the `FROM <table>` source and the
    /// correlated `WHERE` predicate; the `column` parameter picks which
    /// scalar the subquery returns. `T` on both arguments is the same
    /// model, so the compiler rejects a column ref taken from a
    /// different model's `Fields` handle.
    ///
    /// # How the `WHERE` clause flows
    ///
    /// `qs.condition` is cloned verbatim into the
    /// [`SubqueryNode::where_clause`] slot — every Phase 2 lookup op the
    /// caller composed via `filter` / `filter_expr` is preserved. The
    /// clone is structural (the condition tree is shallow `Vec` / `Box`
    /// / enum variants); typical correlated-subquery call sites build
    /// the queryset fresh, so there is nothing to share-ownership with.
    ///
    /// `Condition::True` (the "no filter" identity) is stored as
    /// `Some(True)` and the emitter renders it as `WHERE TRUE`. This
    /// matches the existing `push_where` shim's behaviour — a vacuous
    /// predicate is rare in correlated subqueries, so the emission
    /// overhead is not worth a special case here.
    ///
    /// # What this ignores about `qs`
    ///
    /// A queryset carries ordering, limit/offset, distinct mode, and
    /// prefetch / select_related registrations. The subquery emitter
    /// consumes only the table name and condition tree; ordering and
    /// pagination are ignored because Postgres' scalar-subquery context
    /// does not use them (a scalar subquery must return a single value;
    /// `ORDER BY ... LIMIT 1` would be the usual way to force that but
    /// is out of scope for Task 5). Callers who need a deterministic
    /// scalar from a multi-row source should use `ctx.raw_scalar`
    /// until Phase 5 extends this surface.
    pub fn new(qs: QuerySet<T>, column: FieldRef<T, V>) -> Self {
        Subquery {
            node: SubqueryNode {
                table: T::table_name(),
                select_column: Some(column.column()),
                // Cluster 8γ Stage 2 (T6.9): `qs.condition` is `Q<T>`
                // post-flip. The subquery WHERE clause still consumes
                // `Condition`, so lower through the bridge before
                // handing off. SQL parity: identity round-trip on
                // `Q::Condition(_)`, byte-identical Condition tree
                // shape on every other variant.
                where_clause: condition_to_opt(crate::query::q::q_to_condition(qs.condition)),
            },
            _phantom: PhantomData,
        }
    }

    /// Promote this subquery to a typed [`Expr<V>`] for use in
    /// `filter_expr`, `set_expr`, or any other expression-IR consumer.
    ///
    /// The phantom `V` on `Subquery<T, V>` projects directly onto the
    /// resulting `Expr<V>` — so a subquery that selects a `HeerId`
    /// column produces an `Expr<HeerId>` that can `.eq` another
    /// `Expr<HeerId>`, and nothing else. Type discipline all the way
    /// through.
    pub fn as_expr(self) -> Expr<V> {
        Expr::from_node(ExprNode::Subquery(Box::new(self.node)))
    }
}

// ── Exists ────────────────────────────────────────────────────────────
//
// EXISTS (SELECT 1 FROM T::table_name() WHERE ...) — boolean predicate.
// No `V` parameter on the typed surface because EXISTS always yields a
// boolean (`Expr<bool>`) regardless of what columns the inner queryset
// selected.

/// Typed `EXISTS (...)` predicate — lowers to `EXISTS (SELECT 1 FROM
/// <table> WHERE <cond>)` when emitted.
///
/// `T` pins the source model at construction time (so the emitter
/// knows which table to SELECT from); the typed surface drops `T`
/// after `.as_expr()` because the `Expr<bool>` result is type-erased
/// (every `Exists` produces the same `Expr<bool>` type regardless of
/// source model).
///
/// `#[must_use]` — a dropped `Exists` is usually a mistake; the user
/// likely meant to feed it to `.as_expr()` and thence to
/// [`crate::query::QuerySet::filter_expr`].
#[must_use = "EXISTS predicates are lazy — drop one and the filter is silently omitted"]
pub struct Exists {
    /// Untyped payload. `select_column` is always `None` for this
    /// variant; the emitter special-cases that arm to render
    /// `SELECT 1`.
    node: SubqueryNode,
}

impl Exists {
    /// Build an `EXISTS (...)` predicate from a source queryset.
    ///
    /// Drops the queryset's `T` parameter at construction because the
    /// emitted SQL only needs the table name + condition tree — every
    /// `Exists` carries an `Expr<bool>` result regardless of the inner
    /// model. Callers who want to express "at least one row matches"
    /// over a correlated predicate use this as the idiomatic shape.
    ///
    /// # Why `select_column = None`
    ///
    /// Postgres' EXISTS only evaluates the subquery to the point of
    /// finding (or failing to find) a single row; the selected columns
    /// are never materialised. `SELECT 1` is the idiomatic stand-in —
    /// it tells the planner "any row will do" without binding a
    /// specific column name that might later be renamed or dropped.
    /// Routing through `select_column = None` at the node level keeps
    /// the rendering decision ("`1` vs a column") inside the emitter,
    /// not the typed builder.
    ///
    /// # What this ignores about `qs`
    ///
    /// Mirrors the [`Subquery::new`] rule: only the table name and
    /// condition tree flow into the emitted SQL. Ordering, limit/offset,
    /// distinct mode, prefetch, and select_related registrations on the
    /// inner queryset are silently dropped. For EXISTS this is almost
    /// never a functional problem — the predicate is satisfied by the
    /// existence of any matching row, so `ORDER BY` and `LIMIT` are
    /// vestigial — but it is worth knowing that `Exists::new(qs.limit(1))`
    /// does not constrain the planner's row search at all. Callers who
    /// need row-bounded EXISTS semantics should use `ctx.raw_execute`
    /// until Phase 5 widens this surface.
    pub fn new<T: Model>(qs: QuerySet<T>) -> Self {
        Exists {
            node: SubqueryNode {
                table: T::table_name(),
                select_column: None,
                // Cluster 8γ Stage 2 (T6.9): `qs.condition` is `Q<T>`
                // post-flip. The subquery WHERE clause still consumes
                // `Condition`, so lower through the bridge before
                // handing off. SQL parity: identity round-trip on
                // `Q::Condition(_)`, byte-identical Condition tree
                // shape on every other variant.
                where_clause: condition_to_opt(crate::query::q::q_to_condition(qs.condition)),
            },
        }
    }

    /// Promote this EXISTS predicate to a typed [`Expr<bool>`] for use
    /// in [`crate::query::QuerySet::filter_expr`] or a nested boolean
    /// composition.
    pub fn as_expr(self) -> Expr<bool> {
        Expr::from_node(ExprNode::Exists(Box::new(self.node)))
    }
}

// ── OuterRef<M, V> ────────────────────────────────────────────────────
//
// Outer-scope column reference — shape mirrors `FieldRef<M, V>` so the
// compiler enforces the same value-type discipline on correlated
// predicates that Phase 2's literal-RHS filters already enjoy.

/// Typed reference to a column in the enclosing query scope — the
/// outer-scope counterpart to [`FieldRef<M, V>`].
///
/// Inside a correlated subquery (the typical site for `OuterRef`), an
/// unqualified column name resolves against the enclosing `FROM`
/// clause when the inner query's `FROM` list has no matching column.
/// `OuterRef` is the typed handle for that reference — it carries the
/// column name plus phantom markers that bind it to a specific model
/// (`M`) and a specific value type (`V`), so `.as_expr()` produces an
/// `Expr<V>` that only composes with same-`V` operands.
///
/// # Limitation: unqualified emission
///
/// `as_expr()` renders the column name without a table qualifier. When
/// the inner and outer scopes both expose a same-named column (every
/// Djogi model has `id`, `created_at`, `updated_at`, so this is real
/// for intra-model correlated subqueries), Postgres raises `42702
/// column reference "X" is ambiguous`. Workarounds:
///
/// - Correlate on tables whose bare column names do not collide.
/// - Use `ctx.raw_execute` / `ctx.raw_scalar` for explicitly-aliased
///   correlations.
///
/// The qualified form (carrying an outer-table alias) is deferred
/// alongside the broader `parent_table` threading needed for
/// `select_related + filter_expr` composition — flagged in the
/// [`crate::expr::sql`] module header and on [`OuterRef::as_expr`].
///
/// # Construction
///
/// Users do not call [`OuterRef::new`] directly. The
/// `#[derive(Model)]` macro emits a `{Model}OuterRef` ZST helper with
/// one accessor per column:
///
/// ```ignore
/// // Emitted by the macro for `struct Account { balance: i64 }`:
/// impl AccountOuterRef {
///     pub fn id() -> OuterRef<Account, HeerId> { /* ... */ }
///     pub fn balance() -> OuterRef<Account, i64> { /* ... */ }
///     pub fn created_at() -> OuterRef<Account, DateTime> { /* ... */ }
///     pub fn updated_at() -> OuterRef<Account, DateTime> { /* ... */ }
/// }
/// ```
///
/// The accessors route through [`__macro_support::__make_outer_ref`]
/// so every column string is validated via
/// [`crate::ident::assert_plain_ident`] at construction time.
pub struct OuterRef<M: Model, V> {
    /// Macro-baked column name — validated by
    /// [`__macro_support::__make_outer_ref`] at construction.
    column: &'static str,
    /// Covariant `M` tag — ties the ref to one model so that mixing
    /// `AccountOuterRef::id()` with a `Post`-correlated subquery is a
    /// compile error, not a runtime SQL error.
    _m: PhantomData<fn() -> M>,
    /// Covariant `V` tag — pins the column's value type so the
    /// produced `Expr<V>` only composes with same-`V` operands.
    _v: PhantomData<fn() -> V>,
}

impl<M: Model, V> Copy for OuterRef<M, V> {}
impl<M: Model, V> Clone for OuterRef<M, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model, V> std::fmt::Debug for OuterRef<M, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OuterRef({})", self.column)
    }
}

impl<M: Model, V> OuterRef<M, V> {
    /// Crate-private constructor. The macro-emitted
    /// `{Model}OuterRef::column()` accessors route through
    /// [`__macro_support::__make_outer_ref`], which validates the
    /// identifier before calling this. Downstream code cannot
    /// fabricate an `OuterRef` with an unvalidated column string —
    /// the seal mirrors [`FieldRef::new`]'s `pub(crate)` visibility.
    ///
    /// `const` so the macro-emitted accessors stay trivially
    /// inlinable — same pattern as [`FieldRef::new`].
    pub(crate) const fn new(column: &'static str) -> Self {
        OuterRef {
            column,
            _m: PhantomData,
            _v: PhantomData,
        }
    }

    /// Promote this outer-scope column ref to a typed [`Expr<V>`] for
    /// use inside a correlated subquery's `filter_expr` closure.
    ///
    /// The emitted SQL is the bare column name — Postgres resolves the
    /// reference against the outer scope when the inner query's `FROM`
    /// list has no matching column. Same-named collisions raise a
    /// runtime error; see the type-level limitation note.
    #[must_use = "OuterRef is inert unless promoted to Expr<V>"]
    pub fn as_expr(self) -> Expr<V> {
        Expr::from_node(ExprNode::OuterRef {
            column: self.column,
        })
    }

    /// Promote this outer-scope column ref to a typed [`Expr<V>`] that
    /// emits as `<M::table_name()>.<column>` instead of the bare column
    /// name.
    ///
    /// Use this whenever the inner subquery and outer scope share a
    /// column name (every Djogi model carries `id` / `created_at` /
    /// `updated_at`, so M2M correlations always collide). The qualified
    /// form lets Postgres resolve the reference unambiguously, sidestepping
    /// the `42702 column reference is ambiguous` failure that the bare
    /// [`Self::as_expr`] form falls into.
    ///
    /// `M::table_name()` is the source of the qualifier — every
    /// `Model::table_name()` is validated by Djogi's identifier rules at
    /// `#[model]` expansion time, so the resulting SQL token is safe to
    /// push without re-validation.
    #[must_use = "OuterRef is inert unless promoted to Expr<V>"]
    pub fn as_qualified_expr(self) -> Expr<V> {
        Expr::from_node(ExprNode::OuterRefColumn {
            table: M::table_name(),
            column: self.column,
        })
    }
}

// ── Condition → Option<Condition> helper ──────────────────────────────
//
// `Condition::True` is the accumulator's identity value; storing it
// verbatim on a `SubqueryNode.where_clause` slot would force the
// emitter to render `WHERE TRUE` on every filter-less subquery. Stripping
// the identity here keeps the emitted SQL clean without a special case
// in the emitter itself.

/// Collapse `Condition::True` (the accumulator identity) into `None` so
/// the emitter skips the `WHERE` clause entirely on a filter-less
/// queryset. Non-trivial conditions pass through unchanged.
///
/// Why the collapse: the subquery emitter conditionally emits `WHERE
/// <cond>` based on `Option::is_some`. `Condition::True` would otherwise
/// render as `WHERE TRUE`, which is a semantic no-op but visually noisy
/// in query logs and wastes a planner cycle evaluating the trivial
/// predicate. Callers who explicitly want "all rows of the inner
/// table" (e.g. `Exists::new(T::objects())`) get the short form.
fn condition_to_opt(
    cond: crate::query::condition::Condition,
) -> Option<crate::query::condition::Condition> {
    if cond.is_vacuously_true() {
        None
    } else {
        Some(cond)
    }
}

/// Macro-only entry points. **Not** part of the stable public API.
///
/// `djogi-macros` emits calls into this module from the `{Model}OuterRef`
/// helper struct that `#[derive(Model)]` expands in the user's crate —
/// the items here are `pub` only so cross-crate codegen can reach them.
/// The double-underscore prefix and `#[doc(hidden)]` marker signal to
/// tooling and reviewers that downstream code must not call these
/// directly; the macro is the sole supported caller.
///
/// The seal closes the identifier-smuggling vector that
/// [`OuterRef::new`]'s `pub(crate)` visibility addresses at the crate
/// boundary: cross-crate code reaches an `OuterRef` only through this
/// validator, same pattern as
/// [`crate::query::field::__macro_support::__make_field_ref`] for
/// [`FieldRef`].
#[doc(hidden)]
pub mod __macro_support {
    use super::OuterRef;
    use crate::ident::assert_plain_ident;
    use crate::model::Model;

    /// Construct an [`OuterRef<M, V>`] from a macro-emitted column
    /// name. The only supported caller is the
    /// `{Model}OuterRef::column()` accessor that `#[derive(Model)]`
    /// emits in the user's crate.
    ///
    /// Panics if `column` violates any rule in
    /// [`crate::ident::assert_plain_ident`]: empty, over 63 bytes,
    /// leading digit, a non-identifier byte, or a reserved Postgres
    /// keyword. Mirrors the seal on
    /// [`crate::query::field::__macro_support::__make_field_ref`]
    /// exactly — the two functions close the same identifier vector on
    /// two parallel surfaces.
    #[doc(hidden)]
    pub fn __make_outer_ref<M: Model, V>(column: &'static str) -> OuterRef<M, V> {
        assert_plain_ident(column, "outer_ref_column");
        OuterRef::new(column)
    }
}

#[cfg(test)]
mod tests {
    //! Emitter shape tests — each construct renders the expected SQL
    //! tokens. Live DB coverage is in
    //! `tests/integration/phase4_transactions_expressions.rs`.

    use super::*;
    use crate::Expr;
    use crate::descriptor::ModelDescriptor;
    use crate::expr::sql::emit_expr;
    use crate::pg::accumulator::SqlAccumulator;

    // Inert local model — only `table_name` matters for emission tests.
    struct Ledger;
    impl crate::model::__sealed::Sealed for Ledger {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Ledger {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "ledgers"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
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
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
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
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    struct Entry;
    impl crate::model::__sealed::Sealed for Entry {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Entry {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "entries"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
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
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
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
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    #[test]
    fn exists_no_filter_emits_bare_select_one() {
        // Exists::new(Entry::objects()).as_expr() — no WHERE clause
        // because `Condition::True` collapses to `None` at
        // construction.
        let qs: QuerySet<Entry> = QuerySet::new();
        let expr = Exists::new(qs).as_expr();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
        assert_eq!(sql.trim(), "EXISTS (SELECT 1 FROM entries)", "got: {sql}");
    }

    #[test]
    fn exists_with_correlated_outer_ref() {
        // Exists::new(Entry::objects().filter(|f| <outer predicate>))
        // The outer ref should emit as a bare column name, with the
        // inner column referencing the same bare name. The test here
        // asserts the structural shape — live correlation resolution
        // happens in the Postgres integration test.
        use crate::query::field::FieldRef;
        let inner_col: FieldRef<Entry, i64> = FieldRef::new("ledger_id");
        let outer_ref: OuterRef<Ledger, i64> = OuterRef::new("id");
        let qs: QuerySet<Entry> =
            QuerySet::new().filter_expr(|_| inner_col.as_expr().eq(outer_ref.as_expr()));
        let expr = Exists::new(qs).as_expr();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
        assert_eq!(
            sql.trim(),
            "EXISTS (SELECT 1 FROM entries WHERE ledger_id = id)",
            "got: {sql}"
        );
    }

    #[test]
    fn scalar_subquery_emits_select_col() {
        // Subquery::new(Entry::objects().filter(memo eq "x"), id_col).as_expr()
        use crate::query::field::FieldRef;
        let memo: FieldRef<Entry, String> = FieldRef::new("memo");
        let id_col: FieldRef<Entry, i64> = FieldRef::new("id");
        let qs: QuerySet<Entry> = QuerySet::new().filter(|_| memo.eq("opening".to_string()));
        let expr = Subquery::new(qs, id_col).as_expr();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node);
        let sql = qb.sql();
        // One bind for the "opening" literal — assert structural shape
        // with the bind placeholder.
        assert_eq!(
            sql.trim(),
            "(SELECT id FROM entries WHERE memo = $1)",
            "got: {sql}"
        );
    }

    #[test]
    fn outer_ref_emits_bare_column() {
        // Baseline check — the outer ref on its own emits just the
        // column name, no qualifier.
        let r: OuterRef<Ledger, i64> = OuterRef::new("id");
        let expr: Expr<i64> = r.as_expr();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &expr.node);
        assert_eq!(qb.sql().trim(), "id", "got: {}", qb.sql());
    }

    #[test]
    fn make_outer_ref_seal_validates_identifier() {
        // Identifier validation runs at macro-entry time. A plain
        // column name succeeds; an empty / SQL-metachar payload
        // panics. Full validator coverage lives in `crate::ident`; this
        // test only asserts the seal threads through.
        let result =
            std::panic::catch_unwind(|| __macro_support::__make_outer_ref::<Ledger, i64>("id"));
        assert!(result.is_ok(), "plain column name must pass the seal");

        let result =
            std::panic::catch_unwind(|| __macro_support::__make_outer_ref::<Ledger, i64>("1bad"));
        assert!(result.is_err(), "leading-digit column must panic the seal");

        let result = std::panic::catch_unwind(|| {
            __macro_support::__make_outer_ref::<Ledger, i64>("id) OR 1=1 --")
        });
        assert!(result.is_err(), "SQL-metachar payload must panic the seal");
    }
}
