// #88 — public compile coverage for simple/statistics aggregates.
//
// This target intentionally does not connect to Postgres. It exercises the
// public model-field closure surface (`T::objects().aggregate(|f| ...)`) so
// the simple boolean/bit aggregate family and the statistics/regression
// aggregate family cannot regress to crate-private-only coverage.

use djogi::prelude::*;

#[model(table = "agg_simple_stats_probe", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AggregateProbe {
    pub active: bool,
    pub flags16: i16,
    pub flags32: i32,
    pub flags64: i64,
    pub samples: f64,
    pub x: i64,
    pub y: i64,
}

#[test]
fn simple_boolean_and_bit_aggregate_methods_compile() {
    let _ = AggregateProbe::objects().aggregate(|f| f.active().bool_and());
    let _ = AggregateProbe::objects().aggregate(|f| f.active().bool_or());
    let _ = AggregateProbe::objects().aggregate(|f| f.active().every());
    let _ = AggregateProbe::objects().aggregate(|f| f.flags16().bit_and());
    let _ = AggregateProbe::objects().aggregate(|f| f.flags32().bit_or());
    let _ = AggregateProbe::objects().aggregate(|f| f.flags64().bit_xor());
}

#[test]
fn unary_statistics_aggregate_methods_compile() {
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().stddev());
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().stddev_pop());
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().stddev_samp());
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().variance());
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().var_pop());
    let _ = AggregateProbe::objects().aggregate(|f| f.samples().var_samp());
}

#[test]
fn bivariate_statistics_and_regression_aggregate_methods_compile() {
    let _ = AggregateProbe::objects().aggregate(|f| f.y().corr(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().covar_pop(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().covar_samp(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_count(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_avgx(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_avgy(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_intercept(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_r2(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_slope(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_sxx(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_sxy(f.x()));
    let _ = AggregateProbe::objects().aggregate(|f| f.y().regr_syy(f.x()));
}
