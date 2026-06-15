//! Runtime rollout hooks for live-migration compatibility windows.
//! Hooks ship ONLY when mechanically derivable from the live plan
//! ([§1 D2 ] — narrow runtime hook surface, locked). Two hook
//! families today:
//! 1. **Dual-read** — during the [`StepKind::ExpandSchema`] window,
//! reads consult both the legacy column and the new shadow column.
//! The runtime returns the legacy value if present (it is the
//! source of truth during expand) and falls back to the shadow if
//! the row has not yet been backfilled.
//! 2. **Dual-write** — while both legacy and new columns exist,
//! writes update both columns; [`StepKind::CutoverWrites`] drops
//! the legacy update.
//! Both hook families are derived from the live plan's step graph
//! adopter code does NOT register handlers. The runner activates the
//! hooks when stepping through [`StepKind::BeginCompatibilityWindow`]
//! and deactivates them at [`StepKind::CutoverReads`] (drops dual-read)
//! and [`StepKind::CutoverWrites`] (drops dual-write).
//! Business-specific branching is explicitly NOT supported — those
//! hooks live in app code.
//! # Hook ID convention
//! Pattern emitters in [`crate::live_migrate::patterns`] encode hook
//! identities as strings inside
//! [`StepParameters::BeginCompatibilityWindow::hooks`]. The format
//! uses `::` (two colons) as the field separator so plain colons
//! inside payload bytes do not need escaping. Two grammars are
//! recognised:
//! ```text
//! dual_read::<table>::<column>
//! dual_write::<table>::<column>
//! dual_read::codec::<table>::<column>::<from_codec>
//! dual_write::codec::<table>::<column>::<from_codec>-><to_codec>
//! ```
//! For the **non-codec** form, the shadow column is derived as
//! `<column>_new` — that is the
//! [`crate::live_migrate::patterns::replacement_column`] convention.
//! For the **codec** form, the shadow column carries the same suffix
//! (`<column>_new`) and the codec transition function is recorded as
//! `<from_codec>-><to_codec>` for the dual-write hook.
//! Pattern emitters that wish to register additional hooks must use
//! one of the two grammars above; the parser currently does not extend
//! parser past `dual_read` / `dual_write` because forbids
//! business-logic branching.
//! # What this module does NOT do
//! - It does **not** wire the consumer side into the model save path
//! or the visage projection layer. That wiring is a follow-up;
//! today's surface is the `ActiveHooks` snapshot data structure and
//! the [`active_hooks_at_step`] walker.
//! - It does **not** persist hook state. The walker is pure: same
//! plan + same ordinal always yields the same snapshot.
//! - It does **not** poll the database for hook activation. The only
//! I/O surface is [`side_effects_suppressed`], which queries the
//! per-session GUC set by the chunked backfill runner.
//! [§1 D2 ]: https://github.com/djogi/djogi-spec
//! [`StepKind::ExpandSchema`]: crate::live_migrate::plan::StepKind::ExpandSchema
//! [`StepKind::BeginCompatibilityWindow`]: crate::live_migrate::plan::StepKind::BeginCompatibilityWindow
//! [`StepKind::CutoverReads`]: crate::live_migrate::plan::StepKind::CutoverReads
//! [`StepKind::CutoverWrites`]: crate::live_migrate::plan::StepKind::CutoverWrites
//! [`StepParameters::BeginCompatibilityWindow::hooks`]: crate::live_migrate::plan::StepParameters::BeginCompatibilityWindow

use crate::context::DjogiContext;
use crate::error::{DbError, DjogiError};
use crate::live_migrate::backfill::SIDE_EFFECT_SUPPRESSION_TXN_LOCAL;
use crate::live_migrate::plan::{LivePlan, StepKind, StepParameters};

// ── Hook identifiers ──────────────────────────────────────────────────

