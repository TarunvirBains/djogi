//! Spatial grouping — typed group-key newtypes and helpers for spatial
//! GROUP BY paths (region grouping, DBSCAN clustering, geohash bucketing).
//!
//! T11 ships [`RegionKey<R>`]. T12 adds
//! `ClusterId`, `GeohashKey`, `ClusterRadius`, `GeohashPrecision`.
//!
//! # Why this module exists
//!
//! Spatial GROUP BY operations share structure with plain GROUP BY (keys +
//! aggregates + HAVING + ORDER BY) but derive the group key from a spatial
//! JOIN (`ST_Contains(r.geo, t.geo)`) rather than a plain `GROUP BY col`.
//! This module provides the typed wrappers that carry that structural
//! difference without leaking SQL into the caller.
//!
//! # IntoGroupKeyTuple placement
//!
//! The sealed `IntoGroupKeyTuple` trait is defined in `grouped.rs` with a
//! private `sealed::Sealed` super-trait. Because the seal cannot be named
//! from outside `grouped.rs`, the `IntoGroupKeyTuple` impl for
//! `RegionKey<R>` also lives in `grouped.rs`. This file owns the
//! type definitions and the `SpatialJoinSpec` struct only.
//!
//! # Feature gate
//!
//! Everything in this module is behind the `spatial` feature flag.

#![cfg(feature = "spatial")]

use crate::model::Model;
use std::marker::PhantomData;

// ── SpatialJoinSpec ─────────────────────────────────────────────────────────

/// Parameters captured at `group_by_region` call time. The SQL builder reads
/// these to emit the LEFT JOIN clause and the GROUP BY target.
///
/// All fields are `&'static str` — they come from macro-baked
/// `Model::table_name()` and `FieldDescriptor::name`, so they are always
/// `'static`. No user input flows through this struct.
///
/// This type is internal to `djogi` — it is not constructed or inspected by
/// user code. Only `group_by_region` produces it; only the SQL builder reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpatialJoinSpec {
    /// Column name on the data model (`T`) that holds the geometry value
    /// (e.g. `"location"`).
    pub(crate) t_geo_col: &'static str,
    /// Table name for the region model (`R`) — alias `r` in the JOIN.
    pub(crate) r_table: &'static str,
    /// Column name on `R` that holds the region geometry
    /// (e.g. `"boundary"`).
    pub(crate) r_geo_col: &'static str,
    /// Primary key column name on `R` (e.g. `"id"`). Becomes the GROUP BY
    /// target and the first SELECT column.
    pub(crate) r_pk_col: &'static str,
}

// ── RegionKey<R> ─────────────────────────────────────────────────────────────

/// Group key produced by [`QuerySet::group_by_region`] /
/// [`QuerySet::count_by_region`].
///
/// - `Some(pk)` — the row fell inside region `R` with that primary key.
/// - `None` — the row matched no region (LEFT JOIN semantics; the
///   "unassigned" bucket so rows outside all known regions are not silently
///   dropped).
///
/// # Type parameter
///
/// `R` is the region model — any type that implements [`Model`]. The primary
/// key type is `R::Pk`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionKey<R: Model> {
    /// Primary key of the region this row was matched to, or `None` for the
    /// unassigned bucket (no containing region).
    pub region_pk: Option<R::Pk>,
    /// Primary key column name on `R` — carried internally so
    /// `IntoGroupKeyTuple::push_select_columns` and
    /// `push_group_by_columns` can emit `r.<pk-col>` without extra
    /// parameters. Set by `group_by_region`; `None` in the decoded output
    /// produced by `decode_tuple` (the decoded value is a plain
    /// user-facing key, not a query sentinel).
    pub(crate) r_pk_col: Option<&'static str>,
    pub(crate) _phantom: PhantomData<fn() -> R>,
}

// ── ClusterRadius ─────────────────────────────────────────────────────────────

