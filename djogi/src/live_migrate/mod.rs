//! Phase 7.5 live-migration layer — skeleton.
//!
//! `live_migrate` is the Phase 7.5 home for operator-driven
//! compatibility-window rollouts (the spec's expand → backfill →
//! flip → contract sequence). It sits *above* Phase 7's segment
//! planner: every type that flows through this module is imported
//! from [`crate::migrate`] rather than redefined here. The boundary
//! between the two layers is the [`OnlineSafetyClassification`] enum
//! frozen in `migrate::schema`.
//!
//! # What this module consumes
//!
//! `live_migrate` accepts **only**
//! [`OnlineSafetyClassification::ExpandContract`]. That is the spec's
//! `RequiresLivePlan` handoff marker — the one variant whose
//! orchestration cannot complete inside a single Phase 7 segment.
//! The remaining three variants stay inside Phase 7:
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
//! `OnlineSafetyClassification::ExpandContract` and PK-flip routing
//! are mutually exclusive by construction. The exclusion is
//! architectural: PK-flip routing lives on
//! [`crate::migrate::diff::Classification::PkTypeFlip`] (a
//! per-delta severity verdict), while `ExpandContract` lives on
//! [`OnlineSafetyClassification`] (a per-operation online-safety
//! verdict). The two enums are different by design, and the
//! live-plan layer only ever consumes the latter.
//!
//! # Naming note
//!
//! Throughout this module, [`OnlineSafetyClassification`] is the
//! four-variant online-safety enum (`OnlineSafe`,
//! `FastLockDestructiveGuarded`, `ExpandContract`, `OfflineOnly`)
//! defined in `migrate::schema`. The unrelated per-delta severity
//! classifier [`crate::migrate::diff::Classification`] (which
//! carries the `PkTypeFlip` variant) is reached only by its fully
//! qualified path on the rare occasions this module needs to
//! reason about it. The two enums coexist on `SchemaDelta` at
//! different granularities; the disjoint names ensure they cannot
//! be confused.
//!
//! [`SchemaOperation::PkTypeFlipGroup`]: crate::migrate::SchemaOperation::PkTypeFlipGroup
//! [`SchemaOperation::PkTypeFlipMultiGroup`]: crate::migrate::SchemaOperation::PkTypeFlipMultiGroup

use crate::migrate::OnlineSafetyClassification;

pub mod backfill;
pub mod classify;
pub mod daemon;
pub mod hooks;
pub mod patterns;
pub mod plan;
pub mod plan_file;
pub mod state;

pub use backfill::{BackfillChunk, BackfillError, execute_backfill, resume_backfill};
pub use classify::{ClassifyContext, TargetDatabase, classify_delta, classify_operation};
pub use daemon::{DaemonConfig, DaemonError, run_daemon};
pub use hooks::{
    ActiveHooks, DualReadHook, DualWriteHook, HookError, active_hooks_at_step,
    side_effects_suppressed,
};
pub use patterns::{Pattern, PatternContext, PatternError, dispatch_pattern};
pub use plan::{
    LivePlan, PlanClassification, PlanHeader, PlanValidationError, Step, StepKind, StepParameters,
};
pub use plan_file::{
    PlanFileError, compute_checksum, plan_path, read_plan, verify_checksum, write_plan,
};
pub use state::{INSTALL_SQL, LivePlanRow, PlanStatus};

/// Logging-profile axis read from `Djogi.toml`'s `[logging] profile`
/// at compose time and threaded into [`ClassifyContext`].
///
/// The profile drives the §6.5 three-DB short-circuit: under `Light`
/// and `Balanced` the CRUD-log database degrades audit writes
/// gracefully, so brief lock windows on crud-log mirror tables don't
/// block production application writes. Under `StrictAudit` the audit
/// contract is fail-closed — a lock on a crud-log mirror blocks every
/// `INSERT` / `UPDATE` / `DELETE` on the corresponding application
/// table — so populated crud-log mirrors classify the same way as
/// the application database.
///
/// Defined here (rather than in [`crate::config`]) because Phase 7
/// has not yet shipped the logging profile config plumbing; T5 needs
/// the type to drive its `ClassifyContext`. When Phase 7's logging
/// substrate lands, this enum becomes the canonical home and config
/// parsing maps the `[logging] profile = "..."` string into one of
/// these variants.
///
/// `#[non_exhaustive]` so future profiles (e.g. a `BestEffort` or
/// `Replicated` variant) can be added without breaking downstream
/// matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LoggingProfile {
    /// Lowest-overhead profile — audit writes are best-effort,
    /// failures degrade silently. Migrations on crud-log mirror
    /// tables route directly to Phase 7 (no live-plan).
    Light,
    /// Default profile — bounded retry on audit writes, failures
    /// surface as warnings. Same direct-route policy as `Light`.
    Balanced,
    /// Fail-closed audit contract — application writes refuse to
    /// commit until the audit row is durable. Crud-log lock windows
    /// block production, so the classifier runs the full live-plan
    /// path on populated crud-log mirrors.
    StrictAudit,
}

/// Returns `true` iff `classification` is the variant `live_migrate`
/// is allowed to consume. This is the load-bearing contract
/// assertion — every later live-plan entry point gates on it so the
/// boundary contract documented in [`OnlineSafetyClassification`]
/// cannot be silently violated by a future addition.
pub fn accepts(classification: OnlineSafetyClassification) -> bool {
    matches!(classification, OnlineSafetyClassification::ExpandContract)
}

#[cfg(test)]
mod tests {
    use super::accepts;
    use crate::migrate::OnlineSafetyClassification;

    #[test]
    fn accepts_only_expand_contract() {
        assert!(accepts(OnlineSafetyClassification::ExpandContract));
        assert!(!accepts(OnlineSafetyClassification::OnlineSafe));
        assert!(!accepts(
            OnlineSafetyClassification::FastLockDestructiveGuarded
        ));
        assert!(!accepts(OnlineSafetyClassification::OfflineOnly));
    }
}
