//! Row-shape aggregate terminals — `as_mvt(...)` / `as_geobuf(...)` on
//! [`crate::query::QuerySet`] and [`crate::query::AnnotatedQuerySet`].
//!
//! # What
//!
//! [`AsMvtTerminal<T, A>`] / [`AsGeobufTerminal<T, A>`] are the typed
//! handles produced by [`AnnotatedQuerySet::as_mvt`] /
//! [`AnnotatedQuerySet::as_geobuf`] (and the convenience entry points on
//! [`crate::query::QuerySet`] that synthesise an empty annotation tuple).
//! Each handle terminates with `fetch_one(ctx)` returning `Vec<u8>` —
//! the encoded MVT protobuf bytes or Geobuf bytes for the entire matching
//! row set.
//!
//! # SQL shape
//!
//! ```sql
//! SELECT ST_AsMVT(__djogi_row, $1, $2, $3, $4)
//! FROM (
//!     SELECT t.col1, t.col2, …, <annotations as __djogi_agg_N>
//!     FROM <table> AS t [WHERE …] [ORDER BY …] [LIMIT …]
//! ) AS __djogi_row
//! ```
//!
//! The derived-table alias is the framework-fixed `__djogi_row`; the row
//! aggregate emitter (`crate::expr::sql::emit_row_aggregate`) hard-codes
//! that alias for the first argument of the SQL function call. Splicing
//! a user-controlled alias here would let a row-aggregate IR node land
//! against a hostile FROM alias the emitter cannot validate.
//!
//! # Why a separate module
//!
//! The terminal handles are gated on `feature = "spatial"` (both PostGIS
//! row aggregates Djogi ships at v0.1.0 require the feature). Putting
//! them next to the other spatial query surface keeps the cfg-gating
//! crisp and avoids polluting [`crate::query::annotate`] with spatial
//! references.
//!
//! # Where
//!
//! - [`crate::expr::row_aggregate`] — typed `RowAggregate<Out, K>` wrapper.
//! - [`crate::expr::node::ExprNode::RowAggregate`] — untyped IR variant.
//! - [`crate::expr::sql::emit_row_aggregate`] — SQL emission.
//! - [`crate::query::annotate::AnnotatedQuerySet::as_mvt`] /
//!   [`crate::query::annotate::AnnotatedQuerySet::as_geobuf`] — terminal
//!   entry points on the annotated path.
//! - [`crate::query::queryset::QuerySet::as_mvt`] /
//!   [`crate::query::queryset::QuerySet::as_geobuf`] — terminal entry
//!   points on the plain (un-annotated) path.

#![cfg(feature = "spatial")]
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::expr::row_aggregate::{
    BinaryRowAgg, MvtOptions, RowAggregate, build_geobuf_aggregate, build_mvt_aggregate,
};
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::FromPgRow;
use crate::query::annotate::{AnnotatedQuerySet, IntoAggregateTuple};
use crate::query::queryset::QuerySet;
use crate::query::sql::build_spatial_row_select_with_annotations_for_fetch;
use std::future::Future;

// ── Terminal handles ─────────────────────────────────────────────────────

/// Pending `ST_AsMVT` terminal — built by
/// [`crate::query::annotate::AnnotatedQuerySet::as_mvt`] or
/// [`crate::query::queryset::QuerySet::as_mvt`] and terminated with
/// [`Self::fetch_one`].
///
/// Holds the upstream queryset (for the inner SELECT's `FROM` / `WHERE` /
/// `ORDER BY` / `LIMIT` tail), the annotation tuple shaping the inner
/// SELECT list, and the typed [`RowAggregate`] capturing the encoder
/// configuration (layer name, extent, geom column, optional feature id).
///
/// `#[must_use]` because an unawaited terminal is always a mistake — the
/// `.fetch_one(ctx)` call is what actually runs the SQL.
#[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
pub struct AsMvtTerminal<T: Model, A: IntoAggregateTuple> {
    pub(crate) qs: AnnotatedQuerySet<T, A>,
    pub(crate) agg: RowAggregate<Vec<u8>, BinaryRowAgg>,
}

/// Pending `ST_AsGeobuf` terminal — built by
/// [`crate::query::annotate::AnnotatedQuerySet::as_geobuf`] or
/// [`crate::query::queryset::QuerySet::as_geobuf`] and terminated with
/// [`Self::fetch_one`].
///
/// Same shape as [`AsMvtTerminal`] but carries Geobuf-specific
/// configuration — only the geometry column name.
#[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
pub struct AsGeobufTerminal<T: Model, A: IntoAggregateTuple> {
    pub(crate) qs: AnnotatedQuerySet<T, A>,
    pub(crate) agg: RowAggregate<Vec<u8>, BinaryRowAgg>,
}

// ── Inner-SELECT builder ────────────────────────────────────────────────

