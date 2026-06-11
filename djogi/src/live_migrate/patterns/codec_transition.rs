//! Protected-field codec transition pattern.
//! Covers the "Codec transition (same decoded type)" row of the v3
//! plan §7 classification table. Triggered when a protected field's
//! `FieldCodec` rotates (e.g. AES-256-GCM key rotation, encoding
//! change) and `FieldCodec::classify_transition::<Other>()` reports
//! the transition rewrites ciphertext.
//! # Operation shape
//! Until a dedicated `CodecChange` variant lands on
//! [`SchemaOperation`], this pattern accepts an
//! [`AlterColumn`](SchemaOperation::AlterColumn) carrying
//! [`ColumnChange::ChangeType`] whose `from`/`to` are interpreted by
//! the dispatcher as the old / new codec identifiers. The step
//! graph is a structural mirror of
//! [`replacement_column`](super::replacement_column) — the
//! semantically-meaningful difference is the per-row conversion the
//! dual-write hook performs (`encode_new(decode_old(value))` rather
//! than a SQL cast). The runner consumes the same plan-file shape
//! either way.
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD COLUMN
//! <c>_new BYTEA NULL`. The shadow column lands as `BYTEA` so
//!    the codec can swap encoding shapes (the `to` codec ID lives in
//!    the descriptor, not in the column type). The `_new` suffix
//!    matches the convention pinned by
//!    [`replacement_column`](super::replacement_column) and the
//!    runtime hook parser at
//!    [`crate::live_migrate::hooks`] — every shadow-column-style
//!    pattern uses the same suffix so the parser can derive
//!    `shadow_column` from a hook ID alone.
//! 2. [`StepKind::BeginCompatibilityWindow`] — register the dual-
//!    read / dual-write hooks. The hook IDs include the old + new
//!    codec identifiers so the runtime layer can route encode /
//!    decode calls to the right codec implementation per row.
//! 3. [`StepKind::BackfillChunked`] — copy `<c>` into `<c>_new`
//!    re-encoded under the new codec. The predicate `WHERE
//! <c>_new IS NULL` is structurally idempotent — once a row
//!    is re-encoded the chunk skips it on subsequent passes.
//! 4. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//!    until `SELECT count(*) FROM <t> WHERE <c>_new IS NULL`
//!    returns zero.
//! 5. [`StepKind::CutoverReads`] — visage projection switches reads
//!    onto the new codec.
//! 6. [`StepKind::CutoverWrites`] — writes target the new codec
//!    only.
//! 7. [`StepKind::CleanupLegacyState`] — `DROP COLUMN <c>` then
//!    `RENAME COLUMN <c>_new TO <c>`.
//!
//! # Status (issue #371): staged, not wired
//! This pattern and its `djogi_codec_recode(<col>, '<from>', '<to>')` backfill
//! placeholder are the *staged* implementation of ONLINE codec rotation. They
//! are intentionally NOT reachable today. Codec drift IS detected — the differ
//! emits [`ColumnChange::CodecChange`](crate::migrate::diff::ColumnChange::CodecChange)
//! (see [`crate::migrate::diff`]), the classifier rates it `OfflineOnly`
//! (add / drop) or `ExpandContract` (codec → codec) via
//! [`crate::live_migrate::classify`], and `dispatch_pattern` refuses it with an
//! actionable `CannotEmit` — so a codec change never reaches this online emitter
//! outside its own unit tests. What stays deferred is the ONLINE EXECUTION: an
//! automatic in-place re-encode backfill. That awaits a server-side
//! `djogi_codec_recode` function and a dedicated online-rotation
//! `SchemaOperation`. Until then, codec rotation is "append a ring entry and
//! re-encrypt forward" via an operator-run offline migration; the
//! `djogi_codec_recode` identifier is a placeholder with no server-side
//! definition, surfaced only as a documented manual step on the offline compose
//! path.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::Step;
use crate::migrate::SchemaOperation;
// `StepKind` / `StepParameters` / `ColumnChange` are referenced only by the
// staged (online-rotation) step-graph, which is `#[cfg(test)]`-gated in `emit`
// (issue #371 — the production path is a hard refusal). Gating the imports the
// same way keeps the production build warning-free.
#[cfg(test)]
use crate::live_migrate::plan::{StepKind, StepParameters};
#[cfg(test)]
use crate::migrate::diff::ColumnChange;

