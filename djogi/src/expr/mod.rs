//! Typed expression IR — the substrate for field-vs-field filters,
//! arithmetic assignments, aggregates, and subqueries.
//! # What
//! [`Expr<T>`] is a `PhantomData<fn -> T>`-tagged wrapper around an
//! untyped [`node::ExprNode`] tree. Typed constructors (`Expr::literal`,
//! [`crate::query::field::FieldRef::as_expr`]) promote primitives and
//! columns into `Expr<T>`, and typed methods on `Expr<T>` compose them:
//! - [`Expr::eq`] / [`Expr::neq`] / [`Expr::gt`] / [`Expr::gte`] /
//! [`Expr::lt`] / [`Expr::lte`] — comparison, returning `Expr<bool>`.
//! - `impl Add/Sub/Mul/Div for Expr<T> where T: Numeric` — arithmetic
//! in [`arithmetic`], gated on a sealed [`arithmetic::Numeric`] trait
//! so only framework-blessed numeric types compose. The blessed set
//! is `i16 / i32 / i64 / f32 / f64` for ; `Decimal` / `Interval`
//! extend the trait later.
//! The `Expr<bool>` produced by a comparison slots into the filter tree
//! via the [`crate::query::condition::Condition::Expr`] variant; the
//! [`crate::query::QuerySet::filter_expr`] entry point wires the closure
//! form to that bridge.
//! # Why this shape
//! Two constraints drove the design:
//! 1. **Typed composition, untyped walk.** Users should not be able to
//! build `Expr<String> + Expr<i32>` (nonsense addition) or compare
//! `Expr<i64>.eq(Expr<String>)` (type-mismatched equality). The
//! phantom `T` parameter on [`Expr<T>`] enforces those rules at
//! compile time. But the SQL emitter doesn't care about `T` — it
//! only needs to walk the enum and push bind parameters. The
//! internal [`node::ExprNode`] is therefore untyped (no `T`
//! parameter), and the emitter stays a single monomorphic function.
//! 2. **Phase additivity.** Tasks 4 / 5 add `Case`, `Exists`, `Subquery`,
//! `Aggregate`, and `OuterRef` variants. By storing the dynamic
//! payload in `ExprNode`, those additions are one new variant + one
//! new emitter arm + one new typed constructor per variant — no
//! ripple through type-parameterised code.
//! # Where
//! - [`node::ExprNode`] / [`node::CmpOp`] — the untyped payload enum.
//! - [`literal`] — `impl From<T> for Expr<T>` for every bindable scalar.
//! - [`compare`] — comparison methods on `Expr<T>`.
//! - [`arithmetic`] — sealed `Numeric` + operator overloads.
//! - [`sql::emit_expr`] — the SQL emitter, matched exhaustively on
//! `ExprNode` variants.
//! - [`crate::query::condition::Condition::Expr`] — filter-tree bridge.
//! - [`crate::query::QuerySet::filter_expr`] — closure entry point.
//! # Example
//! ```ignore
//! use djogi::prelude::*;
//!
//! // Field-vs-field comparison — not expressible with the
//! // `filter(|f| f.col.eq(value))` API because the RHS is a literal
//! // there. `filter_expr` closes the gap.
//! let overdrawn = Account::objects
//! .filter_expr(|f| f.balance.as_expr.lt(f.overdraft_limit.as_expr))
//! .fetch_all(&mut ctx).await?;
//! ```

use crate::query::condition::FilterValue;
use std::marker::PhantomData;

pub mod aggregate;
pub(crate) mod arithmetic;
pub mod case;
pub(crate) mod compare;
pub(crate) mod literal;
pub(crate) mod node;
#[cfg(feature = "spatial")]
pub mod row_aggregate;
#[cfg(feature = "spatial")]
pub(crate) mod spatial;
pub(crate) mod sql;
pub mod subquery;
pub mod window;
pub mod window_fn;

