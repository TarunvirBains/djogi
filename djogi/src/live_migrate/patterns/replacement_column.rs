//! Replacement-column expand/contract pattern.
//! Covers the "Change column type — requires rewrite" row of the v3
//! plan §7 classification table. The descriptor change that triggers
//! this pattern is an [`AlterColumn`](SchemaOperation::AlterColumn)
//! carrying [`ColumnChange::ChangeType`] whose `from`/`to` rendered
//! types differ in storage shape (varchar widening within `Pg18`
//! catalog-only paths classifies as `OnlineSafe` and never reaches
//! this pattern).
//! # Step graph
//! 1. [`StepKind::ExpandSchema`] — `ALTER TABLE <t> ADD COLUMN
//! <c>_new <to> NULL`. Adds the shadow column the rewrite drains
//! into.
//! 2. [`StepKind::BeginCompatibilityWindow`] — register the dual-
//! read / dual-write hooks that keep `<c>` and `<c>_new` aligned
//! across in-flight transactions.
//! 3. [`StepKind::BackfillChunked`] — copy `<c>` into `<c>_new`. The
//! predicate template emits a complete `UPDATE`-tail fragment of
//! the shape `SET <c>_new = <c>::<to-type> WHERE id IN (SELECT id
//! FROM <t> WHERE <c>_new IS NULL LIMIT $1)` — bounded to one
//! chunk via the `LIMIT $1` placeholder, idempotent because the
//! inner predicate self-cancels (once a row's shadow column is
//! populated, the chunk skips it).
//! 4. [`StepKind::ValidateBackfill`] — operator gate; runner pauses
//! until `SELECT count(*) FROM <t> WHERE <c>_new IS NULL` returns
//! zero.
//! 5. [`StepKind::CutoverReads`] — visage projection switches reads
//! to `<c>_new`.
//! 6. [`StepKind::CutoverWrites`] — writes target `<c>_new` only.
//! Dual-write hook turns off.
//! 7. [`StepKind::FinalizeConstraints`] — apply NOT NULL / CHECK on
//! `<c>_new` if the original column carried them.
//! 8. [`StepKind::CleanupLegacyState`] — `DROP COLUMN <c>` then
//! `RENAME COLUMN <c>_new TO <c>`.
//! # Idempotency
//! The chunk's WHERE predicate `<c>_new IS NULL` self-cancels — once
//! a row's shadow column has a non-null value, the row falls out of
//! the predicate forever and re-runs are a no-op. The `SET <c>_new =
//! <c>::<to-type>` cast is a stable-volatility expression so a
//! crashed-and-resumed chunk produces the same shadow value as the
//! prior attempt would have.

use super::{Pattern, PatternContext, PatternError};
use crate::live_migrate::plan::{Step, StepKind, StepParameters};
use crate::migrate::SchemaOperation;
use crate::migrate::diff::ColumnChange;

/// Marker type implementing [`Pattern`] for the shadow-column type
/// change rollout.
pub struct ReplacementColumn;

impl Pattern for ReplacementColumn {
    const ID: &'static str = "replacement_column";
    const IDEMPOTENT_PREDICATE: bool = true;

