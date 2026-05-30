//! Live migration plan structures — the typed step graph that `live`
//! plans materialise into JSON on disk and into rows in
//! [`crate::live_migrate::state::LivePlanRow`] in Postgres.
//!
//! # Plan-file vs runtime-state separation (§1 D2)
//!
//! Two artefacts back every live migration:
//!
//! 1. **The plan file** — an immutable JSON document on disk that
//!    encodes the *definition* of the rollout: which steps run, in
//!    which order, with which parameters. Generated once by T8's
//!    pattern emitters, never edited after `djogi live run` opens it.
//! 2. **The DB row** — a mutable row in `djogi_live_plans` that tracks
//!    the *runtime state*: where the operator is in the step graph,
//!    backfill progress, last error, completion timestamp. Updated by
//!    every checkpoint write.
//!
//! The split is the load-bearing safety invariant of Phase 7.5: a plan
//! cannot diverge from its own definition. The DB row records the SHA-256
//! of the file at first run; checksum mismatch on resume aborts the run
//! with an actionable refusal ("plan file edited after start;
//! re-generate or abandon and retry").
//!
//! # Step graph shape
//!
//! The step graph is a flat ordered list, not a DAG. Each step depends
//! solely on the previous one having completed. Operator gates
//! ([`StepKind::ValidateBackfill`], [`StepKind::CutoverReads`],
//! [`StepKind::FinalizeConstraints`]) split sequential execution from
//! operator-driven phases — the runner pauses and surfaces the gate;
//! the operator drives the next-step transition explicitly via the CLI
//! (T10).
//!
//! # Stability
//!
//! [`StepKind`], [`PlanClassification`], and [`StepParameters`] are
//! [`#[non_exhaustive]`] so future patterns / classifications can land
//! without a breaking change. Downstream `match` against these enums
//! from outside the `djogi` crate must include a wildcard arm.

use serde::{Deserialize, Serialize};

use crate::migrate::OnlineSafetyClassification;
use crate::types::HeerId;

// ── StepKind ──────────────────────────────────────────────────────────

/// One step in a live migration plan. Each variant maps to one row in
/// the on-disk plan file's `steps` array; the runner dispatches each
/// step to the matching executor in T7+.
///
/// `#[non_exhaustive]` because the v3 plan §3 explicitly anticipates
/// new step kinds landing in later phases (e.g. a future
/// `RebuildIndexConcurrently` pattern). Adding a variant must not break
/// downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepKind {
    /// Emit additive DDL (new column, new index, etc.). The first step
    /// in every expand → backfill → flip → contract sequence.
    ExpandSchema,
    /// Operator hook that opens the dual-read / dual-write
    /// compatibility window. Runtime services consult the active
    /// plan's compat hooks before every read / write during the window.
    BeginCompatibilityWindow,
    /// Chunked backfill execution. The chunk predicate must be
    /// idempotent — see §3 line 420 of the v3 plan.
    BackfillChunked,
    /// Operator gate. The runner refuses to advance past this step
    /// until a SELECT-style gate query returns the expected count /
    /// shape (e.g. zero rows still carrying the legacy projection).
    ValidateBackfill,
    /// Flip the read path. Visage projections, query filters, and
    /// admin reads switch to the new schema.
    CutoverReads,
    /// Flip the write path. New writes go to the contracted schema
    /// only; the compat window's dual-write is dropped.
    CutoverWrites,
    /// Add the deferred constraints (NOT NULL, FK, unique) now that
    /// data is correct. Runs after both read- and write-cutovers.
    FinalizeConstraints,
    /// Drop the legacy column / index / table that the expand step
    /// added a parallel for. The terminal step in the standard
    /// expand → contract sequence.
    CleanupLegacyState,
    /// v3 fallback for plans that compose Phase 7 reversible operations
    /// without wrapping them in an expand/contract shape. Used by
    /// patterns whose work is one Phase 7 op surrounded by gates rather
    /// than a full expand → backfill → flip → contract sequence.
    RunReversibleSchemaOp,
}

/// Single source of truth for `(StepKind, snake_case-label)` pairs.
///
/// `StepKind` derives `Serialize` / `Deserialize` with
/// `#[serde(rename_all = "snake_case")]`, so plan-file serialisation
/// routes through serde directly. This slice — paired with the
/// `_STEP_KIND_DRIFT_GUARD` block below and the exhaustive snapshot
/// test in the `tests` module — pins the canonical label for every
/// variant so a variant rename in source can't silently change the
/// persisted plan-file JSON.
///
/// Also the backing table for [`StepKind::as_db_str`], which the
/// executor uses to write `djogi_live_plans.current_step` and the
/// daemon's candidate filter matches against. The labels match the
/// serde wire form by construction (the drift-guard test enforces it),
/// so the on-disk JSON and the ledger's `current_step` column stay in
/// lockstep.
const STEP_KIND_LABELS: &[(StepKind, &str)] = &[
    (StepKind::ExpandSchema, "expand_schema"),
    (
        StepKind::BeginCompatibilityWindow,
        "begin_compatibility_window",
    ),
    (StepKind::BackfillChunked, "backfill_chunked"),
    (StepKind::ValidateBackfill, "validate_backfill"),
    (StepKind::CutoverReads, "cutover_reads"),
    (StepKind::CutoverWrites, "cutover_writes"),
    (StepKind::FinalizeConstraints, "finalize_constraints"),
    (StepKind::CleanupLegacyState, "cleanup_legacy_state"),
    (StepKind::RunReversibleSchemaOp, "run_reversible_schema_op"),
];

