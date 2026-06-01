// Verifies that #[derive(DjogiEnum)] compiles for the canonical use cases:
//  1. Default naming (snake_case).
//  2. Explicit rename_all.
//  3. Per-variant name override taking precedence over rename_all.
use djogi::DjogiEnum;

// ── Case 1: default rename_all (snake_case implied) ─────────────────────────

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "task_status")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Done,
}

// ── Case 2: explicit rename_all = "SCREAMING_SNAKE_CASE" ────────────────────

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "priority_level", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PriorityLevel {
    Low,
    MediumHigh,
    Critical,
}

// ── Case 3: per-variant override takes precedence over rename_all ────────────

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "vehicle_status", rename_all = "snake_case")]
pub enum VehicleStatus {
    Active,
    InMaintenance,
    #[djogi_enum_variant(name = "decommissioned")]
    Retired,
}

// ── Case 4: kebab-case ───────────────────────────────────────────────────────

#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "subscription_tier", rename_all = "kebab-case")]
pub enum SubscriptionTier {
    FreeTier,
    ProPlan,
    EnterpriseEdition,
}

// ── Verify variants() fn and wire string values ──────────────────────────────

fn _check_variants() {
    let _v: &[&str] = VehicleStatus::variants();
    assert_eq!(VehicleStatus::variants(), &["active", "in_maintenance", "decommissioned"]);
    assert_eq!(PriorityLevel::variants(), &["LOW", "MEDIUM_HIGH", "CRITICAL"]);
    assert_eq!(SubscriptionTier::variants(), &["free-tier", "pro-plan", "enterprise-edition"]);
    assert_eq!(TaskStatus::variants(), &["pending", "in_progress", "done"]);
}

fn main() {}
