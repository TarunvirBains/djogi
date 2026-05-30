//! Pin test (djogi#364): Postgres rejects a bare top-level
//! `BEGIN ATOMIC ... COMMIT`, the security backstop behind the raw_ddl batch
//! scanner change.
//!
//! After djogi#364, `classify_raw_ddl_transaction_backed_refusal` treats
//! `BEGIN ATOMIC` as a SQL-standard compound-statement delimiter rather than
//! transaction control, so the scanner returns no refusal for a batch such as
//! `BEGIN ATOMIC SELECT 1; COMMIT` — the trailing `COMMIT` is not flagged. That
//! is only safe because Postgres itself rejects a bare top-level `BEGIN ATOMIC`
//! outside a `LANGUAGE SQL` function body: the batch fails to parse, so the
//! `COMMIT` can never execute past djogi's transaction-control bookkeeping.
//! This pin asserts that Postgres rejection empirically on the
//! transaction-backed `raw_ddl` path — the exact surface where the scanner now
//! lets the `BEGIN ATOMIC` head through.

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises PG18 rejection of bare top-level BEGIN ATOMIC
#[djogi::djogi_test]
async fn raw_ddl_bare_top_level_begin_atomic_rejected_by_postgres(mut ctx: djogi::DjogiContext) {
    // Drive the malformed, unterminated atomic block through the
    // transaction-backed raw_ddl path. Inside atomic(), the #364 scanner
    // returns None for the BEGIN ATOMIC head (no refusal), so Postgres is the
    // only thing standing between the caller and an unguarded COMMIT. The
    // closure propagates the raw_ddl error so atomic() rolls the aborted
    // transaction back cleanly and surfaces the failure.
    let outcome = djogi::transaction::atomic(&mut ctx, |tx| {
        Box::pin(async move { tx.raw_ddl("BEGIN ATOMIC SELECT 1; COMMIT").await })
    })
    .await;

    // The error must be a real Postgres driver rejection, NOT a djogi-side
    // scanner refusal. If the scanner ever regresses and re-flags BEGIN ATOMIC
    // as transaction control, `raw_ddl` would fail early with
    // `RawTransactionControlDisallowedInTransaction` — the test would still
    // pass `is_err()`, but the Postgres backstop would go completely untested.
    // Matching on the exact variant prevents that false-green scenario.
    match outcome {
        Ok(_) => panic!(
            "Postgres must reject a bare top-level BEGIN ATOMIC ... COMMIT; an Ok \
             result would mean the COMMIT executed past djogi's transaction-control guard"
        ),
        Err(djogi::DjogiError::RawTransactionControlDisallowedInTransaction {
            statement, ..
        }) => {
            panic!(
                "raw_ddl returned a djogi-side scanner refusal ({statement:?}) instead of a \
                 Postgres error; the #364 scanner change lets BEGIN ATOMIC through so Postgres \
                 is the backstop — a scanner refusal here means the scanner regressed and the \
                 Postgres backstop is silently untested"
            );
        }
        Err(djogi::DjogiError::Db(_)) => {
            // Expected: Postgres rejected the bare top-level BEGIN ATOMIC.
        }
        Err(other) => panic!(
            "unexpected error variant from raw_ddl: {other:?}; expected DjogiError::Db \
             wrapping a Postgres parse/protocol rejection of bare BEGIN ATOMIC"
        ),
    }
}