/// Identifies a column pair under dual-read during a compatibility
/// window. While active, runtime reads consult `legacy_column` first
/// and fall back to `shadow_column` only when the legacy value is
/// absent on the row.
/// The pair is keyed by `table` + `legacy_column` because there is
/// at most one ongoing replacement per legacy column at a time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DualReadHook {
    /// Postgres table the pair belongs to.
    pub table: String,
    /// Column the read path is migrating away from. Source of truth
    /// during the compatibility window.
    pub legacy_column: String,
    /// Column the read path is migrating onto. The runner's backfill
    /// drains the legacy value into this column chunk by chunk.
    pub shadow_column: String,
}

/// Identifies a column pair under dual-write during a compatibility
/// window. While active, every write updates BOTH `legacy_column` and
/// `shadow_column`; [`StepKind::CutoverWrites`] drops the legacy
/// update and pivots writes to `shadow_column` only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DualWriteHook {
    /// Postgres table the pair belongs to.
    pub table: String,
    /// Column the write path will eventually retire.
    pub legacy_column: String,
    /// Column the new write path targets in addition to the legacy
    /// one for the duration of the compatibility window.
    pub shadow_column: String,
    /// Optional codec transition descriptor. When `Some(...)`, the
    /// runtime applies the codec when copying a value into
    /// `shadow_column`. The format is the pattern emitter's choice;
    /// today only `<from>-><to>` is produced (by
    /// [`crate::live_migrate::patterns::codec_transition`]). `None`
    /// means plain assignment.
    pub codec_transform: Option<String>,
}

// ── ActiveHooks snapshot ──────────────────────────────────────────────

/// Snapshot of which hook families are active at a particular point
/// in a live plan's execution. Read by the model emit pipeline and
/// the visage projection layer at the start of each operation.
/// The snapshot is a value type — cheap to clone, no internal
/// references, safe to ferry across `await` points.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveHooks {
    /// Dual-read pairs the read path must consult.
    pub dual_read: Vec<DualReadHook>,
    /// Dual-write pairs the write path must mirror.
    pub dual_write: Vec<DualWriteHook>,
    /// `true` while a [`StepKind::BackfillChunked`] step is currently
    /// the active step. The flag is stepwise: it goes back to `false`
    /// at the next step boundary. Consumers use this to short-circuit
    /// outbox / event emission for rows the backfill itself touched
    /// those events would otherwise duplicate the original write.
    /// Note: this is the same intent as the per-chunk transaction's
    /// session GUC ([`SIDE_EFFECT_SUPPRESSION_TXN_LOCAL`]) but operates
    /// at plan granularity rather than transaction granularity. The
    /// per-chunk GUC is the load-bearing one for write-time
    /// suppression; this flag exists so the visage projection layer
    /// can also short-circuit its read-time fan-out during backfill.
    pub side_effects_suppressed: bool,
}

// ── Hook ID parser ────────────────────────────────────────────────────

/// Hook identifiers emitted by pattern emitters use `::` as a field
/// separator. The double-colon avoids having to escape colons inside
/// payload bytes (table names, column names — both reject `:` at
/// validation time, but a path like `from->to` could carry one).
const FIELD_SEPARATOR: &str = "::";

/// Tag prefix for the dual-read hook family.
const DUAL_READ_TAG: &str = "dual_read";

/// Tag prefix for the dual-write hook family.
const DUAL_WRITE_TAG: &str = "dual_write";

/// Marker that the ID encodes the codec subform — appears as the
/// second token after the family tag.
const CODEC_MARKER: &str = "codec";

/// Suffix appended to the legacy column name to derive the shadow
/// column name. Convention pinned by
/// [`crate::live_migrate::patterns::replacement_column`] and reused
/// by [`crate::live_migrate::patterns::codec_transition`]; the codec-transition pattern follows
/// the same convention so the snapshot's `shadow_column` field is
/// machine-derivable from the hook ID alone.
const SHADOW_SUFFIX: &str = "_new";

/// One parsed hook identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedHook {
    Read(DualReadHook),
    Write(DualWriteHook),
}

