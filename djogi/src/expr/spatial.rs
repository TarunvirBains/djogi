//! Spatial expression nodes — gated behind the `spatial` feature flag.
//!
//! # What
//!
//! [`SpatialExpr`] is an internal sub-IR that plugs into [`super::node::ExprNode`]
//! via the `ExprNode::Spatial(SpatialExpr)` variant. It carries variants for:
//!
//! - [`SpatialExpr::Within`] — emits `ST_DWithin(<col>, ST_Point($lon, $lat)::geography, $r)`
//!   (radius-based predicate)
//! - [`SpatialExpr::Distance`] — emits `ST_Distance(<col>, ST_Point($lon, $lat)::geography)`
//! - [`SpatialExpr::Contains`] — emits `ST_Contains(<col>::geometry, $1::bytea::geometry)`
//! - [`SpatialExpr::Intersects`] — emits `ST_Intersects(<col>, $1::bytea::geography)`
//! - [`SpatialExpr::Touches`] — emits `ST_Touches(<col>::geometry, $1::bytea::geometry)`
//! - [`SpatialExpr::WithinShape`] — emits `ST_Within(<col>::geometry, $1::bytea::geometry)`
//! - [`SpatialExpr::BoundedBy`] — bbox prefilter using `ST_MakeEnvelope` + `&&`
//!
//! # Naming note for `WithinShape`
//!
//! The variant is named `WithinShape` internally to avoid a collision with the
//! radius-based `Within` variant. The public method on
//! `FieldRef<M, G: GeographyValue>` is still called `.within(&geom)` — the two
//! methods coexist on different receivers (`.within_km` is `FieldRef<M, GeoPoint>`
//! only; `.within` is generic over any `GeographyValue`) so there is no ambiguity.
//!
//! # Bind discipline
//!
//! All floating-point values (longitude, latitude, radius) and raw EWKB bytes
//! flow through [`crate::pg::accumulator::SqlAccumulator::push_bind`]. Column
//! names are `&'static str` values validated upstream by `assert_plain_ident` at
//! `FieldRef` construction time — it is safe to push them via `push_sql`.
//!
//! # Why two separate variants rather than one with an optional radius?
//!
//! `Within` is a boolean predicate (`ST_DWithin` returns `bool`); `Distance` is
//! a numeric expression (`ST_Distance` returns `float8`). The distinct variants
//! let the typed [`super::Expr<T>`] wrapper carry the correct phantom type (`bool`
//! vs `f64`) without any runtime type tag.
//!
//! # Where
//!
//! - [`crate::query::field`] is the only non-spatial module that produces these
//!   nodes — `FieldRef<M, GeoPoint>::within_km` builds `Within`;
//!   `FieldRef<M, GeoPoint>::distance_to` builds `Distance`;
//!   the shape-predicate methods on `FieldRef<M, G: GeographyValue>` build
//!   `Contains` / `Intersects` / `Touches` / `WithinShape`;
//!   `FieldRef<M, G: GeographyValue>::bounded_by` builds `BoundedBy`;
//!   and `FieldRef<M, GeoPoint>::order_by_distance` captures `Distance`
//!   indirectly via [`crate::query::order::OrderExpr::SpatialDistance`].
//! - [`super::sql::emit_expr`] has one arm for `ExprNode::Spatial(s)` that
//!   delegates to [`SpatialExpr::emit`].

#[cfg(feature = "spatial")]
use crate::geo::GeoPoint;
#[cfg(feature = "spatial")]
use crate::pg::accumulator::SqlAccumulator;

