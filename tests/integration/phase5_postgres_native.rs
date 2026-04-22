//! Phase 5 integration tests — Postgres-native features.
//!
//! Task 1 scope (this file, initially): `Tracked<T>` round-trips
//! transparently through the postgres-types codec — a value written
//! via `create()` lands in the row as the inner `T`, and a row
//! freshly loaded from Postgres is reconstructed with `dirty = false`
//! (so a caller can distinguish never-mutated loads from local
//! mutations).
//!
//! Later Phase 5 tasks extend this file:
//!
//! - Task 2 adds `save_only_updates_dirty_tracked_fields` (dirty-aware
//!   SET emission).
//! - Task 3 adds `optimistic_lock_*` (version predicate on save).
//! - Task 4 adds enum round-trip tests.
//! - Task 5+ adds array operators, JSONB path tests, etc.
//!
//! All tests use `#[djogi::djogi_test]` — the Phase 5-Zero harness that
//! installs HeeRanjID, seeds node 1, and sets `heer.node_id = '1'` at
//! the database level before each test body runs.

use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

// Minimal Account used across Task 1-3. The migration provisions
// `balance BIGINT NOT NULL DEFAULT 0` and
// `revision INTEGER NOT NULL DEFAULT 0` so this struct can stay
// small in Task 1; Task 2 and Task 3 will expand it.
//
// Task 2: `name` is Tracked<String> (dirty-aware), `balance` is a plain
// non-Tracked i64 (always emitted unconditionally in SET). This lets the
// tests verify both halves of the contract:
// - Dirty Tracked fields appear in the UPDATE SET list.
// - Clean Tracked fields do NOT appear in the UPDATE SET list.
// - Non-Tracked fields always appear regardless of dirty state.
#[model(table = "accounts")]
#[derive(Debug, Clone)]
pub struct Account {
    pub name: Tracked<String>,
    pub balance: i64,
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

async fn setup_phase5(ctx: &mut djogi::DjogiContext) {
    const ACCOUNTS_DDL: &str = include_str!("migrations/phase5/001_accounts.sql");
    ctx.raw_execute(ACCOUNTS_DDL, &[])
        .await
        .expect("apply 001_accounts.sql");
}

// ---------------------------------------------------------------------------
// Task 1 — Tracked<T> round-trip through postgres-types
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn tracked_round_trips_through_pg(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    // CREATE: the Tracked<String> value encodes as TEXT via the transparent
    // ToSql impl — Postgres sees a plain string, not a wrapped payload.
    let created = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    // RETURNING * rehydration reconstructs `name` via FromSql. Fresh rows
    // must be clean — local mutations haven't happened yet, so Tracked's
    // dirty bit is false.
    assert!(
        !created.name.is_dirty(),
        "fresh-from-DB Tracked field must start clean"
    );
    assert_eq!(&*created.name, "alice");

    // Reload the row via get() and assert the same invariant holds on a
    // second hydration path. This proves the FromSql contract applies
    // uniformly across CRUD RETURNING and CRUD SELECT.
    let reloaded = Account::get(&mut ctx, created.id)
        .await
        .expect("get account");
    assert!(
        !reloaded.name.is_dirty(),
        "Tracked fields loaded via get() must also start clean"
    );
    assert_eq!(&*reloaded.name, "alice");
}

#[djogi::djogi_test]
async fn tracked_deref_mut_marks_dirty(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    let mut account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    // Mutating the wrapped value via DerefMut flips the dirty bit.
    *account.name = "bob".to_string();
    assert!(
        account.name.is_dirty(),
        "mutation through DerefMut must mark Tracked dirty"
    );

    // Confirm the inner value changed too (not just the flag).
    assert_eq!(&*account.name, "bob");
}

// ---------------------------------------------------------------------------
// Task 2 — dirty-aware save() SET emission
// ---------------------------------------------------------------------------

/// Prove that only dirty Tracked fields appear in the UPDATE SET list.
///
/// Strategy: create an account with balance=100, then use a raw SQL update
/// to set balance=999 in the DB (simulating an out-of-band change). Then
/// mutate only `name` (a Tracked field) and call `save()`. If the SET list
/// included balance unconditionally, save() would overwrite the DB balance
/// back to 100 (the stale in-memory value). If dirty-aware emission is
/// working correctly, balance stays at 999 because the clean Tracked `name`
/// was not dirty before the mutation and... wait, balance is non-Tracked
/// here so it IS unconditional. Let me reframe:
///
/// The correct test: `name` is Tracked<String> (the only Tracked field).
/// We create with name="alice", balance=100. Then raw-SQL set balance=999.
/// Then mutate only `name` → "bob" (Tracked, now dirty). save().
/// Assert name="bob" persisted. Assert balance is still 999 (the raw-SQL
/// update) NOT 100 (stale in-memory), which proves that balance is emitted
/// unconditionally (non-Tracked path — correct) but also that the
/// in-memory `balance` is rehydrated from RETURNING (so it syncs to 999).
/// The real test: add a SECOND Tracked field to prove clean Tracked is omitted.
///
/// Actually: with `balance: i64` (non-Tracked), the test proves the non-Tracked
/// path is unconditional. To prove the Tracked-clean-skip, we need two Tracked
/// fields. The model above has `name: Tracked<String>` and `balance: i64`.
///
/// The test that truly isolates "clean Tracked is skipped":
/// - balance is plain i64 (non-Tracked — always in SET)
/// - We add a raw-SQL-updated column to prove things that ARE skipped stay skipped
///
/// Since we have only one Tracked field (name), we prove clean-skip by:
/// - Create account with name="alice", balance=100.
/// - Raw-SQL: UPDATE accounts SET name='raw' WHERE id=$1 (simulate external write).
/// - Now in-memory: name is clean Tracked("alice"), balance=100.
/// - Do NOT touch name (stays clean). Call save() — only balance should appear in SET.
/// - Assert: DB row has name='raw' (was NOT overwritten — clean Tracked skipped).
/// - Assert: DB row has balance=100 (non-Tracked, was emitted unconditionally).
/// - Assert: in-memory name.is_dirty() == false after save.
#[djogi::djogi_test]
async fn save_only_updates_dirty_tracked_fields(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    // Create with name="alice" (clean after create) and balance=100.
    let mut account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            balance: 100,
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    // Confirm initial state from DB.
    assert_eq!(&*account.name, "alice");
    assert_eq!(account.balance, 100);
    assert!(
        !account.name.is_dirty(),
        "fresh-from-DB Tracked must be clean"
    );

    // Raw-SQL: silently rename the row in the DB to "raw" without touching
    // the in-memory struct. The in-memory `name` stays "alice" and clean.
    ctx.raw_execute(
        "UPDATE accounts SET name = 'raw' WHERE id = $1",
        &[&account.id.as_i64()],
    )
    .await
    .expect("raw-sql update name to 'raw'");

    // Now: in-memory name="alice" (clean Tracked), DB name="raw".
    // Mutate balance (non-Tracked) in memory — but do NOT touch name.
    account.balance = 200;

    // save() — since name is clean Tracked, it must NOT appear in the SET list.
    // Only balance (non-Tracked, unconditional) and updated_at = now() fire.
    account
        .save(&mut ctx)
        .await
        .expect("save after raw-sql name change");

    // After save(), RETURNING * rehydrates self. The DB name is still "raw"
    // (we never touched it in the UPDATE) and balance is now 200.
    assert_eq!(
        &*account.name, "raw",
        "DB-side name 'raw' must survive save() — clean Tracked must be omitted from SET"
    );
    assert_eq!(
        account.balance, 200,
        "non-Tracked balance must be unconditionally emitted and persisted"
    );

    // After save + rehydration, all Tracked fields must be clean.
    assert!(
        !account.name.is_dirty(),
        "Tracked fields must be clean after save() rehydration"
    );

    // Double-check via a fresh SELECT so we aren't trusting in-memory state.
    let reloaded = Account::get(&mut ctx, account.id)
        .await
        .expect("reload after save");
    assert_eq!(&*reloaded.name, "raw", "DB confirms name='raw' after save");
    assert_eq!(reloaded.balance, 200, "DB confirms balance=200 after save");
}

/// Prove that after save() returns, every Tracked field has `is_dirty() == false`.
///
/// This exercises the mark_clean walk in the macro-emitted save() body —
/// RETURNING-based rehydration already constructs `Tracked::new(value)`
/// (dirty=false), but the explicit mark_clean walk is required by the
/// Task 2 contract as a defensive invariant for future in-place rehydration.
#[djogi::djogi_test]
async fn save_rehydration_marks_tracked_fields_clean(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    let mut account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    // Mutate the Tracked field — mark it dirty.
    *account.name = "bob".to_string();
    assert!(account.name.is_dirty(), "must be dirty before save");

    // save() must rehydrate and mark_clean.
    account.save(&mut ctx).await.expect("save");

    // After save, the Tracked field must be clean regardless of how the
    // rehydration is implemented internally.
    assert!(
        !account.name.is_dirty(),
        "Tracked fields must be clean (is_dirty() == false) after save() returns"
    );
    assert_eq!(
        &*account.name, "bob",
        "value must persist across save + rehydration"
    );
}