/// Compile-time exhaustiveness witness for `StepKind`. Adding a
/// variant without extending the inner `match` fails to compile here.
/// The block has no runtime effect — the value is `()` — but the
/// exhaustive match enforces "every variant is named in source" so
/// [`STEP_KIND_LABELS`] is forced to grow alongside.
const _STEP_KIND_DRIFT_GUARD: () = {
    const fn check(k: StepKind) -> u8 {
        match k {
            StepKind::ExpandSchema => 0,
            StepKind::BeginCompatibilityWindow => 1,
            StepKind::BackfillChunked => 2,
            StepKind::ValidateBackfill => 3,
            StepKind::CutoverReads => 4,
            StepKind::CutoverWrites => 5,
            StepKind::FinalizeConstraints => 6,
            StepKind::CleanupLegacyState => 7,
            StepKind::RunReversibleSchemaOp => 8,
        }
    }
    let _ = check(StepKind::ExpandSchema);
};

impl StepKind {
    /// Snake_case label for this step kind — identical to the serde
    /// `rename_all = "snake_case"` wire form. Written to
    /// `djogi_live_plans.current_step` by the executor as it advances
    /// through the step graph, and matched by the daemon's candidate
    /// filter (`backfill_chunked` / `validate_backfill`).
    ///
    /// Single source of truth: [`STEP_KIND_LABELS`]. The compile-time
    /// `_STEP_KIND_DRIFT_GUARD` block guarantees every variant has a row.
    pub fn as_db_str(self) -> &'static str {
        STEP_KIND_LABELS
            .iter()
            .find_map(|(v, label)| (*v == self).then_some(*label))
            .expect("invariant: StepKind variant missing from STEP_KIND_LABELS")
    }
}

// ── PlanClassification ────────────────────────────────────────────────

/// The classification recorded in the `djogi_live_plans.classification`
/// CHECK constraint. A subset of [`OnlineSafetyClassification`]:
/// `FastLockDestructiveGuarded` is **not** a live-plan classification
/// (the operator pulls the trigger directly via `--allow-destructive`),
/// so the CHECK constraint omits it.
///
/// `#[non_exhaustive]` — future classifications that route through a
/// live plan can land without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanClassification {
    /// Pure additive change. Recorded for plans the runner could have
    /// applied directly but where the operator chose to drive the
    /// rollout through `live` for staging / dry-run reasons.
    OnlineSafe,
    /// The dominant classification. Cannot complete safely in a single
    /// segment — Phase 7.5 generates a live plan and the operator
    /// drives the expand → backfill → flip → contract sequence.
    ExpandContract,
    /// Djogi refuses to emit SQL automatically. Operator is performing
    /// the change by hand; the plan file documents the intended
    /// sequence for audit / runbook purposes.
    OfflineOnly,
}

impl From<OnlineSafetyClassification> for Option<PlanClassification> {
    /// Lossless mapping from the four-variant online-safety verdict
    /// down to the three-variant plan-file classification. The lone
    /// dropped variant — `FastLockDestructiveGuarded` — does not
    /// route through Phase 7.5, so `None` is the correct
    /// "no plan emitted; operator runs Phase 7 with
    /// `--allow-destructive`" signal.
    fn from(value: OnlineSafetyClassification) -> Self {
        match value {
            OnlineSafetyClassification::OnlineSafe => Some(PlanClassification::OnlineSafe),
            OnlineSafetyClassification::ExpandContract => Some(PlanClassification::ExpandContract),
            OnlineSafetyClassification::OfflineOnly => Some(PlanClassification::OfflineOnly),
            OnlineSafetyClassification::FastLockDestructiveGuarded => None,
        }
    }
}

/// Single source of truth for `(PlanClassification, db-string)`
/// pairs. Both [`PlanClassification::as_db_str`] and
/// [`PlanClassification::from_db_str`] linear-scan this slice; the
/// compile-time `_PLAN_CLASSIFICATION_DRIFT_GUARD` block is the
/// guarantee that every variant lives here. Adding a variant fails
/// compile until the drift-guard match grows; updating the match
/// without updating the slice causes `as_db_str` to panic for the
/// new variant in tests.
const PLAN_CLASSIFICATION_LABELS: &[(PlanClassification, &str)] = &[
    (PlanClassification::OnlineSafe, "online_safe"),
    (PlanClassification::ExpandContract, "expand_contract"),
    (PlanClassification::OfflineOnly, "offline_only"),
];