/// Radius for [`QuerySet::cluster_by_proximity`] — DBSCAN's `eps` parameter.
///
/// # Choosing a radius
///
/// `meters(n)` converts to degrees using the equatorial approximation
/// (111 320 m/degree). For high-latitude precision, supply a precomputed
/// degree value with `degrees(n)` instead.
///
/// # `min_points`
///
/// The `min_points` builder sets DBSCAN's `minpoints` threshold — the minimum
/// number of points within `eps` of a point for that point to be a *core
/// point*. Points that are reachable from a core point but not core themselves
/// are *border points*; remaining points are *noise* (exposed as
/// `ClusterId(None)`). Default is `1` (every isolated point becomes its own
/// single-member cluster; nothing is noise).
///
/// # Example
///
/// ```ignore
/// // Cluster stores within 500 m, requiring at least 3 nearby stores
/// // for a point to anchor a cluster.
/// let radius = ClusterRadius::meters(500.0).min_points(3);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ClusterRadius {
    /// DBSCAN `eps` parameter in degrees (computed from meters at construction).
    pub(crate) eps_degrees: f64,
    /// DBSCAN `minpoints` parameter. Default `1`.
    pub(crate) minpoints: u32,
}

impl ClusterRadius {
    /// Build a `ClusterRadius` from a distance in **metres**.
    ///
    /// Converts using the equatorial approximation: 1 degree ≈ 111 320 m.
    /// For high-latitude use cases, prefer [`ClusterRadius::degrees`] with a
    /// precomputed value.
    pub fn meters(m: f64) -> Self {
        const METERS_PER_DEGREE: f64 = 111_320.0;
        Self {
            eps_degrees: m / METERS_PER_DEGREE,
            minpoints: 1,
        }
    }

    /// Build a `ClusterRadius` from a distance in **degrees** directly.
    ///
    /// Use this when you have a precomputed degree value or when metre-based
    /// conversion is not accurate enough for your latitude.
    pub fn degrees(d: f64) -> Self {
        Self {
            eps_degrees: d,
            minpoints: 1,
        }
    }

    /// Set the DBSCAN `minpoints` threshold (builder pattern, consumes `self`).
    ///
    /// Points with fewer than `n` neighbours within `eps` are *noise*
    /// (`ClusterId(None)`). Default is `1`.
    pub fn min_points(mut self, n: u32) -> Self {
        self.minpoints = n;
        self
    }
}

// ── GeohashPrecision ──────────────────────────────────────────────────────────

/// Precision level for [`QuerySet::bucket_by_cell`] geohash bucketing.
///
/// Geohash encodes a geographic point as a short string where longer strings
/// represent smaller, more precise grid cells:
///
/// | Precision | Cell size (approx) |
/// |---|---|
/// | P1 | 5 000 km × 5 000 km (continent) |
/// | P3 | 156 km × 156 km (city region) |
/// | P5 | 4.9 km × 4.9 km (district) |
/// | P7 | 153 m × 153 m (street block) |
/// | P9 | 4.8 m × 4.8 m (building) |
/// | P12 | sub-metre |
///
/// `P5` is a popular default for heatmaps and sharding use cases.
///
/// This enum is `#[non_exhaustive]` — future PostGIS versions may support
/// precision levels beyond 12, and Djogi reserves the right to add variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeohashPrecision {
    /// 1-character geohash (~5 000 km cells).
    P1,
    /// 2-character geohash (~1 250 km cells).
    P2,
    /// 3-character geohash (~156 km cells).
    P3,
    /// 4-character geohash (~39 km cells).
    P4,
    /// 5-character geohash (~4.9 km cells).
    P5,
    /// 6-character geohash (~1.2 km cells).
    P6,
    /// 7-character geohash (~153 m cells).
    P7,
    /// 8-character geohash (~38 m cells).
    P8,
    /// 9-character geohash (~4.8 m cells).
    P9,
    /// 10-character geohash (~1.2 m cells).
    P10,
    /// 11-character geohash (~15 cm cells).
    P11,
    /// 12-character geohash (sub-metre cells).
    P12,
}

impl GeohashPrecision {
    /// Return the integer precision value passed to `ST_GeoHash`.
    pub(crate) fn as_i32(self) -> i32 {
        match self {
            Self::P1 => 1,
            Self::P2 => 2,
            Self::P3 => 3,
            Self::P4 => 4,
            Self::P5 => 5,
            Self::P6 => 6,
            Self::P7 => 7,
            Self::P8 => 8,
            Self::P9 => 9,
            Self::P10 => 10,
            Self::P11 => 11,
            Self::P12 => 12,
        }
    }
}