/// Parse one hook identifier in the canonical pattern-emitter format.
/// Accepted shapes:
/// ```text
/// dual_read::<table>::<column>
/// dual_write::<table>::<column>
/// dual_read::codec::<table>::<column>::<from_codec>
/// dual_write::codec::<table>::<column>::<from_codec>-><to_codec>
/// ```
/// Returns [`HookError::MalformedHookId`] for any other shape. The
/// parser is a byte-level scan over the `::`-separated tokens — no
/// regex (per CLAUDE.md) and no allocation beyond the resulting
/// hook struct.
fn parse_hook_id(id: &str) -> Result<ParsedHook, HookError> {
    let tokens: Vec<&str> = id.split(FIELD_SEPARATOR).collect();
    // Empty tokens (consecutive `::::`) and an empty leading or
    // trailing token are both signs of a malformed ID. Catch them up
    // front so later index reads don't have to.
    if tokens.iter().any(|t| t.is_empty()) {
        return Err(HookError::MalformedHookId(id.to_owned()));
    }

    match tokens.as_slice() {
        // dual_read::<table>::<column>
        [DUAL_READ_TAG, table, column] => Ok(ParsedHook::Read(DualReadHook {
            table: (*table).to_owned(),
            legacy_column: (*column).to_owned(),
            shadow_column: format!("{column}{SHADOW_SUFFIX}"),
        })),

        // dual_read::codec::<table>::<column>::<from_codec>
        [DUAL_READ_TAG, CODEC_MARKER, table, column, _from_codec] => {
            Ok(ParsedHook::Read(DualReadHook {
                table: (*table).to_owned(),
                legacy_column: (*column).to_owned(),
                shadow_column: format!("{column}{SHADOW_SUFFIX}"),
            }))
        }

        // dual_write::<table>::<column>
        [DUAL_WRITE_TAG, table, column] => Ok(ParsedHook::Write(DualWriteHook {
            table: (*table).to_owned(),
            legacy_column: (*column).to_owned(),
            shadow_column: format!("{column}{SHADOW_SUFFIX}"),
            codec_transform: None,
        })),

        // dual_write::codec::<table>::<column>::<from_codec>-><to_codec>
        [DUAL_WRITE_TAG, CODEC_MARKER, table, column, codec_transform] => {
            Ok(ParsedHook::Write(DualWriteHook {
                table: (*table).to_owned(),
                legacy_column: (*column).to_owned(),
                shadow_column: format!("{column}{SHADOW_SUFFIX}"),
                codec_transform: Some((*codec_transform).to_owned()),
            }))
        }

        _ => Err(HookError::MalformedHookId(id.to_owned())),
    }
}

// ── Plan walker ───────────────────────────────────────────────────────

/// Walk the live plan up to (and including) the given step ordinal
/// and produce the [`ActiveHooks`] snapshot. Pure function — same
/// plan + same ordinal always yields the same snapshot.
/// The walker emulates the runner's state machine without executing
/// any DDL or DML:
/// 1. `BeginCompatibilityWindow` registers the dual-read / dual-write
/// pairs encoded in its `hooks: Vec<String>` field.
/// 2. `CutoverReads` drops every active `DualReadHook`.
/// 3. `CutoverWrites` drops every active `DualWriteHook`.
/// 4. `BackfillChunked` toggles `side_effects_suppressed = true` for
/// the duration of that step; the flag goes back to `false` at the
/// next step boundary. (Consumers needing transaction-grained
/// suppression should consult [`side_effects_suppressed`] instead.)
/// `step_ordinal` is the zero-based position at which the snapshot is
/// taken. Ordinals beyond the plan's step count saturate at the
/// terminal state — the snapshot returns the state after the last
/// step has run.
/// Returns `Err(HookError::MalformedHookId)` if any
/// `BeginCompatibilityWindow` step carries a hook ID outside the
/// canonical pattern-emitter grammar.
pub fn active_hooks_at_step(plan: &LivePlan, step_ordinal: u32) -> Result<ActiveHooks, HookError> {
    let mut snapshot = ActiveHooks::default();

    for step in &plan.steps {
        if step.ordinal > step_ordinal {
            break;
        }

        // The `side_effects_suppressed` flag is stepwise — only the
        // currently-active step's `BackfillChunked` contributes. Reset
        // at every step boundary; set inside the step's match arm.
        snapshot.side_effects_suppressed = false;

        match (&step.kind, &step.parameters) {
            (
                StepKind::BeginCompatibilityWindow,
                StepParameters::BeginCompatibilityWindow { hooks },
            ) => {
                for hook_id in hooks {
                    match parse_hook_id(hook_id)? {
                        ParsedHook::Read(read_hook) => snapshot.dual_read.push(read_hook),
                        ParsedHook::Write(write_hook) => snapshot.dual_write.push(write_hook),
                    }
                }
            }
            (StepKind::CutoverReads, _) => {
                snapshot.dual_read.clear();
            }
            (StepKind::CutoverWrites, _) => {
                snapshot.dual_write.clear();
            }
            (StepKind::BackfillChunked, _) if step.ordinal == step_ordinal => {
                snapshot.side_effects_suppressed = true;
            }
            _ => {}
        }
    }

    Ok(snapshot)
}

