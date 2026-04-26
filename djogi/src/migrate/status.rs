//! `migrations status` rendering — T6's read-only status command.
//!
//! Walks the migration ledger, groups by `app_label`, sorts by
//! `applied_at` ASC within an app, and renders an operator-facing
//! table. Implements the v3 §3 + §6 amendment exit-code matrix:
//!
//! - All applied → exit 0.
//! - Any pending / failed → exit 1.
//! - Unknown `app_label` (D010 inline warning) → exit 1.
//!
//! # Read-only
//!
//! Status NEVER acquires the workspace file lock and never writes to
//! the database. The lock-free contract is per the v3 file-lock
//! contract. Concurrent compose / apply invocations may race the read
//! — that is acceptable: status is informational, and an interleaved
//! mutation produces a snapshot of the ledger as of the read instant.
//!
//! # Pure rendering
//!
//! [`render`] is a pure function from `(ledger rows, registered apps)`
//! to a [`StatusReport`] holding owned strings. The CLI surface
//! prints the report; tests assert on the report shape directly so
//! we never have to parse stdout.

use std::collections::BTreeMap;

use super::diff::Classification;
use super::ledger::{LedgerStatus, LedgerSummaryRow};
use super::segment::MigrationPlan;
use crate::DjogiContext;
use crate::error::DjogiError;

/// Output of [`render`]. Pre-formatted lines + the exit code so the
/// CLI just prints and exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// Operator-facing report — one entry per `(app, row)` pair plus
    /// inline D010 warnings. The CLI prints each line with a trailing
    /// newline.
    pub lines: Vec<String>,
    /// Number of D010 warnings issued. Surfaced separately so the
    /// CLI can decide on summary text without re-walking `lines`.
    pub d010_warnings: usize,
    /// `0` when every ledger row is `applied` / `baseline` /
    /// `rolled_back` and no D010 fires; `1` otherwise.
    pub exit_code: i32,
}

impl StatusReport {
    /// `true` when [`exit_code`](Self::exit_code) is non-zero.
    pub fn is_error_exit(&self) -> bool {
        self.exit_code != 0
    }
}

/// Render the status report.
///
/// `rows` is the result of [`super::ledger::select_all`]; the order
/// is already `(app_label ASC, applied_at ASC, id ASC)` so `render`
/// just walks the slice once.
///
/// `registered_apps` is the union of every label currently present
/// in [`crate::apps::AppRegistry::all`]. A row whose `app_label`
/// doesn't appear in this set surfaces as a D010 warning inline.
/// The synthetic global bucket (`""`) is always considered registered
/// — every project carries it implicitly.
pub fn render(rows: &[LedgerSummaryRow], registered_apps: &[String]) -> StatusReport {
    let mut registered: std::collections::BTreeSet<&str> =
        registered_apps.iter().map(String::as_str).collect();
    registered.insert(""); // global bucket always registered.

    // Group rows by `app_label`. The inputs are already pre-sorted by
    // app then time, but we re-group defensively so this rendering
    // is robust to a future caller that passes unsorted data.
    let mut grouped: BTreeMap<String, Vec<&LedgerSummaryRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.app_label.clone()).or_default().push(row);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut any_pending_or_failed = false;
    let mut d010_warnings = 0usize;

    if grouped.is_empty() {
        lines.push("No migrations recorded.".to_string());
    }

    for (app, app_rows) in grouped {
        let app_display = if app.is_empty() {
            "_global_"
        } else {
            app.as_str()
        };
        lines.push(format!("App {app_display}:"));
        if !registered.contains(app.as_str()) {
            // D010 inline warning — the ledger references an app
            // that is no longer in the AppRegistry.
            lines.push(format!(
                "  D010: ledger references app \"{app}\" which is no longer in AppRegistry; \
                 was the app removed without a #[app(tombstone)]?"
            ));
            d010_warnings += 1;
        }
        for row in app_rows {
            let run_short = format_run_id_short(row.run_id);
            let status_str = row.status.as_db_str();
            // T7: prefix the line with `[ooo]` when the row was
            // recorded as out-of-order so operators reading the status
            // listing immediately see the historical drift. The marker
            // is a fixed-width prefix (with a single trailing space)
            // so non-ooo rows align cleanly when the listing mixes
            // both kinds.
            let ooo_marker = if row.out_of_order_flag {
                "[ooo] "
            } else {
                "      "
            };
            let line = format!(
                "  {marker}{version}  {status:<11}  {applied_at}  {applied_by}  run={run_short}  {ms}ms",
                marker = ooo_marker,
                version = row.version,
                status = status_str,
                applied_at = row.applied_at_rfc3339,
                applied_by = row.applied_by,
                run_short = run_short,
                ms = row.execution_time_ms,
            );
            lines.push(line);
            if let Some(note) = &row.partial_apply_note {
                lines.push(format!("    partial-apply-note: {note}"));
            }
            if matches!(row.status, LedgerStatus::Pending | LedgerStatus::Failed) {
                any_pending_or_failed = true;
            }
        }
    }

    let exit_code = if any_pending_or_failed || d010_warnings > 0 {
        1
    } else {
        0
    };

    StatusReport {
        lines,
        d010_warnings,
        exit_code,
    }
}