pub use aggregate::{AggregateExpr, grouping_of};
pub use aggregate::{HypotheticalSetAgg, KindEvidence, MetadataAgg, OrderedSetAgg, ValueAgg};
pub use case::{Case, CaseBuilder};
use node::ExprNode;
#[cfg(feature = "spatial")]
pub use row_aggregate::{BinaryRowAgg, MvtOptions, RowAggregate, RowKindEvidence};
pub use subquery::{Exists, OuterRef, Subquery};
pub use window::{FrameBound, FrameExclude, FrameKind, WindowBuilder, WindowSpec};
pub use window_fn::{
    CumeDistWindow, DenseRank, FirstValueWindow, LagWindow, LastValueWindow, LeadWindow,
    NthValueWindow, NtileWindow, PercentRankWindow, QualifyCondition, QualifyOp, Rank, RowNumber,
    WindowRanking,
};

/// Typed expression handle — the public entry point for the IR.
/// Carries a `PhantomData<fn -> T>` tag so the type parameter is
/// covariant and the struct is `Send + Sync` regardless of `T`'s own
/// markers. `T` never appears as an owned field — it only tags the
/// Rust-level type the underlying SQL expression evaluates to, which
/// lets comparisons and arithmetic enforce type discipline at compile
/// time while the emitter walks a type-erased enum.
/// Always returned by-value from composition methods; `#[must_use]`
/// because a dropped expression is usually a mistake (the user likely
/// meant to hand it to `filter_expr` / `set_expr` / similar).
/// `Clone` + `Debug` — the underlying [`ExprNode`] is also `Clone +
/// Debug`, so copies and diagnostics are cheap. `Expr` is **not**
/// `Copy`: expression trees contain boxed sub-nodes, and making the
/// whole struct `Copy` would hide the allocation cost of cloning a
/// deep tree.
#[must_use = "expressions are lazy — dropping one silently omits the predicate"]
#[derive(Clone, Debug)]
pub struct Expr<T> {
    pub(crate) node: ExprNode,
    pub(crate) _phantom: PhantomData<fn() -> T>,
}

impl<T> Expr<T>
where
    T: Into<Expr<T>>,
{
    /// Construct an `Expr<T>` from a Rust value.
    /// Every SQL-bindable scalar Djogi ships with has an `impl From<V>
    /// for Expr<V>` in [`literal`], which in turn means
    /// `V: Into<Expr<V>>`. This wrapper lets call sites read
    /// `Expr::literal(100i32)` instead of `Expr::<i32>::from(100i32)`
    /// the inference direction matches the plan's pseudo-code and
    /// reads naturally alongside `field.as_expr`.
    /// The `T: Into<Expr<T>>` bound on the impl block (rather than on
    /// the method's own generic parameter) means Rust infers `T`
    /// directly from the argument type at the call site. Turbofish
    /// (`Expr::literal::<i32>(100)`) is never needed in practice.
    pub fn literal(v: T) -> Expr<T> {
        v.into()
    }
}

impl Expr<i32> {
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` — the current calendar year
    /// as an `Expr<i32>`, evaluated server-side per query.
    /// Composes with the rest of the `Expr<i32>` arithmetic IR. The canonical
    /// use is age-from-birth-year:
    /// ```ignore
    /// // SQL: (EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER - estimated_birth_year)
    /// let age_years = Expr::current_year - f.estimated_birth_year.as_expr;
    /// Elephant::objects
    /// .filter_expr(|f| age_years.gte(Expr::literal(15i32)))
    /// .fetch_all(&mut ctx).await?;
    /// ```
    /// # Why an associated `Expr<i32>` constructor?
    /// `current_year` takes no arguments and is conceptually a constant for
    /// the duration of a single query. Routing it through the typed `Expr`
    /// surface — rather than a `FieldRef` method — keeps the call site
    /// independent of any specific column, the same way `Expr::literal(...)`
    /// is column-independent.
    /// # Volatility
    /// `CURRENT_DATE` is `STABLE` per Postgres semantics — it returns a
    /// fresh value per statement, but stays constant within one transaction.
    /// Callers who need a frozen snapshot of "now" across a long-running
    /// transaction should bind a literal `i32` (`Expr::literal(2026i32)`)
    /// rather than calling this helper.
    /// # Where
    /// - [`ExprNode::CurrentYear`] is the underlying IR variant.
    /// - [`sql::emit_expr`] renders the SQL token stream.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn current_year() -> Expr<i32> {
        Expr::from_node(ExprNode::CurrentYear)
    }
}