/// Build the wrapping SQL `SELECT <row_agg> FROM (<inner annotated select>)
/// AS __djogi_row`, common to both `AsMvtTerminal` and `AsGeobufTerminal`.
///
/// Per the emitter contract in [`crate::expr::sql::emit_row_aggregate`],
/// the row aggregate's record argument is the framework-fixed
/// `__djogi_row` alias. This helper splices the same alias into the
/// outer SELECT so the IR and the FROM clause agree.
///
/// # Why the row-aggregate inner select shape
///
/// Adopters compose the row content via the existing annotate surface
/// (model columns + optional aggregate annotations). The inner select
/// emits `SELECT t.col1, t.geog::geometry AS geog, …, agg AS
/// __djogi_agg_0 FROM <table> AS t [WHERE …]` — exactly the projection
/// the row aggregate folds. Geography columns are cast only in this path:
/// ordinary `AnnotatedQuerySet::fetch_all` must keep geography wire values
/// intact for `FromPgRow`, while `ST_AsMVT` / `ST_AsGeobuf` inspect
/// geometry-typed record columns by name and the outer terminal decodes
/// only the aggregate `bytea`.
fn build_row_aggregate_select<T, A>(
    aqs: &AnnotatedQuerySet<T, A>,
    agg: &RowAggregate<Vec<u8>, BinaryRowAgg>,
) -> Result<SqlAccumulator, crate::query::portable::PortablePredicateError>
where
    T: Model + FromPgRow,
    A: IntoAggregateTuple + crate::query::annotate::PlainAnnotationTuple,
{
    // Inner SELECT — row-aggregate-specific variant of the annotated
    // select. `aggregates.check_legality()` and
    // `aggregates.check_no_column_collision(T::COLUMNS)` are intentionally
    // NOT called here — each terminal's `fetch_one` runs both checks before
    // calling this builder so that validation errors surface before SQL
    // emission. These terminals consume qualify via the wrapped
    // AnnotatedQuerySet; `build_spatial_row_select_with_annotations_for_fetch`
    // (djogi/src/query/sql.rs:1804-1813) emits the same
    // `SELECT * FROM (…) AS __djogi_q WHERE <alias> …` outer qualify wrap
    // as `AnnotatedQuerySet::fetch_all`, so a window alias shadowing a
    // model column must be rejected at the terminal level.
    let inner = build_spatial_row_select_with_annotations_for_fetch(
        &aqs.qs,
        |acc| {
            aqs.aggregates.push_plain_columns(acc);
        },
        aqs.qualify.as_ref(),
    )?;

    // Outer SELECT — wrap the inner select in a derived table whose
    // alias is the framework-fixed `__djogi_row` (matching the alias
    // emit_row_aggregate uses for the record argument).
    let mut outer = SqlAccumulator::new("SELECT ");
    crate::expr::sql::emit_expr(
        &mut outer,
        &agg.node,
        crate::query::portable::SqlEmitContext::root(),
    )?;
    outer.push_sql(" FROM (");
    outer.extend_with(inner);
    outer.push_sql(") AS __djogi_row");
    Ok(outer)
}

// ── AsMvtTerminal ────────────────────────────────────────────────────────