/// Marker type implementing [`Pattern`] for codec rotation.
pub struct CodecTransition;

impl Pattern for CodecTransition {
    const ID: &'static str = "codec_transition";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        // Hard stop in production builds: `djogi_codec_recode` has no
        // server-side implementation yet (issue #371 — online codec rotation is
        // staged, not wired). This pattern is unreachable in practice
        // (`dispatch_pattern` never routes here, and codec-driven changes
        // classify OfflineOnly via the dedicated `ColumnChange::CodecChange`
        // op), but if a future refactor ever wires it into `dispatch_pattern`
        // without first implementing the server-side function, a live plan would
        // emit a backfill calling SQL that does not exist. Refuse loudly rather
        // than emit unrunnable SQL. The staged step-graph below is gated
        // `#[cfg(test)]` so its unit tests stay green while the production path
        // is a hard refusal — explicit, tested, guarded deferral, not silent
        // dead code.
        #[cfg(not(test))]
        {
            // Consume both params so they are not flagged unused in the
            // production build, where the staged body below is compiled out.
            let _ = (op, ctx);
            Err(PatternError::CannotEmit {
                pattern: Self::ID,
                reason: "online codec rotation is not implemented in this release: the \
                         `djogi_codec_recode` backfill function has no server-side \
                         definition yet (issue #371). Codec changes are offline-only — \
                         re-encrypt rows via an operator-run migration. This pattern is \
                         staged for a post-v1 codec-rotation operation and must not emit a \
                         live plan."
                    .to_string(),
            })
        }