#[cfg(feature = "spatial")]
impl Expr<f64> {
    /// `ST_Area($1::bytea::geography)` — area of the bound geometry in
    /// **square meters** (geography overload — meters, not square degrees).
    /// Returns `Expr<f64>` so it composes with the existing arithmetic IR
    /// for ratios such as `Expr::area_of_intersection(a, b) / Expr::area_of(a)`
    /// the canonical territory-overlap-percentage shape from the
    /// elephant-tracker mating-pairs demo .
    /// # SQL emission
    /// Emits `ST_Area($n::bytea::geography)` — the `::bytea::geography`
    /// double cast matches the bind discipline of the geometry-only shape
    /// predicates (see [`crate::expr::spatial::SpatialExpr`]). The
    /// `geography` overload yields square meters; the `geometry` overload
    /// would yield square degrees, which is the wrong unit.
    /// # Example
    /// ```ignore
    /// // Mating-pairs scoring — territory overlap as a ratio of areas.
    /// // hull_a / hull_b are convex-hull polygons fetched in an earlier query.
    /// let overlap_pct: f64 = ctx.raw_scalar(
    /// "SELECT ($1) / ($2)",
    /// &[/* bound subexpressions, illustrative */],
    /// ).await?;
    /// // Or compose typed inside an annotate / select:
    /// let ratio = Expr::area_of_intersection(&hull_a, &hull_b) / Expr::area_of(&hull_a);
    /// ```
    /// # Where
    /// - [`crate::expr::spatial::SpatialExpr::Area`] — IR variant.
    /// - [`Self::area_of_intersection`] — composed shape for the demo.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn area_of<G>(geom: &G) -> Expr<f64>
    where
        G: crate::geo::GeographyValue,
    {
        Expr::from_node(ExprNode::Spatial(crate::expr::spatial::SpatialExpr::Area {
            geom_ewkb: geom.to_ewkb_bytes(),
        }))
    }

    /// `ST_Area(ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography)`
    /// area in square meters of the intersection of two bound geometries.
    /// The composed shape powers the territory-overlap-percentage scoring in
    /// the elephant-tracker mating-pairs demo:
    /// ```ignore
    /// // overlap_pct = area(intersection(a, b)) / area(a)
    /// let pct = Expr::area_of_intersection(&hull_a, &hull_b)
    /// / Expr::area_of(&hull_a);
    /// ```
    /// Emitted as a single inline form rather than nesting `intersection_of`
    /// inside `area_of` — keeping the two geometry casts co-located in one
    /// emitter arm makes the SQL output predictable and the emitter arm
    /// self-contained.
    /// # Empty intersection
    /// When the two geometries do not overlap, `ST_Intersection` returns
    /// an empty geometry and `ST_Area` over an empty geometry returns
    /// `0.0`. The ratio path therefore yields `0.0` for non-overlapping
    /// pairs without any explicit guard. For cases where the raw
    /// intersection geometry is needed rather than its area, see
    /// [`Expr::intersection_of`].
    /// # Where
    /// - [`crate::expr::spatial::SpatialExpr::AreaOfIntersection`] — IR variant.
    /// - [`Self::area_of`] — denominator of the demo's overlap-pct ratio.
    /// - [`Self::intersection_of`] — raw intersection geometry (as `Expr<Polygon>`).
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn area_of_intersection<A, B>(a: &A, b: &B) -> Expr<f64>
    where
        A: crate::geo::GeographyValue,
        B: crate::geo::GeographyValue,
    {
        Expr::from_node(ExprNode::Spatial(
            crate::expr::spatial::SpatialExpr::AreaOfIntersection {
                a_ewkb: a.to_ewkb_bytes(),
                b_ewkb: b.to_ewkb_bytes(),
            },
        ))
    }
}