impl<T: Model, A: IntoAggregateTuple + Send> AsMvtTerminal<T, A>
where
    T: FromPgRow + Send + Unpin,
    A: crate::query::annotate::PlainAnnotationTuple,
{
    /// Execute the `ST_AsMVT(...)` query and decode the encoded MVT
    /// protobuf bytes.
    ///
    /// Returns a single `Vec<u8>` — the full MVT tile payload for the
    /// matching row set. The PostGIS aggregate folds every row in the
    /// inner SELECT into one protobuf, so the return is always one
    /// `bytea`, never an array.
    ///
    /// # Empty input rows
    ///
    /// `QuerySet::none()` short-circuits to `Ok(Vec::new())` without
    /// emitting SQL.
    ///
    /// For non-`.none()` queries, a SQL filter that matches zero rows
    /// yields `NULL` for `ST_AsMVT` from PostGIS. We map that to
    /// `Ok(Vec::new())` to keep terminal behavior ergonomic and avoid
    /// a `WasNull` decode error.
    ///
    /// # Errors
    ///
    /// - The annotation tuple fails its `check_legality()` (e.g. an
    ///   illegal aggregate modifier survived earlier validation).
    /// - A window annotation alias collides with a `T` model column name —
    ///   returns [`crate::DjogiError::Validation`] with a remediation hint.
    ///   This terminal routes qualify through
    ///   `build_spatial_row_select_with_annotations_for_fetch`
    ///   (`djogi/src/query/sql.rs:1804-1813`) which emits the same
    ///   `SELECT * FROM (…) AS __djogi_q WHERE <alias> …` outer qualify wrap
    ///   as [`AnnotatedQuerySet::fetch_all`]; the collision check fires here
    ///   for the same reason.
    /// - The geometry column named in [`MvtOptions::with_geom_name`] does
    ///   not exist in the model's projection — PostGIS raises this at
    ///   execute time as a `42703 column does not exist` error.
    /// - The inner queryset emits any portable-predicate validation
    ///   error (matching ordinary `fetch_all` semantics).
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<u8>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        A: 'ctx,
    {
        async move {
            let AsMvtTerminal { qs, agg } = self;
            if qs.qs.is_empty() {
                return Ok(Vec::new());
            }

            // Validate the annotation tuple's modifier discipline
            // before SQL emission — same call site the ordinary
            // annotated `fetch_all` makes. The row aggregate itself
            // carries no modifiers so its own legality is trivially Ok.
            qs.aggregates.check_legality()?;
            // Reject window annotation aliases that collide with model
            // column names. This terminal consumes qualify via the wrapped
            // AnnotatedQuerySet; the spatial SQL builder
            // (djogi/src/query/sql.rs:1804-1813) emits the same
            // `SELECT * FROM (…) AS __djogi_q WHERE <alias> …` outer
            // qualify wrap as AnnotatedQuerySet::fetch_all, so a colliding
            // alias produces an ambiguous outer-WHERE at execute time.
            qs.aggregates.check_no_column_collision(T::COLUMNS)?;

            // Pair-tuple aggregates are not legal on the single-Model
            // annotated path (would require pair-tuple `l.` / `r.`
            // scope). Inherit the same rejection from
            // `AnnotatedQuerySet::fetch_all` so adopter code gets the
            // identical typed diagnostic.
            if qs.aggregates.requires_pair_tuple_scope()
                || qs.aggregates.requires_closure_pair_join()
            {
                return Err(DjogiError::Validation(
                    "row-shape aggregate terminals cannot host pair-tuple aggregates in the \
                     annotation tuple (for example `PairAreaOverlapRatio` / `PairClosureKinshipSum`). \
                     These aggregates reference pair-tuple aliases (`l.`, `r.`, `la.`, `ra.`) that are \
                     only in scope on joined-pair query surfaces. Use a paired-query annotation \
                     surface (e.g. `QuerySet::self_pairs()` / `cross_join_with(...)` plus \
                     `.annotate(...)`) and a joined terminal that supports that joined aliasing model."
                        .to_string(),
                ));
            }

            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;

            let acc = build_row_aggregate_select(&qs, &agg).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            // Position 0 — the row aggregate is the sole SELECT-list
            // entry of the outer query.
            let bytes: Option<Vec<u8>> = row.try_get(0).map_err(DjogiError::from)?;
            Ok(bytes.unwrap_or_default())
        }
    }
}

// ── AsGeobufTerminal ─────────────────────────────────────────────────────

impl<T: Model, A: IntoAggregateTuple + Send> AsGeobufTerminal<T, A>
where
    T: FromPgRow + Send + Unpin,
    A: crate::query::annotate::PlainAnnotationTuple,
{
    /// Execute the `ST_AsGeobuf(...)` query and decode the encoded Geobuf
    /// bytes.
    ///
    /// Same semantics as [`AsMvtTerminal::fetch_one`] — folds every row
    /// in the inner SELECT into one `bytea`, returns the single row's
    /// scalar value.
    ///
    /// # Empty input rows
    ///
    /// `QuerySet::none()` short-circuits to `Ok(Vec::new())` without
    /// emitting SQL.
    ///
    /// For non-`.none()` queries, a SQL filter that matches zero rows
    /// yields `NULL` for `ST_AsGeobuf` from PostGIS. We map that to
    /// `Ok(Vec::new())` to keep terminal behavior ergonomic and avoid
    /// a `WasNull` decode error.
    ///
    /// # Errors
    ///
    /// - The annotation tuple fails its `check_legality()` (e.g. an
    ///   illegal aggregate modifier survived earlier validation).
    /// - A window annotation alias collides with a `T` model column name —
    ///   returns [`crate::DjogiError::Validation`] with a remediation hint.
    ///   This terminal routes qualify through
    ///   `build_spatial_row_select_with_annotations_for_fetch`
    ///   (`djogi/src/query/sql.rs:1804-1813`) which emits the same
    ///   `SELECT * FROM (…) AS __djogi_q WHERE <alias> …` outer qualify wrap
    ///   as [`AnnotatedQuerySet::fetch_all`]; the collision check fires here
    ///   for the same reason.
    /// - The geometry column named in the `geom_name` argument passed to
    ///   [`AnnotatedQuerySet::as_geobuf`] does not exist in the model's
    ///   projection — PostGIS raises this at execute time as a
    ///   `42703 column does not exist` error.
    /// - The inner queryset emits any portable-predicate validation
    ///   error (matching ordinary `fetch_all` semantics).
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<u8>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        A: 'ctx,
    {
        async move {
            let AsGeobufTerminal { qs, agg } = self;
            if qs.qs.is_empty() {
                return Ok(Vec::new());
            }

            qs.aggregates.check_legality()?;
            // Same alias-collision guard as AsMvtTerminal::fetch_one —
            // this terminal routes qualify through the same spatial SQL
            // builder that emits the __djogi_q outer-WHERE wrap
            // (djogi/src/query/sql.rs:1804-1813).
            qs.aggregates.check_no_column_collision(T::COLUMNS)?;

            if qs.aggregates.requires_pair_tuple_scope()
                || qs.aggregates.requires_closure_pair_join()
            {
                return Err(DjogiError::Validation(
                    "row-shape aggregate terminals cannot host pair-tuple aggregates in the \
                     annotation tuple (for example `PairAreaOverlapRatio` / `PairClosureKinshipSum`). \
                     These aggregates reference pair-tuple aliases (`l.`, `r.`, `la.`, `ra.`) that are \
                     only in scope on joined-pair query surfaces. Use a paired-query annotation \
                     surface (e.g. `QuerySet::self_pairs()` / `cross_join_with(...)` plus \
                     `.annotate(...)`) and a joined terminal that supports that joined aliasing model."
                        .to_string(),
                ));
            }

            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;

            let acc = build_row_aggregate_select(&qs, &agg).map_err(DjogiError::from)?;
            let (sql, binds) = acc.into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let bytes: Option<Vec<u8>> = row.try_get(0).map_err(DjogiError::from)?;
            Ok(bytes.unwrap_or_default())
        }
    }
}

