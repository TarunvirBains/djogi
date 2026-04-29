//! Phase 7.5 live-migration layer — skeleton.
//!
//! `live_migrate` is the Phase 7.5 home for operator-driven
//! compatibility-window rollouts (the spec's expand → backfill →
//! flip → contract sequence). It sits *above* Phase 7's segment
//! planner: every type that flows through this module is imported
//! from [`crate::migrate`] rather than redefined here. The boundary
//! between the two layers is the [`Classification`] enum frozen in
//! `migrate::schema`.
//!
//! # What this module consumes
//!
//! `live_migrate` accepts **only** [`Classification::ExpandContract`].
//! That is the spec's `RequiresLivePlan` handoff marker — the one
//! variant whose orchestration cannot complete inside a single
//! Phase 7 segment. The remaining three variants stay inside
//! Phase 7:
//!
//! - `OnlineSafe` — the runner applies it directly.
//! - `FastLockDestructiveGuarded` — the runner applies it behind
//!   the `--allow-destructive` gate.
//! - `OfflineOnly` — Djogi refuses to emit SQL; the operator must
//!   acknowledge downtime or handle the change manually.
//!
//! Those three are operator-acknowledgement or direct-apply
//! branches; none of them are live-plan branches.
//!
//! # What this module does *not* consume
//!
//! Primary-key type flips are routed through their own dedicated
//! [`SchemaOperation::PkTypeFlipGroup`] /
//! [`SchemaOperation::PkTypeFlipMultiGroup`] cascade emitters in
//! [`crate::migrate::pk_flip`]. PK-flip orchestration is below
//! `live_migrate`'s layer — no file under `live_migrate/` should
//! ever match on a PK-flip operation. The classifier short-circuits
//! before classification when a delta carries a PK-flip group, so
//! `Classification::ExpandContract` and PK-flip routing are
//! mutually exclusive by construction.
//!
//! # Naming note
//!
//! Throughout this module, `Classification` always refers to
//! [`crate::migrate::schema::Classification`] — the four-variant
//! online-safety enum (`OnlineSafe`, `FastLockDestructiveGuarded`,
//! `ExpandContract`, `OfflineOnly`). The unrelated per-delta
//! severity classifier `crate::migrate::diff::Classification`
//! (which carries the `PkTypeFlip` variant) is reached only by its
//! fully qualified path on the rare occasions this module needs to
//! reason about it. The two enums coexist on `SchemaDelta` at
//! different granularities and must not be confused.
//!
//! [`SchemaOperation::PkTypeFlipGroup`]: crate::migrate::SchemaOperation::PkTypeFlipGroup
//! [`SchemaOperation::PkTypeFlipMultiGroup`]: crate::migrate::SchemaOperation::PkTypeFlipMultiGroup

use crate::migrate::schema::Classification;

/// Returns `true` iff `classification` is the variant `live_migrate`
/// is allowed to consume. This is the load-bearing contract
/// assertion — every later live-plan entry point gates on it so the
/// boundary contract documented in [`Classification`] cannot be
/// silently violated by a future addition.
pub fn accepts(classification: Classification) -> bool {
    matches!(classification, Classification::ExpandContract)
}

#[cfg(test)]
mod tests {
    use super::accepts;
    use crate::migrate::schema::Classification;

    #[test]
    fn accepts_only_expand_contract() {
        assert!(accepts(Classification::ExpandContract));
        assert!(!accepts(Classification::OnlineSafe));
        assert!(!accepts(Classification::FastLockDestructiveGuarded));
        assert!(!accepts(Classification::OfflineOnly));
    }
}