/// Spatial expression node — plugged into the query IR via `ExprNode::Spatial`.
///
/// Variants carry a `&'static str` column name (baked in by the `#[model]`
/// macro, validated by `assert_plain_ident`) plus the query parameters needed
/// for each PostGIS function call.
///
/// The emitter pushes all user-supplied values as bind parameters and embeds
/// the column name as raw SQL.
#[cfg(feature = "spatial")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SpatialExpr {
    /// `ST_DWithin(<field>, ST_Point($lon, $lat)::geography, $radius_m)`
    ///
    /// Returns a boolean: `true` when `<field>` is within `radius_meters`
    /// meters of `center` using PostGIS's `GEOGRAPHY` distance model
    /// (great-circle distance, not Euclidean).
    Within {
        /// Column name — a `&'static str` from the macro descriptor.
        /// Validated by `assert_plain_ident`; safe to push as raw SQL.
        field_column: &'static str,
        /// The query center point.
        center: GeoPoint,
        /// Search radius in meters.
        radius_meters: f64,
    },

    /// `ST_Distance(<field>, ST_Point($lon, $lat)::geography)`
    ///
    /// Returns a `float8` (Rust `f64`): the great-circle distance in meters
    /// between `<field>` and `center`. Exposed as a first-class composable
    /// expression method via [`crate::query::field::FieldRef::distance_to`],
    /// enabling `.filter`, `.annotate`, and `.order_by` composition with the
    /// distance expression.
    ///
    /// The ordering path embeds `ST_Distance` SQL inline in
    /// `OrderExpr::SpatialDistance::emit` for performance, but this variant
    /// powers the expression-IR path that lets callers compose:
    /// `filter_expr(|f| f.loc().distance_to(&center).lt(1000.0))`.
    Distance {
        /// Column name — a `&'static str` from the macro descriptor.
        /// Validated by `assert_plain_ident`; safe to push as raw SQL.
        field_column: &'static str,
        /// The reference point.
        center: GeoPoint,
    },

    // ── Shape-based predicates ────────────────────────────────────────────────
    /// `ST_Contains(<col>::geometry, $1::bytea::geometry)`
    ///
    /// Returns `true` when the geometry stored in `<col>` entirely contains
    /// the bound geometry. The bound geometry goes through `push_bind` as
    /// its EWKB byte representation; see [`emit_binary_predicate`] for the
    /// cast rationale.
    ///
    /// Constructed by [`crate::query::field::FieldRef::contains`].
    Contains {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test containment against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Intersects(<col>, $1::bytea::geography)`
    ///
    /// Returns `true` when the geometry stored in `<col>` and the bound
    /// geometry share at least one point. The bound geometry goes through
    /// `push_bind` as its EWKB byte representation. This is the only
    /// shape predicate with a native `geography` overload — see
    /// [`emit_binary_predicate`].
    ///
    /// Constructed by [`crate::query::field::FieldRef::intersects`].
    Intersects {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test intersection against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Touches(<col>::geometry, $1::bytea::geometry)`
    ///
    /// Returns `true` when the geometry stored in `<col>` and the bound
    /// geometry share boundary points but no interior points (touch but do
    /// not overlap). The bound geometry goes through `push_bind`; see
    /// [`emit_binary_predicate`] for the cast rationale.
    ///
    /// Constructed by [`crate::query::field::FieldRef::touches`].
    Touches {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test touch against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Within(<col>::geometry, $1::bytea::geometry)`
    ///
    /// Returns `true` when the geometry stored in `<col>` is entirely within
    /// the bound geometry. Named `WithinShape` internally to avoid a
    /// variant-name collision with the radius-based [`SpatialExpr::Within`];
    /// the public method on `FieldRef` is still called `.within(&geom)`.
    /// See [`emit_binary_predicate`] for the cast rationale.
    ///
    /// Constructed by [`crate::query::field::FieldRef::within`].
    WithinShape {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test containment by.
        other_ewkb: Vec<u8>,
    },

    /// `ST_MakeEnvelope($min_lon, $min_lat, $max_lon, $max_lat, 4326)::geography && <col>`
    ///
    /// GiST-indexed bbox prefilter — returns `true` when the geometry stored
    /// in `<col>` overlaps the bounding box defined by the four coordinate
    /// bounds. Uses the `&&` operator so Postgres can use a GiST index for
    /// fast pre-filtering before more expensive shape predicates.
    ///
    /// Constructed by [`crate::query::field::FieldRef::bounded_by`].
    /// The Rust API accepts `(min_lat, min_lon, max_lat, max_lon)` to match
    /// the `GeoPoint` (lat, lon) convention; the emitter reorders to
    /// Postgres's (x, y) = (lon, lat) convention.
    BoundedBy {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// Southern bound (minimum latitude).
        min_lat: f64,
        /// Western bound (minimum longitude).
        min_lon: f64,
        /// Northern bound (maximum latitude).
        max_lat: f64,
        /// Eastern bound (maximum longitude).
        max_lon: f64,
    },

    // Scalar geometry/area helpers
    /// `ST_Area($1::bytea::geography)`
    ///
    /// Returns `f64` — the area in **square meters** of the bound geometry,
    /// computed on the spheroid via Postgres's `geography`-typed
    /// `ST_Area` overload (the geometry-typed overload returns square
    /// degrees, which is rarely what callers want). Mirrors the
    /// `::geography` cast convention used by [`Self::Within`] /
    /// [`Self::Distance`] so the meters-units invariant of the
    /// Phase 6 spatial surface holds for T17 too.
    ///
    /// Constructed by [`super::Expr::area_of`]. Composes with the
    /// `Expr<f64>` arithmetic IR for ratios such as
    /// `area_of_intersection(a, b) / area_of(a)`.
    Area {
        /// EWKB encoding of the geometry whose area is computed.
        geom_ewkb: Vec<u8>,
    },

    /// `ST_Intersection($1::bytea::geometry, $2::bytea::geometry)`
    ///
    /// Returns a geometry — the spatial intersection of the two bound
    /// geometries. The empty geometry is the natural sentinel when the
    /// two inputs do not overlap; Postgres's `ST_Area` over an empty
    /// geometry returns `0.0`, which is the correct "no overlap"
    /// answer for the territory-overlap-percentage demo use case.
    ///
    /// Both args are bound as raw EWKB `bytea` and cast at query time —
    /// matches the cast discipline of the geometry-only shape predicates
    /// (`ST_Contains` / `ST_Touches` / `ST_Within`) which have no
    /// `geography` overload in PostGIS 3.x.
    ///
    /// # No public typed constructor today
    ///
    /// This variant is reachable only by IR-level construction; the public
    /// surface ships [`super::Expr::area_of_intersection`] (which uses the
    /// fused [`Self::AreaOfIntersection`] variant) because the framework
    /// does not yet model a geometry-typed `Expr<G>` (no `Expr<Polygon>`
    /// codec path). Splitting the standalone `intersection_of` constructor
    /// out is a future-phase amendment once geometry-typed `Expr` lands;
    /// the variant exists today so the SpatialExpr family is structurally
    /// complete and tests can pin its emission shape.
    #[allow(dead_code)]
    Intersection {
        /// EWKB encoding of the first geometry argument.
        a_ewkb: Vec<u8>,
        /// EWKB encoding of the second geometry argument.
        b_ewkb: Vec<u8>,
    },

    /// `ST_Area(ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography)`
    ///
    /// Composed shape — returns `f64` square meters of the intersection of
    /// two bound geometries. Equivalent to nesting [`Self::Intersection`]
    /// inside [`Self::Area`], emitted as a single inline form because the
    /// IR does not currently model geometry-typed intermediate `Expr` nodes
    /// (see the rustdoc on [`Self::Intersection`]).
    ///
    /// This is the canonical territory-overlap-percentage expression: the
    /// demo uses it as the numerator of
    /// `area_of_intersection(a, b) / area_of(a)`. Constructed by
    /// [`super::Expr::area_of_intersection`].
    AreaOfIntersection {
        /// EWKB encoding of the first geometry argument.
        a_ewkb: Vec<u8>,
        /// EWKB encoding of the second geometry argument.
        b_ewkb: Vec<u8>,
    },
    // Cluster E round-5 BLOCK-2 closure: convex-hull was migrated
    // out of this enum into `AggOp::SpatialConvexHull`. The old
    // `SpatialExpr::ConvexHull{..}` variant silently dropped
    // `AggregateExpr` modifiers (.distinct/.filter/.over/.order_by)
    // because those mutate `ExprNode::Aggregate` only. Routing
    // through `AggOp` puts ConvexHull on the same modifier substrate
    // as the rest of the spatial aggregate family.
}

#[cfg(feature = "spatial")]
impl SpatialExpr {
    /// Emit the SQL fragment for this spatial expression onto `acc`.
    ///
    /// - The column name is pushed via `push_sql` (trusted static identifier).
    /// - Longitude, latitude, radius, and EWKB bytes are pushed via
    ///   `push_bind` — no string interpolation of user-supplied values.
    ///
    /// ## SQL shapes
    ///
    /// `Within` emits:
    /// ```sql
    /// ST_DWithin(<col>, ST_Point($1, $2)::geography, $3)
    /// ```
    /// where `$1 = center.lon`, `$2 = center.lat`, `$3 = radius_meters`.
    ///
    /// `Distance` emits:
    /// ```sql
    /// ST_Distance(<col>, ST_Point($1, $2)::geography)
    /// ```
    /// where `$1 = center.lon`, `$2 = center.lat`.
    ///
    /// `Contains`, `Touches`, `WithinShape` emit:
    /// ```sql
    /// ST_<Function>(<col>::geometry, $1::bytea::geometry)
    /// ```
    /// because in PostGIS 3.x these three functions only have a `geometry`
    /// overload — `ST_Contains(geography, ...)` etc. do not exist.
    ///
    /// `Intersects` emits:
    /// ```sql
    /// ST_Intersects(<col>, $1::bytea::geography)
    /// ```
    /// because `ST_Intersects` has a native `geography` overload.
    ///
    /// In both cases `$1` is bound as raw EWKB `bytea` and cast at query
    /// time — `tokio_postgres` prepares the parameter as `bytea`
    /// (which matches `Vec<u8>: ToSql`) and Postgres performs the
    /// `bytea::geometry` / `bytea::geography` cast via the implicit
    /// PostGIS input functions.
    ///
    /// `BoundedBy` emits:
    /// ```sql
    /// ST_MakeEnvelope($1, $2, $3, $4, 4326)::geography && <col>
    /// ```
    /// where `$1 = min_lon`, `$2 = min_lat`, `$3 = max_lon`, `$4 = max_lat`.
    /// The `&&` operator enables GiST index usage for cheap bbox prefiltering.
    ///
    /// The parameter numbers shown are relative to when `emit` is called —
    /// the accumulator's global counter determines the actual `$n` values
    /// in context.
    pub(crate) fn emit(&self, acc: &mut SqlAccumulator) {
        match self {
            SpatialExpr::Within {
                field_column,
                center,
                radius_meters,
            } => {
                // ST_DWithin(col, ST_Point($lon, $lat)::geography, $radius)
                acc.push_sql("ST_DWithin(");
                acc.push_sql(field_column);
                acc.push_sql(", ST_Point(");
                acc.push_bind(center.lon);
                acc.push_sql(", ");
                acc.push_bind(center.lat);
                acc.push_sql(")::geography, ");
                acc.push_bind(*radius_meters);
                acc.push_sql(")");
            }
            SpatialExpr::Distance {
                field_column,
                center,
            } => {
                // ST_Distance(col, ST_Point($lon, $lat)::geography)
                acc.push_sql("ST_Distance(");
                acc.push_sql(field_column);
                acc.push_sql(", ST_Point(");
                acc.push_bind(center.lon);
                acc.push_sql(", ");
                acc.push_bind(center.lat);
                acc.push_sql(")::geography)");
            }
            // ── Shape predicates ──────────────────────────────────────────────
            SpatialExpr::Contains {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, ShapePredicate::Contains, field_column, other_ewkb);
            }
            SpatialExpr::Intersects {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, ShapePredicate::Intersects, field_column, other_ewkb);
            }
            SpatialExpr::Touches {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, ShapePredicate::Touches, field_column, other_ewkb);
            }
            SpatialExpr::WithinShape {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, ShapePredicate::Within, field_column, other_ewkb);
            }
            SpatialExpr::BoundedBy {
                field_column,
                min_lat,
                min_lon,
                max_lat,
                max_lon,
            } => {
                // Postgres order: ST_MakeEnvelope(min_x, min_y, max_x, max_y, srid)
                // where x = longitude, y = latitude. Our API keeps lat first to match
                // GeoPoint convention; emission reorders.
                acc.push_sql("ST_MakeEnvelope(");
                acc.push_bind(*min_lon);
                acc.push_sql(", ");
                acc.push_bind(*min_lat);
                acc.push_sql(", ");
                acc.push_bind(*max_lon);
                acc.push_sql(", ");
                acc.push_bind(*max_lat);
                acc.push_sql(", 4326)::geography && ");
                acc.push_sql(field_column);
            }
            // T17 scalar geometry / area helpers
            SpatialExpr::Area { geom_ewkb } => {
                // ST_Area($n::bytea::geography) — geography overload returns
                // square meters; the geometry overload returns square degrees
                // and is the wrong unit for the demo use case.
                acc.push_sql("ST_Area(");
                push_ewkb_arg(acc, geom_ewkb, EwkbCast::Geography);
                acc.push_sql(")");
            }
            SpatialExpr::Intersection { a_ewkb, b_ewkb } => {
                // ST_Intersection($1::bytea::geometry, $2::bytea::geometry) —
                // PostGIS 3.x has no `geography` overload for ST_Intersection;
                // both args go through the `::geometry` cast pair (matches
                // the discipline of `emit_binary_predicate` for non-Intersects
                // shape predicates).
                acc.push_sql("ST_Intersection(");
                push_ewkb_arg(acc, a_ewkb, EwkbCast::Geometry);
                acc.push_sql(", ");
                push_ewkb_arg(acc, b_ewkb, EwkbCast::Geometry);
                acc.push_sql(")");
            }
            SpatialExpr::AreaOfIntersection { a_ewkb, b_ewkb } => {
                // ST_Area(ST_Intersection(..)::geography) — composed inline
                // because the IR does not yet model geometry-typed Expr
                // intermediates. The outer `::geography` cast keeps the
                // meters-units invariant from `Area` end-to-end.
                acc.push_sql("ST_Area(ST_Intersection(");
                push_ewkb_arg(acc, a_ewkb, EwkbCast::Geometry);
                acc.push_sql(", ");
                push_ewkb_arg(acc, b_ewkb, EwkbCast::Geometry);
                acc.push_sql(")::geography)");
            }
        }
    }
}

