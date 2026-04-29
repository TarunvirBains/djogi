//! Protected-field codec transition pattern.
//!
//! Covers the "Codec transition (same decoded type)" row of the v3
//! plan §7 classification table. Triggered when a protected field's
//! `FieldCodec` rotates (e.g. AES-256-GCM key rotation, encoding
//! change) and `FieldCodec::classify_transition::<Other>()` reports
//! the transition rewrites ciphertext.
//!
//! # Operation shape
//!
//! Until a dedicated `CodecChange` variant lands on
//! [`SchemaOperation`], this pattern accepts an
//! [`AlterColumn`](SchemaOperation::AlterColumn) carrying
//! [`ColumnChange::ChangeType`] whose `from`/`to` are interpreted by
//! the dispatcher (T10) as the old / new codec identifiers. The step
//! graph is a structural mirror of
//! [`replacement_column`](super::replacement_column) — the
//! semantically-meaningful difference is the per-row conversion the
//! dual-write hook performs (`encode_new(decode_old(value))` rather
//! than a SQL cast). The runner consumes the same plan-file shape
//! either way.
//!
//! # Step graph
//!
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD COLUMN
//!    <c>_recoded BYTEA NULL`. The shadow column lands as `BYTEA` so
//!    the codec can swap encoding shapes (the `to` codec ID lives in
//!    the descriptor, not in the column type).
//! 2. [`StepKind::BeginCompatibilityWindow`] — register the dual-
//!    read / dual-write hooks. The hook IDs include the old + new
//!    codec identifiers so the runtime layer can route encode /
//!    decode calls to the right codec implementation per row.
//! 3. [`StepKind::BackfillChunked`] — copy `<c>` into `<c>_recoded`
//!    re-encoded under the new codec. The predicate `WHERE
//!    <c>_recoded IS NULL` is structurally idempotent — once a row
//!    is re-encoded the chunk skips it on subsequent passes.
//! 4. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//!    until `SELECT count(*) FROM <t> WHERE <c>_recoded IS NULL`
//!    returns zero.
//! 5. [`StepKind::CutoverReads`] — visage projection switches reads
//!    onto the new codec.
//! 6. [`StepKind::CutoverWrites`] — writes target the new codec
//!    only.
//! 7. [`StepKind::CleanupLegacyState`] — `DROP COLUMN <c>` then
//!    `RENAME COLUMN <c>_recoded TO <c>`.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::diff::ColumnChange;

/// Marker type implementing [`Pattern`] for codec rotation.
pub struct CodecTransition;

impl Pattern for CodecTransition {
    const ID: &'static str = "codec_transition";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        let (table, column, from_codec, to_codec) = match op {
            SchemaOperation::AlterColumn {
                table,
                column,
                change: ColumnChange::ChangeType { from, to },
            } => (table, column, from, to),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AlterColumn { change: ChangeType } carrying codec ids"
                        .to_string(),
                });
            }
        };

        let shadow = format!("{column}_recoded");
        let expand_sql = format!(
            "ALTER TABLE {tbl} ADD COLUMN {shadow_q} BYTEA NULL",
            tbl = quote_ident(table),
            shadow_q = quote_ident(&shadow),
        );
        let backfill_predicate = format!(
            "WHERE {shadow_q} IS NULL AND id BETWEEN $1 AND $2",
            shadow_q = quote_ident(&shadow),
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
                        format!("dual_write::codec::{table}::{column}::{from_codec}->{to_codec}",),
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
    }
}

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
    fn backfill_predicate_is_idempotent_via_is_null() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        assert!(predicate_template.contains("IS NULL"));
        assert!(predicate_template.contains("\"ciphertext_recoded\""));
    }

    #[test]
    fn cleanup_drops_legacy_then_renames_shadow() {
        let steps = CodecTransition::emit(&op(), &ctx()).unwrap();
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[6].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 2);
        assert!(sql_segments[0].contains("DROP COLUMN \"ciphertext\""));
        assert!(sql_segments[1].contains("RENAME COLUMN \"ciphertext_recoded\" TO \"ciphertext\""));
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
}