// ── Entry points on AnnotatedQuerySet ────────────────────────────────────

impl<T: Model + FromPgRow, A: IntoAggregateTuple> AnnotatedQuerySet<T, A> {
    /// Encode every matching row as Mapbox Vector Tile bytes — terminal
    /// equivalent of PostGIS's `ST_AsMVT(record, …)` row aggregate.
    ///
    /// The row content is whatever this annotated queryset projects —
    /// model columns plus any aggregate annotations. PostGIS resolves
    /// the geometry column at runtime by the name in
    /// [`MvtOptions::with_geom_name`] (default `"geom"`).
    ///
    /// # Why a row aggregate (not a column aggregate)
    ///
    /// `ST_AsMVT` takes the whole row as input — every column becomes
    /// either the encoded geometry, the feature id, or a feature
    /// property. The typed surface routes through
    /// [`RowAggregate`](crate::expr::row_aggregate::RowAggregate)
    /// rather than [`AggregateExpr`](crate::expr::AggregateExpr) so the
    /// column-aggregate modifier set (`.distinct()`, `.filter()`,
    /// `.over()`, …) cannot accidentally compose with a row aggregate
    /// — Postgres rejects every such combination, so the type-level
    /// gate keeps the call sites well-formed.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    /// use djogi::expr::row_aggregate::MvtOptions;
    ///
    /// let tile_bytes: Vec<u8> = Feature::objects()
    ///     .filter(|f| f.tile_z().eq(z).and(f.tile_x().eq(x)).and(f.tile_y().eq(y)))
    ///     .annotate(|f| f.id().count_star()) // adds per-row count to features
    ///     .as_mvt("features")
    ///     .fetch_one(&mut ctx)
    ///     .await?;
    /// ```
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// SELECT ST_AsMVT(__djogi_row, $1, $2, $3 [, $4])
    /// FROM (
    ///     SELECT t.id, t.geom, …, <annotations…>
    ///     FROM features AS t [WHERE …]
    /// ) AS __djogi_row
    /// ```
    ///
    /// # Where
    ///
    /// - [`AsMvtTerminal::fetch_one`] — runs the query and decodes the
    ///   `Vec<u8>` payload.
    /// - [`MvtOptions`] — non-default encoder configuration.
    /// - [`Self::as_mvt_with_options`] — explicit-options entry point.
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_mvt(self, layer_name: impl Into<String>) -> AsMvtTerminal<T, A> {
        self.as_mvt_with_options(MvtOptions::new(layer_name))
    }

    /// Encode every matching row as MVT bytes with non-default encoder
    /// options.
    ///
    /// Composes with the same per-call defaults as [`Self::as_mvt`] when
    /// the supplied [`MvtOptions`] is left at its built-in defaults; use
    /// [`MvtOptions::with_extent`] / [`MvtOptions::with_geom_name`] /
    /// [`MvtOptions::with_feature_id_name`] to deviate.
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_mvt_with_options(self, opts: MvtOptions) -> AsMvtTerminal<T, A> {
        // T's canonical column list pins the row shape on the IR side;
        // PostGIS uses it for documentation only at v0.1.0, but the
        // typed surface records it so future row aggregates that need
        // per-column projection slot in without rev'ing the IR.
        let columns = inner_columns_for::<T>();
        let agg = build_mvt_aggregate(opts, columns);
        AsMvtTerminal { qs: self, agg }
    }

    /// Encode every matching row as Geobuf bytes — terminal equivalent
    /// of PostGIS's `ST_AsGeobuf(rowset anyelement, geom_name text)` row
    /// aggregate.
    ///
    /// Same composition story as [`Self::as_mvt`] — the row aggregate
    /// folds every row in the inner annotated select into one `bytea`,
    /// and the row aggregate's modifier discipline is enforced by
    /// the [`RowAggregate`] type having no modifier methods.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// let geobuf_bytes: Vec<u8> = Feature::objects()
    ///     .filter(|f| f.region_id().eq(region_id))
    ///     .annotate(|_| ()) // no extra columns — model row only
    ///     .as_geobuf("location")
    ///     .fetch_one(&mut ctx)
    ///     .await?;
    /// ```
    ///
    /// # SQL emission
    ///
    /// ```sql
    /// SELECT ST_AsGeobuf(__djogi_row, $1)
    /// FROM (SELECT t.id, t.location, … FROM features AS t [WHERE …]) AS __djogi_row
    /// ```
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_geobuf(self, geom_name: impl Into<String>) -> AsGeobufTerminal<T, A> {
        let columns = inner_columns_for::<T>();
        let agg = build_geobuf_aggregate(geom_name, columns);
        AsGeobufTerminal { qs: self, agg }
    }
}