/// PostGIS cast target for an EWKB bind argument.
///
/// Used by [`push_ewkb_arg`] to keep the per-arm emit bodies free of
/// stringly-typed `"::bytea::geometry"` / `"::bytea::geography"` literals —
/// the variants are the only two PostGIS overload directions a binary EWKB
/// blob can flow into.
#[cfg(feature = "spatial")]
#[derive(Clone, Copy)]
enum EwkbCast {
    Geometry,
    Geography,
}

/// Push `$N::bytea::<cast>` for an EWKB bind. Centralises the 3-step splice
/// (`push_bind` + `::bytea` + `::geometry`/`::geography`) that every T17 arm
/// repeats — without the helper, each emit body is `acc.push_bind(...);
/// acc.push_sql("::bytea::geometry")` which a 4th arm would faithfully copy.
#[cfg(feature = "spatial")]
fn push_ewkb_arg(acc: &mut SqlAccumulator, ewkb: &[u8], cast: EwkbCast) {
    acc.push_bind(ewkb.to_vec());
    acc.push_sql(match cast {
        EwkbCast::Geometry => "::bytea::geometry",
        EwkbCast::Geography => "::bytea::geography",
    });
}

/// Which PostGIS shape predicate `emit_binary_predicate` should emit.
///
/// Replaces the previous stringly-typed `func: &'static str` parameter so a
/// typo (`"ST_intersects"`) cannot silently flip the geometry-cast logic.
/// The variant set is closed at compile time; adding a new predicate is a
/// single match-arm change rather than a fragile string comparison.
#[cfg(feature = "spatial")]
#[derive(Clone, Copy)]
enum ShapePredicate {
    Contains,
    Intersects,
    Touches,
    Within,
}