/// Render the T9 PK-flip warning lines for a pending migration plan.
///
/// **Inputs.** The caller passes the [`MigrationPlan`] returned by
/// [`super::segment::plan_delta`]. When the plan classifies as
/// `PkTypeFlip`, this fn returns the operator-facing warning lines:
///
/// - The exact PoNR sentence for every flip plan — see
///   [`POINT_OF_NO_RETURN_WARNING`] for the verbatim byte string.
/// - `"⚠ Partitioned-table cutover is seconds-to-minutes class —
///   benchmark in staging first"` when any segment in the plan
///   carries a partitioned-cutover label
///   (`PkFlipPartitionedCutover`).
///
/// Non-flip plans return an empty `Vec`. The warnings are
/// pre-formatted strings ready to print; the CLI prepends them to
/// the regular status output for the affected pending plan.
///
/// The PoNR sentence wording is contractual — operators cite it in
/// runbooks. The unit test `point_of_no_return_warning_byte_exact`
/// asserts the exact bytes so review-driven wording drift produces a
/// loud test failure rather than silent rephrasing.
pub fn render_pending_plan_warnings(plan: &MigrationPlan) -> Vec<String> {
    if !matches!(plan.classification, Classification::PkTypeFlip { .. }) {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    out.push(POINT_OF_NO_RETURN_WARNING.to_string());
    let has_partitioned = plan.segments.iter().any(|s| {
        s.statements
            .iter()
            .any(|stmt| stmt.label.starts_with("PkFlipPartitionedCutover "))
    });
    if has_partitioned {
        out.push(
            "⚠ Partitioned-table cutover is seconds-to-minutes class — benchmark in staging first"
                .to_string(),
        );
    }
    // INVALID-index advisories live alongside the PoNR/partitioned
    // warnings here only when the caller wants them inline in the
    // pending-plan render. Status-time invalid-index detection lives
    // in [`render_invalid_index_warnings`] which queries the live DB.
    out
}

/// Query the live database for any `INVALID` indexes and render an
/// operator-facing warning line per index found.
///
/// **Scope.** Postgres marks an index `pg_index.indisvalid = false`
/// when a `CREATE INDEX CONCURRENTLY` was interrupted (operator
/// cancel, deadlock-cancel, crash, or constraint violation). Such
/// indexes are present in `pg_class` but unusable — query planner
/// skips them, and a re-run of the same `CREATE INDEX CONCURRENTLY`
/// will collide on the index name. They are forensic litter from
/// failed concurrent index builds and the operator must explicitly
/// `REINDEX INDEX CONCURRENTLY` or `DROP INDEX` + recreate.
///
/// **Output shape.** One line per invalid index, format:
/// `"⚠ INVALID index detected: <schema>.<index> on <table> — likely
/// an interrupted CREATE INDEX CONCURRENTLY. Run \`REINDEX INDEX
/// CONCURRENTLY <schema>.<index>\` or DROP and recreate."`
///
/// The warning is unconditional (not just for pending PK-flips):
/// invalid indexes can come from any interrupted concurrent build,
/// not only flips. Status surfacing is the operator-visible signal
/// the catalog still carries the broken entry.
///
/// **Read-only.** No DDL is issued; only `pg_index` / `pg_class` /
/// `pg_namespace` SELECTs.
pub async fn render_invalid_index_warnings(
    ctx: &mut DjogiContext,
) -> Result<Vec<String>, DjogiError> {
    let rows = ctx
        .query_all(
            "SELECT n.nspname, c.relname, i.indrelid::regclass::text \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT i.indisvalid \
             ORDER BY n.nspname, c.relname",
            &[],
        )
        .await?;
    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for r in &rows {
        let schema: String = r.try_get(0).unwrap_or_default();
        let index_name: String = r.try_get(1).unwrap_or_default();
        let table_name: String = r.try_get(2).unwrap_or_default();
        out.push(format!(
            "\u{26a0} INVALID index detected: {schema}.{index} on {table} \u{2014} likely \
             an interrupted CREATE INDEX CONCURRENTLY. Run \
             `REINDEX INDEX CONCURRENTLY {schema}.{index}` or DROP and recreate.",
            schema = schema,
            index = index_name,
            table = table_name,
        ));
    }
    Ok(out)
}

/// Exact byte string of the POINT OF NO RETURN warning emitted ahead
/// of any pending PK-type-flip plan. The wording is **contractual**:
/// runbooks and operator dashboards cite this sentence verbatim, so
/// any change here MUST be paired with a v3 plan amendment AND the
/// `point_of_no_return_warning_byte_exact` regression test update.
///
/// The leading codepoint is U+26A0 (warning sign) followed by U+0020.
/// The em dash between "commits" and "reverse" is U+2014.
pub const POINT_OF_NO_RETURN_WARNING: &str =
    "⚠ POINT OF NO RETURN after this cutover commits — reverse requires an inverse migration";

/// Truncate a run_id BIGINT to a stable short token (last 8 hex
/// digits of the unsigned reinterpretation). Long-form available via
/// the [`LedgerSummaryRow::run_id`] field for callers needing the
/// exact value.
fn format_run_id_short(run_id: i64) -> String {
    let unsigned = run_id as u64;
    let truncated = unsigned & 0xFFFF_FFFF;
    let mut s = String::with_capacity(8);
    for shift in (0..8).rev() {
        let nibble = ((truncated >> (shift * 4)) & 0x0f) as u8;
        s.push(if nibble < 10 {
            (b'0' + nibble) as char
        } else {
            (b'a' + nibble - 10) as char
        });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::ledger::ExecutionMode;

    fn synth_row(
        version: &str,
        app: &str,
        status: LedgerStatus,
        applied_at: &str,
    ) -> LedgerSummaryRow {
        let _ = ExecutionMode::Transactional; // keep import meaningful
        LedgerSummaryRow {
            id: 1,
            version: version.to_string(),
            description: String::new(),
            status,
            execution_time_ms: 12,
            applied_at_rfc3339: applied_at.to_string(),
            applied_by: "djogi".to_string(),
            run_id: 0x1234_5678_9abc_def0_u64 as i64,
            partial_apply_note: None,
            app_label: app.to_string(),
            out_of_order_flag: false,
        }
    }

    fn synth_row_ooo(
        version: &str,
        app: &str,
        status: LedgerStatus,
        applied_at: &str,
    ) -> LedgerSummaryRow {
        let mut r = synth_row(version, app, status, applied_at);
        r.out_of_order_flag = true;
        r
    }

    #[test]
    fn empty_ledger_renders_no_migrations_recorded() {
        let report = render(&[], &[]);
        assert_eq!(report.lines, vec!["No migrations recorded.".to_string()]);
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.d010_warnings, 0);
    }

    #[test]
    fn single_applied_row_exits_zero() {
        let rows = vec![synth_row(
            "V20260425010203__init",
            "",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        )];
        let report = render(&rows, &[]);
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.d010_warnings, 0);
        // First line groups by app.
        assert_eq!(report.lines[0], "App _global_:");
        // Second line carries the version + status.
        assert!(report.lines[1].contains("V20260425010203__init"));
        assert!(report.lines[1].contains("applied"));
    }

    #[test]
    fn pending_row_exits_nonzero() {
        let rows = vec![synth_row(
            "V20260425010203__init",
            "",
            LedgerStatus::Pending,
            "2026-04-25T01:02:03Z",
        )];
        let report = render(&rows, &[]);
        assert_eq!(report.exit_code, 1);
    }

    #[test]
    fn failed_row_exits_nonzero() {
        let rows = vec![synth_row(
            "V20260425010203__init",
            "",
            LedgerStatus::Failed,
            "2026-04-25T01:02:03Z",
        )];
        let report = render(&rows, &[]);
        assert_eq!(report.exit_code, 1);
    }

    #[test]
    fn unknown_app_label_emits_d010() {
        let rows = vec![synth_row(
            "V20260425010203__legacy",
            "old_billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        )];
        let report = render(&rows, &["billing".to_string()]);
        assert_eq!(report.d010_warnings, 1);
        assert_eq!(report.exit_code, 1);
        // D010 line lives between the App header and the version row.
        assert_eq!(report.lines[0], "App old_billing:");
        assert!(report.lines[1].contains("D010"));
        assert!(report.lines[1].contains("old_billing"));
    }

    #[test]
    fn known_app_does_not_emit_d010() {
        let rows = vec![synth_row(
            "V20260425010203__add_invoices",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        )];
        let report = render(&rows, &["billing".to_string()]);
        assert_eq!(report.d010_warnings, 0);
        assert_eq!(report.exit_code, 0);
    }

    #[test]
    fn mixed_status_groups_by_app() {
        let rows = vec![
            synth_row(
                "V1__init",
                "billing",
                LedgerStatus::Applied,
                "2026-04-25T01:02:03Z",
            ),
            synth_row(
                "V2__add_invoices",
                "billing",
                LedgerStatus::Faked,
                "2026-04-25T02:02:03Z",
            ),
            synth_row(
                "V3__add_users",
                "users",
                LedgerStatus::Failed,
                "2026-04-25T03:02:03Z",
            ),
        ];
        let report = render(&rows, &["billing".to_string(), "users".to_string()]);
        // Two app headers and one failure means non-zero exit.
        assert_eq!(report.exit_code, 1);
        let app_headers: Vec<&String> = report
            .lines
            .iter()
            .filter(|l| l.starts_with("App "))
            .collect();
        assert_eq!(app_headers.len(), 2);
        assert!(app_headers[0].contains("billing"));
        assert!(app_headers[1].contains("users"));
    }

    #[test]
    fn run_id_short_format_is_stable() {
        // Confirm bit-twiddling gives 8 hex chars.
        let s = format_run_id_short(0x1234_5678_9abc_def0_u64 as i64);
        assert_eq!(s, "9abcdef0");
    }

    #[test]
    fn run_id_short_zero_pads() {
        let s = format_run_id_short(0x0000_0000_0000_0010);
        assert_eq!(s, "00000010");
    }

    #[test]
    fn partial_apply_note_emitted_inline() {
        let mut row = synth_row(
            "V1__init",
            "billing",
            LedgerStatus::Failed,
            "2026-04-25T01:02:03Z",
        );
        row.partial_apply_note = Some("step 2 of 3 crashed".to_string());
        let report = render(&[row], &["billing".to_string()]);
        let note_line = report
            .lines
            .iter()
            .find(|l| l.contains("partial-apply-note"))
            .expect("note line");
        assert!(note_line.contains("step 2 of 3 crashed"));
    }

    // ── T7: out-of-order marker ──────────────────────────────────────────

    #[test]
    fn out_of_order_flag_renders_ooo_marker() {
        let row = synth_row_ooo(
            "V20260101000001__feature",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        );
        let report = render(&[row], &["billing".to_string()]);
        let row_line = report
            .lines
            .iter()
            .find(|l| l.contains("V20260101000001__feature"))
            .expect("row line");
        assert!(
            row_line.contains("[ooo]"),
            "out-of-order rows must show [ooo] marker; got: {row_line}"
        );
    }

    #[test]
    fn non_ooo_row_has_no_marker() {
        let row = synth_row(
            "V20260101000001__feature",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        );
        let report = render(&[row], &["billing".to_string()]);
        let row_line = report
            .lines
            .iter()
            .find(|l| l.contains("V20260101000001__feature"))
            .expect("row line");
        assert!(
            !row_line.contains("[ooo]"),
            "non-ooo rows must NOT show [ooo]; got: {row_line}"
        );
    }

    #[test]
    fn ooo_marker_does_not_break_exit_code_for_applied_status() {
        // An applied + out-of-order row is still a successful apply
        // from the lifecycle perspective; the marker is informational.
        // Status exit_code stays 0 unless we have pending/failed rows
        // or D010 warnings.
        let row = synth_row_ooo(
            "V20260101000001__feature",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        );
        let report = render(&[row], &["billing".to_string()]);
        assert_eq!(
            report.exit_code, 0,
            "[ooo] applied row must not cause non-zero exit"
        );
    }

    #[test]
    fn point_of_no_return_warning_byte_exact() {
        // The PoNR sentence is contractual — operators cite it in
        // runbooks. Any wording drift must be paired with a v3 plan
        // amendment AND this fixture update; we assert the exact byte
        // sequence so silent rephrasing fails loud.
        let expected = "\u{26a0} POINT OF NO RETURN after this cutover commits \u{2014} \
                        reverse requires an inverse migration";
        assert_eq!(POINT_OF_NO_RETURN_WARNING, expected);
        assert_eq!(POINT_OF_NO_RETURN_WARNING.as_bytes()[0], 0xE2);
        assert_eq!(POINT_OF_NO_RETURN_WARNING.as_bytes()[1], 0x9A);
        assert_eq!(POINT_OF_NO_RETURN_WARNING.as_bytes()[2], 0xA0);
    }

    #[test]
    fn render_pending_plan_warnings_uses_exact_ponr_constant() {
        use crate::migrate::diff::Classification;
        use crate::migrate::projection::BucketKey;
        use crate::migrate::segment::MigrationPlan;
        let plan = MigrationPlan {
            bucket: BucketKey {
                database: "main".to_string(),
                app: String::new(),
            },
            classification: Classification::PkTypeFlip {
                co_destructive: false,
                co_lossy: false,
            },
            segments: Vec::new(),
        };
        let warnings = render_pending_plan_warnings(&plan);
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str() == POINT_OF_NO_RETURN_WARNING),
            "PoNR warning must match the contractual byte string verbatim; got {warnings:?}",
        );
    }

    #[test]
    fn mixed_ooo_and_non_ooo_rows_align_with_consistent_prefix() {
        // The non-ooo rows use a 6-byte spaces prefix so the columns
        // line up with `[ooo] ` on rows that have the marker. The
        // assertion here is loose — we only check that both lines
        // emit and the version label appears in both.
        let r1 = synth_row(
            "V20260101000001__a",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T01:02:03Z",
        );
        let r2 = synth_row_ooo(
            "V20251201000002__b",
            "billing",
            LedgerStatus::Applied,
            "2026-04-25T02:02:03Z",
        );
        let report = render(&[r1, r2], &["billing".to_string()]);
        let line_a = report
            .lines
            .iter()
            .find(|l| l.contains("V20260101000001__a"))
            .expect("line a");
        let line_b = report
            .lines
            .iter()
            .find(|l| l.contains("V20251201000002__b"))
            .expect("line b");
        // Spot-check: line A has the spaces prefix; line B has [ooo].
        assert!(!line_a.contains("[ooo]"));
        assert!(line_b.contains("[ooo]"));
        // Both lines start with two indenting spaces matching `App :`.
        assert!(line_a.starts_with("  "));
        assert!(line_b.starts_with("  "));
    }
}