// ── ClusterId ─────────────────────────────────────────────────────────────────

/// Group key produced by [`QuerySet::cluster_by_proximity`].
///
/// - `ClusterId(Some(id))` — the row belongs to cluster `id`. Ids are
///   assigned by PostGIS `ST_ClusterDBSCAN` and are dense non-negative
///   integers starting at `0`, but their values should not be interpreted
///   beyond "same id ⟹ same cluster".
/// - `ClusterId(None)` — the row is a *noise point*: isolated, with fewer
///   than `minpoints` neighbours within `eps`. Only possible when
///   `ClusterRadius::min_points(n)` is set to `n > 1`.
///
/// # When `None` appears
///
/// With `ClusterRadius::meters(500.0)` (default `min_points = 1`), every
/// point is always a core point of its own cluster, so `None` is never
/// produced. Increase `min_points` to push sparse / isolated points into the
/// noise bucket.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterId(pub Option<i32>);

// ── GeohashKey ────────────────────────────────────────────────────────────────

/// Group key produced by [`QuerySet::bucket_by_cell`].
///
/// Holds the geohash string at the chosen [`GeohashPrecision`].  Every point
/// maps to exactly one geohash cell, so `GeohashKey` is always `Some` — there
/// is no noise bucket.
///
/// # Interpreting the key
///
/// Geohash strings are prefix-ordered: `"dr5r"` falls inside the coarser cell
/// `"dr5"`, which falls inside `"dr"`, etc. You can therefore perform
/// coarser-grained re-aggregation by truncating the key string on the client
/// side without re-querying.
///
/// # Example
///
/// ```ignore
/// let buckets: Vec<(GeohashKey, i64)> = Store::objects()
///     .bucket_by_cell(|f| f.location(), GeohashPrecision::P5)
///     .annotate(|f| f.id.count_star())
///     .fetch_all(&mut ctx).await?;
///
/// for (key, count) in &buckets {
///     println!("{}: {} stores", key.0, count);
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct GeohashKey(pub String);

// ── ClusterSpec / GeohashSpec ─────────────────────────────────────────────────

/// Internal parameters captured at `cluster_by_proximity` call time.
/// Consumed by the SQL builder to emit the `ST_ClusterDBSCAN` window call.
///
/// Not part of the public API — constructed only by `cluster_by_proximity`;
/// read only by the SQL builder.
#[derive(Debug, Clone)]
pub(crate) struct ClusterSpec {
    /// Column name on the data model that holds the geometry value
    /// (e.g. `"location"`). Always a `&'static str` from `FieldRef::column`.
    pub(crate) t_geo_col: &'static str,
    /// DBSCAN radius in degrees.
    pub(crate) eps_degrees: f64,
    /// DBSCAN `minpoints` parameter.
    pub(crate) minpoints: u32,
}

/// Internal parameters captured at `bucket_by_cell` call time.
/// Consumed by the SQL builder to emit the `ST_GeoHash` scalar call.
///
/// Not part of the public API — constructed only by `bucket_by_cell`;
/// read only by the SQL builder.
#[derive(Debug, Clone)]
pub(crate) struct GeohashSpec {
    /// Column name on the data model that holds the geometry value.
    pub(crate) t_geo_col: &'static str,
    /// Geohash precision level (1..=12).
    pub(crate) precision: i32,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::grouped::IntoGroupKeyTuple;

    // ── Minimal Region stub ────────────────────────────────────────────────

    struct FakeRegion;
    impl crate::model::__sealed::Sealed for FakeRegion {}
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeRegion {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "regions"
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

    // ── T11.1: RegionKey implements IntoGroupKeyTuple ─────────────────────

    #[test]
    fn region_key_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<RegionKey<FakeRegion>>();
    }

    // ── T11.1: push_select_columns emits `r.<pk-col> AS rk0` ─────────────

