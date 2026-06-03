// #88 — public compile coverage for ordered-set and
// hypothetical-set aggregates.
//
// This target intentionally does not connect to Postgres. It exercises the
// adopter-facing model-field closure surface so the single-column
// `WITHIN GROUP` ordered/hypothetical aggregate slice cannot regress to
// crate-private-only coverage.

use djogi::prelude::*;

#[model(table = "agg_ordered_hypothetical_probe", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OrderedHypotheticalProbe {
    pub bucket: String,
    pub latency_ms: f64,
    pub amount: i64,
    pub score: i64,
}

#[test]
fn ordered_set_aggregate_methods_compile_on_public_surface() {
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.latency_ms().percentile_cont(0.95));
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.amount().percentile_disc(0.5));
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.bucket().mode());
}

#[test]
fn ordered_set_within_group_override_and_filter_compile() {
    // Safe same-type override: amount (i64) ordered by score (i64).
    // The replacement target must match the receiver's type so the
    // aggregate's return-type contract (i64 here) is preserved.
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| {
        f.amount()
            .percentile_disc(0.5)
            .within_group_order_by(f.score().desc())
    });

    let _ = OrderedHypotheticalProbe::objects()
        .group_by(|f| f.bucket())
        .annotate(|f| {
            f.amount()
                .mode()
                .filter(f.score().as_expr().gt(Expr::literal(0_i64)))
        });
}

#[test]
fn hypothetical_set_aggregate_methods_compile_on_public_surface() {
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.amount().rank_of(500_i64));
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.amount().dense_rank_of(500_i64));
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.amount().percent_rank_of(500_i64));
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| f.amount().cume_dist_of(500_i64));
}

#[test]
fn hypothetical_set_within_group_override_and_filter_compile() {
    // Safe comparable-arg override: amount receiver (i64), argument 500_i64,
    // replacement score column (i64) — all the same type, keeping the
    // hypothetical-set comparability contract intact.
    let _ = OrderedHypotheticalProbe::objects().aggregate(|f| {
        f.amount()
            .rank_of(500_i64)
            .within_group_order_by(f.score().asc())
    });

    let _ = OrderedHypotheticalProbe::objects()
        .group_by(|f| f.bucket())
        .annotate(|f| {
            f.amount()
                .percent_rank_of(500_i64)
                .filter(f.score().as_expr().gt(Expr::literal(0_i64)))
        });
}