/// Compile-time exhaustiveness witness. Adding a variant without
/// extending the inner `match` fails to compile here. The block has
/// no runtime effect — the value is `()` — but the exhaustive match
/// enforces "every variant is named in source" so
/// [`PLAN_CLASSIFICATION_LABELS`] is forced to grow alongside.
const _PLAN_CLASSIFICATION_DRIFT_GUARD: () = {
    const fn check(c: PlanClassification) -> u8 {
        match c {
            PlanClassification::OnlineSafe => 0,
            PlanClassification::ExpandContract => 1,
            PlanClassification::OfflineOnly => 2,
        }
    }
    let _ = check(PlanClassification::OnlineSafe);
};

impl PlanClassification {
    /// String form used in the `djogi_live_plans.classification` CHECK
    /// constraint. Keep in lockstep with the SQL CHECK clause in
    /// [`crate::live_migrate::state::INSTALL_SQL`].
    ///
    /// Single source of truth: [`PLAN_CLASSIFICATION_LABELS`]. Linear
    /// scan over a 3-element slice is microsecond-cheap and the
    /// drift-guard block above ensures every variant has a row.
    pub fn as_db_str(self) -> &'static str {
        PLAN_CLASSIFICATION_LABELS
            .iter()
            .find_map(|(v, label)| (*v == self).then_some(*label))
            .expect("invariant: PlanClassification variant missing from PLAN_CLASSIFICATION_LABELS")
    }

    /// Inverse of [`PlanClassification::as_db_str`]. Returns `None`
    /// for strings not in the CHECK list — callers surface this as a
    /// database-corruption indicator.
    pub fn from_db_str(s: &str) -> Option<Self> {
        PLAN_CLASSIFICATION_LABELS
            .iter()
            .find_map(|(variant, label)| (*label == s).then_some(*variant))
    }
}

// ── StepParameters ────────────────────────────────────────────────────

/// Per-step parameters. Each variant carries the smallest payload the
/// matching executor needs; variant tags align with [`StepKind`] so the
/// runner can pair `Step.kind` with `Step.parameters` by enum tag.
///
/// `#[non_exhaustive]` because T8's pattern emitters will refine which
/// fields each variant carries. The serde tag is the kind name in
/// `snake_case`; all internal field names follow the same convention so
/// JSON consumers (T11 visage admin) can decode without per-variant
/// per-field rename annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepParameters {
    /// Additive DDL fragments to execute in order. Each fragment is
    /// expected to be a single Postgres statement; the runner does not
    /// split on `;`.
    ExpandSchema { sql_segments: Vec<String> },
    /// Hook IDs to register with the runtime when the compatibility
    /// window opens. Hook resolution happens in T9; this step records
    /// the IDs only.
    BeginCompatibilityWindow { hooks: Vec<String> },
    /// Chunked backfill description. `predicate_template` must be an
    /// idempotent SQL expression — re-running the same chunk against
    /// already-backfilled rows must produce no observable change.
    BackfillChunked {
        table: String,
        predicate_template: String,
        chunk_size: u32,
    },
    /// Operator gate query — the runner pauses on this step and
    /// surfaces the query result; the operator decides whether to
    /// advance.
    ValidateBackfill { gate_query: String },
    /// Free-text description shown to the operator at the read-cutover
    /// gate. The actual flip happens in application config / runtime
    /// hooks; the step records the operator-facing prompt.
    CutoverReads { description: String },
    /// Free-text description shown to the operator at the write-cutover
    /// gate. Same shape as `CutoverReads`.
    CutoverWrites { description: String },
    /// DDL fragments that finalise constraints (NOT NULL, FK,
    /// unique) once data is correct.
    FinalizeConstraints { sql_segments: Vec<String> },
    /// DDL fragments that drop the legacy column / index / table.
    CleanupLegacyState { sql_segments: Vec<String> },
    /// v3 fallback — a Phase 7 reversible op packaged with its
    /// matching `down` so the live runner can dispatch it the same
    /// way Phase 7's runner does.
    RunReversibleSchemaOp { up_sql: String, down_sql: String },
}

impl StepParameters {
    /// Returns the [`StepKind`] tag matching this variant. Used by
    /// [`Step::is_consistent`] to detect on-disk plans whose
    /// `kind` field has drifted from the `parameters.kind` discriminator.
    pub const fn kind(&self) -> StepKind {
        match self {
            StepParameters::ExpandSchema { .. } => StepKind::ExpandSchema,
            StepParameters::BeginCompatibilityWindow { .. } => StepKind::BeginCompatibilityWindow,
            StepParameters::BackfillChunked { .. } => StepKind::BackfillChunked,
            StepParameters::ValidateBackfill { .. } => StepKind::ValidateBackfill,
            StepParameters::CutoverReads { .. } => StepKind::CutoverReads,
            StepParameters::CutoverWrites { .. } => StepKind::CutoverWrites,
            StepParameters::FinalizeConstraints { .. } => StepKind::FinalizeConstraints,
            StepParameters::CleanupLegacyState { .. } => StepKind::CleanupLegacyState,
            StepParameters::RunReversibleSchemaOp { .. } => StepKind::RunReversibleSchemaOp,
        }
    }
}

