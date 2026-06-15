//! Row-shape aggregate expressions — the typed surface for `ST_AsMVT` /
//! `ST_AsGeobuf`.
//! # What
//! [`RowAggregate<Out, K>`] is the row-shape sibling of
//! [`super::aggregate::AggregateExpr<Out, K>`]. Same shape — `PhantomData`
//! tags pinning the Rust decode type and a sealed kind marker — different
//! IR backing: [`super::node::ExprNode::RowAggregate`] (whole-row input,
//! no modifier slots) instead of [`super::node::ExprNode::Aggregate`]
//! (column input, full modifier surface).
//! ## Two aggregate families, one design pattern
//! | | Column aggregate | Row aggregate |
//! |---|---|---|
//! | Examples | `COUNT(col)`, `SUM(col)`, `ST_Centroid(col)` | `ST_AsMVT(t)`, `ST_AsGeobuf(t)` |
//! | Input | single column / expression | the entire row |
//! | Modifiers (`distinct` / `filter` / `over` / `order_by` / `within_group_order_by`) | yes (per-Kind) | **no** |
//! | IR variant | [`super::node::ExprNode::Aggregate`] | [`super::node::ExprNode::RowAggregate`] |
//! | Typed wrapper | [`super::aggregate::AggregateExpr`] | [`RowAggregate`] (this file) |
//! | Kind discipline | sealed [`super::aggregate::KindEvidence`] | sealed [`RowKindEvidence`] |
//! ## Why a separate IR variant (not an `AggregateExpr` extension)
//! Postgres rejects every column-aggregate modifier when applied to a
//! row-shape aggregate:
//! - `ST_AsMVT(DISTINCT t, …)` — `42883: row-level expression cannot be DISTINCT`
//! - `ST_AsMVT(t, …) FILTER (WHERE …)` — `FILTER` is a column-aggregate-only modifier
//! - `ST_AsMVT(t, …) OVER (…)` — row aggregates are not windowable
//! - `ST_AsMVT(t, … ORDER BY …)` — ORDER BY inside the parens is rejected on row aggregates
//! - `ST_AsMVT(t, …) WITHIN GROUP (ORDER BY …)` — same
//! Sharing [`super::aggregate::AggregateExpr`] would force every modifier
//! method onto a wrapper that semantically rejects them. A sibling type
//! makes the discipline structural: row aggregates simply have no
//! modifier methods.
//! # Bytes at the Rust boundary
//! Both shipped row aggregates return `bytea` at the Postgres level — MVT
//! protobuf bytes / Geobuf bytes. The typed surface decodes that into
//! `Vec<u8>` via `postgres_types::FromSql` on `Vec<u8>` (Postgres `bytea`
//! decodes natively into `Vec<u8>`). The `Out = Vec<u8>` parameter on
//! every constructor pins this at the type level.
//! # Where
//! - [`super::node::ExprNode::RowAggregate`] / [`super::node::RowAggOp`]
//! the untyped IR payload.
//! - [`super::sql::emit_expr`] — renders the SQL tokens.
//! - [`crate::query::annotate::AnnotatedQuerySet::as_mvt`] /
//! [`crate::query::annotate::AnnotatedQuerySet::as_geobuf`] — annotated
//! terminal that wraps an annotation tuple's projection in the row
//! aggregate.
//! - [`crate::query::queryset::QuerySet::as_mvt`] /
//! [`crate::query::queryset::QuerySet::as_geobuf`] — bare terminal that
//! wraps the model's canonical projection.

#![cfg(feature = "spatial")]

use crate::expr::node::{ExprNode, RowAggOp};
use std::marker::PhantomData;

/// Crate-private seal for [`RowKindEvidence`]. Mirrors the seal pattern on
/// [`super::aggregate::sealed::Sealed`] — downstream crates cannot fabricate
/// kind tags because they cannot name `Sealed`.
pub(crate) mod sealed {
    pub trait Sealed {}
}