    fn emit(op: &SchemaOperation, ctx: &PatternContext) -> Result<Vec<Step>, PatternError> {
        // belt-and-braces refusal when the adopter supplied
        // a `#[field(type_change_using = "<expr>")]` clause. The
        // classifier
        // ([`crate::live_migrate::classify::classify_column_change`])
        // routes `using.is_some()` to `OfflineOnly` so this pattern
        // should never be dispatched in that case; the explicit refusal
        // below is a defense-in-depth guard. The shadow-column backfill
        // can only emit a plain SQL cast (`SET <shadow> = <col>::<to>`)
        // and cannot replicate the adopter's USING body — emitting the
        // default cast anyway would silently corrupt or fail-per-row on
        // exactly the rows the adopter wrote the expression to handle.
        if let SchemaOperation::AlterColumn {
            change: ColumnChange::ChangeType { using: Some(_), .. },
            ..
        } = op
        {
            return Err(PatternError::CannotEmit {
                pattern: Self::ID,
                reason: "ColumnChange::ChangeType carries adopter-supplied `using` \
       (#[field(type_change_using = \"...\")]); the shadow-column \
       backfill cannot replicate a custom USING expression. The \
       classifier routes this case to OfflineOnly — apply the \
       migration via the offline path"
                    .to_string(),
            });
        }
        let (table, column, to_type) = match op {
            SchemaOperation::AlterColumn {
                table,
                column,
                change: ColumnChange::ChangeType { to, .. },
            } => (table, column, to),
            _ => {
                return Err(PatternError::WrongOperation {
                    pattern: Self::ID,
                    reason: "expected AlterColumn { change: ChangeType }".to_string(),
                });
            }
        };

        let shadow = format!("{column}_new");
        let expand_sql = format!(
            "ALTER TABLE {tbl} ADD COLUMN {shadow_q} {ty} NULL",
            tbl = quote_ident(table),
            shadow_q = quote_ident(&shadow),
            ty = to_type,
        );
        // Backfill template: a complete UPDATE-tail fragment that the
        // runner concatenates onto `UPDATE <table> `. The shape is
        // SET <shadow> = <legacy>::<to-type>
        // WHERE id IN (SELECT id FROM <table>
        // WHERE <shadow> IS NULL
        // LIMIT $1)
        // - `SET <shadow> = <legacy>::<to-type>` is the conversion. A
        // plain SQL cast suffices for the shapes the classifier
        // routes here (binary-coercible widening was filtered out by
        // the classifier and never reaches this pattern; rewrites
        // that need a non-cast transform must classify as
        // OfflineOnly per the §7 amendment).
        // - The WHERE predicate `<shadow> IS NULL` is structurally
        // idempotent: once a row converges, it falls out forever.
        // - `LIMIT $1` bounds the row count to one chunk; `$1` is the
        // only placeholder the runner binds.
        let backfill_predicate = format!(
            "SET {shadow_q} = {col_q}::{ty} WHERE id IN (SELECT id FROM {tbl} WHERE {shadow_q} IS NULL LIMIT $1)",
            shadow_q = quote_ident(&shadow),
            col_q = quote_ident(column),
            ty = to_type,
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
                        format!("dual_read::{table}::{column}"),
                        format!("dual_write::{table}::{column}"),
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
                    description: format!("flip read path for {table}.{column} onto {shadow}"),
                },
            },
            Step {
                kind: StepKind::CutoverWrites,
                ordinal: 5,
                parameters: StepParameters::CutoverWrites {
                    description: format!(
                        "flip write path for {table}.{column} onto {shadow}; drop dual-write",
                    ),
                },
            },
            Step {
                kind: StepKind::FinalizeConstraints,
                ordinal: 6,
                parameters: StepParameters::FinalizeConstraints {
                    sql_segments: Vec::new(),
                },
            },
            Step {
                kind: StepKind::CleanupLegacyState,
                ordinal: 7,
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
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "INTEGER".to_string(),
                to: "BIGINT".to_string(),
                using: None,
            },
        }
    }

    #[test]
    fn emits_eight_step_expand_contract_sequence() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        assert_eq!(steps.len(), 8);
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
                StepKind::FinalizeConstraints,
                StepKind::CleanupLegacyState,
            ],
        );
    }

    #[test]
    fn emitted_ordinals_are_sequential_and_kinds_are_consistent() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        for (idx, step) in steps.iter().enumerate() {
            assert_eq!(step.ordinal as usize, idx);
            assert_eq!(step.kind, step.parameters.kind());
        }
    }

    #[test]
    fn expand_step_adds_shadow_column_with_target_type() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::ExpandSchema { sql_segments } = &steps[0].parameters else {
            panic!("expected ExpandSchema");
        };
        assert_eq!(sql_segments.len(), 1);
        assert!(sql_segments[0].contains("ADD COLUMN"));
        assert!(sql_segments[0].contains("\"amount_new\""));
        assert!(sql_segments[0].contains("BIGINT"));
        assert!(sql_segments[0].contains("NULL"));
    }

    #[test]
    fn backfill_template_emits_complete_update_tail_with_set_and_limit() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::BackfillChunked {
            predicate_template, ..
        } = &steps[2].parameters
        else {
            panic!("expected BackfillChunked");
        };
        // SET clause names the shadow column and the conversion.
        assert!(
            predicate_template.contains("SET"),
            "template must include SET clause: {predicate_template}",
        );
        assert!(
            predicate_template.contains("\"amount_new\""),
            "template must reference shadow column: {predicate_template}",
        );
        // Idempotent WHERE inside a subquery, bounded by LIMIT $1.
        assert!(
            predicate_template.contains("IS NULL"),
            "WHERE predicate must self-cancel via IS NULL: {predicate_template}",
        );
        assert!(
            predicate_template.contains("LIMIT $1"),
            "template must bound row count via LIMIT $1: {predicate_template}",
        );
        // Subquery that bounds the rewrite to one chunk.
        assert!(
            predicate_template.contains("WHERE id IN (SELECT id FROM"),
            "template must use canonical id-IN-subquery shape: {predicate_template}",
        );
        // No stray placeholder beyond `$1`.
        assert!(
            !predicate_template.contains("$2"),
            "template must not bind any placeholder beyond $1: {predicate_template}",
        );
    }

    #[test]
    fn backfill_template_concatenates_into_valid_update_statement() {
        // End-to-end check: the runner builds the chunk SQL by
        // concatenating "UPDATE <table> " with the predicate template.
        // The result must be a valid Postgres UPDATE statement
        // specifically, it must contain `UPDATE`, `SET`, `WHERE`, and
        // a `LIMIT $1` somewhere after the SET.
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
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
        // SET must come before WHERE — otherwise the statement is
        // syntactically broken.
        let set_pos = stmt.find("SET").unwrap();
        let where_pos = stmt.find("WHERE").unwrap();
        assert!(
            set_pos < where_pos,
            "SET must precede WHERE in the synthesised UPDATE: {stmt}",
        );
    }

    #[test]
    fn cleanup_drops_legacy_then_renames_shadow() {
        let steps = ReplacementColumn::emit(&op(), &ctx()).unwrap();
        let StepParameters::CleanupLegacyState { sql_segments } = &steps[7].parameters else {
            panic!("expected CleanupLegacyState");
        };
        assert_eq!(sql_segments.len(), 2);
        assert!(sql_segments[0].contains("DROP COLUMN \"amount\""));
        assert!(sql_segments[1].contains("RENAME COLUMN \"amount_new\" TO \"amount\""));
    }

    #[test]
    fn rejects_non_change_type_alter_column() {
        let op = SchemaOperation::AlterColumn {
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        let err = ReplacementColumn::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_drop_column() {
        let op = SchemaOperation::DropColumn {
            table: "ledger_entry".to_string(),
            column: "amount".to_string(),
        };
        let err = ReplacementColumn::emit(&op, &ctx()).unwrap_err();
        assert!(matches!(err, PatternError::WrongOperation { .. }));
    }

    #[test]
    fn rejects_change_type_with_adopter_using() {
        // adopter-supplied `using` forces the offline path.
        // The classifier
        // ([`crate::live_migrate::classify::classify_column_change`])
        // routes this case to `OfflineOnly` so the dispatcher never
        // reaches the pattern. The explicit refusal here is the
        // defense-in-depth guard against future composers that bypass
        // the classifier — emitting the default cast in the backfill
        // would silently corrupt or fail-per-row on exactly the rows
        // the adopter wrote the expression to handle.
        let op = SchemaOperation::AlterColumn {
            table: "items".to_string(),
            column: "kind".to_string(),
            change: ColumnChange::ChangeType {
                from: "TEXT".to_string(),
                to: "UUID".to_string(),
                using: Some("(\"kind\"::text)::uuid".to_string()),
            },
        };
        let err = ReplacementColumn::emit(&op, &ctx()).unwrap_err();
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