// ── Entry points on QuerySet ────────────────────────────────────────────

impl<T: Model + FromPgRow> QuerySet<T> {
    /// Encode every matching row as Mapbox Vector Tile bytes — terminal
    /// equivalent of PostGIS's `ST_AsMVT(record, …)` row aggregate.
    ///
    /// Bare-queryset entry point — equivalent to chaining
    /// `.annotate(|_| ())` + `.as_mvt(layer_name)` but lighter on the
    /// call site for the common case where no annotations are needed.
    /// The inner SELECT projects every model column from `T::COLUMNS`;
    /// PostGIS resolves the geometry column at runtime by the name in
    /// [`MvtOptions::with_geom_name`] (default `"geom"`).
    ///
    /// See [`AnnotatedQuerySet::as_mvt`] for the annotated variant
    /// (which lets adopters extend the row with aggregate columns
    /// before encoding) and for full SQL-emission documentation.
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_mvt(self, layer_name: impl Into<String>) -> AsMvtTerminal<T, EmptyAnnotation> {
        self.as_mvt_with_options(MvtOptions::new(layer_name))
    }

    /// Encode every matching row as MVT bytes with non-default encoder
    /// options. Bare-queryset variant of
    /// [`AnnotatedQuerySet::as_mvt_with_options`].
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_mvt_with_options(self, opts: MvtOptions) -> AsMvtTerminal<T, EmptyAnnotation> {
        let columns = inner_columns_for::<T>();
        let agg = build_mvt_aggregate(opts, columns);
        AsMvtTerminal {
            qs: AnnotatedQuerySet {
                qs: self,
                aggregates: EmptyAnnotation,
                qualify: None,
                _a: std::marker::PhantomData,
            },
            agg,
        }
    }

    /// Encode every matching row as Geobuf bytes — bare-queryset variant
    /// of [`AnnotatedQuerySet::as_geobuf`].
    #[must_use = "row-shape aggregate terminals are lazy — dropping discards the query"]
    pub fn as_geobuf(self, geom_name: impl Into<String>) -> AsGeobufTerminal<T, EmptyAnnotation> {
        let columns = inner_columns_for::<T>();
        let agg = build_geobuf_aggregate(geom_name, columns);
        AsGeobufTerminal {
            qs: AnnotatedQuerySet {
                qs: self,
                aggregates: EmptyAnnotation,
                qualify: None,
                _a: std::marker::PhantomData,
            },
            agg,
        }
    }
}

// ── Empty annotation tuple ──────────────────────────────────────────────

/// Sentinel annotation tuple representing "no aggregate annotations" —
/// used by [`QuerySet::as_mvt`] / [`QuerySet::as_geobuf`] to satisfy
/// [`AsMvtTerminal`] / [`AsGeobufTerminal`]'s `A: IntoAggregateTuple`
/// bound without forcing adopters to chain `.annotate(|_| ())`.
///
/// Implements both [`IntoAggregateTuple`] and the narrower
/// [`crate::query::annotate::PlainAnnotationTuple`] — emitting no
/// columns and decoding to `()`. The row aggregate emitter never reads
/// the annotation columns (it folds the entire inner row regardless of
/// SELECT-list shape), so an empty tuple is the natural sentinel.
///
/// `#[doc(hidden)]` because adopter code never names this type
/// directly — it appears only as the second type parameter of the
/// terminal handles produced by [`QuerySet::as_mvt`] /
/// [`QuerySet::as_geobuf`], and adopters write
/// `AsMvtTerminal<MyModel, _>` to elide it.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct EmptyAnnotation;

impl crate::query::annotate::sealed::Sealed for EmptyAnnotation {}

impl IntoAggregateTuple for EmptyAnnotation {
    type Decoded = ();

    fn push_columns(&self, _acc: &mut SqlAccumulator) {
        // No-op — the empty annotation contributes no columns. The
        // outer row aggregate folds the entire row regardless of
        // annotation slots; pushing nothing here keeps the inner
        // SELECT list to `t.col1, t.col2, …` only.
    }

    fn push_columns_bare(&self, _acc: &mut SqlAccumulator) {}

    fn push_columns_bare_after(&self, _acc: &mut SqlAccumulator, _has_previous_columns: bool) {}

    fn decode_tuple(
        &self,
        _row: &tokio_postgres::Row,
    ) -> Result<Self::Decoded, tokio_postgres::Error> {
        // The row aggregate terminal never decodes the annotation slot
        // — it decodes only position 0 (the row aggregate's bytea).
        // This impl is reachable from the standard `AnnotatedQuerySet::fetch_all`
        // path only if an adopter manually constructed an
        // `AnnotatedQuerySet<T, EmptyAnnotation>`, which the
        // public surface does not expose. The `()` return matches the
        // documented contract.
        Ok(())
    }