#[cfg(feature = "spatial")]
impl ShapePredicate {
    /// PostGIS function name as it appears in the emitted SQL.
    fn function_name(self) -> &'static str {
        match self {
            Self::Contains => "ST_Contains",
            Self::Intersects => "ST_Intersects",
            Self::Touches => "ST_Touches",
            Self::Within => "ST_Within",
        }
    }

    /// Whether this predicate needs a `::geometry` cast on both sides.
    ///
    /// Only `ST_Intersects` has a native `geography(geography, geography)`
    /// overload in PostGIS 3.x; the other three are geometry-only and need
    /// both arguments coerced before the call.
    fn needs_geometry_cast(self) -> bool {
        !matches!(self, Self::Intersects)
    }
}

/// Emit a binary spatial predicate call.
///
/// # Cast selection
///
/// PostGIS 3.x splits these four functions across two type families:
///
/// - `ST_Intersects` has native `geography` overloads, so both the column
///   and the bind stay in the `geography` space. The column reference is
///   emitted unadorned (it already has the `geography` column type) and the
///   bind is cast `::bytea::geography`.
/// - `ST_Contains`, `ST_Touches`, and `ST_Within` are **geometry-only**:
///   `ST_Contains(geography, geography)` etc. do not exist. Both sides are
///   cast to `geometry` — the column via `::geometry`, the bind via
///   `::bytea::geometry`.
///
/// # Bind encoding
///
/// `Vec<u8>: ToSql` binds as Postgres `bytea`. The target parameter type
/// registered at prepare time must therefore be `bytea`; the explicit
/// `$n::bytea::<type>` double-cast forces that. A plain `$n::geography`
/// (or `$n::geometry`) would make `tokio_postgres` prepare the parameter
/// as `geography` and reject the `Vec<u8>` bind, because `Vec<u8>` cannot
/// satisfy a `geography`-typed slot.
///
/// The column name flows through `push_sql` (already validated as a
/// plain identifier by `assert_plain_ident`); the EWKB bytes flow through
/// `push_bind`.
#[cfg(feature = "spatial")]
fn emit_binary_predicate(
    acc: &mut SqlAccumulator,
    predicate: ShapePredicate,
    field_column: &'static str,
    other_ewkb: &[u8],
) {
    let use_geometry = predicate.needs_geometry_cast();
    let col_cast = if use_geometry { "::geometry" } else { "" };
    let bind_cast = if use_geometry {
        EwkbCast::Geometry
    } else {
        EwkbCast::Geography
    };

    acc.push_sql(predicate.function_name());
    acc.push_sql("(");
    acc.push_sql(field_column);
    acc.push_sql(col_cast);
    acc.push_sql(", ");
    push_ewkb_arg(acc, other_ewkb, bind_cast);
    acc.push_sql(")");
}