// ── Side-effect suppression flag consumer ─────────────────────────────

/// Returns `true` if the current Postgres session has the
/// live-migrate side-effect suppression flag active. Set by the
/// chunk-transaction wrapper via `SET LOCAL`; this function reads
/// it back via `current_setting('<name>', true)` (the second argument
/// `true` is `missing_ok` — Postgres returns NULL rather than erroring
/// when the GUC is unset).
/// Used by `#[model(events)]` emitters and the visage projection
/// fan-out to short-circuit work that would duplicate or contradict
/// the backfill itself.
/// The GUC name is shared with [`SIDE_EFFECT_SUPPRESSION_TXN_LOCAL`]
/// so the setter and this reader cannot drift.
pub async fn side_effects_suppressed(ctx: &mut DjogiContext) -> Result<bool, HookError> {
    let sql = format!(
        "SELECT current_setting('{name}', true)",
        name = SIDE_EFFECT_SUPPRESSION_TXN_LOCAL,
    );
    let row_opt = ctx
        .__query_opt_for_macros(&sql, &[])
        .await
        .map_err(HookError::from)?;
    let row = match row_opt {
        Some(row) => row,
        None => return Ok(false),
    };
    let value: Option<String> = row
        .try_get(0)
        .map_err(|e| HookError::from(DjogiError::Db(DbError::other(e.to_string()))))?;
    Ok(matches!(
        value.as_deref(),
        Some("1") | Some("true") | Some("on")
    ))
}

// ── Errors ────────────────────────────────────────────────────────────