        #[cfg(test)]
        {
            // belt-and-braces refusal when the adopter supplied
            // a `#[field(type_change_using = "<expr>")]` clause. The
            // classifier
            // ([`crate::live_migrate::classify::classify_column_change`])
            // routes `using.is_some()` to `OfflineOnly`, so this pattern
            // should never be dispatched in that case. Like
            // [`super::replacement_column`], the codec_transition backfill
            // cannot replicate a custom USING body — the conversion is
            // `djogi_codec_recode(...)` and there is no slot for an
            // adopter SQL fragment. Refuse loudly so a future codec route
            // cannot silently mis-emit.
            if let SchemaOperation::AlterColumn {
                change: ColumnChange::ChangeType { using: Some(_), .. },
                ..
            } = op
            {
                return Err(PatternError::CannotEmit {
                    pattern: Self::ID,
                    reason: "ColumnChange::ChangeType carries adopter-supplied `using` \
                         (#[field(type_change_using = \"...\")]); codec rotation \
                         emits `djogi_codec_recode(...)` in its backfill and has \
                         no slot for an adopter SQL fragment. The classifier \
                         routes this case to OfflineOnly — apply the migration \
                         via the offline path"
                        .to_string(),
                });
            }
            let (table, column, from_codec, to_codec) = match op {
                SchemaOperation::AlterColumn {
                    table,
                    column,
                    // codec transitions key off (from, to)
                    // only; the adopter USING expression does not
                    // influence shadow-column staging. The
                    // `using.is_some()` arm is refused above, so binding
                    // with `..` here is correct for the `using.is_none()`
                    // remainder.
                    change: ColumnChange::ChangeType { from, to, .. },
                } => (table, column, from, to),
                _ => {
                    return Err(PatternError::WrongOperation {
                        pattern: Self::ID,
                        reason: "expected AlterColumn { change: ChangeType } carrying codec ids"
                            .to_string(),
                    });
                }
            };

            // Shadow column convention: every shadow-column-style pattern
            // uses the `_new` suffix (see module docs above and the parser
            // in [`crate::live_migrate::hooks`]). Codec transition is no
            // exception — uniformity lets the hook parser derive
            // `shadow_column` from a hook ID alone, with no per-pattern
            // suffix table.
            let shadow = format!("{column}_new");
            let expand_sql = format!(
                "ALTER TABLE {tbl} ADD COLUMN {shadow_q} BYTEA NULL",
                tbl = quote_ident(table),
                shadow_q = quote_ident(&shadow),
            );
            // Backfill template: the runner concatenates this onto
            // `UPDATE <table> `, producing a complete UPDATE statement.
            // The conversion expression `djogi_codec_recode(<col>, '<from>',
            // '<to>')` is a placeholder identifier — the runtime layer
            // resolves it through the codec dispatch registry. The SQL
            // body is shaped the same as
            // [`super::replacement_column`]: idempotent inner predicate
            // (`<shadow> IS NULL`) bounded by `LIMIT $1`, no other
            // placeholders. The codec function name is interpolated as a
            // bare SQL identifier (no quoting required — the registry
            // owns the namespace) and the codec IDs travel as quoted
            // single-quoted literals; both sides of the conversion are
            // controlled by the framework, not by user input.
            let backfill_predicate = format!(
                "SET {shadow_q} = djogi_codec_recode({col_q}, '{from}', '{to}') WHERE id IN (SELECT id FROM {tbl} WHERE {shadow_q} IS NULL LIMIT $1)",
                shadow_q = quote_ident(&shadow),
                col_q = quote_ident(column),
                from = from_codec,
                to = to_codec,
                tbl = quote_ident(table),
            );
            let gate_query = format!(
                "SELECT count(*) FROM {tbl} WHERE {shadow_q} IS NULL",
                tbl = quote_ident(table),
                shadow_q = quote_ident(&shadow),
            );
            let drop_legacy_sql = format!(
                "ALTER TABLE {tbl} DROP COLUMN {col_q}",
                tbl = quote_ident(table),
                col_q = quote_ident(column),
            );
            let rename_sql = format!(
                "ALTER TABLE {tbl} RENAME COLUMN {shadow_q} TO {col_q}",
                tbl = quote_ident(table),
                shadow_q = quote_ident(&shadow),
                col_q = quote_ident(column),
            );

            Ok(vec![
                Step {
                    kind: StepKind::ExpandSchema,
                    ordinal: 0,
                    parameters: StepParameters::ExpandSchema {
                        sql_segments: vec![expand_sql],
                    },
                },
                Step {
                    kind: StepKind::BeginCompatibilityWindow,
                    ordinal: 1,
                    parameters: StepParameters::BeginCompatibilityWindow {
                        hooks: vec![
                            format!("dual_read::codec::{table}::{column}::{from_codec}"),
                            format!(
                                "dual_write::codec::{table}::{column}::{from_codec}->{to_codec}",
                            ),
                        ],
                    },
                },
                Step {
                    kind: StepKind::BackfillChunked,
                    ordinal: 2,
                    parameters: StepParameters::BackfillChunked {
                        table: table.clone(),
                        predicate_template: backfill_predicate,
                        chunk_size: ctx.backfill_chunk_size,
                    },
                },
                Step {
                    kind: StepKind::ValidateBackfill,
                    ordinal: 3,
                    parameters: StepParameters::ValidateBackfill { gate_query },
                },
                Step {
                    kind: StepKind::CutoverReads,
                    ordinal: 4,
                    parameters: StepParameters::CutoverReads {
                        description: format!(
                            "flip read path for {table}.{column} onto codec {to_codec}",
                        ),
                    },
                },
                Step {
                    kind: StepKind::CutoverWrites,
                    ordinal: 5,
                    parameters: StepParameters::CutoverWrites {
                        description: format!(
                            "flip write path for {table}.{column} onto codec {to_codec}; drop dual-write",
                        ),
                    },
                },
                Step {
                    kind: StepKind::CleanupLegacyState,
                    ordinal: 6,
                    parameters: StepParameters::CleanupLegacyState {
                        sql_segments: vec![drop_legacy_sql, rename_sql],
                    },
                },
            ])
        } // end #[cfg(test)] staged-implementation block
    }
}