/// Compile-time marker for row-aggregate modifier families.
/// Row aggregates share a single kind family at v0.1.0 ([`BinaryRowAgg`]):
/// every shipped row aggregate (`ST_AsMVT` / `ST_AsGeobuf`) returns binary
/// `bytea` and admits no modifiers. The trait is sealed via [`sealed::Sealed`]
/// only framework-internal markers implement it, mirroring the
/// [`super::aggregate::KindEvidence`] discipline.
/// The trait exists today even though the v0.1.0 family is singular because
/// future row-shape aggregates (text-returning encoders, JSON-returning
/// shape encoders) will slot in as additional kind markers without rev'ing
/// the [`RowAggregate`] type signature.
pub trait RowKindEvidence: sealed::Sealed {}

/// Binary `bytea`-returning row aggregate family — `ST_AsMVT` and
/// `ST_AsGeobuf`. The Rust decode type is always `Vec<u8>`.
/// No modifier impl block exists for this kind — the row aggregate
/// universe is modifier-free at v0.1.0. The phantom marker is still
/// load-bearing because it pins the kind discipline at type level and
/// keeps the surface uniform with [`super::aggregate::AggregateExpr`]'s
/// per-kind impl block pattern.
pub struct BinaryRowAgg;

impl sealed::Sealed for BinaryRowAgg {}
impl RowKindEvidence for BinaryRowAgg {}

/// Typed row-shape aggregate expression — the result of
/// [`crate::query::annotate::AnnotatedQuerySet::as_mvt`] /
/// [`crate::query::annotate::AnnotatedQuerySet::as_geobuf`] (and the
/// matching helpers on [`crate::query::queryset::QuerySet`]).
/// Carries an [`ExprNode::RowAggregate`] payload plus phantom tags for the
/// Rust decode type `Out` and the modifier-kind marker `K`. The kind tag
/// is enforced via the sealed [`RowKindEvidence`] trait so downstream
/// crates cannot smuggle in custom kinds.
/// `#[must_use]` because a dropped row aggregate is always a mistake — the
/// terminal builder consumes it and dropping silently discards the entire
/// encoded output.
/// # Manual `Clone` / `Debug`
/// Same rationale as [`super::aggregate::AggregateExpr`]: the kind marker
/// types are ZSTs with no derives, so a `#[derive(Clone, Debug)]` would
/// add unwanted `K: Clone` / `K: Debug` bounds at every call site that
/// takes a `RowAggregate` by value through a clone-bounded slot.
#[must_use = "row aggregates are lazy — dropping one silently discards the encoded output"]
pub struct RowAggregate<Out, K = BinaryRowAgg> {
    pub(crate) node: ExprNode,
    pub(crate) _out: PhantomData<fn() -> Out>,
    pub(crate) _kind: PhantomData<fn() -> K>,
}

impl<Out, K> Clone for RowAggregate<Out, K> {
    fn clone(&self) -> Self {
        RowAggregate {
            node: self.node.clone(),
            _out: PhantomData,
            _kind: PhantomData,
        }
    }
}

impl<Out, K> std::fmt::Debug for RowAggregate<Out, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowAggregate")
            .field("node", &self.node)
            .finish()
    }
}

impl<Out, K: RowKindEvidence> RowAggregate<Out, K> {
    /// Crate-private constructor. The terminal builder methods on
    /// [`crate::query::annotate::AnnotatedQuerySet`] /
    /// [`crate::query::queryset::QuerySet`] are the supported entry
    /// points; downstream code cannot fabricate a row aggregate with
    /// arbitrary `Out`/`K` parameters by smuggling in a raw [`ExprNode`].
    pub(crate) fn from_node(node: ExprNode) -> Self {
        RowAggregate {
            node,
            _out: PhantomData,
            _kind: PhantomData,
        }
    }
}

// NOTE: No modifier impl block exists for any kind. `RowAggregate<Vec<u8>,
// BinaryRowAgg>` deliberately has no `.distinct()` / `.filter(...)` /
// `.over(...)` / `.order_by(...)` / `.within_group_order_by(...)` methods
// because Postgres rejects every column-aggregate modifier on a row-shape
// aggregate. The compile-fail fixtures under `djogi/tests/compile_fail/`
// (filenames `row_aggregate_no_*.rs`) pin this discipline at the
// rustc-diagnostic level.

