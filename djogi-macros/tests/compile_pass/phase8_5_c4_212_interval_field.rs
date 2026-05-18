// Phase 8.5 Cluster 4 (djogi#212) — `djogi::Interval` field type.
//
// Exercises the macro's parse + lower path for the new `Interval`
// typed Postgres column type:
//
// 1. A simple, non-nullable `pub duration: Interval` field imported from
//    the prelude maps to `FieldSqlType::Interval` in the emitted descriptor.
// 2. A nullable `pub maybe_duration: Option<djogi::Interval>` field
//    composes cleanly with the `Option<…>` wrapper (the standard
//    nullable-field convention).
// 3. The fully-qualified `djogi::types::Interval` path is also
//    accepted by the type-mapping table.
// 4. The typed query surface accepts `Interval` binds for filters and
//    bulk-update assignments.
//
// `no_default` because `djogi::Interval` derives `Default` (zero in
// every component) but the framework-injected `id` / `created_at` /
// `updated_at` columns do not. The macro injects them, and we let the
// caller construct them explicitly in the integration test.

use djogi::prelude::*;

// ── (1) Non-nullable + nullable Interval columns ─────────────────────────

#[model(table = "intervals_212", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct IntervalRow212 {
    /// Bare `Interval` from `djogi::prelude::*` — canonical adopter
    /// spelling. Lowers to `FieldSqlType::Interval` in the descriptor.
    pub duration: Interval,
    /// `Option<djogi::Interval>` — nullable counterpart. The macro
    /// composes the `Option<…>` wrapper with the typed Interval field
    /// without any additional `#[field(...)]` ceremony.
    pub maybe_duration: Option<djogi::Interval>,
    pub label: String,
}

// ── (2) Fully-qualified path ─────────────────────────────────────────────

#[model(table = "intervals_212_alt", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct IntervalRow212Alt {
    /// `djogi::types::Interval` — the internal path. Also accepted by
    /// the macro's `rust_type_to_sql` arm (parallel to
    /// `djogi::types::DateTime`, `djogi::types::Date`).
    pub duration: djogi::types::Interval,
}

fn _check_field_types(row: &IntervalRow212, alt: &IntervalRow212Alt) {
    let _: &Interval = &row.duration;
    let _: &Option<djogi::Interval> = &row.maybe_duration;
    let _: &djogi::types::Interval = &alt.duration;
}

fn _check_interval_query_surface() {
    let _filtered = IntervalRow212::objects().filter(|f| {
        f.duration().eq(Interval::days_only(7))
            & f.maybe_duration().eq(Interval::months_only(1))
    });
    let _update = IntervalRow212::objects().update(|f| {
        vec![
            f.duration().set(Interval::microseconds_only(500_000)),
            f.label().set("updated".to_string()),
        ]
    });
}

fn main() {}
