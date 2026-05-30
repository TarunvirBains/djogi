// Phase 7.5 / T12 — Live integration tests for chunked backfill execute,
// resume, and chunk-boundary behavior under a real Postgres 18.
//
// Drives the public `live_migrate::backfill` API end-to-end:
//
// 1. **`execute_drives_chunks_to_completion`** — seed N rows with a
//    NULL target column, INSERT a plan in `djogi_live_plans`, run
//    [`execute_backfill`](djogi::live_migrate::execute_backfill) with
//    a small `chunk_size` to force multiple chunks. Asserts every
//    chunk's `rows_affected` equals `chunk_size` until the final
//    short-chunk that exhausts the predicate, every row gets
//    populated, and the ledger's `backfill_rows_done` matches.
//
// 2. **`resume_picks_up_inserts_and_is_idempotent`** — after the
//    predicate exhausts, INSERT more NULL rows. Call
//    [`resume_backfill`](djogi::live_migrate::resume_backfill) — the
//    new rows must be picked up and the ledger must advance. Then
//    call `resume_backfill` again with the predicate exhausted —
//    expect zero chunks (idempotent no-op). This is the
//    chunk-boundary regression and the resume-idempotency regression
//    bundled into one fixture, since a fresh test DB lets the
//    expensive seed run once.
//
// Both tests run inside `#[djogi::djogi_test]` which provisions a
// per-test database, so the tests are mutually independent and
// parallel-safe.
//
// # Why one combined file
//
// v3 §T12 lists four separate buckets — interrupted-backfill resume,
// cutover/finalize gating, chunk-boundary tests, resume idempotency.
// In the public API today (Phase 7.5 ships only `execute_backfill`
// and `resume_backfill`; full-pipeline orchestration is CLI-internal),
// the testable surface for live integration tests is the chunk loop
// itself. Splitting that one surface across multiple files would
// duplicate the harness (table creation, plan-row INSERT, helper
// assertions) without adding distinct coverage. The full-pipeline
// tests against the CLI are deferred — surfaced as a known follow-up.

use djogi::live_migrate::plan::PlanClassification;
use djogi::live_migrate::state::{LivePlanRow, PlanStatus};
use djogi::live_migrate::{execute_backfill, resume_backfill, state};
use djogi::prelude::*;

// Source table — a pretend `users` model gaining a `email_lower`
// column populated from `email` via the nullable_not_null pattern.
const SOURCE_TABLE: &str = "phase7_5_backfill_users";

/// Idempotent install + clean-slate re-create of the source table.
async fn setup_source_table(ctx: &mut DjogiContext) {
    ctx.raw_execute(&format!("DROP TABLE IF EXISTS {SOURCE_TABLE}"), &[])
        .await
        .expect("DROP TABLE IF EXISTS source");

    ctx.raw_execute(
        &format!(
            "CREATE TABLE {SOURCE_TABLE} (
                 id          BIGSERIAL    PRIMARY KEY,
                 email       TEXT         NOT NULL,
                 email_lower TEXT
             )"
        ),
        &[],
    )
    .await
    .expect("CREATE TABLE source");
}

/// Seed `n` rows with mixed-case `email` and `NULL email_lower`. One
/// round-trip via `generate_series` rather than N INSERTs — keeps the
/// helper cheap if a future test ever bumps `n` beyond the current
/// 5/10-row fixtures.
async fn seed_rows(ctx: &mut DjogiContext, n: i64) {
    ctx.raw_execute(
        &format!(
            "INSERT INTO {SOURCE_TABLE} (email) \
             SELECT format('User-%s@Example.COM', g) \
             FROM generate_series(0, $1::int - 1) AS g"
        ),
        &[&(n as i32)],
    )
    .await
    .expect("INSERT seed rows via generate_series");
}

/// Insert a `djogi_live_plans` row in `Running` state, ready for the
/// backfill runner.
async fn insert_running_plan(ctx: &mut DjogiContext, plan_id: HeerId) {
    let row = LivePlanRow {
        plan_id,
        slug: "phase7_5_backfill_test".to_string(),
        plan_file_checksum: "V1:0000000000000000000000000000000000000000000000000000000000000000"
            .to_string(),
        classification: PlanClassification::ExpandContract,
        status: PlanStatus::Running,
        current_step: Some("backfill_chunked".to_string()),
        current_step_index: 0,
        backfill_rows_done: 0,
        backfill_rows_total: None,
        started_at: Some(time::OffsetDateTime::now_utc()),
        last_progress_at: None,
        completed_at: None,
        last_error: None,
        originating_migration: "phase7_5_test_migration".to_string(),
        target_database: "main".to_string(),
        app_label: String::new(),
        daemon_session_token: None,
    };
    state::insert_row(ctx, &row)
        .await
        .expect("INSERT djogi_live_plans row");
}

/// Count rows whose `email_lower` is still NULL (i.e. the predicate
/// frontier).
async fn null_count(ctx: &mut DjogiContext) -> i64 {
    ctx.raw_scalar::<i64>(
        &format!("SELECT COUNT(*)::bigint FROM {SOURCE_TABLE} WHERE email_lower IS NULL"),
        &[],
    )
    .await
    .expect("count NULLs")
}

/// Build the standard predicate used by the nullable_not_null pattern's
/// chunk loop. The runner concatenates `UPDATE <table> ` with this
/// template and binds `chunk_size` at `$1`. Derived from `SOURCE_TABLE`
/// so a rename of the source table can't drift into a stale predicate
/// targeting a non-existent table.
fn predicate_template() -> String {
    format!(
        "SET email_lower = LOWER(email) \
         WHERE id IN ( \
             SELECT id FROM {SOURCE_TABLE} \
             WHERE email_lower IS NULL \
             LIMIT $1 \
         )"
    )
}