    fn annotation_count(&self) -> usize {
        0
    }

    fn check_legality(&self) -> Result<(), crate::DjogiError> {
        Ok(())
    }
}

impl crate::query::annotate::PlainAnnotationTuple for EmptyAnnotation {
    fn push_plain_columns(&self, _acc: &mut SqlAccumulator) {
        // No-op — see `push_columns` above.
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Return the canonical column list for `T` as a `Vec<&'static str>`
/// suitable for the [`crate::expr::node::ExprNode::RowAggregate::columns`]
/// slot.
///
/// Pulls from [`FromPgRow::COLUMNS`] — the same source of truth the
/// annotated SELECT list uses. The IR variant stores this for
/// documentation / future-projection purposes; v0.1.0's emitter does
/// not inspect it.
fn inner_columns_for<T: FromPgRow>() -> Vec<&'static str> {
    T::COLUMNS.to_vec()
}

#[cfg(test)]
mod tests {
    //! Unit-level tests for the row-aggregate terminal SQL shape.
    //!
    //! Live PostGIS round-trip behaviour lives in
    //! `tests/integration/phase8_5_c4f_row_aggregate_mvt_live.rs`.

    use super::*;
    use crate::descriptor::{
        FieldSqlType, GeographySubtype, ModelDescriptor, PkType, field_descriptor, model_descriptor,
    };
    use crate::expr::RowNumber;
    use crate::testing;

    // ── Fake model — bare minimum to exercise the SQL builder ──────────

    struct TileFeature {
        id: i64,
    }

    impl crate::model::__sealed::Sealed for TileFeature {}
    #[allow(clippy::manual_async_fn)]
    impl Model for TileFeature {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "tile_features"
        }
        fn pk_value(&self) -> &i64 {
            &self.id
        }
        fn descriptor() -> &'static ModelDescriptor {
            static FIELDS: &[crate::descriptor::FieldDescriptor] = &[
                field_descriptor("id", FieldSqlType::BigInt, false),
                field_descriptor(
                    "geom",
                    FieldSqlType::Geography {
                        subtype: GeographySubtype::Point,
                        srid: 4326,
                    },
                    false,
                ),
                field_descriptor("name", FieldSqlType::Text, false),
            ];
            static DESCRIPTOR: ModelDescriptor =
                model_descriptor("TileFeature", "tile_features", PkType::Serial, FIELDS);
            &DESCRIPTOR
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