#[cfg(all(test, feature = "spatial"))]
mod tests {
    use super::*;
    use crate::geo::GeoPoint;
    use crate::pg::accumulator::SqlAccumulator;

    /// `Within` must emit `ST_DWithin(...)` with the column name, and
    /// bind exactly three parameters: lon, lat, radius_meters.
    #[test]
    fn within_km_emits_st_dwithin() {
        let center = GeoPoint::new(37.7749, -122.4194).unwrap();
        let expr = SpatialExpr::Within {
            field_column: "location",
            center,
            radius_meters: 5000.0,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_DWithin"),
            "expected ST_DWithin in SQL; got: {sql}"
        );
        assert!(
            sql.contains("location"),
            "expected column name 'location' in SQL; got: {sql}"
        );
        // All three parameters (lon, lat, radius) must be bind params.
        assert_eq!(
            acc.bind_count(),
            3,
            "Within must bind exactly 3 params (lon, lat, radius_meters); got {}",
            acc.bind_count()
        );
        // Each parameter appears as a placeholder.
        assert!(
            sql.contains("$1") && sql.contains("$2") && sql.contains("$3"),
            "expected $1, $2, $3 in SQL; got: {sql}"
        );
    }

    /// `Distance` must emit `ST_Distance(...)` with the column name, and
    /// bind exactly two parameters: lon, lat.
    #[test]
    fn distance_emits_st_distance() {
        let center = GeoPoint::new(37.7749, -122.4194).unwrap();
        let expr = SpatialExpr::Distance {
            field_column: "location",
            center,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Distance"),
            "expected ST_Distance in SQL; got: {sql}"
        );
        assert!(
            sql.contains("location"),
            "expected column name 'location' in SQL; got: {sql}"
        );
        // Two parameters: lon and lat.
        assert_eq!(
            acc.bind_count(),
            2,
            "Distance must bind exactly 2 params (lon, lat); got {}",
            acc.bind_count()
        );
        assert!(
            sql.contains("$1") && sql.contains("$2"),
            "expected $1 and $2 in SQL; got: {sql}"
        );
    }