#[djogi::djogi_test]
async fn execute_drives_chunks_to_completion(mut ctx: DjogiContext) {
    state::install(&mut ctx)
        .await
        .expect("install djogi_live_plans");
    setup_source_table(&mut ctx).await;
    seed_rows(&mut ctx, 10).await;

    let plan_id = HeerId::from_i64(7_500_001).expect("valid HeerId");
    insert_running_plan(&mut ctx, plan_id).await;

    // chunk_size = 3 against 10 rows expects 4 chunks: 3 + 3 + 3 + 1.
    let chunks = execute_backfill(
        &mut ctx,
        plan_id,
        SOURCE_TABLE,
        &predicate_template(),
        3,
        false,
    )
    .await
    .expect("execute_backfill drives chunks to completion");

    assert_eq!(
        chunks.len(),
        4,
        "expect 4 chunks for 10 rows at chunk_size=3 (3+3+3+1), got: {chunks:?}",
    );
    assert_eq!(chunks[0].rows_affected, 3);
    assert_eq!(chunks[1].rows_affected, 3);
    assert_eq!(chunks[2].rows_affected, 3);
    assert_eq!(
        chunks[3].rows_affected, 1,
        "final chunk reports the short row count that exhausts the predicate",
    );
    assert_eq!(chunks[3].rows_done_total, 10);

    // Every row's email_lower must be populated and lower-cased.
    assert_eq!(null_count(&mut ctx).await, 0, "no NULL email_lower remains");

    // Ledger anchor is updated to match the cumulative work.
    let row = state::fetch_row_by_id(&mut ctx, plan_id, "main", "")
        .await
        .expect("fetch plan row")
        .expect("plan row exists");
    assert_eq!(row.backfill_rows_done, 10);

    // Spot-check one row was lower-cased correctly.
    let lowered = ctx
        .raw_scalar::<String>(
            &format!("SELECT email_lower FROM {SOURCE_TABLE} WHERE email = $1"),
            &[&"User-0@Example.COM"],
        )
        .await
        .expect("spot-check email_lower");
    assert_eq!(lowered, "user-0@example.com");
}

#[djogi::djogi_test]
async fn resume_picks_up_inserts_and_is_idempotent(mut ctx: DjogiContext) {
    state::install(&mut ctx)
        .await
        .expect("install djogi_live_plans");
    setup_source_table(&mut ctx).await;
    seed_rows(&mut ctx, 5).await;

    let plan_id = HeerId::from_i64(7_500_002).expect("valid HeerId");
    insert_running_plan(&mut ctx, plan_id).await;

    // Phase 1: run to completion against the original 5 rows.
    let initial = execute_backfill(
        &mut ctx,
        plan_id,
        SOURCE_TABLE,
        &predicate_template(),
        10,
        false,
    )
    .await
    .expect("initial execute_backfill");
    assert_eq!(initial.len(), 1, "5 rows finish in one chunk_size=10 chunk");
    assert_eq!(initial[0].rows_done_total, 5);
    assert_eq!(null_count(&mut ctx).await, 0, "all 5 rows populated");

    // After `execute_backfill` exhausts the predicate, the runner
    // auto-transitions the plan from `Running` → `Validating` (see
    // backfill.rs module docs). To call `resume_backfill` we must
    // first put the plan back in `Running` — production does this via
    // the CLI's `djogi live resume` command after the operator clears
    // the validation gate.
    state::update_status(&mut ctx, plan_id, "main", "", PlanStatus::Running)
        .await
        .expect("re-arm plan to Running for resume");

    // Phase 2: chunk-boundary scenario — INSERT 4 fresh rows AFTER the
    // initial backfill completed. The predicate is now re-armed (rows
    // exist with email_lower IS NULL) but the ledger still says
    // `backfill_rows_done = 5`. resume_backfill must pick them up.
    seed_rows(&mut ctx, 4).await;
    assert_eq!(null_count(&mut ctx).await, 4, "4 fresh NULL rows seeded");

    let resumed = resume_backfill(
        &mut ctx,
        plan_id,
        SOURCE_TABLE,
        &predicate_template(),
        10,
        false,
    )
    .await
    .expect("resume_backfill picks up post-initial inserts");
    assert_eq!(
        resumed.len(),
        1,
        "4 new rows finish in one chunk_size=10 chunk after resume",
    );
    assert_eq!(resumed[0].rows_affected, 4);
    assert_eq!(
        resumed[0].rows_done_total, 9,
        "ledger advances cumulative — 5 from initial + 4 from resume",
    );
    assert_eq!(null_count(&mut ctx).await, 0, "all 9 rows populated");

    // After the resume completes, the runner has again transitioned
    // the plan to `Validating`. Re-arm to `Running` for the no-op
    // resume below — same operator-driven step as in production.
    state::update_status(&mut ctx, plan_id, "main", "", PlanStatus::Running)
        .await
        .expect("re-arm plan to Running for idempotent resume");

    // Phase 3: idempotent no-op resume. Predicate is exhausted; the
    // runner observes a short chunk (rows_affected = 0) and returns.
    let idempotent = resume_backfill(
        &mut ctx,
        plan_id,
        SOURCE_TABLE,
        &predicate_template(),
        10,
        false,
    )
    .await
    .expect("resume_backfill on exhausted predicate");
    assert_eq!(
        idempotent.len(),
        1,
        "exhausted-predicate resume reports the zero-row terminator chunk",
    );
    assert_eq!(idempotent[0].rows_affected, 0);
    assert_eq!(
        idempotent[0].rows_done_total, 9,
        "ledger unchanged on no-op"
    );
}