// ── Step ──────────────────────────────────────────────────────────────

/// One entry in a live plan's step list. The `ordinal` is the step's
/// position in the sequence (0-based) and is recorded explicitly so the
/// JSON document stays self-describing under partial reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The step's discriminator. Doubles as the ledger
    /// `current_step` value when the runner advances onto this step.
    pub kind: StepKind,
    /// Position in the step sequence (0-based). Steps are sorted on
    /// `ordinal` after deserialization; gaps and duplicates are
    /// rejected by [`LivePlan::validate`].
    pub ordinal: u32,
    /// Per-kind parameters. The variant tag MUST match `kind` —
    /// see [`Step::is_consistent`].
    pub parameters: StepParameters,
}

impl Step {
    /// Returns `true` iff [`Step::kind`] matches the variant of
    /// [`Step::parameters`]. Used by [`LivePlan::validate`] to reject
    /// hand-edited plan files whose `kind` field drifted from the
    /// nested `parameters.kind` discriminator.
    pub fn is_consistent(&self) -> bool {
        self.kind == self.parameters.kind()
    }

    /// Returns `true` iff this step's execution would emit destructive
    /// SQL (DROP COLUMN / DROP TABLE / DROP INDEX-style state removal).
    ///
    /// Two variants qualify:
    ///
    /// - [`StepKind::CleanupLegacyState`] — the canonical destructive
    ///   step in every expand → contract sequence; drops the legacy
    ///   column / table / index that the [`StepKind::ExpandSchema`] step
    ///   parallelled.
    /// - [`StepKind::RunReversibleSchemaOp`] when its `up_sql` payload
    ///   contains a token that materially drops state. The check walks
    ///   the SQL byte-by-byte (no regex per project policy) and matches
    ///   the conservative set of literal tokens — false positives lead
    ///   to a `--justify` requirement, which is the safe direction.
    ///
    /// All other step kinds are non-destructive — they either add state
    /// (ExpandSchema, FinalizeConstraints), gate on operator confirmation
    /// (ValidateBackfill / CutoverReads / CutoverWrites), or are pure
    /// data writes (BackfillChunked).
    pub fn emits_destructive_sql(&self) -> bool {
        match (&self.kind, &self.parameters) {
            (StepKind::CleanupLegacyState, _) => true,
            (
                StepKind::RunReversibleSchemaOp,
                StepParameters::RunReversibleSchemaOp { up_sql, .. },
            ) => sql_contains_destructive_token(up_sql),
            _ => false,
        }
    }
}