// ── ST_AsMVT typed constructor ───────────────────────────────────────────

/// MVT (Mapbox Vector Tile) encoder options.
/// PostGIS's `ST_AsMVT(row, layer_name, extent, geom_name, feature_id_name)`
/// signature is variadic at the SQL level — all but the row argument carry
/// per-call defaults. The typed surface always emits explicit values for
/// the first three trailing arguments so the emission shape stays stable
/// across PostGIS releases (which could rev the SQL-side defaults), and
/// emits the optional `feature_id_name` only when set so the call falls
/// through to PostGIS's `NULL` default when omitted.
/// # Construction
/// Start with [`MvtOptions::new`] which seeds the PostGIS defaults that
/// are sound for most adopter workflows:
/// ```ignore
/// use djogi::expr::row_aggregate::MvtOptions;
/// let opts = MvtOptions::new("my_layer")
/// .with_extent(4096)   // default — match PostGIS
/// .with_geom_name("geom") // default — match PostGIS
/// .with_feature_id_name("id"); // opt-in — adds feature_id to encoded protobuf
/// ```
#[derive(Debug, Clone)]
pub struct MvtOptions {
    pub(crate) layer_name: String,
    pub(crate) extent: i32,
    pub(crate) geom_name: String,
    pub(crate) feature_id_name: Option<String>,
}

impl MvtOptions {
    /// Construct MVT options with PostGIS defaults (`extent = 4096`,
    /// `geom_name = "geom"`, no feature id).
    /// The `layer_name` is bound through the SQL accumulator so a
    /// runtime-computed layer name cannot SQL-inject — but the caller
    /// must still ensure the layer name is meaningful to the MVT
    /// consumer (Mapbox tooling matches layers by name).
    pub fn new(layer_name: impl Into<String>) -> Self {
        MvtOptions {
            layer_name: layer_name.into(),
            extent: 4096,
            geom_name: "geom".to_string(),
            feature_id_name: None,
        }
    }

    /// Override the tile extent — the number of MVT coordinate units per
    /// tile edge. PostGIS defaults to 4096; smaller values produce
    /// coarser geometries (smaller payloads), larger values are
    /// uncommon outside high-DPI tooling.
    #[must_use = "MvtOptions are builders — discarding the return drops the override"]
    pub fn with_extent(mut self, extent: i32) -> Self {
        self.extent = extent;
        self
    }

    /// Override the geometry column name. The named column must exist in
    /// the inner SELECT's row shape and decode as PostGIS `geometry` or
    /// `geography`. PostGIS resolves this at runtime — a non-existent
    /// column surfaces as a Postgres error from the terminal's
    /// `fetch_one` rather than a compile error.
    #[must_use = "MvtOptions are builders — discarding the return drops the override"]
    pub fn with_geom_name(mut self, geom_name: impl Into<String>) -> Self {
        self.geom_name = geom_name.into();
        self
    }

    /// Set the column whose value should be encoded as the MVT feature
    /// id (a numeric identifier each feature carries in the protobuf).
    /// Defaults to no feature id; setting an empty string keeps the
    /// default — the typed surface treats `""` as "no feature id".
    #[must_use = "MvtOptions are builders — discarding the return drops the override"]
    pub fn with_feature_id_name(mut self, feature_id_name: impl Into<String>) -> Self {
        let s = feature_id_name.into();
        self.feature_id_name = if s.is_empty() { None } else { Some(s) };
        self
    }
}