/// Errors raised by the hooks module.
/// `#[non_exhaustive]` so future hook families (or stricter parser
/// rejections) can land without breaking downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HookError {
    /// Database / driver error reading the per-session suppression
    /// GUC. Wraps [`DjogiError`] so callers can `?`-bubble.
    #[error(transparent)]
    Database(#[from] DjogiError),

    /// A `BeginCompatibilityWindow` step carried a hook ID outside
    /// the canonical pattern-emitter grammar (see module docs).
    /// Indicates a pattern-emitter bug or a hand-edited plan file.
    #[error("malformed hook id: {0}")]
    MalformedHookId(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_migrate::plan::{LivePlan, PlanClassification, PlanHeader, Step};
    use crate::types::HeerId;

    // ── Hook ID parser ───────────────────────────────────────────────

    #[test]
    fn parse_hook_id_accepts_dual_read_basic_form() {
        let parsed = parse_hook_id("dual_read::ledger_entry::amount").unwrap();
        let ParsedHook::Read(hook) = parsed else {
            panic!("expected ParsedHook::Read");
        };
        assert_eq!(hook.table, "ledger_entry");
        assert_eq!(hook.legacy_column, "amount");
        assert_eq!(hook.shadow_column, "amount_new");
    }

    #[test]
    fn parse_hook_id_accepts_dual_write_basic_form() {
        let parsed = parse_hook_id("dual_write::ledger_entry::amount").unwrap();
        let ParsedHook::Write(hook) = parsed else {
            panic!("expected ParsedHook::Write");
        };
        assert_eq!(hook.table, "ledger_entry");
        assert_eq!(hook.legacy_column, "amount");
        assert_eq!(hook.shadow_column, "amount_new");
        assert!(hook.codec_transform.is_none());
    }

    #[test]
    fn parse_hook_id_accepts_dual_read_codec_form() {
        let parsed = parse_hook_id("dual_read::codec::secret::ciphertext::aes_gcm_v1").unwrap();
        let ParsedHook::Read(hook) = parsed else {
            panic!("expected ParsedHook::Read");
        };
        assert_eq!(hook.table, "secret");
        assert_eq!(hook.legacy_column, "ciphertext");
        assert_eq!(hook.shadow_column, "ciphertext_new");
    }

    #[test]
    fn parse_hook_id_accepts_dual_write_codec_form_with_transform() {
        let parsed =
            parse_hook_id("dual_write::codec::secret::ciphertext::aes_gcm_v1->aes_gcm_v2").unwrap();
        let ParsedHook::Write(hook) = parsed else {
            panic!("expected ParsedHook::Write");
        };
        assert_eq!(hook.table, "secret");
        assert_eq!(hook.legacy_column, "ciphertext");
        assert_eq!(hook.shadow_column, "ciphertext_new");
        assert_eq!(
            hook.codec_transform.as_deref(),
            Some("aes_gcm_v1->aes_gcm_v2"),
        );
    }

    #[test]
    fn parse_hook_id_rejects_unknown_family_tag() {
        let err = parse_hook_id("dual_replicate::ledger::amount").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
    }

    #[test]
    fn parse_hook_id_rejects_empty_token() {
        // Trailing empty token from `::` at the end.
        let err = parse_hook_id("dual_read::ledger::").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
        // Leading empty token.
        let err = parse_hook_id("::ledger::amount").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
        // Doubled separator inside the payload.
        let err = parse_hook_id("dual_read::ledger::::amount").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
    }

    #[test]
    fn parse_hook_id_rejects_too_few_tokens() {
        // Only the family tag — no table or column.
        let err = parse_hook_id("dual_read").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
        // Family tag + table only.
        let err = parse_hook_id("dual_read::ledger").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
    }

    #[test]
    fn parse_hook_id_rejects_too_many_tokens_for_basic_form() {
        // Five tokens but no `codec` marker — neither basic nor codec
        // shape matches.
        let err = parse_hook_id("dual_read::ledger::amount::extra::tail").unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
    }

    // ── DualReadHook / DualWriteHook traits ──────────────────────────

    #[test]
    fn dual_read_hook_round_trips_through_hash_set() {
        use std::collections::HashSet;
        let mut set: HashSet<DualReadHook> = HashSet::new();
        let hook = DualReadHook {
            table: "t".to_owned(),
            legacy_column: "c".to_owned(),
            shadow_column: "c_new".to_owned(),
        };
        assert!(set.insert(hook.clone()));
        assert!(!set.insert(hook.clone()));
        assert!(set.contains(&hook));
    }

    #[test]
    fn dual_write_hook_round_trips_through_hash_set() {
        use std::collections::HashSet;
        let mut set: HashSet<DualWriteHook> = HashSet::new();
        let hook = DualWriteHook {
            table: "t".to_owned(),
            legacy_column: "c".to_owned(),
            shadow_column: "c_new".to_owned(),
            codec_transform: Some("v1->v2".to_owned()),
        };
        assert!(set.insert(hook.clone()));
        assert!(!set.insert(hook.clone()));
        assert!(set.contains(&hook));
    }

    // ── Plan walker ──────────────────────────────────────────────────

    fn step(kind: StepKind, ordinal: u32, params: StepParameters) -> Step {
        Step {
            kind,
            ordinal,
            parameters: params,
        }
    }

    /// Build a representative plan that exercises every hook
    /// transition: expand → compat-window-open → backfill →
    /// validate → cutover-reads → cutover-writes → cleanup.
    fn full_plan() -> LivePlan {
        LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo".to_owned(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428000000__demo".to_owned(),
                target_database: "main".to_owned(),
                app_label: "".to_owned(),
            },
            steps: vec![
                step(
                    StepKind::ExpandSchema,
                    0,
                    StepParameters::ExpandSchema {
                        sql_segments: vec!["ALTER TABLE t ADD COLUMN c_new INT".to_owned()],
                    },
                ),
                step(
                    StepKind::BeginCompatibilityWindow,
                    1,
                    StepParameters::BeginCompatibilityWindow {
                        hooks: vec!["dual_read::t::c".to_owned(), "dual_write::t::c".to_owned()],
                    },
                ),
                step(
                    StepKind::BackfillChunked,
                    2,
                    StepParameters::BackfillChunked {
                        table: "t".to_owned(),
                        predicate_template: "WHERE c_new IS NULL LIMIT $1 RETURNING id".to_owned(),
                        chunk_size: 1000,
                    },
                ),
                step(
                    StepKind::ValidateBackfill,
                    3,
                    StepParameters::ValidateBackfill {
                        gate_query: "SELECT count(*) FROM t WHERE c_new IS NULL".to_owned(),
                    },
                ),
                step(
                    StepKind::CutoverReads,
                    4,
                    StepParameters::CutoverReads {
                        description: "flip reads".to_owned(),
                    },
                ),
                step(
                    StepKind::CutoverWrites,
                    5,
                    StepParameters::CutoverWrites {
                        description: "flip writes".to_owned(),
                    },
                ),
                step(
                    StepKind::CleanupLegacyState,
                    6,
                    StepParameters::CleanupLegacyState {
                        sql_segments: vec!["ALTER TABLE t DROP COLUMN c".to_owned()],
                    },
                ),
            ],
        }
    }

    #[test]
    fn active_hooks_empty_before_compatibility_window_opens() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 0).unwrap();
        assert!(snap.dual_read.is_empty());
        assert!(snap.dual_write.is_empty());
        assert!(!snap.side_effects_suppressed);
    }

    #[test]
    fn active_hooks_populated_after_begin_compatibility_window() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 1).unwrap();
        assert_eq!(snap.dual_read.len(), 1);
        assert_eq!(snap.dual_read[0].legacy_column, "c");
        assert_eq!(snap.dual_read[0].shadow_column, "c_new");
        assert_eq!(snap.dual_write.len(), 1);
        assert_eq!(snap.dual_write[0].legacy_column, "c");
        assert_eq!(snap.dual_write[0].shadow_column, "c_new");
        assert!(!snap.side_effects_suppressed);
    }

    #[test]
    fn active_hooks_side_effects_suppressed_during_backfill_step() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 2).unwrap();
        assert!(
            snap.side_effects_suppressed,
            "side_effects_suppressed must be set while a BackfillChunked step is active",
        );
        // Hooks remain registered through backfill.
        assert_eq!(snap.dual_read.len(), 1);
        assert_eq!(snap.dual_write.len(), 1);
    }

    #[test]
    fn active_hooks_side_effects_suppressed_resets_after_backfill_step() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 3).unwrap();
        assert!(
            !snap.side_effects_suppressed,
            "side_effects_suppressed must reset at the next step boundary",
        );
        // Validate step does not unregister the hooks.
        assert_eq!(snap.dual_read.len(), 1);
        assert_eq!(snap.dual_write.len(), 1);
    }

    #[test]
    fn active_hooks_dual_read_dropped_after_cutover_reads() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 4).unwrap();
        assert!(
            snap.dual_read.is_empty(),
            "CutoverReads must drop dual_read entries",
        );
        assert_eq!(
            snap.dual_write.len(),
            1,
            "CutoverReads must NOT drop dual_write entries",
        );
    }

    #[test]
    fn active_hooks_dual_write_dropped_after_cutover_writes() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 5).unwrap();
        assert!(snap.dual_read.is_empty());
        assert!(
            snap.dual_write.is_empty(),
            "CutoverWrites must drop dual_write entries",
        );
    }

    #[test]
    fn active_hooks_terminal_state_clean() {
        let plan = full_plan();
        let snap = active_hooks_at_step(&plan, 6).unwrap();
        assert!(snap.dual_read.is_empty());
        assert!(snap.dual_write.is_empty());
        assert!(!snap.side_effects_suppressed);
    }

    #[test]
    fn active_hooks_ordinal_beyond_plan_saturates_to_terminal_state() {
        let plan = full_plan();
        let last_ordinal: u32 = plan.steps.last().unwrap().ordinal;
        let terminal = active_hooks_at_step(&plan, last_ordinal).unwrap();
        let beyond = active_hooks_at_step(&plan, last_ordinal + 100).unwrap();
        assert_eq!(terminal, beyond);
    }

    #[test]
    fn active_hooks_walker_propagates_malformed_hook_id() {
        let mut plan = full_plan();
        plan.steps[1].parameters = StepParameters::BeginCompatibilityWindow {
            hooks: vec!["this_is_not_a_valid_hook".to_owned()],
        };
        let err = active_hooks_at_step(&plan, 1).unwrap_err();
        assert!(matches!(err, HookError::MalformedHookId(_)));
    }

    #[test]
    fn active_hooks_codec_form_round_trips_through_walker() {
        let mut plan = full_plan();
        plan.steps[1].parameters = StepParameters::BeginCompatibilityWindow {
            hooks: vec![
                "dual_read::codec::secret::ciphertext::aes_gcm_v1".to_owned(),
                "dual_write::codec::secret::ciphertext::aes_gcm_v1->aes_gcm_v2".to_owned(),
            ],
        };
        let snap = active_hooks_at_step(&plan, 1).unwrap();
        assert_eq!(snap.dual_read.len(), 1);
        assert_eq!(snap.dual_read[0].table, "secret");
        assert_eq!(snap.dual_read[0].legacy_column, "ciphertext");
        assert_eq!(snap.dual_read[0].shadow_column, "ciphertext_new");
        assert_eq!(snap.dual_write.len(), 1);
        assert_eq!(
            snap.dual_write[0].codec_transform.as_deref(),
            Some("aes_gcm_v1->aes_gcm_v2"),
        );
    }

    #[test]
    fn active_hooks_walker_aggregates_multiple_compat_windows() {
        // A plan with two BeginCompatibilityWindow steps registers
        // the union of the two hook sets. (No real pattern emits this
        // shape today, but the walker must remain composition-safe so
        // future patterns that stage two compat windows don't trip
        // the snapshot.)
        let plan = LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "stacked".to_owned(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428000000__stacked".to_owned(),
                target_database: "main".to_owned(),
                app_label: "".to_owned(),
            },
            steps: vec![
                step(
                    StepKind::BeginCompatibilityWindow,
                    0,
                    StepParameters::BeginCompatibilityWindow {
                        hooks: vec!["dual_read::t::a".to_owned()],
                    },
                ),
                step(
                    StepKind::BeginCompatibilityWindow,
                    1,
                    StepParameters::BeginCompatibilityWindow {
                        hooks: vec!["dual_write::t::b".to_owned()],
                    },
                ),
            ],
        };
        let snap = active_hooks_at_step(&plan, 1).unwrap();
        assert_eq!(snap.dual_read.len(), 1);
        assert_eq!(snap.dual_read[0].legacy_column, "a");
        assert_eq!(snap.dual_write.len(), 1);
        assert_eq!(snap.dual_write[0].legacy_column, "b");
    }

    // ── Side-effect suppression GUC name pinning ─────────────────────

    #[test]
    fn side_effect_suppression_const_shared_with_backfill_module() {
        // The chunked-backfill module is the canonical home of the
        // GUC name; this module's consumer path imports it directly
        // via `use crate::live_migrate::backfill::*`. The assertion
        // pins the namespace prefix so a future rename has to update
        // both the producer (the chunked-backfill writer) and this
        // consumer in lockstep.
        assert!(
            SIDE_EFFECT_SUPPRESSION_TXN_LOCAL.starts_with("djogi."),
            "GUC name must live under djogi.* namespace: {SIDE_EFFECT_SUPPRESSION_TXN_LOCAL:?}",
        );
    }

    // ── HookError display ────────────────────────────────────────────

    #[test]
    fn hook_error_malformed_id_message_includes_offending_value() {
        let err = HookError::MalformedHookId("garbage".to_owned());
        let msg = format!("{err}");
        assert!(
            msg.contains("garbage"),
            "expected offending id in message: {msg}",
        );
        assert!(
            msg.contains("malformed"),
            "expected `malformed` hint: {msg}",
        );
    }
}