#[cfg(feature = "spatial")]
impl Expr<crate::geo::Polygon> {
    /// `ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography`
    /// the spatial intersection of two bound geometries, returned as a
    /// typed `Expr<`[`crate::geo::Polygon`]`>`.
    /// # SQL emission
    /// Emits:
    /// ```sql
    /// ST_Intersection($n::bytea::geometry, $m::bytea::geometry)::geography
    /// ```
    /// Both inputs are cast `::bytea::geometry` because PostGIS 3.x has no
    /// `geography` input overload for `ST_Intersection`. The result is cast
    /// `::geography` so [`crate::geo::Polygon`]'s `FromSql` implementation
    /// can decode it (the geography codec accepts columns whose Postgres type
    /// name is `"geography"`; without the cast the result is typed `"geometry"`
    /// and the codec rejects it).
    /// # Typical use
    /// Use this when you need the raw intersection geometry for further
    /// spatial analysis — for example, to examine the shape of the overlap
    /// region. For the simpler territory-overlap-percentage pattern (area of
    /// intersection over area of one input), prefer
    /// [`Expr::area_of_intersection`] / [`Expr::area_of`], which handle the
    /// empty-geometry sentinel cleanly:
    /// ```ignore
    /// // Overlap percentage (safe for disjoint polygons — yields 0.0).
    /// let pct = Expr::area_of_intersection(&hull_a, &hull_b)
    /// / Expr::area_of(&hull_a);
    ///
    /// // Raw intersection geometry — only decode when the result is
    /// // guaranteed to be a single POLYGON (see "Decode safety" below).
    /// let overlap_shape: Expr<Polygon> =
    /// Expr::intersection_of(&hull_a, &hull_b);
    /// ```
    /// # Decode safety
    /// The returned expression decodes as [`crate::geo::Polygon`]. Decode
    /// succeeds **only** when `ST_Intersection` returns a single `POLYGON`.
    /// Even when both inputs are polygonal and their interiors overlap,
    /// Postgres/PostGIS may return a `MULTIPOLYGON` or `GEOMETRYCOLLECTION`
    /// (for example, when two non-convex polygons overlap such that the
    /// intersection splits into two disconnected sub-regions). The decode will
    /// fail whenever the result is not a simple `POLYGON`:
    /// - **Disjoint inputs** — `ST_Intersection` returns an empty geometry;
    /// `Polygon::FromSql` will return a decode error.
    /// - **Boundary-only or point contact** — the result is a `LINESTRING`
    /// or `POINT`; decode will fail.
    /// - **Multi-part or collection result** — even for genuinely overlapping
    /// polygons, the result may be a `MULTIPOLYGON` or `GEOMETRYCOLLECTION`;
    /// decode will fail.
    /// [`crate::query::field::FieldRef::intersects`] is **not** a sufficient
    /// guard: it only rules out the disjoint case and still permits
    /// boundary-only contact and multi-part results.
    /// For queries that must survive any of these cases, prefer
    /// [`Expr::area_of_intersection`] (wraps the result in `ST_Area` and
    /// always returns `f64`, yielding `0.0` for non-overlapping pairs).
    /// # Where
    /// - [`crate::expr::spatial::SpatialExpr::Intersection`] — IR variant.
    /// - [`Expr::area_of_intersection`] — safe area-ratio form for the disjoint case.
    /// - [`Expr::area_of`] — area of a single geometry.
    #[must_use = "expressions are lazy — dropping one silently omits the predicate"]
    pub fn intersection_of<A, B>(a: &A, b: &B) -> Expr<crate::geo::Polygon>
    where
        A: crate::geo::GeographyValue,
        B: crate::geo::GeographyValue,
    {
        Expr::from_node(ExprNode::Spatial(
            crate::expr::spatial::SpatialExpr::Intersection {
                a_ewkb: a.to_ewkb_bytes(),
                b_ewkb: b.to_ewkb_bytes(),
            },
        ))
    }
}

impl<T> Expr<T> {
    /// Package an already-constructed `ExprNode` into `Expr<T>`. Crate-
    /// private so downstream code cannot fabricate an arbitrarily-typed
    /// expression by bypassing the typed constructors (`literal`,
    /// `FieldRef::as_expr`, operator overloads).
    /// Used internally by [`compare`] and [`arithmetic`] to wrap the
    /// newly-built node without repeating the `PhantomData` boilerplate
    /// at every call site.
    pub(crate) fn from_node(node: ExprNode) -> Self {
        Expr {
            node,
            _phantom: PhantomData,
        }
    }

