//! Spatial expression nodes — gated behind the `spatial` feature flag.
//!
//! # What
//!
//! [`SpatialExpr`] is an internal sub-IR that plugs into [`super::node::ExprNode`]
//! via the `ExprNode::Spatial(SpatialExpr)` variant. It carries two variants:
//!
//! - [`SpatialExpr::Within`] — emits `ST_DWithin(<col>, ST_Point($lon, $lat)::geography, $r)`
//! - [`SpatialExpr::Distance`] — emits `ST_Distance(<col>, ST_Point($lon, $lat)::geography)`
//!
//! # Bind discipline
//!
//! All floating-point values (longitude, latitude, radius) flow through
//! [`crate::pg::accumulator::SqlAccumulator::push_bind`]. The column name is a
//! `&'static str` validated upstream by `assert_plain_ident` at `FieldRef`
//! construction time — it is safe to push via `push_sql` without quoting.
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
//!   nodes — `FieldRef<M, GeoPoint>::within_km` builds `Within`, and
//!   `FieldRef<M, GeoPoint>::order_by_distance` captures `Distance` indirectly
//!   via [`crate::query::order::OrderExpr::SpatialDistance`].
//! - [`super::sql::emit_expr`] has one arm for `ExprNode::Spatial(s)` that
//!   delegates to [`SpatialExpr::emit`].

#[cfg(feature = "spatial")]
use crate::geo::GeoPoint;
#[cfg(feature = "spatial")]
use crate::pg::accumulator::SqlAccumulator;

/// Spatial expression node — plugged into the query IR via `ExprNode::Spatial`.
///
/// Both variants carry a `&'static str` column name (baked in by the
/// `#[model]` macro, validated by `assert_plain_ident`) and a [`GeoPoint`]
/// center. `Within` additionally carries the radius in meters.
///
/// The emitter pushes all user-supplied floating-point values as bind
/// parameters and embeds the column name as raw SQL.
#[cfg(feature = "spatial")]
#[derive(Debug, Clone)]
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
}

#[cfg(feature = "spatial")]
impl SpatialExpr {
    /// Emit the SQL fragment for this spatial expression onto `acc`.
    ///
    /// - The column name is pushed via `push_sql` (trusted static identifier).
    /// - Longitude, latitude, and (for `Within`) radius are pushed via
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
        }
    }
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
}
