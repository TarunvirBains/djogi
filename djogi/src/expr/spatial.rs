//! Spatial expression nodes — gated behind the `spatial` feature flag.
//!
//! # What
//!
//! [`SpatialExpr`] is an internal sub-IR that plugs into [`super::node::ExprNode`]
//! via the `ExprNode::Spatial(SpatialExpr)` variant. It carries variants for:
//!
//! - [`SpatialExpr::Within`] — emits `ST_DWithin(<col>, ST_Point($lon, $lat)::geography, $r)`
//!   (radius-based predicate; Phase 6)
//! - [`SpatialExpr::Distance`] — emits `ST_Distance(<col>, ST_Point($lon, $lat)::geography)`
//! - [`SpatialExpr::Contains`] — emits `ST_Contains(<col>, $1::geography)` (T9)
//! - [`SpatialExpr::Intersects`] — emits `ST_Intersects(<col>, $1::geography)` (T9)
//! - [`SpatialExpr::Touches`] — emits `ST_Touches(<col>, $1::geography)` (T9)
//! - [`SpatialExpr::WithinShape`] — emits `ST_Within(<col>, $1::geography)` (T9)
//! - [`SpatialExpr::BoundedBy`] — bbox prefilter; emission wired in T10
//!
//! # Naming note for `WithinShape`
//!
//! The variant is named `WithinShape` internally to avoid a collision with the
//! radius-based `Within` variant that Phase 6 shipped. The public method on
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
//!   nodes — `FieldRef<M, GeoPoint>::within_km` builds `Within`; the T9 methods
//!   on `FieldRef<M, G: GeographyValue>` build `Contains` / `Intersects` /
//!   `Touches` / `WithinShape`; and `FieldRef<M, GeoPoint>::order_by_distance`
//!   captures `Distance` indirectly via
//!   [`crate::query::order::OrderExpr::SpatialDistance`].
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
    /// between `<field>` and `center`. Used in `ORDER BY` expressions via
    /// [`crate::query::order::OrderExpr::SpatialDistance`] — it is not normally
    /// used directly as a `Condition`.
    ///
    /// T4 live-PostGIS tests and future annotate / expression-composition
    /// contexts will consume this variant. The T3 ordering path embeds
    /// `ST_Distance` SQL inline in `OrderExpr::SpatialDistance::emit` for
    /// performance, but this variant powers the expression-IR path that lets
    /// callers use `filter_expr(|f| f.loc().distance(center).lt(1000.0))`.
    #[allow(dead_code)]
    Distance {
        /// Column name — a `&'static str` from the macro descriptor.
        /// Validated by `assert_plain_ident`; safe to push as raw SQL.
        field_column: &'static str,
        /// The reference point.
        center: GeoPoint,
    },

    // ── Shape-based predicates (T9) ───────────────────────────────────────────
    /// `ST_Contains(<col>, $1::geography)`
    ///
    /// Returns `true` when the geometry stored in `<col>` entirely contains
    /// the bound geometry. The bound geometry goes through `push_bind` as
    /// its EWKB byte representation.
    ///
    /// Constructed by [`crate::query::field::FieldRef::contains`].
    Contains {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test containment against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Intersects(<col>, $1::geography)`
    ///
    /// Returns `true` when the geometry stored in `<col>` and the bound
    /// geometry share at least one point. The bound geometry goes through
    /// `push_bind` as its EWKB byte representation.
    ///
    /// Constructed by [`crate::query::field::FieldRef::intersects`].
    Intersects {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test intersection against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Touches(<col>, $1::geography)`
    ///
    /// Returns `true` when the geometry stored in `<col>` and the bound
    /// geometry share boundary points but no interior points (touch but do
    /// not overlap). The bound geometry goes through `push_bind`.
    ///
    /// Constructed by [`crate::query::field::FieldRef::touches`].
    Touches {
        /// Column name — validated by `assert_plain_ident`; safe as raw SQL.
        field_column: &'static str,
        /// EWKB encoding of the geometry to test touch against.
        other_ewkb: Vec<u8>,
    },

    /// `ST_Within(<col>, $1::geography)`
    ///
    /// Returns `true` when the geometry stored in `<col>` is entirely within
    /// the bound geometry. Named `WithinShape` internally to avoid a
    /// variant-name collision with the radius-based [`SpatialExpr::Within`];
    /// the public method on `FieldRef` is still called `.within(&geom)`.
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
    /// Emission is wired in T10. Constructing a `BoundedBy` node before T10
    /// lands will panic with a clear diagnostic at emit time.
    // `dead_code`: no public constructor exists in T9; T10 wires `.bounded_by`
    // on `FieldRef`. The variant is defined here so T10 can add its method
    // and emit arm without touching this declaration.
    #[allow(dead_code)]
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
    /// `Contains` / `Intersects` / `Touches` / `WithinShape` each emit:
    /// ```sql
    /// ST_<Function>(<col>, $1::geography)
    /// ```
    /// where `$1` is the EWKB bytes of the other geometry.
    ///
    /// `BoundedBy` is not yet implemented — reaching its emit arm before T10
    /// lands will panic with a diagnostic message.
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
            // ── Shape predicates (T9) ─────────────────────────────────────────
            SpatialExpr::Contains {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, "ST_Contains", field_column, other_ewkb);
            }
            SpatialExpr::Intersects {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, "ST_Intersects", field_column, other_ewkb);
            }
            SpatialExpr::Touches {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, "ST_Touches", field_column, other_ewkb);
            }
            SpatialExpr::WithinShape {
                field_column,
                other_ewkb,
            } => {
                emit_binary_predicate(acc, "ST_Within", field_column, other_ewkb);
            }
            SpatialExpr::BoundedBy { .. } => {
                // T10 implements this — panic with a clear message to flag the
                // missing implementation if it's reached before T10 lands.
                // No public constructor for BoundedBy exists in T9, so this
                // arm is unreachable in practice until T10 wires it.
                panic!("SpatialExpr::BoundedBy emission is wired in T10");
            }
        }
    }
}