    /// Build an `Expr<T>` directly from a [`FilterValue`]. Crate-private
    /// convenience for the `impl From<V> for Expr<V>` bridges in
    /// [`literal`] — every scalar impl calls this after mapping itself
    /// into the right `FilterValue` variant, so the literal constructors
    /// all route through the same typed seal.
    /// `T` is whatever the enclosing `impl<T> Expr<T>` block specialises
    /// to at the call site — the `From` impls in [`literal`] call this
    /// through `Expr::<Self>::from_literal(..)` after `Self == Expr<V>`
    /// nails `T` to the scalar type.
    pub(crate) fn from_literal(v: FilterValue) -> Self {
        Expr {
            node: ExprNode::Literal(v),
            _phantom: PhantomData,
        }
    }

    /// Construct an `Expr<T>` from a raw SQL fragment — 2.
    /// Macro-only constructor, emitted by `#[computed(sql = "...")]`
    /// and the `{Model}Computed` ZST accessors (T4.5). The `T`
    /// parameter must match the SQL fragment's return type; the macro
    /// wires this from the computed getter's signature so the typed
    /// seal stays intact at the public boundary.
    /// **Macro-only.** Calling this directly from adopter code is
    /// unsupported behaviour at the framework level — `#[doc(hidden)]`
    /// and the `__`-prefix are a convention, not a visibility gate. The
    /// only supported path is the macro-emitted `{Model}Computed`
    /// accessor; direct calls bypass token-level validation and can
    /// inject arbitrary SQL into `filter_expr`. This is accepted
    /// pre-alpha risk (parallel to
    /// [`crate::query::condition::Condition::__from_raw_sql_fragment`]),
    /// documented here so adopters cannot claim ignorance.
    /// Not part of the user-facing `Expr` API — the `__`-prefix +
    /// `#[doc(hidden)]` pair signals that downstream code must not
    /// call this directly, mirroring
    /// [`crate::query::condition::Condition::__from_raw_sql_fragment`]
    /// from the same task. Earlier shapes named this
    /// `raw_sql_fragment` (no underscores), which read as a regular
    /// API method name and surfaced as a SQL-injection foothold:
    /// `Expr::<bool>::raw_sql_fragment("'; DROP TABLE users; --")`
    /// in safe adopter code routed straight into `filter_expr`. The
    /// rename to the established `__`-prefix convention puts the
    /// constructor on the same explicit "macro-only" footing as the
    /// `Condition` sibling.
    /// `djogi-macros` cannot have `pub(crate)` access to `djogi`'s
    /// internals (separate crate), and routing this through
    /// `Condition::__from_raw_sql_fragment` is shape-incompatible
    /// that returns `Condition`, but the `{Model}Computed` accessors
    /// must return a typed `Expr<T>` so they compose with the rest of
    /// the typed `Expr` surface. The macro-only `pub` constructor with
    /// a sentinel name is the working pattern.
    /// The `&'static str` argument is the user-authored SQL expression,
    /// baked at macro expansion time after the token-level validation
    /// pass in `model::computed`. Adopters who need a runtime-bound
    /// SQL fragment must drop down to `DjogiContext::raw_query` /
    /// `raw_execute` — the typed `Expr<T>` surface stays free of
    /// runtime SQL composition.
    /// # Why parens-wrapped at every emission site (not in the
    /// constructor)
    /// Wrapping happens inside `expr::sql::emit_expr` rather than at
    /// fragment-construction time so the `Expr<T>` IR stays a clean
    /// tree of well-typed nodes — adding parens here would make the
    /// fragment opaque to any future tree-walking optimisation pass.
    /// The emitter's universal wrapping is the operator-precedence
    /// safety net.
    #[doc(hidden)]
    pub fn __raw_sql_fragment(s: &'static str) -> Self {
        // `Expr<T>` carries its own `#[must_use]` via the type-level
        // attribute — no method-level must_use needed.
        Expr {
            node: ExprNode::RawSql(s),
            _phantom: PhantomData,
        }
    }
}
