//! Ordering expressions — accumulated by `QuerySet::order_by`.
//! # What
//! An [`OrderExpr`] is the minimal description of a single `ORDER BY`
//! clause element. There are two variants:
//! - [`OrderExpr::Column`] — a column name, sort direction, and optional NULLS
//! position. Produced by [`FieldRef::asc`] / [`FieldRef::desc`] and the fluent
//! [`OrderExpr::nulls_first`] / [`OrderExpr::nulls_last`] modifiers.
//! - [`OrderExpr::SpatialDistance`] — an expression-backed ordering that emits
//! `ST_Distance(col, ST_Point($lon, $lat)::geography) ASC, <pk> ASC`. Produced
//! by [`crate::query::field::FieldRef<M, GeoPoint>::order_by_distance`] when
//! the `spatial` feature flag is enabled.
//! # Why an enum
//! The `Column` variant covers ordinary `column ASC|DESC NULLS FIRST|LAST`
//! ordering. The `SpatialDistance` variant carries the
//! `GeoPoint` center and the model's PK column so a single `OrderExpr`
//! emits two comma-separated `ORDER BY` terms (the `ST_Distance`
//! expression plus the PK tiebreak). A wrapper enum around an
//! always-Copy column-only struct would force callers and tests to
//! double-name every site; a side-field on `QuerySet` would diverge
//! from the one-IR principle the rest of the query engine follows.
//! Dropping `Copy` from `OrderExpr` (because `SpatialDistance` holds a
//! `Clone`-but-not-`Copy` `GeoPoint`) is the price of admission.
//! # Primary-key tiebreak
//! [`OrderExpr::SpatialDistance`] always appends the model's PK column as
//! a deterministic tiebreaker. Equidistant rows would otherwise sort in
//! arbitrary order, causing flaky tests and inconsistent pagination. The
//! PK column is captured at construction time (see
//! [`OrderExpr::spatial_distance_with_pk_tiebreak`]).
//! # Where
//! - Accumulated by [`crate::query::queryset::QuerySet::order_by`].
//! - Emitted to SQL by [`OrderExpr::emit`], called from `query::sql`'s
//! ordering tail helpers.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::field::FieldRef;

/// Sort direction for a single `ORDER BY` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Direction {
    /// `ASC` — smallest first.
    Asc,
    /// `DESC` — largest first.
    Desc,
}

/// NULL positioning for an ordering expression.
/// `Default` lets the Postgres default win (NULLS LAST for ASC, NULLS FIRST
/// for DESC); pick `First` / `Last` explicitly for deterministic ordering
/// across dialects or documented row ordering contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NullsOrder {
    /// Postgres default — no `NULLS FIRST|LAST` clause emitted.
    Default,
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// A single `ORDER BY` element.
/// This is an enum rather than a struct so spatial ordering can emit two
/// comma-separated terms (`ST_Distance(…) ASC, pk ASC`) from one `OrderExpr`.
/// The `Column` variant holds the original column-name, direction, and nulls
/// fields; `SpatialDistance` (feature-gated) holds the spatial payload.
/// Direct construction is only possible via [`FieldRef::asc`] / [`FieldRef::desc`]
/// (which produce `Column`) and
/// [`crate::query::field::FieldRef<M, GeoPoint>::order_by_distance`] (which
/// produces `SpatialDistance`). All construction paths ensure the column string
/// has already been validated by [`crate::ident::assert_plain_ident`].
/// # Note on `Copy`
/// `OrderExpr` was `Copy` before. The `SpatialDistance` variant stores a
/// `GeoPoint`, which is `Clone` but not `Copy`. `Copy` is therefore not derived
/// on the enum — callers that need a second handle should `.clone()` explicitly.
#[derive(Debug, Clone)]
pub enum OrderExpr {
    /// Standard column ordering: `<column> [ASC|DESC] [NULLS FIRST|LAST]`.
    /// Produced by [`FieldRef::asc`] / [`FieldRef::desc`] and the fluent
    /// `nulls_first` / `nulls_last` modifiers.
    /// `#[non_exhaustive]` seals downstream construction — callers cannot
    /// name the variant fields outside the `djogi` crate. This mirrors the
    /// injection-prevention guarantee the previous `OrderExpr` struct provided
    /// via `pub(crate)` field visibility.
    #[non_exhaustive]
    Column {
        /// Column name — always a validated macro-baked literal.
        column: &'static str,
        /// Sort direction.
        direction: Direction,
        /// NULL positioning.
        nulls: NullsOrder,
    },