/// Emit `<func>(<col>, $n::geography)` where `$n` is bound to `other_ewkb`.
///
/// The four T9 shape predicates (`ST_Contains`, `ST_Intersects`, `ST_Touches`,
/// `ST_Within`) share an identical two-argument structure; this helper
/// eliminates the repetition while keeping each match arm a single readable
/// line. The column name flows through `push_sql` (already validated as a
/// plain identifier); the EWKB bytes flow through `push_bind`.
#[cfg(feature = "spatial")]
fn emit_binary_predicate(
    acc: &mut SqlAccumulator,
    func: &'static str,
    field_column: &'static str,
    other_ewkb: &[u8],
) {
    acc.push_sql(func);
    acc.push_sql("(");
    acc.push_sql(field_column);
    acc.push_sql(", ");
    acc.push_bind(other_ewkb.to_vec());
    acc.push_sql("::geography)");
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

    // ── T9: Shape predicate tests ─────────────────────────────────────────────

    /// `Contains` must emit `ST_Contains(...)` with the column name, the
    /// `::geography` cast, and exactly one bind parameter (the EWKB bytes).
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
        assert!(sql.contains("area"), "expected column 'area', got: {sql}");
        assert!(
            sql.contains("::geography"),
            "expected ::geography cast, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind (the EWKB bytes), got {}",
            acc.bind_count()
        );
    }

    /// `Intersects` must emit `ST_Intersects(...)` with column + cast + 1 bind.
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
            sql.contains("::geography"),
            "expected ::geography cast, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind, got {}",
            acc.bind_count()
        );
    }

    /// `Touches` must emit `ST_Touches(...)` with column + cast + 1 bind.
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
            sql.contains("boundary"),
            "expected column 'boundary', got: {sql}"
        );
        assert!(
            sql.contains("::geography"),
            "expected ::geography cast, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "expected 1 bind, got {}",
            acc.bind_count()
        );
    }

    /// `WithinShape` must emit `ST_Within(...)` (not ST_DWithin) with column + cast + 1 bind.
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
        assert!(sql.contains("zone"), "expected column 'zone', got: {sql}");
        assert!(
            sql.contains("::geography"),
            "expected ::geography cast, got: {sql}"
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
}