// Only the `#[cfg(test)]`-gated staged step-graph uses this helper (the
// production `emit` path is a hard refusal — issue #371).
#[cfg(test)]
fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    out.push_str(name);
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PatternContext {
        PatternContext::with_defaults()
    }

    fn op() -> SchemaOperation {
        SchemaOperation::AlterColumn {
            table: "secret".to_string(),
            column: "ciphertext".to_string(),
            change: ColumnChange::ChangeType {
                from: "aes_gcm_v1".to_string(),
                to: "aes_gcm_v2".to_string(),
                using: None,
            },
        }
    }

    #[test]
    fn emits_seven_step_codec_rotation_sequence() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 7);
        let kinds: Vec<_> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                StepKind::ExpandSchema,
                StepKind::BeginCompatibilityWindow,
                StepKind::BackfillChunked,
                StepKind::ValidateBackfill,
                StepKind::CutoverReads,
                StepKind::CutoverWrites,
                StepKind::CleanupLegacyState,
            ],
        );
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn compatibility_window_hooks_carry_old_and_new_codec_ids() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::BeginCompatibilityWindow { hooks } = &steps[1].parameters else {
            panic!("expected BeginCompatibilityWindow at ordinal 1");
        };
        assert_eq!(hooks.len(), 2);
        assert!(hooks.iter().any(|h| h.contains("aes_gcm_v1")));
        assert!(hooks.iter().any(|h| h.contains("aes_gcm_v2")));
    }

    #[test]
    fn backfill_template_emits_complete_update_tail_with_set_and_limit() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        assert!(predicate_template.contains("SET"));
        assert!(predicate_template.contains("\"ciphertext_new\""));
        assert!(predicate_template.contains("djogi_codec_recode"));
        assert!(predicate_template.contains("'aes_gcm_v1'"));
        assert!(predicate_template.contains("'aes_gcm_v2'"));
        assert!(predicate_template.contains("IS NULL"));
        assert!(predicate_template.contains("LIMIT $1"));
        assert!(predicate_template.contains("WHERE id IN (SELECT id FROM"));
        assert!(
            !predicate_template.contains("$2"),
            "template must not bind any placeholder beyond $1: {predicate_template}",
        );
    }

    #[test]
    fn backfill_template_concatenates_into_valid_update_statement() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            table,
            predicate_template,
            ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        let stmt = format!("UPDATE {table} {predicate_template}");
        assert!(stmt.contains("UPDATE"));
        assert!(stmt.contains("SET"));
        assert!(stmt.contains("WHERE"));
        assert!(stmt.contains("LIMIT $1"));
        let set_pos = stmt.find("SET").unwrap();
        let where_pos = stmt.find("WHERE").unwrap();
        assert!(set_pos < where_pos, "SET must precede WHERE: {stmt}");
    }

    #[test]
    fn shadow_column_uses_canonical_new_suffix() {
        // The runtime hook parser in [`crate::live_migrate::hooks`]
        // derives `shadow_column` by appending `_new` to the legacy
        // column. Every shadow-column-style pattern (replacement_column
        // and codec_transition both) must follow that convention so the
        // parser can recover the shadow column name from a hook ID
        // without a per-pattern suffix table.
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(
            sql_segments[0].contains("\"ciphertext_new\""),
            "shadow column must use the canonical _new suffix: {}",
            sql_segments[0],
        );
        assert!(
            !sql_segments[0].contains("_recoded"),
            "shadow column must NOT use the legacy _recoded suffix: {}",
            sql_segments[0],
        );
    }

    #[test]
    fn cleanup_drops_legacy_then_renames_shadow() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[6].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 2);
        assert!(sql_segments[0].contains("DROP COLUMN \"ciphertext\""));
        assert!(sql_segments[1].contains("RENAME COLUMN \"ciphertext_new\" TO \"ciphertext\""));
    }

    #[test]
    fn rejects_set_nullable() {
        let op = SchemaOperation::AlterColumn {
            table: "secret".to_string(),
            column: "ciphertext".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        let err = CodecTransition::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_change_type_with_adopter_using() {
        // adopter-supplied `using` forces the offline path
        // for codec transitions too. While the codec_transition
        // dispatcher route is currently unreachable (the
        // ChangeType→pattern dispatcher routes every non-using
        // ChangeType to replacement_column), the refusal here is the
        // defense-in-depth guard for the future codec route: codec
        // rotation emits `djogi_codec_recode(...)` in its backfill
        // and has no slot for an adopter SQL fragment, so silently
        // dropping the USING would silently corrupt data.
        let op = SchemaOperation::AlterColumn {
            table: "secret".to_string(),
            column: "ciphertext".to_string(),
            change: ColumnChange::ChangeType {
                from: "aes_gcm_v1".to_string(),
                to: "aes_gcm_v2".to_string(),
                using: Some("djogi_codec_recode(ciphertext, 'a', 'b')".to_string()),
            },
        };
        let err = CodecTransition::emit(&op, &ctx()).unwrap_err();
        match err {
            PatternError::CannotEmit { reason, .. } => {
                assert!(
                    reason.contains("type_change_using") || reason.contains("USING"),
                    "refusal reason must name the corrective attribute: {reason}",
                );
                assert!(
                    reason.contains("offline") || reason.contains("OfflineOnly"),
                    "refusal reason must point at the offline path: {reason}",
                );
            }
            other => panic!("expected CannotEmit, got: {other:?}"),
        }
    }
}