    impl FromPgRow for TileFeature {
        const COLUMNS: &'static [&'static str] = &["id", "geom", "name"];
        const COLUMN_LIST: &'static str = "id, geom, name";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError> {
            unreachable!("SQL-text unit tests do not exercise row decode")
        }
    }

    fn empty_aqs() -> AnnotatedQuerySet<TileFeature, EmptyAnnotation> {
        AnnotatedQuerySet {
            qs: QuerySet::new(),
            aggregates: EmptyAnnotation,
            qualify: None,
            _a: std::marker::PhantomData,
        }
    }

    #[test]
    fn as_mvt_default_options_emit_expected_sql() {
        let terminal = empty_aqs().as_mvt("test_layer");
        let acc = build_row_aggregate_select(&terminal.qs, &terminal.agg)
            .expect("SQL build must succeed");
        let (sql, _binds) = acc.into_parts();
        // Outer wrap: SELECT ST_AsMVT(...) FROM (<inner>) AS __djogi_row
        assert!(
            sql.starts_with("SELECT ST_AsMVT(__djogi_row, $1, $2, $3) FROM ("),
            "unexpected outer SELECT shape: {sql}"
        );
        // Inner: geography columns are cast to geometry for PostGIS row encoders.
        assert!(
            sql.contains("SELECT t.id, t.geom::geometry AS geom, t.name FROM tile_features AS t"),
            "unexpected inner SELECT shape: {sql}"
        );
        // Closing alias
        assert!(sql.ends_with(") AS __djogi_row"), "missing alias: {sql}");
        // No feature_id_name → only 3 bind params before the closing paren
        assert!(
            !sql.contains("ST_AsMVT(__djogi_row, $1, $2, $3, $4)"),
            "feature_id_name was unexpectedly emitted: {sql}"
        );
    }

    #[test]
    fn as_mvt_with_feature_id_emits_fifth_argument() {
        let terminal = empty_aqs().as_mvt_with_options(
            MvtOptions::new("layer")
                .with_extent(8192)
                .with_geom_name("the_geom")
                .with_feature_id_name("feature_pk"),
        );
        let acc = build_row_aggregate_select(&terminal.qs, &terminal.agg)
            .expect("SQL build must succeed");
        let (sql, binds) = acc.into_parts();
        assert!(
            sql.contains("ST_AsMVT(__djogi_row, $1, $2, $3, $4)"),
            "expected feature_id_name argument: {sql}"
        );
        // Four binds: layer_name, extent, geom_name, feature_id_name
        assert_eq!(
            binds.len(),
            4,
            "expected 4 bind params for full-options MVT, got {}: {sql}",
            binds.len()
        );
    }

    #[test]
    fn as_geobuf_emits_expected_sql() {
        let terminal = empty_aqs().as_geobuf("the_geom");
        let acc = build_row_aggregate_select(&terminal.qs, &terminal.agg)
            .expect("SQL build must succeed");
        let (sql, binds) = acc.into_parts();
        assert!(
            sql.starts_with("SELECT ST_AsGeobuf(__djogi_row, $1) FROM ("),
            "unexpected outer SELECT shape: {sql}"
        );
        assert!(sql.ends_with(") AS __djogi_row"), "missing alias: {sql}");
        // One bind: geom_name
        assert_eq!(
            binds.len(),
            1,
            "expected 1 bind param for Geobuf, got {}: {sql}",
            binds.len()
        );
    }

    #[test]
    fn queryset_as_mvt_synthesises_empty_annotation() {
        // QuerySet::as_mvt should produce a terminal with the same
        // SQL shape as AnnotatedQuerySet::as_mvt over an empty
        // annotation tuple.
        let qs_terminal: AsMvtTerminal<TileFeature, EmptyAnnotation> =
            QuerySet::<TileFeature>::new().as_mvt("layer");
        let aqs_terminal: AsMvtTerminal<TileFeature, EmptyAnnotation> = empty_aqs().as_mvt("layer");

        let qs_sql = build_row_aggregate_select(&qs_terminal.qs, &qs_terminal.agg)
            .expect("queryset terminal builds SQL")
            .into_parts()
            .0;
        let aqs_sql = build_row_aggregate_select(&aqs_terminal.qs, &aqs_terminal.agg)
            .expect("annotated terminal builds SQL")
            .into_parts()
            .0;
        assert_eq!(
            qs_sql, aqs_sql,
            "QuerySet::as_mvt and AnnotatedQuerySet::as_mvt over empty annotation must produce identical SQL"
        );
    }

    #[tokio::test]
    async fn as_mvt_short_circuits_on_none_queryset() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }

        let (_cleanup, mut ctx) = testing::setup_test_db_with_extensions(&["postgis"])
            .await
            .expect("DATABASE_URL must be set for row-aggregate none short-circuit tests");

        let bytes = QuerySet::<TileFeature>::new()
            .none()
            .as_mvt("layer")
            .fetch_one(&mut ctx)
            .await
            .expect("none queryset should short-circuit before emitting SQL");

        assert!(
            bytes.is_empty(),
            "none queryset should return an empty payload without DB round-trip"
        );

        drop(_cleanup);
    }

    #[tokio::test]
    async fn as_geobuf_short_circuits_on_none_queryset() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }

        let (_cleanup, mut ctx) = testing::setup_test_db_with_extensions(&["postgis"])
            .await
            .expect("DATABASE_URL must be set for row-aggregate none short-circuit tests");

        let bytes = QuerySet::<TileFeature>::new()
            .none()
            .as_geobuf("location")
            .fetch_one(&mut ctx)
            .await
            .expect("none queryset should short-circuit before emitting SQL");

        assert!(
            bytes.is_empty(),
            "none queryset should return an empty payload without DB round-trip"
        );

        drop(_cleanup);
    }

    // ── Issue #71 — alias-collision guard on row-aggregate terminals ─────────
    //
    // `AsMvtTerminal::fetch_one` and `AsGeobufTerminal::fetch_one` consume
    // qualify via the wrapped `AnnotatedQuerySet`; the spatial SQL builder
    // (`djogi/src/query/sql.rs:1804-1813`) emits the same
    // `SELECT * FROM (…) AS __djogi_q WHERE <alias> …` outer qualify wrap as
    // `AnnotatedQuerySet::fetch_all`. A window alias that shadows a model column
    // must be rejected before SQL emission, not surface as a cryptic Postgres
    // "column reference is ambiguous" error at execute time.
    //
    // Two layers of regression pin:
    //
    // 1. Unit-level (database-free): the `*_alias_colliding_*` tests below call
    //    `IntoAggregateTuple::check_no_column_collision` directly — same framing
    //    as the analogous annotate.rs tests — to pin the validator logic against
    //    `TileFeature::COLUMNS`. Fast, runnable without DATABASE_URL.
    //
    // 2. Terminal-level (DATABASE_URL-gated): `*_fetch_one_rejects_colliding_alias`
    //    tests call the actual `fetch_one` methods with a non-empty (non-`.none()`)
    //    queryset and a colliding alias. The collision check fires before any DB
    //    access so the DB is never queried, but the test verifies that removing
    //    `check_no_column_collision` from `fetch_one` would cause the assertion to
    //    fail (the query would fall through to SQL emission / DB execution instead
    //    of returning `DjogiError::Validation`).

    #[test]
    fn as_mvt_terminal_alias_colliding_with_tile_feature_column_returns_validation_error() {
        // `TileFeature::COLUMNS = &["id", "geom", "name"]`. A RowNumber aliased
        // to "id" must be rejected before SQL emission so the MVT terminal
        // surfaces a typed DjogiError::Validation instead of a Postgres
        // ambiguous-column error when the qualify outer-WHERE wraps the inner
        // SELECT.
        let rn = RowNumber::new().alias("id");
        let result = IntoAggregateTuple::check_no_column_collision(&rn, TileFeature::COLUMNS);
        assert!(
            matches!(result, Err(crate::DjogiError::Validation(_))),
            "MVT terminal: alias colliding with TileFeature::COLUMNS must yield \
             DjogiError::Validation, got: {result:?}"
        );
        if let Err(crate::DjogiError::Validation(msg)) = result {
            assert!(
                msg.contains("id"),
                "error message must name the conflicting alias, got: {msg}"
            );
        }
    }

    #[test]
    fn as_geobuf_terminal_alias_colliding_with_tile_feature_column_returns_validation_error() {
        // Same guard as the MVT terminal — AsGeobufTerminal::fetch_one routes
        // qualify through the same spatial SQL builder that emits the
        // __djogi_q outer-WHERE wrap. A RowNumber aliased to "geom" (another
        // TileFeature column) must be rejected.
        let rn = RowNumber::new().alias("geom");
        let result = IntoAggregateTuple::check_no_column_collision(&rn, TileFeature::COLUMNS);
        assert!(
            matches!(result, Err(crate::DjogiError::Validation(_))),
            "Geobuf terminal: alias colliding with TileFeature::COLUMNS must yield \
             DjogiError::Validation, got: {result:?}"
        );
        if let Err(crate::DjogiError::Validation(msg)) = result {
            assert!(
                msg.contains("geom"),
                "error message must name the conflicting alias, got: {msg}"
            );
        }
    }

    #[test]
    fn as_mvt_terminal_non_colliding_alias_passes_collision_check() {
        // A RowNumber aliased to a name that is not in TileFeature::COLUMNS
        // must pass — only collisions are rejected.
        let rn = RowNumber::new().alias("rank");
        assert!(
            IntoAggregateTuple::check_no_column_collision(&rn, TileFeature::COLUMNS).is_ok(),
            "MVT terminal: alias not matching any TileFeature column must pass the collision check"
        );
    }

    // ── Terminal-level regression pins (DATABASE_URL-gated) ──────────────────

    #[tokio::test]
    async fn as_mvt_terminal_fetch_one_rejects_colliding_alias() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }

        // A real context satisfies `fetch_one`'s lifetime bound; the alias-
        // collision check fires before any DB access, so no SQL is emitted and
        // no DB query is executed. Removing `check_no_column_collision` from
        // `AsMvtTerminal::fetch_one` would let this query fall through to SQL
        // emission and a Postgres error, breaking the assertion below.
        let (_cleanup, mut ctx) = testing::setup_test_db()
            .await
            .expect("DATABASE_URL must be set for row-aggregate terminal collision tests");

        // `QuerySet::new()` is non-empty (no `.none()`), so `fetch_one`
        // reaches the `check_no_column_collision` guard. Alias "id" collides
        // with `TileFeature::COLUMNS = &["id", "geom", "name"]`.
        let result = QuerySet::<TileFeature>::new()
            .annotate(|_| RowNumber::new().alias("id"))
            .as_mvt("layer")
            .fetch_one(&mut ctx)
            .await;

        assert!(
            matches!(result, Err(crate::DjogiError::Validation(_))),
            "AsMvtTerminal::fetch_one must reject alias colliding with TileFeature column, \
             got: {result:?}"
        );
        if let Err(crate::DjogiError::Validation(msg)) = result {
            assert!(
                msg.contains("id"),
                "error message must name the conflicting alias, got: {msg}"
            );
        }

        drop(_cleanup);
    }

    #[tokio::test]
    async fn as_geobuf_terminal_fetch_one_rejects_colliding_alias() {
        if std::env::var("DATABASE_URL").is_err() {
            return;
        }

        // Same terminal-level pin as the MVT variant above, exercising
        // `AsGeobufTerminal::fetch_one`. Removing `check_no_column_collision`
        // from that method would let this query fall through to SQL emission
        // instead of returning `DjogiError::Validation`, failing the assertion.
        let (_cleanup, mut ctx) = testing::setup_test_db()
            .await
            .expect("DATABASE_URL must be set for row-aggregate terminal collision tests");

        // Alias "geom" collides with `TileFeature::COLUMNS = &["id", "geom", "name"]`.
        let result = QuerySet::<TileFeature>::new()
            .annotate(|_| RowNumber::new().alias("geom"))
            .as_geobuf("geom")
            .fetch_one(&mut ctx)
            .await;

        assert!(
            matches!(result, Err(crate::DjogiError::Validation(_))),
            "AsGeobufTerminal::fetch_one must reject alias colliding with TileFeature column, \
             got: {result:?}"
        );
        if let Err(crate::DjogiError::Validation(msg)) = result {
            assert!(
                msg.contains("geom"),
                "error message must name the conflicting alias, got: {msg}"
            );
        }

        drop(_cleanup);
    }
}