    /// Spatial distance ordering with a deterministic primary-key tiebreak.
    /// Emits two comma-separated ORDER BY terms:
    /// ```sql
    /// ST_Distance(<col>, ST_Point($lon, $lat)::geography) ASC, <pk_col> ASC
    /// ```
    /// The PK tiebreak is always `<pk_col> ASC` and is appended unconditionally
    /// per the plan's RQ6 ("order_by_distance determinism"). Callers who chain
    /// additional `.order_by(...)` calls get their keys appended after the tiebreak.
    /// `#[non_exhaustive]` seals downstream construction — the `center` `GeoPoint`
    /// and column strings must originate from `FieldRef::order_by_distance`.
    #[cfg(feature = "spatial")]
    #[non_exhaustive]
    SpatialDistance {
        /// The geography column name — a `&'static str` from the macro descriptor.
        field_column: &'static str,
        /// The reference point.
        center: crate::geo::GeoPoint,
        /// The model's primary key column name — used for the tiebreak.
        /// Captured at construction time from `Model::pk_column()`.
        pk_column: &'static str,
    },
}

impl OrderExpr {
    /// Force `NULLS FIRST` on a `Column` ordering expression.
    /// Consumes and returns `Self` for fluent chaining:
    /// `f.title.asc().nulls_first()`.
    /// When the `spatial` feature is on, this is a no-op on `SpatialDistance`
    /// that variant carries no NULL position.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn nulls_first(mut self) -> Self {
        match self {
            OrderExpr::Column { ref mut nulls, .. } => {
                *nulls = NullsOrder::First;
            }
            #[cfg(feature = "spatial")]
            OrderExpr::SpatialDistance { .. } => {}
        }
        self
    }

    /// Force `NULLS LAST` on a `Column` ordering expression.
    /// Consumes and returns `Self` for fluent chaining:
    /// `f.title.desc().nulls_last()`.
    /// When the `spatial` feature is on, this is a no-op on `SpatialDistance`
    /// that variant carries no NULL position.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn nulls_last(mut self) -> Self {
        match self {
            OrderExpr::Column { ref mut nulls, .. } => {
                *nulls = NullsOrder::Last;
            }
            #[cfg(feature = "spatial")]
            OrderExpr::SpatialDistance { .. } => {}
        }
        self
    }

    /// Emit the SQL fragment for this ordering expression onto `acc`.
    /// `Column` emits `<col> [ASC|DESC] [NULLS FIRST|LAST]`.
    /// `SpatialDistance` emits two comma-separated terms:
    /// `ST_Distance(<col>, ST_Point($lon, $lat)::geography) ASC, <pk_col> ASC`.
    /// The `table_qualifier` is prepended as `<table>.` before column names when
    /// the ordering is used inside a `select_related` joined query. Pass `None`
    /// for single-table queries.
    /// # Bind parameters
    /// `Column` emits no bind parameters.
    /// `SpatialDistance` binds `$lon` and `$lat` for the ST_Point expression.
    pub(crate) fn emit(&self, acc: &mut SqlAccumulator, table_qualifier: Option<&'static str>) {
        match self {
            OrderExpr::Column {
                column,
                direction,
                nulls,
            } => {
                if let Some(table) = table_qualifier {
                    acc.push_sql(table);
                    acc.push_sql(".");
                }
                acc.push_sql(column);
                match direction {
                    Direction::Asc => acc.push_sql(" ASC"),
                    Direction::Desc => acc.push_sql(" DESC"),
                }
                match nulls {
                    NullsOrder::First => acc.push_sql(" NULLS FIRST"),
                    NullsOrder::Last => acc.push_sql(" NULLS LAST"),
                    NullsOrder::Default => {}
                }
            }

            #[cfg(feature = "spatial")]
            OrderExpr::SpatialDistance {
                field_column,
                center,
                pk_column,
            } => {
                // First term: ST_Distance(col, ST_Point($lon, $lat)::geography) ASC
                acc.push_sql("ST_Distance(");
                if let Some(table) = table_qualifier {
                    acc.push_sql(table);
                    acc.push_sql(".");
                }
                acc.push_sql(field_column);
                acc.push_sql(", ST_Point(");
                acc.push_bind(center.lon);
                acc.push_sql(", ");
                acc.push_bind(center.lat);
                acc.push_sql(")::geography) ASC");
                // Second term: pk_col ASC — deterministic tiebreak.
                acc.push_sql(", ");
                if let Some(table) = table_qualifier {
                    acc.push_sql(table);
                    acc.push_sql(".");
                }
                acc.push_sql(pk_column);
                acc.push_sql(" ASC");
            }
        }
    }
}