    #[test]
    fn push_select_columns_emits_qualified_alias() {
        let key = RegionKey::<FakeRegion> {
            region_pk: None,
            r_pk_col: Some("id"),
            _phantom: PhantomData,
        };
        let mut acc = SqlAccumulator::new("");
        key.push_select_columns(&mut acc);
        assert_eq!(acc.sql(), "r.id AS rk0");
    }

    // ── T11.1: push_group_by_columns emits `r.<pk-col>` ──────────────────

    #[test]
    fn push_group_by_columns_emits_qualified_column() {
        let key = RegionKey::<FakeRegion> {
            region_pk: None,
            r_pk_col: Some("id"),
            _phantom: PhantomData,
        };
        let mut acc = SqlAccumulator::new("");
        key.push_group_by_columns(&mut acc);
        assert_eq!(acc.sql(), "r.id");
    }

    // ── T11.1: SpatialJoinSpec fields are accessible (crate-internal) ─────

    #[test]
    fn spatial_join_spec_fields_are_readable() {
        let spec = SpatialJoinSpec {
            t_geo_col: "location",
            r_table: "regions",
            r_geo_col: "boundary",
            r_pk_col: "id",
        };
        assert_eq!(spec.t_geo_col, "location");
        assert_eq!(spec.r_table, "regions");
        assert_eq!(spec.r_geo_col, "boundary");
        assert_eq!(spec.r_pk_col, "id");
    }

    // ── T12: ClusterRadius constructors ───────────────────────────────────

    #[test]
    fn cluster_radius_meters_converts_to_degrees() {
        let r = ClusterRadius::meters(500.0);
        let expected = 500.0_f64 / 111_320.0;
        let diff = (r.eps_degrees - expected).abs();
        assert!(
            diff < 1e-12,
            "eps_degrees should be 500/111320, got {}",
            r.eps_degrees
        );
        assert_eq!(r.minpoints, 1, "default min_points should be 1");
    }

    #[test]
    fn cluster_radius_min_points_builder() {
        let r = ClusterRadius::meters(500.0).min_points(3);
        assert_eq!(r.minpoints, 3);
    }

    #[test]
    fn cluster_radius_degrees_constructor() {
        let r = ClusterRadius::degrees(0.01);
        let diff = (r.eps_degrees - 0.01_f64).abs();
        assert!(
            diff < 1e-12,
            "expected eps_degrees = 0.01, got {}",
            r.eps_degrees
        );
        assert_eq!(r.minpoints, 1);
    }

    // ── T12: GeohashPrecision::as_i32 ─────────────────────────────────────

    #[test]
    fn geohash_precision_as_i32_returns_correct_value() {
        assert_eq!(GeohashPrecision::P1.as_i32(), 1);
        assert_eq!(GeohashPrecision::P5.as_i32(), 5);
        assert_eq!(GeohashPrecision::P12.as_i32(), 12);
    }

    // ── T12: ClusterId IntoGroupKeyTuple ──────────────────────────────────

    #[test]
    fn cluster_id_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<ClusterId>();
    }

    #[test]
    fn cluster_id_push_select_emits_alias() {
        let key = ClusterId(None);
        let mut acc = SqlAccumulator::new("");
        key.push_select_columns(&mut acc);
        assert_eq!(acc.sql(), "cluster_id");
    }

    #[test]
    fn cluster_id_push_group_by_emits_alias() {
        let key = ClusterId(None);
        let mut acc = SqlAccumulator::new("");
        key.push_group_by_columns(&mut acc);
        assert_eq!(acc.sql(), "cluster_id");
    }

    // ── T12: GeohashKey IntoGroupKeyTuple ─────────────────────────────────

    #[test]
    fn geohash_key_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<GeohashKey>();
    }

    #[test]
    fn geohash_key_push_select_emits_alias() {
        let key = GeohashKey(String::new());
        let mut acc = SqlAccumulator::new("");
        key.push_select_columns(&mut acc);
        assert_eq!(acc.sql(), "geohash");
    }

    #[test]
    fn geohash_key_push_group_by_emits_alias() {
        let key = GeohashKey(String::new());
        let mut acc = SqlAccumulator::new("");
        key.push_group_by_columns(&mut acc);
        assert_eq!(acc.sql(), "geohash");
    }
}
