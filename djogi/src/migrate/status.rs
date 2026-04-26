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

use super::ledger::{LedgerStatus, LedgerSummaryRow};

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
            let line = format!(
                "  {version}  {status:<11}  {applied_at}  {applied_by}  run={run_short}  {ms}ms",
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
        }
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
}
