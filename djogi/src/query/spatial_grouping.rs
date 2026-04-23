//! Spatial grouping — typed group-key newtypes and helpers for spatial
//! GROUP BY paths (region grouping, DBSCAN clustering, geohash bucketing).
//!
//! T11 ships [`RegionKey<R>`] and [`RegionKeyWithCol<R>`]. T12 adds
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
//! `RegionKeyWithCol<R>` also lives in `grouped.rs`. This file owns the
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialJoinSpec {
    /// Column name on the data model (`T`) that holds the geometry value
    /// (e.g. `"location"`).
    pub t_geo_col: &'static str,
    /// Table name for the region model (`R`) — alias `r` in the JOIN.
    pub r_table: &'static str,
    /// Column name on `R` that holds the region geometry
    /// (e.g. `"boundary"`).
    pub r_geo_col: &'static str,
    /// Primary key column name on `R` (e.g. `"id"`). Becomes the GROUP BY
    /// target and the first SELECT column.
    pub r_pk_col: &'static str,
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
    pub(crate) _phantom: PhantomData<fn() -> R>,
}

// ── RegionKeyWithCol<R> ───────────────────────────────────────────────────────

/// `RegionKey` with the pk-column name attached so `IntoGroupKeyTuple`
/// methods can emit qualified `r.<pk-col>` references.
///
/// This internal newtype keeps the pk-column name out of the user-facing
/// `RegionKey<R>` struct while still making it available to the SQL builder.
/// The `IntoGroupKeyTuple` impl lives in `grouped.rs` because the sealed
/// super-trait (`grouped::sealed::Sealed`) cannot be named from here.
///
/// `region_pk` is `None` when this type plays the role of the "keys"
/// sentinel on `GroupedQuerySet` (construction time). `decode_tuple` in
/// `grouped.rs` produces the decoded `RegionKey` (which carries the actual
/// pk value); the sentinel field is never read there because `decode_tuple`
/// is a static method that reads from the row, not from `self`.
#[allow(dead_code)] // `region_pk` is a construction sentinel; decode_tuple reads from the row
#[derive(Debug, Clone)]
pub struct RegionKeyWithCol<R: Model> {
    pub(crate) region_pk: Option<R::Pk>,
    pub(crate) r_pk_col: &'static str,
    pub(crate) _phantom: PhantomData<fn() -> R>,
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

    // ── T11.1: RegionKeyWithCol implements IntoGroupKeyTuple ──────────────

    #[test]
    fn region_key_with_col_implements_into_group_key_tuple() {
        fn assert_bound<T: IntoGroupKeyTuple>() {}
        assert_bound::<RegionKeyWithCol<FakeRegion>>();
    }

    // ── T11.1: push_select_columns emits `r.<pk-col> AS rk0` ─────────────

    #[test]
    fn push_select_columns_emits_qualified_alias() {
        let key = RegionKeyWithCol::<FakeRegion> {
            region_pk: None,
            r_pk_col: "id",
            _phantom: PhantomData,
        };
        let mut acc = SqlAccumulator::new("");
        key.push_select_columns(&mut acc);
        assert_eq!(acc.sql(), "r.id AS rk0");
    }

    // ── T11.1: push_group_by_columns emits `r.<pk-col>` ──────────────────

    #[test]
    fn push_group_by_columns_emits_qualified_column() {
        let key = RegionKeyWithCol::<FakeRegion> {
            region_pk: None,
            r_pk_col: "id",
            _phantom: PhantomData,
        };
        let mut acc = SqlAccumulator::new("");
        key.push_group_by_columns(&mut acc);
        assert_eq!(acc.sql(), "r.id");
    }

    // ── T11.1: SpatialJoinSpec fields are accessible ──────────────────────

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
}