/// Conservative destructive-token scan. Walks `sql` byte-by-byte and
/// returns `true` if any of a fixed set of destructive ASCII tokens
/// appears (ASCII case-insensitive) at a non-identifier word boundary:
/// `DROP` (followed at any offset by `TABLE` / `COLUMN` / `INDEX` /
/// `CONSTRAINT` / `SCHEMA` / `TYPE` / `VIEW` / `FUNCTION` / `TRIGGER`)
/// and `TRUNCATE`. Whitespace between `DROP` and the kind keyword is one
/// or more ASCII space / tab / newline / CR bytes.
///
/// The match requires a non-alphanumeric / non-underscore boundary on
/// each side so identifiers like `do_drop_table` do not trigger.
///
/// No regex engine — per project policy, use byte-level checks.
fn sql_contains_destructive_token(sql: &str) -> bool {
    const DROP_KIND_KEYWORDS: &[&[u8]] = &[
        b"TABLE",
        b"COLUMN",
        b"INDEX",
        b"CONSTRAINT",
        b"SCHEMA",
        b"TYPE",
        b"VIEW",
        b"FUNCTION",
        b"TRIGGER",
    ];
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let is_ws_byte = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let mut i = 0usize;
    while i < n {
        // Token boundary: start of input or previous byte is non-ident.
        let prev_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
        if prev_ok {
            // Try TRUNCATE.
            if bytes_match_kw(&bytes[i..], b"TRUNCATE") {
                let after = i + b"TRUNCATE".len();
                let next_ok = after == n || !is_ident_byte(bytes[after]);
                if next_ok {
                    return true;
                }
            }
            // Try DROP <whitespace+> <KIND>.
            if bytes_match_kw(&bytes[i..], b"DROP") {
                let after_drop = i + b"DROP".len();
                // Require a non-ident byte after DROP (so `dropped` doesn't trip).
                if after_drop < n && !is_ident_byte(bytes[after_drop]) {
                    let mut j = after_drop;
                    while j < n && is_ws_byte(bytes[j]) {
                        j += 1;
                    }
                    if j < n {
                        for kw in DROP_KIND_KEYWORDS {
                            if bytes_match_kw(&bytes[j..], kw) {
                                let after_kw = j + kw.len();
                                let next_ok = after_kw == n || !is_ident_byte(bytes[after_kw]);
                                if next_ok {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// ASCII case-insensitive prefix match. Returns `true` if the first
/// `kw.len()` bytes of `input` equal `kw` ignoring ASCII case. `kw` must
/// be ASCII; `input` may be any byte slice.
fn bytes_match_kw(input: &[u8], kw: &[u8]) -> bool {
    if input.len() < kw.len() {
        return false;
    }
    for (a, b) in input.iter().zip(kw.iter()) {
        if !a.eq_ignore_ascii_case(b) {
            return false;
        }
    }
    true
}

// ── PlanHeader ────────────────────────────────────────────────────────

/// Top-level metadata recorded once per plan file. The header is
/// emitted alongside the step list so a plan can be identified without
/// inspecting any individual step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHeader {
    /// HeerId — surfaced as the `djogi_live_plans.plan_id` primary
    /// key. Serialised as a JSON string (HeerId's canonical form) so
    /// JS callers preserve precision.
    pub plan_id: HeerId,
    /// Operator-facing slug, included in the on-disk filename. Same
    /// shape as a migration version's slug body — ASCII letters,
    /// digits, and underscores only (validated by the compose pipeline
    /// before this struct is constructed).
    pub slug: String,
    /// Subset of [`OnlineSafetyClassification`] — the v3 plan §3
    /// CHECK constraint accepts three values.
    pub classification: PlanClassification,
    /// Phase 7 migration version that triggered this plan. Empty
    /// string when the plan was generated outside the runner (rare —
    /// e.g. an operator-authored runbook plan).
    pub originating_migration: String,
    /// Which of the three Djogi databases (`main`, `crud_log`,
    /// `event_log`) this plan targets. Defaults to `main`.
    pub target_database: String,
    /// App label this plan belongs to. Empty string for the synthetic
    /// global bucket.
    pub app_label: String,
}

// ── LivePlan ──────────────────────────────────────────────────────────

/// A complete live plan — header plus ordered step list. This is the
/// shape serialised to disk under
/// `migrations/<target>/live/<plan_id>_<slug>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePlan {
    pub header: PlanHeader,
    pub steps: Vec<Step>,
}

impl LivePlan {
    /// Validate the in-memory plan. Returns [`PlanValidationError`] on
    /// any of:
    ///
    /// - any step's `kind` disagrees with its `parameters` variant tag,
    /// - the step list is empty,
    /// - ordinals don't form `0..steps.len()` exactly (gap or duplicate),
    /// - the slug contains a byte that is not ASCII-alphanumeric or
    ///   underscore (the on-disk filename derives directly from the
    ///   slug, so non-portable bytes would corrupt the path).
    ///
    /// Called by [`crate::live_migrate::plan_file::write_plan`] before
    /// the file is written and by
    /// [`crate::live_migrate::plan_file::read_plan`] after parsing.
    /// Returns `true` iff at least one step in the plan emits
    /// destructive SQL per [`Step::emits_destructive_sql`]. The CLI's
    /// `live run` / `live finalize` gates use this to refuse a plan
    /// without the operator-supplied `--allow-destructive --justify`
    /// pair.
    pub fn has_destructive_steps(&self) -> bool {
        self.steps.iter().any(Step::emits_destructive_sql)
    }

    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.steps.is_empty() {
            return Err(PlanValidationError::EmptySteps);
        }
        for (idx, step) in self.steps.iter().enumerate() {
            if !step.is_consistent() {
                return Err(PlanValidationError::KindMismatch {
                    ordinal: step.ordinal,
                    declared: step.kind,
                    parameters_kind: step.parameters.kind(),
                });
            }
            let expected = u32::try_from(idx).map_err(|_| PlanValidationError::TooManySteps)?;
            if step.ordinal != expected {
                return Err(PlanValidationError::OrdinalGap {
                    position: idx,
                    expected,
                    observed: step.ordinal,
                });
            }
        }
        validate_slug_bytes(&self.header.slug)?;
        Ok(())
    }
}

/// Reasons [`LivePlan::validate`] can refuse a plan. Exposed publicly
/// so the runner can surface the precise mismatch in operator messages
/// without re-deriving the structural rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanValidationError {
    /// The plan carries zero steps — a plan with no work is not a
    /// plan, and the runner refuses to register it.
    #[error("live plan has no steps")]
    EmptySteps,
    /// More than `u32::MAX` steps. Practically impossible; surfaced
    /// for completeness so the `try_from(usize)` arm is total.
    #[error("live plan has more than u32::MAX steps")]
    TooManySteps,
    /// A step's declared `kind` disagreed with its `parameters`
    /// variant tag. Typically signals a hand-edited plan file.
    #[error(
        "step ordinal {ordinal}: kind {declared:?} disagrees with parameters tag {parameters_kind:?}"
    )]
    KindMismatch {
        ordinal: u32,
        declared: StepKind,
        parameters_kind: StepKind,
    },
    /// The step list's `ordinal` field skipped a number or duplicated
    /// one — ordinals must form `0..steps.len()` exactly.
    #[error("step at position {position}: expected ordinal {expected}, observed {observed}")]
    OrdinalGap {
        position: usize,
        expected: u32,
        observed: u32,
    },
    /// The header's slug contained a byte outside the portable set
    /// used for the on-disk filename — that set is "ASCII letter,
    /// ASCII digit, or underscore". The offset and offending byte are
    /// reported so the operator can pinpoint the bad character.
    #[error("slug contains non-portable byte 0x{byte:02x} at offset {offset}")]
    SlugByte { offset: usize, byte: u8 },
    /// The header's slug was the empty string.
    #[error("slug is empty")]
    EmptySlug,
}

/// Validate that every byte of `slug` is an ASCII letter, ASCII digit,
/// or underscore. Per the no-regex policy the rule is byte-level; per
/// CLAUDE.md the rule is spelled out in plain English in the doc
/// comment rather than as a bracket-class shorthand.
fn validate_slug_bytes(slug: &str) -> Result<(), PlanValidationError> {
    if slug.is_empty() {
        return Err(PlanValidationError::EmptySlug);
    }
    for (offset, &byte) in slug.as_bytes().iter().enumerate() {
        let portable = byte.is_ascii_alphanumeric() || byte == b'_';
        if !portable {
            return Err(PlanValidationError::SlugByte { offset, byte });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_step(kind: StepKind, ordinal: u32) -> Step {
        let parameters = match kind {
            StepKind::ExpandSchema => StepParameters::ExpandSchema {
                sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
            },
            StepKind::BeginCompatibilityWindow => StepParameters::BeginCompatibilityWindow {
                hooks: vec!["dual_read_foo_bar".to_string()],
            },
            StepKind::BackfillChunked => StepParameters::BackfillChunked {
                table: "foo".to_string(),
                predicate_template: "WHERE bar IS NULL AND id BETWEEN $1 AND $2".to_string(),
                chunk_size: 10_000,
            },
            StepKind::ValidateBackfill => StepParameters::ValidateBackfill {
                gate_query: "SELECT COUNT(*) FROM foo WHERE bar IS NULL".to_string(),
            },
            StepKind::CutoverReads => StepParameters::CutoverReads {
                description: "flip read path to use bar".to_string(),
            },
            StepKind::CutoverWrites => StepParameters::CutoverWrites {
                description: "flip write path to use bar".to_string(),
            },
            StepKind::FinalizeConstraints => StepParameters::FinalizeConstraints {
                sql_segments: vec!["ALTER TABLE foo ALTER COLUMN bar SET NOT NULL".to_string()],
            },
            StepKind::CleanupLegacyState => StepParameters::CleanupLegacyState {
                sql_segments: vec!["ALTER TABLE foo DROP COLUMN baz".to_string()],
            },
            StepKind::RunReversibleSchemaOp => StepParameters::RunReversibleSchemaOp {
                up_sql: "CREATE INDEX idx_foo ON foo(bar)".to_string(),
                down_sql: "DROP INDEX idx_foo".to_string(),
            },
        };
        Step {
            kind,
            ordinal,
            parameters,
        }
    }

    fn sample_plan() -> LivePlan {
        LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo_slug".to_string(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428010203__demo".to_string(),
                target_database: "main".to_string(),
                app_label: "".to_string(),
            },
            steps: vec![
                sample_step(StepKind::ExpandSchema, 0),
                sample_step(StepKind::BackfillChunked, 1),
                sample_step(StepKind::CutoverReads, 2),
            ],
        }
    }

    #[test]
    fn step_kind_round_trips_through_serde_json() {
        // Exhaustive — adding a StepKind variant trips this.
        let all = [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
            StepKind::CleanupLegacyState,
            StepKind::RunReversibleSchemaOp,
        ];
        for kind in all {
            let s = serde_json::to_string(&kind).unwrap();
            let back: StepKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn step_kind_serializes_in_snake_case() {
        let s = serde_json::to_string(&StepKind::BeginCompatibilityWindow).unwrap();
        assert_eq!(s, "\"begin_compatibility_window\"");
        let s = serde_json::to_string(&StepKind::RunReversibleSchemaOp).unwrap();
        assert_eq!(s, "\"run_reversible_schema_op\"");
    }

    #[test]
    fn step_kind_label_slice_covers_every_variant_with_canonical_form() {
        // Drift guard for the on-disk plan-file JSON: every StepKind
        // variant must serialise to exactly the label recorded in
        // STEP_KIND_LABELS. The compile-time `_STEP_KIND_DRIFT_GUARD`
        // block enforces "every variant named in source"; this test
        // pins "every variant has a row in the slice AND that row's
        // label matches what serde emits". Together they make a
        // variant rename a compile failure or a test failure, never a
        // silent change to persisted plan-file bytes.
        let exhaustive_list = [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
            StepKind::CleanupLegacyState,
            StepKind::RunReversibleSchemaOp,
        ];
        assert_eq!(
            STEP_KIND_LABELS.len(),
            exhaustive_list.len(),
            "STEP_KIND_LABELS must contain exactly one row per StepKind variant"
        );
        for variant in exhaustive_list {
            let row = STEP_KIND_LABELS
                .iter()
                .find(|(v, _)| *v == variant)
                .unwrap_or_else(|| panic!("STEP_KIND_LABELS missing row for {variant:?}"));
            let serialized = serde_json::to_string(&variant).unwrap();
            let expected = format!("\"{}\"", row.1);
            assert_eq!(
                serialized, expected,
                "serde label for {variant:?} drifted from STEP_KIND_LABELS",
            );
        }
    }

    #[test]
    fn plan_classification_label_slice_covers_every_variant() {
        // Drift guard for as_db_str / from_db_str: linear scan over
        // PLAN_CLASSIFICATION_LABELS must reach every variant, and
        // every label must round-trip.
        let exhaustive_list = [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ];
        assert_eq!(
            PLAN_CLASSIFICATION_LABELS.len(),
            exhaustive_list.len(),
            "PLAN_CLASSIFICATION_LABELS must contain exactly one row per variant"
        );
        for variant in exhaustive_list {
            let label = variant.as_db_str();
            assert_eq!(
                PlanClassification::from_db_str(label),
                Some(variant),
                "round-trip failed for {variant:?}",
            );
        }
    }

    #[test]
    fn plan_classification_round_trips_through_serde_json() {
        for c in [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ] {
            let s = serde_json::to_string(&c).unwrap();
            let back: PlanClassification = serde_json::from_str(&s).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn plan_classification_db_str_round_trip() {
        for c in [
            PlanClassification::OnlineSafe,
            PlanClassification::ExpandContract,
            PlanClassification::OfflineOnly,
        ] {
            assert_eq!(PlanClassification::from_db_str(c.as_db_str()), Some(c));
        }
        assert_eq!(PlanClassification::from_db_str("nope"), None);
    }

    #[test]
    fn online_safety_classification_to_plan_classification() {
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::OnlineSafe),
            Some(PlanClassification::OnlineSafe)
        );
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::ExpandContract),
            Some(PlanClassification::ExpandContract)
        );
        assert_eq!(
            <Option<PlanClassification>>::from(OnlineSafetyClassification::OfflineOnly),
            Some(PlanClassification::OfflineOnly)
        );
        // The load-bearing case — fast-lock destructive does NOT get
        // a live plan; the operator drives Phase 7 directly with
        // `--allow-destructive`.
        assert_eq!(
            <Option<PlanClassification>>::from(
                OnlineSafetyClassification::FastLockDestructiveGuarded
            ),
            None
        );
    }

    #[test]
    fn step_parameters_kind_returns_matching_tag() {
        for kind in [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
            StepKind::CleanupLegacyState,
            StepKind::RunReversibleSchemaOp,
        ] {
            let step = sample_step(kind, 0);
            assert_eq!(step.parameters.kind(), kind);
            assert!(step.is_consistent());
        }
    }

    #[test]
    fn step_inconsistent_when_kind_disagrees_with_parameters() {
        let mut step = sample_step(StepKind::ExpandSchema, 0);
        step.kind = StepKind::CleanupLegacyState;
        assert!(!step.is_consistent());
    }

    #[test]
    fn live_plan_round_trips_through_serde_json() {
        let plan = sample_plan();
        let s = serde_json::to_string_pretty(&plan).unwrap();
        let back: LivePlan = serde_json::from_str(&s).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn live_plan_plan_id_serializes_as_string() {
        // HeerId's serialize_display impl emits a string. The runner
        // depends on this so JS callers don't lose precision on the
        // 64-bit id.
        let plan = sample_plan();
        let s = serde_json::to_string(&plan).unwrap();
        assert!(
            s.contains("\"plan_id\":\"0\""),
            "plan_id must serialize as a JSON string; got: {s}",
        );
    }

    #[test]
    fn live_plan_validate_accepts_well_formed_plan() {
        let plan = sample_plan();
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn live_plan_validate_rejects_kind_mismatch() {
        let mut plan = sample_plan();
        plan.steps[0].kind = StepKind::CleanupLegacyState;
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::KindMismatch {
                ordinal: 0,
                declared: StepKind::CleanupLegacyState,
                parameters_kind: StepKind::ExpandSchema,
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_ordinal_gap() {
        let mut plan = sample_plan();
        plan.steps[1].ordinal = 5;
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::OrdinalGap {
                position: 1,
                expected: 1,
                observed: 5
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_empty_steps() {
        let mut plan = sample_plan();
        plan.steps.clear();
        assert!(matches!(
            plan.validate().unwrap_err(),
            PlanValidationError::EmptySteps
        ));
    }

    #[test]
    fn live_plan_validate_rejects_non_portable_slug_byte() {
        let mut plan = sample_plan();
        plan.header.slug = "bad slug".to_string();
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::SlugByte {
                offset: 3,
                byte: b' '
            }
        ));
    }

    #[test]
    fn live_plan_validate_rejects_empty_slug() {
        let mut plan = sample_plan();
        plan.header.slug = "".to_string();
        assert!(matches!(
            plan.validate().unwrap_err(),
            PlanValidationError::EmptySlug
        ));
    }

    #[test]
    fn step_ordinal_sort_is_stable() {
        let mut steps = [
            sample_step(StepKind::CutoverReads, 2),
            sample_step(StepKind::ExpandSchema, 0),
            sample_step(StepKind::BackfillChunked, 1),
        ];
        steps.sort_by_key(|s| s.ordinal);
        assert_eq!(steps[0].kind, StepKind::ExpandSchema);
        assert_eq!(steps[1].kind, StepKind::BackfillChunked);
        assert_eq!(steps[2].kind, StepKind::CutoverReads);
    }

    // ── destructive-step detection (Finding 1) ────────────────────────

    #[test]
    fn cleanup_legacy_state_step_is_always_destructive() {
        let step = sample_step(StepKind::CleanupLegacyState, 0);
        assert!(step.emits_destructive_sql());
    }

    #[test]
    fn expand_and_backfill_steps_are_not_destructive() {
        for kind in [
            StepKind::ExpandSchema,
            StepKind::BeginCompatibilityWindow,
            StepKind::BackfillChunked,
            StepKind::ValidateBackfill,
            StepKind::CutoverReads,
            StepKind::CutoverWrites,
            StepKind::FinalizeConstraints,
        ] {
            let step = sample_step(kind, 0);
            assert!(
                !step.emits_destructive_sql(),
                "{kind:?} must not be classified destructive",
            );
        }
    }

    #[test]
    fn run_reversible_op_destructive_when_up_sql_drops_state() {
        for sql in [
            "DROP TABLE foo",
            "alter table foo drop column bar",
            "DROP INDEX CONCURRENTLY idx_x",
            "TRUNCATE foo",
            "DROP\nTABLE foo",
            "ALTER TABLE foo DROP CONSTRAINT bar_fk",
        ] {
            let step = Step {
                kind: StepKind::RunReversibleSchemaOp,
                ordinal: 0,
                parameters: StepParameters::RunReversibleSchemaOp {
                    up_sql: sql.to_string(),
                    down_sql: "".to_string(),
                },
            };
            assert!(
                step.emits_destructive_sql(),
                "expected `{sql}` to be classified destructive",
            );
        }
    }

    #[test]
    fn run_reversible_op_not_destructive_when_up_sql_only_creates() {
        for sql in [
            "CREATE INDEX idx_foo ON foo(bar)",
            "ALTER TABLE foo ADD COLUMN bar INT",
            "CREATE TABLE bar (id BIGINT PRIMARY KEY)",
            "-- DROP TABLE in a comment shouldn't trip but the helper is conservative",
        ] {
            let step = Step {
                kind: StepKind::RunReversibleSchemaOp,
                ordinal: 0,
                parameters: StepParameters::RunReversibleSchemaOp {
                    up_sql: sql.to_string(),
                    down_sql: "".to_string(),
                },
            };
            // Note: the comment case WILL trip the helper because we
            // don't strip SQL comments. That's the conservative choice
            // — false positive forces a `--justify`, which is safe.
            // Pin behaviour so future refactors notice.
            let observed = step.emits_destructive_sql();
            if sql.contains("DROP TABLE") || sql.contains("TRUNCATE") {
                assert!(observed, "conservative: comment with DROP trips: {sql}");
            } else {
                assert!(!observed, "no destructive token in: {sql}");
            }
        }
    }

    #[test]
    fn destructive_token_does_not_match_inside_identifiers() {
        for sql in [
            "INSERT INTO do_not_drop_table VALUES (1)",
            "UPDATE dropped_records SET x = 1",
            "SELECT truncate_log_id FROM foo",
        ] {
            assert!(
                !sql_contains_destructive_token(sql),
                "must not match inside identifier: {sql}",
            );
        }
    }

    #[test]
    fn live_plan_has_destructive_steps_walks_all_steps() {
        // Plan with no cleanup step is non-destructive.
        let plan = sample_plan();
        assert!(!plan.has_destructive_steps());
        // Adding a cleanup step trips the predicate.
        let mut plan2 = sample_plan();
        plan2
            .steps
            .push(sample_step(StepKind::CleanupLegacyState, 3));
        assert!(plan2.has_destructive_steps());
    }
}