    /// The emitted SQL must NOT contain any user-supplied coordinate values
    /// as literal text — they must only appear as bind parameters. This
    /// guards against future regressions of the bind discipline.
    #[test]
    fn within_km_injection_safe() {
        // Use a distinctive coordinate value that would be obvious if it
        // appeared literally in the SQL text.
        let center = GeoPoint::new(12.3456, -98.7654).unwrap();
        let expr = SpatialExpr::Within {
            field_column: "location",
            center,
            radius_meters: 1234.5,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        // The coordinate strings must not appear verbatim in the SQL.
        assert!(
            !sql.contains("12.3456"),
            "latitude appeared literally in SQL — bind discipline violated; got: {sql}"
        );
        assert!(
            !sql.contains("98.7654"),
            "longitude appeared literally in SQL — bind discipline violated; got: {sql}"
        );
        assert!(
            !sql.contains("1234.5"),
            "radius appeared literally in SQL — bind discipline violated; got: {sql}"
        );
    }

    /// The `::geography` cast appears in the ST_Point expression for both variants.
    /// PostGIS requires this cast to use the geography (spherical) distance model.
    #[test]
    fn both_variants_include_geography_cast() {
        let center = GeoPoint::new(0.0, 0.0).unwrap();

        let within = SpatialExpr::Within {
            field_column: "loc",
            center,
            radius_meters: 100.0,
        };
        let mut acc = SqlAccumulator::new("");
        within.emit(&mut acc);
        assert!(
            acc.sql().contains("::geography"),
            "Within must include ::geography cast; got: {}",
            acc.sql()
        );

        let distance = SpatialExpr::Distance {
            field_column: "loc",
            center,
        };
        let mut acc2 = SqlAccumulator::new("");
        distance.emit(&mut acc2);
        assert!(
            acc2.sql().contains("::geography"),
            "Distance must include ::geography cast; got: {}",
            acc2.sql()
        );
    }

    // ── Shape predicate tests ─────────────────────────────────────────────────

    /// `Contains` must emit `ST_Contains(<col>::geometry, $1::bytea::geometry)`
    /// with the column name cast to `::geometry` (PostGIS 3.x has no
    /// `ST_Contains(geography, geography)` overload) and exactly one bind
    /// parameter for the EWKB bytes.
    #[test]
    fn contains_emits_st_contains_with_ewkb_bind() {
        let other_poly_bytes = vec![0x01, 0x02, 0x03]; // dummy EWKB — real one in live tests
        let expr = SpatialExpr::Contains {
            field_column: "area",
            other_ewkb: other_poly_bytes.clone(),
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Contains"),
            "expected ST_Contains, got: {sql}"
        );
        assert!(
            sql.contains("area::geometry"),
            "expected column 'area::geometry', got: {sql}"
        );
        assert!(
            sql.contains("::bytea::geometry"),
            "expected ::bytea::geometry bind cast, got: {sql}"
        );
        assert!(
            !sql.contains("::geography"),
            "ST_Contains must not use ::geography (no such overload in PostGIS 3.x); got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind (the EWKB bytes), got {}",
            acc.bind_count()
        );
    }

    /// `Intersects` keeps the geography path — both the bare column
    /// reference and the `::bytea::geography` bind cast stay in the geography
    /// type family because `ST_Intersects` has native geography overloads.
    #[test]
    fn intersects_emits_st_intersects_with_ewkb_bind() {
        let ewkb = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let expr = SpatialExpr::Intersects {
            field_column: "route",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Intersects"),
            "expected ST_Intersects, got: {sql}"
        );
        assert!(sql.contains("route"), "expected column 'route', got: {sql}");
        assert!(
            sql.contains("::bytea::geography"),
            "expected ::bytea::geography bind cast, got: {sql}"
        );
        assert!(
            !sql.contains("route::geometry"),
            "ST_Intersects must keep geography column (no ::geometry cast); got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind, got {}",
            acc.bind_count()
        );
    }

    /// `Touches` is geometry-only in PostGIS 3.x — both the column and the
    /// bind must be cast to `geometry`.
    #[test]
    fn touches_emits_st_touches_with_ewkb_bind() {
        let ewkb = vec![0xAA, 0xBB];
        let expr = SpatialExpr::Touches {
            field_column: "boundary",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Touches"),
            "expected ST_Touches, got: {sql}"
        );
        assert!(
            sql.contains("boundary::geometry"),
            "expected column 'boundary::geometry', got: {sql}"
        );
        assert!(
            sql.contains("::bytea::geometry"),
            "expected ::bytea::geometry bind cast, got: {sql}"
        );
        assert!(
            !sql.contains("::geography"),
            "ST_Touches must not use ::geography (no such overload); got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind, got {}",
            acc.bind_count()
        );
    }

    /// `WithinShape` must emit `ST_Within(...)` (not ST_DWithin) with
    /// `::geometry` casts on both sides — `ST_Within(geography, geography)`
    /// does not exist in PostGIS 3.x.
    #[test]
    fn within_shape_emits_st_within_not_st_dwithin() {
        let ewkb = vec![0x01, 0xFF];
        let expr = SpatialExpr::WithinShape {
            field_column: "zone",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(sql.contains("ST_Within"), "expected ST_Within, got: {sql}");
        assert!(
            !sql.contains("ST_DWithin"),
            "got ST_DWithin instead of ST_Within: {sql}"
        );
        assert!(
            sql.contains("zone::geometry"),
            "expected column 'zone::geometry', got: {sql}"
        );
        assert!(
            sql.contains("::bytea::geometry"),
            "expected ::bytea::geometry bind cast, got: {sql}"
        );
        assert!(
            !sql.contains("::geography"),
            "ST_Within must not use ::geography (no such overload); got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind, got {}",
            acc.bind_count()
        );
    }

    // ── Injection safety: EWKB bytes must not appear as literal text in SQL ───

    /// Injection safety — the EWKB bytes must not appear as literal SQL text;
    /// they must appear only as a bind placeholder (`$1`).
    #[test]
    fn contains_injection_safe() {
        // Use a distinctive byte sequence; if it leaked into SQL it would be
        // visible as hex-encoded bytes or similar.
        let ewkb = vec![0x01, 0x03, 0x00, 0x00, 0x20]; // EWKB Polygon preamble
        let expr = SpatialExpr::Contains {
            field_column: "coverage",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        // The raw bytes must not appear as "103000020" or similar decimal string.
        // The key invariant: SQL contains exactly one placeholder and the byte
        // content lives in the bound params, never in the SQL text.
        assert_eq!(
            acc.bind_count(),
            1,
            "EWKB must be a bind param, not embedded in SQL; bind_count={}",
            acc.bind_count()
        );
        // SQL should only have `$1` as a parameter reference, not literal byte values.
        assert!(sql.contains("$1"), "expected $1 placeholder, got: {sql}");
    }

    /// Injection safety for `Intersects`.
    #[test]
    fn intersects_injection_safe() {
        let ewkb = vec![0x01, 0x02, 0x00, 0x00, 0x20];
        let expr = SpatialExpr::Intersects {
            field_column: "coverage",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert_eq!(acc.bind_count(), 1);
        assert!(sql.contains("$1"), "expected $1 placeholder, got: {sql}");
    }

    /// Injection safety for `Touches`.
    #[test]
    fn touches_injection_safe() {
        let ewkb = vec![0x01, 0x05, 0x00, 0x00, 0x20];
        let expr = SpatialExpr::Touches {
            field_column: "coverage",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert_eq!(acc.bind_count(), 1);
        assert!(sql.contains("$1"), "expected $1 placeholder, got: {sql}");
    }

    /// Injection safety for `WithinShape`.
    #[test]
    fn within_shape_injection_safe() {
        let ewkb = vec![0x01, 0x06, 0x00, 0x00, 0x20];
        let expr = SpatialExpr::WithinShape {
            field_column: "coverage",
            other_ewkb: ewkb,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert_eq!(acc.bind_count(), 1);
        assert!(sql.contains("$1"), "expected $1 placeholder, got: {sql}");
    }

    // ── Sequential bind numbering when multiple expressions are emitted ───────

    // ── T10: BoundedBy emission tests ────────────────────────────────────────

    /// `BoundedBy` must emit `ST_MakeEnvelope(...)` using Postgres (x, y) =
    /// (lon, lat) order even though the Rust API accepts (lat, lon).
    /// The column name must appear after the `&&` operator.
    #[test]
    fn bounded_by_emits_st_makeenvelope_in_xy_order() {
        // min_lat=37.0, min_lon=-123.0, max_lat=38.0, max_lon=-122.0
        let expr = SpatialExpr::BoundedBy {
            field_column: "area",
            min_lat: 37.0,
            min_lon: -123.0,
            max_lat: 38.0,
            max_lon: -122.0,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        // Must use ST_MakeEnvelope with the geography cast and && operator.
        assert!(
            sql.contains("ST_MakeEnvelope("),
            "expected ST_MakeEnvelope in SQL; got: {sql}"
        );
        assert!(
            sql.contains("::geography &&"),
            "expected ::geography && in SQL; got: {sql}"
        );
        assert!(
            sql.contains("area"),
            "expected column name 'area' after &&; got: {sql}"
        );
        // Bind order: $1=min_lon, $2=min_lat, $3=max_lon, $4=max_lat.
        // SQL must contain all four placeholders.
        assert!(
            sql.contains("$1") && sql.contains("$2") && sql.contains("$3") && sql.contains("$4"),
            "expected $1 $2 $3 $4 in SQL; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            4,
            "BoundedBy must bind exactly 4 params; got {}",
            acc.bind_count()
        );
    }

    /// All four coordinate values must flow through `push_bind` — none may
    /// appear as literal text in the emitted SQL fragment.
    #[test]
    fn bounded_by_emits_all_four_coords_as_binds() {
        // Use distinctive values that would be visible if they leaked into SQL.
        let expr = SpatialExpr::BoundedBy {
            field_column: "zone",
            min_lat: 11.1111,
            min_lon: 22.2222,
            max_lat: 33.3333,
            max_lon: 44.4444,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        // None of the coordinate values may appear literally.
        assert!(
            !sql.contains("11.1111"),
            "min_lat leaked into SQL; got: {sql}"
        );
        assert!(
            !sql.contains("22.2222"),
            "min_lon leaked into SQL; got: {sql}"
        );
        assert!(
            !sql.contains("33.3333"),
            "max_lat leaked into SQL; got: {sql}"
        );
        assert!(
            !sql.contains("44.4444"),
            "max_lon leaked into SQL; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            4,
            "expected 4 binds, got {}",
            acc.bind_count()
        );
    }

    /// SRID 4326 must appear as a literal integer — it is a fixed constant,
    /// not a user-supplied value, so it is safe to embed directly.
    #[test]
    fn bounded_by_includes_srid_4326_literal() {
        let expr = SpatialExpr::BoundedBy {
            field_column: "coverage",
            min_lat: 0.0,
            min_lon: 0.0,
            max_lat: 1.0,
            max_lon: 1.0,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        assert!(
            acc.sql().contains("4326"),
            "expected literal 4326 SRID in SQL; got: {}",
            acc.sql()
        );
    }

    // ── T10: Distance emission tests ─────────────────────────────────────────

    /// `Distance` variant must emit `ST_Distance(<col>, ST_Point($lon, $lat)::geography)`.
    /// Bind order: $1 = lon, $2 = lat.
    #[test]
    fn distance_emits_st_distance_with_correct_structure() {
        let center = GeoPoint::new(37.7749, -122.4194).unwrap();
        let expr = SpatialExpr::Distance {
            field_column: "loc",
            center,
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Distance"),
            "expected ST_Distance; got: {sql}"
        );
        assert!(sql.contains("loc"), "expected column 'loc'; got: {sql}");
        assert!(
            sql.contains("::geography"),
            "expected ::geography cast; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            2,
            "Distance binds lon + lat (2 params); got {}",
            acc.bind_count()
        );
    }

    /// When two shape predicates are emitted sequentially onto the same
    /// accumulator, the second bind parameter must be `$2` (not `$1`).
    /// This verifies the accumulator's global counter increments correctly
    /// across calls.
    #[test]
    fn sequential_predicates_increment_bind_counter() {
        let ewkb_a = vec![0xAA];
        let ewkb_b = vec![0xBB];
        let expr_a = SpatialExpr::Contains {
            field_column: "area",
            other_ewkb: ewkb_a,
        };
        let expr_b = SpatialExpr::Intersects {
            field_column: "route",
            other_ewkb: ewkb_b,
        };
        let mut acc = SqlAccumulator::new("");
        expr_a.emit(&mut acc);
        acc.push_sql(" AND ");
        expr_b.emit(&mut acc);
        let sql = acc.sql();
        assert_eq!(
            acc.bind_count(),
            2,
            "expected 2 total binds, got {}",
            acc.bind_count()
        );
        assert!(sql.contains("$1"), "first bind must be $1; got: {sql}");
        assert!(sql.contains("$2"), "second bind must be $2; got: {sql}");
    }

    // ── Phase 8-Zero Cluster C C1 — T16 + T17 emission tests ─────────────────

    /// `Area { geom_ewkb }` emits `ST_Area($1::bytea::geography)` — geography
    /// overload yields square meters; the geometry overload yields square
    /// degrees and is the wrong unit for the demo use case.
    #[test]
    fn area_emits_st_area_with_geography_cast() {
        let expr = SpatialExpr::Area {
            geom_ewkb: vec![0x01, 0x02, 0x03],
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(sql.contains("ST_Area("), "expected ST_Area, got: {sql}");
        assert!(
            sql.contains("::bytea::geography"),
            "expected ::bytea::geography cast for meters-units, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "Area binds exactly one EWKB param; got {}",
            acc.bind_count()
        );
    }

    /// `Intersection { a_ewkb, b_ewkb }` emits
    /// `ST_Intersection($1::bytea::geometry, $2::bytea::geometry)` — both args
    /// double-cast to geometry because PostGIS 3.x has no `geography`
    /// overload for `ST_Intersection`.
    #[test]
    fn intersection_emits_st_intersection_with_geometry_cast() {
        let expr = SpatialExpr::Intersection {
            a_ewkb: vec![0xAA],
            b_ewkb: vec![0xBB],
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(
            sql.contains("ST_Intersection("),
            "expected ST_Intersection, got: {sql}"
        );
        // Both args carry the geometry cast.
        assert!(
            sql.contains("$1::bytea::geometry"),
            "expected $1::bytea::geometry, got: {sql}"
        );
        assert!(
            sql.contains("$2::bytea::geometry"),
            "expected $2::bytea::geometry, got: {sql}"
        );
        // Must NOT use the geography path — that overload doesn't exist.
        assert!(
            !sql.contains("::geography"),
            "ST_Intersection has no geography overload; got: {sql}"
        );
        assert_eq!(acc.bind_count(), 2);
    }

    /// `AreaOfIntersection { a_ewkb, b_ewkb }` — the fused composed shape used
    /// by the territory-overlap-percentage demo. Emits
    /// `ST_Area(ST_Intersection($1::bytea::geometry, $2::bytea::geometry)::geography)`.
    /// Both inner args cast to geometry (no Intersection overload otherwise);
    /// the outer geometry-result is cast to geography so ST_Area returns
    /// square meters rather than square degrees.
    #[test]
    fn area_of_intersection_emits_composed_st_area_st_intersection() {
        let expr = SpatialExpr::AreaOfIntersection {
            a_ewkb: vec![0xCC],
            b_ewkb: vec![0xDD],
        };
        let mut acc = SqlAccumulator::new("");
        expr.emit(&mut acc);
        let sql = acc.sql();
        assert!(sql.contains("ST_Area("), "got: {sql}");
        assert!(sql.contains("ST_Intersection("), "got: {sql}");
        assert!(sql.contains("$1::bytea::geometry"), "got: {sql}");
        assert!(sql.contains("$2::bytea::geometry"), "got: {sql}");
        assert!(
            sql.contains(")::geography)"),
            "expected outer geography cast for meters-units, got: {sql}"
        );
        assert_eq!(acc.bind_count(), 2);
    }

    // Cluster E round-5 BLOCK-2 closure: ConvexHull was migrated
    // out of `SpatialExpr` into `AggOp::SpatialConvexHull`. The
    // bare-emission test moved alongside, see
    // `djogi/src/query/field.rs::convex_hull_emits_*` (added in
    // round-4) for the new bare and windowed emission tests.

    /// Composition contract — sequential emission keeps bind counters in
    /// lockstep so `area_of_intersection / area_of` ratios bind correctly
    /// when emitted inline in a SELECT list.
    #[test]
    fn cluster_c_sequential_emission_preserves_bind_counter() {
        let area_a = SpatialExpr::Area {
            geom_ewkb: vec![0x11],
        };
        let area_int = SpatialExpr::AreaOfIntersection {
            a_ewkb: vec![0x22],
            b_ewkb: vec![0x33],
        };
        let mut acc = SqlAccumulator::new("");
        area_int.emit(&mut acc);
        acc.push_sql(" / ");
        area_a.emit(&mut acc);
        let sql = acc.sql();
        // 2 binds from AreaOfIntersection ($1, $2), 1 from Area ($3) = 3 total.
        assert_eq!(acc.bind_count(), 3);
        assert!(
            sql.contains("$1") && sql.contains("$2") && sql.contains("$3"),
            "expected $1 $2 $3, got: {sql}"
        );
    }
}