/// Spatial ordering constructors — gated on the `spatial` feature flag.
#[cfg(feature = "spatial")]
impl OrderExpr {
    /// Build an `OrderExpr::SpatialDistance` for the given geography column and
    /// center point.
    /// The `pk_column` is appended as a deterministic tiebreaker — same
    /// distance buckets must yield the same row order across pages. Pass
    /// `M::pk_column()` here; for models with the default primary key this
    /// is `"id"`.
    /// Emits:
    /// ```sql
    /// ST_Distance(<field_column>, ST_Point($lon, $lat)::geography) ASC, <pk_column> ASC
    /// ```
    pub(crate) fn spatial_distance_with_pk_tiebreak(
        field_column: &'static str,
        center: crate::geo::GeoPoint,
        pk_column: &'static str,
    ) -> Self {
        OrderExpr::SpatialDistance {
            field_column,
            center,
            pk_column,
        }
    }
}

impl<M: Model, V> FieldRef<M, V> {
    /// Ascending ordering for this column. `NULLS` position is left at the
    /// Postgres default (NULLS LAST for ASC); call `.nulls_first()` /
    /// `.nulls_last()` on the result to override.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn asc(self) -> OrderExpr {
        OrderExpr::Column {
            column: self.column(),
            direction: Direction::Asc,
            nulls: NullsOrder::Default,
        }
    }

    /// Descending ordering for this column. `NULLS` position defaults to the
    /// Postgres default (NULLS FIRST for DESC); call `.nulls_last()` to
    /// override.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn desc(self) -> OrderExpr {
        OrderExpr::Column {
            column: self.column(),
            direction: Direction::Desc,
            nulls: NullsOrder::Default,
        }
    }
}

/// Accept a single `OrderExpr` where a `Vec<OrderExpr>` is expected
/// `QuerySet::order_by(|f| f.title.asc())` closes over a single expression
/// and this `From` impl lifts it into the one-element vec the builder stores.
impl From<OrderExpr> for Vec<OrderExpr> {
    fn from(o: OrderExpr) -> Self {
        vec![o]
    }
}

impl OrderExpr {
    /// Macro-only constructor for a `Column` ordering — 4.
    /// `#[non_exhaustive]` on the variant prevents downstream
    /// (user-crate) `OrderExpr::Column { column, direction, nulls }`
    /// literals; the proxy macro emits `default_order = [(field,
    /// Asc|Desc),...]` overrides in the *user's* crate, so we expose
    /// this `#[doc(hidden)]` constructor as the macro-only path. Mirrors
    /// the [`crate::query::Condition::__from_raw_sql_fragment`] pattern
    /// from the same task.
    /// Not part of the user-facing API. The macro never quotes the
    /// `column` argument at the call site, but `#[model(default_order)]`
    /// only accepts a parsed `syn::Ident` (validated against the
    /// unquoted-identifier byte grammar in
    /// `model::proxy::parse_default_order_list`), so an out-of-grammar
    /// column name cannot reach this constructor.
    #[doc(hidden)]
    #[must_use]
    pub fn __from_macro_column(
        column: &'static str,
        direction: Direction,
        nulls: NullsOrder,
    ) -> Self {
        OrderExpr::Column {
            column,
            direction,
            nulls,
        }
    }
}