/// Build a typed `RowAggregate` for `ST_AsMVT` over the supplied row
/// columns. Used by the terminal builders on
/// [`crate::query::annotate::AnnotatedQuerySet`] /
/// [`crate::query::queryset::QuerySet`]; not part of the public surface.
/// `columns` is the projected column list of the inner SELECT — the
/// emitter does not currently inspect it (PostGIS resolves columns from
/// the record type at runtime), but it is stored on the IR variant so
/// future row aggregates that need explicit column projection can
/// extend the emission without an IR rev.
pub(crate) fn build_mvt_aggregate(
    opts: MvtOptions,
    columns: Vec<&'static str>,
) -> RowAggregate<Vec<u8>, BinaryRowAgg> {
    RowAggregate::from_node(ExprNode::RowAggregate {
        op: RowAggOp::AsMvt {
            layer_name: opts.layer_name,
            extent: opts.extent,
            geom_name: opts.geom_name,
            feature_id_name: opts.feature_id_name,
        },
        columns,
    })
}

/// Build a typed `RowAggregate` for `ST_AsGeobuf` over the supplied row
/// columns. Used by the terminal builders on
/// [`crate::query::annotate::AnnotatedQuerySet`] /
/// [`crate::query::queryset::QuerySet`]; not part of the public surface.
pub(crate) fn build_geobuf_aggregate(
    geom_name: impl Into<String>,
    columns: Vec<&'static str>,
) -> RowAggregate<Vec<u8>, BinaryRowAgg> {
    RowAggregate::from_node(ExprNode::RowAggregate {
        op: RowAggOp::AsGeobuf {
            geom_name: geom_name.into(),
        },
        columns,
    })
}

#[cfg(test)]
mod tests {
    //! Unit-level tests for row-aggregate IR + emission shape. The full
    //! live PostGIS round trip lives in
    //! `tests/integration/row_aggregate_mvt_live.rs`.

    use super::*;
    use crate::expr::node::ExprNode;

    #[test]
    fn build_mvt_aggregate_carries_options_into_ir() {
        let opts = MvtOptions::new("test_layer")
            .with_extent(8192)
            .with_geom_name("the_geom")
            .with_feature_id_name("feature_pk");
        let agg = build_mvt_aggregate(opts, vec!["id", "the_geom", "name"]);
        match &agg.node {
            ExprNode::RowAggregate {
                op:
                    RowAggOp::AsMvt {
                        layer_name,
                        extent,
                        geom_name,
                        feature_id_name,
                    },
                columns,
            } => {
                assert_eq!(layer_name, "test_layer");
                assert_eq!(*extent, 8192);
                assert_eq!(geom_name, "the_geom");
                assert_eq!(feature_id_name.as_deref(), Some("feature_pk"));
                assert_eq!(columns, &vec!["id", "the_geom", "name"]);
            }
            other => panic!("unexpected IR shape: {other:?}"),
        }
    }

    #[test]
    fn build_mvt_aggregate_empty_feature_id_disables_argument() {
        // Empty string is treated as "no feature_id_name" per the
        // builder's contract — PostGIS sees no fifth argument and uses
        // its default `NULL` for the feature id.
        let opts = MvtOptions::new("layer").with_feature_id_name("");
        let agg = build_mvt_aggregate(opts, vec!["id"]);
        match &agg.node {
            ExprNode::RowAggregate {
                op: RowAggOp::AsMvt {
                    feature_id_name, ..
                },
                ..
            } => assert!(feature_id_name.is_none()),
            other => panic!("unexpected IR shape: {other:?}"),
        }
    }

    #[test]
    fn build_geobuf_aggregate_carries_geom_name_into_ir() {
        let agg = build_geobuf_aggregate("the_geom", vec!["id", "the_geom"]);
        match &agg.node {
            ExprNode::RowAggregate {
                op: RowAggOp::AsGeobuf { geom_name },
                columns,
            } => {
                assert_eq!(geom_name, "the_geom");
                assert_eq!(columns, &vec!["id", "the_geom"]);
            }
            other => panic!("unexpected IR shape: {other:?}"),
        }
    }

    #[test]
    fn mvt_options_defaults_match_postgis_defaults() {
        let opts = MvtOptions::new("layer");
        assert_eq!(opts.layer_name, "layer");
        assert_eq!(opts.extent, 4096);
        assert_eq!(opts.geom_name, "geom");
        assert!(opts.feature_id_name.is_none());
    }
}
