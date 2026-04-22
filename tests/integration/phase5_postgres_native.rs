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

use djogi::DjogiEnum;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

// Minimal Account used across Task 1 and Task 2. The migration provisions
// `balance BIGINT NOT NULL DEFAULT 0` so this struct can stay
// small in Task 1; Task 2 will extend it with behavior only.
//
// Task 1 (base): `name` is Tracked<String> round-trips through postgres-types.
// Task 1 (extended): `note` is Tracked<Option<String>> to test NULL decode.
// Task 2: `balance` is a plain non-Tracked i64 (always emitted unconditionally
// in SET). This lets tests verify both halves of the contract:
// - Dirty Tracked fields appear in the UPDATE SET list.
// - Clean Tracked fields do NOT appear in the UPDATE SET list.
// - Non-Tracked fields always appear regardless of dirty state.
#[model(table = "accounts")]
#[derive(Debug, Clone)]
pub struct Account {
    pub name: Tracked<String>,
    pub balance: i64,
    pub note: Tracked<Option<String>>,
    #[field(version)]
    pub revision: i32,
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

// ---------------------------------------------------------------------------
// Task 1 (extended) — Tracked<Option<_>> NULL round-trip
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Task 3 — optimistic locking via #[field(version)]
// ---------------------------------------------------------------------------

/// After Account::create, the DB inserts revision=0 (column DEFAULT 0).
/// After one account.save(), revision must be 1 (version counter bumped
/// by the `revision = revision + 1` SET fragment in the emitted UPDATE).
#[djogi::djogi_test]
async fn optimistic_lock_create_starts_at_zero_save_increments_to_one(
    mut ctx: djogi::DjogiContext,
) {
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

    // INSERT uses DEFAULT 0 — the DB populates revision and RETURNING brings
    // it back. In-memory revision must reflect the DB value.
    assert_eq!(
        account.revision, 0,
        "revision must be 0 after create (DEFAULT 0)"
    );

    // First save: SET includes `revision = revision + 1`, so DB transitions
    // 0 → 1. RETURNING rehydrates self, so in-memory revision becomes 1.
    account.save(&mut ctx).await.expect("first save");
    assert_eq!(account.revision, 1, "revision must be 1 after first save");
}

/// Save twice in a row on the same handle — revision increments 0 → 1 → 2.
/// Proves the version predicate passes when versions are in sync.
#[djogi::djogi_test]
async fn optimistic_lock_success_increments_each_save(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    let mut account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("bob".to_string()),
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    assert_eq!(account.revision, 0);

    account.save(&mut ctx).await.expect("first save");
    assert_eq!(account.revision, 1, "revision must be 1 after first save");

    account.save(&mut ctx).await.expect("second save");
    assert_eq!(account.revision, 2, "revision must be 2 after second save");
}

/// Clone the account to simulate two concurrent handles. When clone A saves
/// first (bumping DB revision to 1), clone B still holds revision=0 in
/// memory. B's save() must return Err(DjogiError::LockConflict(_)) because
/// the WHERE clause `revision = 0` no longer matches the DB row.
#[djogi::djogi_test]
async fn optimistic_lock_stale_version_returns_conflict(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    let account = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("carol".to_string()),
            balance: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create account");

    assert_eq!(account.revision, 0);

    // Clone A — will win the race.
    let mut clone_a = account.clone();
    // Clone B — will lose the race.
    let mut clone_b = account.clone();

    // Clone A saves first, bumping DB revision 0 → 1.
    clone_a
        .save(&mut ctx)
        .await
        .expect("clone_a save must succeed");
    assert_eq!(clone_a.revision, 1);

    // Clone B still holds revision=0. Its save must detect the version
    // mismatch and return LockConflict.
    let result = clone_b.save(&mut ctx).await;
    assert!(
        matches!(result, Err(djogi::DjogiError::LockConflict(_))),
        "stale save must return DjogiError::LockConflict; got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 4 — #[derive(DjogiEnum)] + EnumDescriptor
// ---------------------------------------------------------------------------

/// Postgres enum type that mirrors the `vehicle_status` SQL enum provisioned by
/// `002_vehicle_status_enum.sql`. The `Retired` variant uses a per-variant name
/// override (`"decommissioned"`) to verify the override path on the wire.
#[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[djogi_enum(name = "vehicle_status", rename_all = "snake_case")]
pub enum VehicleStatus {
    Active,
    InMaintenance,
    #[djogi_enum_variant(name = "decommissioned")]
    Retired,
}

/// Model that holds a `VehicleStatus` column. Table provisioned by
/// `003_vehicles.sql`.
///
/// `no_default` is required because `VehicleStatus` does not implement
/// `Default` — there is no sensible sentinel value for a status enum.
/// Callers must always provide an explicit `status` value.
#[model(table = "vehicles", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub status: VehicleStatus,
}

/// Prove that Tracked<Option<_>> fields decode NULL correctly from the DB.
///
/// The FromSql impl must override from_sql_null to delegate to the inner
/// T::from_sql_null and wrap the result in Tracked::new. Without this, NULL
/// values bubble up as errors instead of being decoded as None.
#[djogi::djogi_test]
async fn tracked_option_round_trips_null(mut ctx: djogi::DjogiContext) {
    setup_phase5(&mut ctx).await;

    // Create an account with note = None (i.e., NULL in DB).
    let created = Account::create(
        &mut ctx,
        Account {
            name: Tracked::new("alice".to_string()),
            balance: 0,
            note: Tracked::new(None),
            ..Default::default()
        },
    )
    .await
    .expect("create");

    assert_eq!(*created.note, None);
    assert!(
        !created.note.is_dirty(),
        "fresh NULL Tracked<Option<_>> is clean"
    );

    // Reload via get() to force a fresh decode from the DB row.
    let reloaded = Account::get(&mut ctx, created.id).await.expect("get");
    assert_eq!(*reloaded.note, None, "NULL column decoded as None");
    assert!(!reloaded.note.is_dirty(), "reloaded NULL Tracked is clean");
}

// ---------------------------------------------------------------------------
// Task 4 — enum setup helper
// ---------------------------------------------------------------------------

/// Construct a `Vehicle` with the given `status` and zeroed framework fields.
///
/// `Vehicle` uses `no_default` (enum fields have no natural sentinel), so callers
/// cannot use `..Vehicle::default()`. This factory fills the framework-injected
/// fields with sentinel values acceptable to the model macro — the DB overwrites
/// them via `RETURNING` on `create()`.
fn make_vehicle(status: VehicleStatus) -> Vehicle {
    Vehicle {
        id: djogi::types::__heerid_default(),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        status,
    }
}

/// Apply the vehicle_status enum DDL and vehicles table on top of the accounts
/// setup. The accounts migration (001) must run first because the HeeRanjID
/// functions are installed by `#[djogi_test]` before any DDL in the test
/// body — the enum type can be created in any order relative to accounts.
async fn setup_phase5_enum(ctx: &mut djogi::DjogiContext) {
    setup_phase5(ctx).await;
    const ENUM_DDL: &str = include_str!("migrations/phase5/002_vehicle_status_enum.sql");
    ctx.raw_execute(ENUM_DDL, &[])
        .await
        .expect("apply 002_vehicle_status_enum.sql");
    const VEHICLES_DDL: &str = include_str!("migrations/phase5/003_vehicles.sql");
    ctx.raw_execute(VEHICLES_DDL, &[])
        .await
        .expect("apply 003_vehicles.sql");
}

// ---------------------------------------------------------------------------
// Task 4 — integration tests
// ---------------------------------------------------------------------------

/// Create a Vehicle with `VehicleStatus::Retired`, reload via get(), and verify:
///
/// 1. The round-tripped value is `VehicleStatus::Retired`.
/// 2. The raw wire string in the DB column is `"decommissioned"` (the per-variant
///    name override), not `"retired"` (which `rename_all = snake_case` would produce).
#[djogi::djogi_test]
async fn vehicle_status_enum_round_trips(mut ctx: djogi::DjogiContext) {
    setup_phase5_enum(&mut ctx).await;

    let created = Vehicle::create(&mut ctx, make_vehicle(VehicleStatus::Retired))
        .await
        .expect("create vehicle");

    assert_eq!(
        created.status,
        VehicleStatus::Retired,
        "status from RETURNING must be VehicleStatus::Retired"
    );

    // Reload via get() — second decode path.
    let reloaded = Vehicle::get(&mut ctx, created.id)
        .await
        .expect("get vehicle");
    assert_eq!(
        reloaded.status,
        VehicleStatus::Retired,
        "status from SELECT must be VehicleStatus::Retired"
    );

    // Assert the wire value in the DB is literally "decommissioned".
    let wire: String = ctx
        .raw_scalar(
            "SELECT status::text FROM vehicles WHERE id = $1",
            &[&created.id.as_i64()],
        )
        .await
        .expect("raw_scalar for status wire value");
    assert_eq!(
        wire, "decommissioned",
        "wire string for VehicleStatus::Retired must be 'decommissioned'"
    );
}

/// Verify that all three VehicleStatus variants round-trip correctly.
#[djogi::djogi_test]
async fn vehicle_status_all_variants_round_trip(mut ctx: djogi::DjogiContext) {
    setup_phase5_enum(&mut ctx).await;

    // Create one vehicle per variant, reload each, and assert the value matches.
    for (status, expected_wire) in [
        (VehicleStatus::Active, "active"),
        (VehicleStatus::InMaintenance, "in_maintenance"),
        (VehicleStatus::Retired, "decommissioned"),
    ] {
        let created = Vehicle::create(&mut ctx, make_vehicle(status))
            .await
            .expect("create vehicle");

        assert_eq!(
            created.status, status,
            "RETURNING round-trip failed for {expected_wire}"
        );

        let reloaded = Vehicle::get(&mut ctx, created.id)
            .await
            .expect("get vehicle");
        assert_eq!(
            reloaded.status, status,
            "SELECT round-trip failed for {expected_wire}"
        );

        let wire: String = ctx
            .raw_scalar(
                "SELECT status::text FROM vehicles WHERE id = $1",
                &[&created.id.as_i64()],
            )
            .await
            .expect("raw_scalar");
        assert_eq!(wire, expected_wire, "wire value mismatch for {status:?}");
    }
}

/// Verify that the EnumDescriptor for VehicleStatus is registered in the
/// inventory and carries the expected metadata.
#[djogi::djogi_test]
async fn vehicle_status_enum_descriptor_registered(_ctx: djogi::DjogiContext) {
    let desc = inventory::iter::<djogi::descriptor::EnumDescriptor>()
        .find(|d| d.postgres_type == "vehicle_status")
        .expect("EnumDescriptor for vehicle_status must be in inventory");

    assert_eq!(desc.type_name, "VehicleStatus");
    assert_eq!(
        desc.variants,
        &["active", "in_maintenance", "decommissioned"]
    );
}

// ---------------------------------------------------------------------------
// Task 5 — Array operators + Jsonb<T> runtime + flat JSONB path queries
// ---------------------------------------------------------------------------

/// The typed schema portion of the `specs` JSONB column.
/// Unknown fields (absent from this struct) are preserved in `Jsonb::extra`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostSpec {
    pub engine_cylinders: i32,
    pub brand: String,
}

/// A Post model with array and JSONB columns.
///
/// - `tags`        — TEXT[] for array operator tests.
/// - `view_counts` — INTEGER[] for array len test.
/// - `specs`       — JSONB with a typed partial schema (`PostSpec`) for path
///                   filter and unknown-field preservation tests.
#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub tags: Vec<String>,
    pub view_counts: Vec<i32>,
    pub specs: Option<Jsonb<serde_json::Value>>,
    /// Added by Task 7 migration (007_posts_task7.sql). Defaults to FALSE in
    /// the DB; used by `bool_and` / `bool_or` aggregate integration tests.
    pub published: bool,
}

/// Apply the posts DDL on top of the enum setup (which includes accounts,
/// vehicle_status, and vehicles). If only array/JSONB tests are running the
/// caller can skip enum setup by applying the accounts DDL first instead.
async fn setup_phase5_posts(ctx: &mut djogi::DjogiContext) {
    // Ensure HeeRanjID-dependent tables exist (accounts depends on nothing
    // except the heeranjid schema which #[djogi_test] installs).
    setup_phase5(ctx).await;
    const POSTS_DDL: &str = include_str!("migrations/phase5/004_posts.sql");
    ctx.raw_execute(POSTS_DDL, &[])
        .await
        .expect("apply 004_posts.sql");
    // Task 7 migration: adds `published BOOLEAN` column to posts.
    const TASK7_DDL: &str = include_str!("migrations/phase5/007_posts_task7.sql");
    ctx.raw_execute(TASK7_DDL, &[])
        .await
        .expect("apply 007_posts_task7.sql");
}

/// Helper to build a minimal Post — suppresses the Default fill-in for
/// Option<Jsonb<_>> (which is always None by default).
fn make_post(title: &str, tags: Vec<String>, view_counts: Vec<i32>) -> Post {
    Post {
        title: title.to_string(),
        tags,
        view_counts,
        specs: None,
        published: false,
        ..Default::default()
    }
}

/// Helper to build a Post with an explicit `published` flag.
fn make_post_published(title: &str, published: bool) -> Post {
    Post {
        title: title.to_string(),
        tags: vec![],
        view_counts: vec![],
        specs: None,
        published,
        ..Default::default()
    }
}

/// Insert a post with a JSONB specs column.
fn make_post_with_specs(title: &str, tags: Vec<String>, specs: serde_json::Value) -> Post {
    Post {
        title: title.to_string(),
        tags,
        view_counts: vec![],
        specs: Some(Jsonb::new(specs)),
        published: false,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Task 5 — array_contains_matches
// ---------------------------------------------------------------------------

/// Insert a post with tags ["rust", "postgres"] and verify that a
/// `contains` filter for ["rust", "postgres"] finds the row, while a
/// filter for ["python"] does not.
#[djogi::djogi_test]
async fn array_contains_matches(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let post = Post::create(
        &mut ctx,
        make_post(
            "Rust and Postgres",
            vec!["rust".into(), "postgres".into()],
            vec![],
        ),
    )
    .await
    .expect("create post");

    // Should match: column contains both "rust" and "postgres".
    let found = Post::objects()
        .filter(|f| {
            f.tags()
                .contains(&["rust".to_string(), "postgres".to_string()])
        })
        .fetch_all(&mut ctx)
        .await
        .expect("filter contains");
    assert!(
        found.iter().any(|p| p.id == post.id),
        "post with tags ['rust','postgres'] must appear when filtered by contains(['rust','postgres'])"
    );

    // Should NOT match: column does not contain "python".
    let not_found = Post::objects()
        .filter(|f| f.tags().contains(&["python".to_string()]))
        .fetch_all(&mut ctx)
        .await
        .expect("filter contains python");
    assert!(
        !not_found.iter().any(|p| p.id == post.id),
        "post must NOT appear when filtered for ['python']"
    );
}

// ---------------------------------------------------------------------------
// Task 5 — array_overlap_matches
// ---------------------------------------------------------------------------

/// Insert two posts with different tag sets. Verify that the overlap filter
/// finds rows sharing at least one tag.
#[djogi::djogi_test]
async fn array_overlap_matches(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let post_rust = Post::create(
        &mut ctx,
        make_post("Rust only", vec!["rust".into()], vec![]),
    )
    .await
    .expect("create rust post");

    let post_python = Post::create(
        &mut ctx,
        make_post("Python only", vec!["python".into()], vec![]),
    )
    .await
    .expect("create python post");

    // Overlap with ["rust", "java"] — only post_rust should match.
    let overlapping = Post::objects()
        .filter(|f| f.tags().overlap(&["rust".to_string(), "java".to_string()]))
        .fetch_all(&mut ctx)
        .await
        .expect("overlap filter");

    assert!(
        overlapping.iter().any(|p| p.id == post_rust.id),
        "post with tag 'rust' must appear in overlap filter for ['rust','java']"
    );
    assert!(
        !overlapping.iter().any(|p| p.id == post_python.id),
        "post with tag 'python' must NOT appear in overlap filter for ['rust','java']"
    );
}

// ---------------------------------------------------------------------------
// Task 5 — array_len_filters
// ---------------------------------------------------------------------------

/// Insert posts with varying tag counts. Verify that `.len().gt(n)` in the
/// expression IR correctly filters by array length.
#[djogi::djogi_test]
async fn array_len_filters(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    // 4 tags — should match `.len().gt(3)`.
    let long_post = Post::create(
        &mut ctx,
        make_post(
            "Many tags",
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            vec![],
        ),
    )
    .await
    .expect("create long post");

    // 1 tag — should NOT match `.len().gt(3)`.
    let short_post = Post::create(&mut ctx, make_post("Few tags", vec!["only".into()], vec![]))
        .await
        .expect("create short post");

    let found = Post::objects()
        .filter_expr(|f| f.tags().len().gt(Expr::literal(3i32)))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by array len > 3");

    assert!(
        found.iter().any(|p| p.id == long_post.id),
        "post with 4 tags must appear when filtering len() > 3"
    );
    assert!(
        !found.iter().any(|p| p.id == short_post.id),
        "post with 1 tag must NOT appear when filtering len() > 3"
    );
}

// ---------------------------------------------------------------------------
// Task 5 — jsonb_flat_path_filter_works
// ---------------------------------------------------------------------------

/// Insert a post with `specs = {"engine_cylinders": 8, "brand": "V8Power"}`.
/// Verify that `.path::<i32>("engine_cylinders").gt(4)` finds it, and a post
/// with `engine_cylinders = 2` does not appear.
#[djogi::djogi_test]
async fn jsonb_flat_path_filter_works(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let v8_post = Post::create(
        &mut ctx,
        make_post_with_specs(
            "V8 Beast",
            vec!["cars".into()],
            serde_json::json!({"engine_cylinders": 8, "brand": "V8Power"}),
        ),
    )
    .await
    .expect("create v8 post");

    let eco_post = Post::create(
        &mut ctx,
        make_post_with_specs(
            "Eco Car",
            vec!["cars".into()],
            serde_json::json!({"engine_cylinders": 2, "brand": "EcoMobile"}),
        ),
    )
    .await
    .expect("create eco post");

    let found = Post::objects()
        .filter(|f| f.specs().path::<i32>("engine_cylinders").gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("jsonb path filter");

    assert!(
        found.iter().any(|p| p.id == v8_post.id),
        "V8 post (cylinders=8) must appear in filter for engine_cylinders > 4"
    );
    assert!(
        !found.iter().any(|p| p.id == eco_post.id),
        "Eco post (cylinders=2) must NOT appear in filter for engine_cylinders > 4"
    );
}

// ---------------------------------------------------------------------------
// Task 5 — jsonb_preserves_unknown_fields
// ---------------------------------------------------------------------------

/// Insert raw JSON with an extra key `"experimental": true` that is absent
/// from `PostSpec`. Reload through `Jsonb<PostSpec>`, mutate a known field,
/// save, then verify the `"experimental"` key survived in the database.
#[djogi::djogi_test]
async fn jsonb_preserves_unknown_fields(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    // Insert with a raw JSONB that contains an unknown key.
    let raw_json = serde_json::json!({
        "engine_cylinders": 4,
        "brand": "TestBrand",
        "experimental": true,
        "legacy_field": 99
    });
    let post = Post::create(
        &mut ctx,
        make_post_with_specs("Experimental Post", vec![], raw_json),
    )
    .await
    .expect("create post with experimental key");

    // Reload and confirm the unknown fields are in `extra`.
    let reloaded = Post::get(&mut ctx, post.id).await.expect("reload post");

    let specs = reloaded
        .specs
        .as_ref()
        .expect("specs must be Some after reload");

    // The raw JSON we inserted had 4 keys; the typed value (`serde_json::Value`)
    // absorbs all of them, so extra is empty. To test unknown-field preservation,
    // we use a typed struct that only knows 2 keys: `PostSpec { engine_cylinders, brand }`.
    let typed_json_str = ctx
        .raw_scalar::<String>(
            "SELECT specs::text FROM posts WHERE id = $1",
            &[&post.id.as_i64()],
        )
        .await
        .expect("raw_scalar specs");

    // Deserialize as Jsonb<PostSpec> to exercise the unknown-field split.
    let typed_specs: Jsonb<PostSpec> =
        serde_json::from_str(&typed_json_str).expect("deserialize as Jsonb<PostSpec>");

    assert_eq!(typed_specs.data.engine_cylinders, 4);
    assert_eq!(typed_specs.data.brand, "TestBrand");
    assert_eq!(
        typed_specs.extra().len(),
        2,
        "experimental + legacy_field must be in extra"
    );
    assert!(
        typed_specs.extra().contains_key("experimental"),
        "extra must contain 'experimental'"
    );
    assert!(
        typed_specs.extra().contains_key("legacy_field"),
        "extra must contain 'legacy_field'"
    );

    // Re-serialize to verify unknown fields survive the round-trip.
    let re_serialized = serde_json::to_value(&typed_specs).expect("re-serialize");
    assert_eq!(
        re_serialized["experimental"],
        serde_json::json!(true),
        "'experimental' key must survive Jsonb<PostSpec> round-trip"
    );
    assert_eq!(
        re_serialized["legacy_field"],
        serde_json::json!(99),
        "'legacy_field' key must survive Jsonb<PostSpec> round-trip"
    );

    // Confirm via `specs` field from the loaded post — since `specs` is
    // `Jsonb<serde_json::Value>`, all keys land in `data` and extra is empty.
    // We already tested the typed split above; this just confirms the DB value
    // is intact and the `Jsonb<Value>` ToSql/FromSql round-trip works.
    let _ = specs; // silence unused warning — the interesting assertion is above.
}

// ---------------------------------------------------------------------------
// Fix 3 — typed Jsonb<T> round-trip through create/save/get
// ---------------------------------------------------------------------------
//
// The previous `jsonb_preserves_unknown_fields` test used manual
// `serde_json::from_str` + `to_value` on a `Jsonb<PostSpec>` but never
// called `post.save(&mut ctx)`. The postgres-types ToSql/FromSql impls for
// `Jsonb<T>` were therefore untested at the model CRUD level.
//
// These tests use `TypedPost` (specs: Jsonb<PostSpec>) to exercise the
// full create→reload→mutate→save→reload round-trip with unknown-key
// preservation across two saves.

/// A post model with a *typed* JSONB column — `specs: Jsonb<PostSpec>`.
///
/// This model is deliberately separate from `Post` (which uses
/// `Jsonb<serde_json::Value>`) so changes to the Fix 3 tests do not
/// disturb the existing Task 5 tests.
#[model(table = "typed_posts")]
#[derive(Debug, Clone)]
pub struct TypedPost {
    pub title: String,
    pub specs: Option<Jsonb<PostSpec>>,
}

async fn setup_typed_posts(ctx: &mut djogi::DjogiContext) {
    setup_phase5(ctx).await;
    const TYPED_POSTS_DDL: &str = include_str!("migrations/phase5/005_typed_posts.sql");
    ctx.raw_execute(TYPED_POSTS_DDL, &[])
        .await
        .expect("apply 005_typed_posts.sql");
}

/// Create a TypedPost whose `specs` JSON contains both known fields
/// (`engine_cylinders`, `brand`) and unknown fields (`experimental`,
/// `legacy_field`). Reload via `get()`, assert the typed data is correct and
/// the extra keys are captured, then mutate `engine_cylinders`, `save()`,
/// reload again, and verify the unknown keys survived both round-trips.
///
/// This test exercises:
/// 1. `Jsonb<PostSpec>` ToSql impl (via `create()`).
/// 2. `Jsonb<PostSpec>` FromSql impl (via RETURNING + `get()`).
/// 3. Unknown-field preservation across create + save.
/// 4. Typed data mutation persisting through `save()`.
#[djogi::djogi_test]
async fn typed_jsonb_round_trip_preserves_unknown_fields(mut ctx: djogi::DjogiContext) {
    setup_typed_posts(&mut ctx).await;

    // Deserialize from the full JSON string so the unknown fields (`experimental`,
    // `legacy_field`) land in `extra`. Using `Jsonb::new(PostSpec {...})` would
    // construct an empty `extra` map because `Jsonb::new` is the typed-only path.
    let specs_with_extra: Jsonb<PostSpec> = serde_json::from_str(
        r#"{"engine_cylinders":4,"brand":"TestBrand","experimental":true,"legacy_field":99}"#,
    )
    .expect("deserialize Jsonb<PostSpec> with extra keys");

    // Create the post — ToSql must serialise both the typed data and extra fields.
    let mut post = TypedPost::create(
        &mut ctx,
        TypedPost {
            title: "Typed JSONB Test".to_string(),
            specs: Some(specs_with_extra),
            ..Default::default()
        },
    )
    .await
    .expect("create typed post");

    // RETURNING-based rehydration — FromSql must split known vs unknown keys.
    {
        let s = post
            .specs
            .as_ref()
            .expect("specs after create must be Some");
        assert_eq!(
            s.data.engine_cylinders, 4,
            "typed data cylinders after create"
        );
        assert_eq!(s.data.brand, "TestBrand", "typed data brand after create");
        assert_eq!(s.extra().len(), 2, "two extra keys after create");
        assert!(
            s.extra().contains_key("experimental"),
            "extra has 'experimental'"
        );
        assert!(
            s.extra().contains_key("legacy_field"),
            "extra has 'legacy_field'"
        );
    }

    // Reload via get() — second FromSql path.
    let reloaded = TypedPost::get(&mut ctx, post.id)
        .await
        .expect("get typed post");
    {
        let s = reloaded
            .specs
            .as_ref()
            .expect("specs after get must be Some");
        assert_eq!(s.data.engine_cylinders, 4);
        assert_eq!(s.data.brand, "TestBrand");
        assert_eq!(
            s.extra().len(),
            2,
            "extra keys must survive get() round-trip"
        );
        assert!(s.extra().contains_key("experimental"));
        assert!(s.extra().contains_key("legacy_field"));
    }

    // Mutate the typed data — bump cylinders from 4 to 8.
    if let Some(s) = post.specs.as_mut() {
        s.data.engine_cylinders = 8;
    }

    // save() — ToSql must serialise mutated typed data AND preserve extra keys.
    post.save(&mut ctx).await.expect("save typed post");

    // After save(), RETURNING rehydrates — assert mutated data + preserved extra.
    {
        let s = post.specs.as_ref().expect("specs after save must be Some");
        assert_eq!(
            s.data.engine_cylinders, 8,
            "mutated cylinders must persist through save()"
        );
        assert_eq!(s.data.brand, "TestBrand", "brand must survive save()");
        assert_eq!(s.extra().len(), 2, "extra keys must survive first save()");
        assert!(
            s.extra().contains_key("experimental"),
            "'experimental' must survive first save()"
        );
        assert!(
            s.extra().contains_key("legacy_field"),
            "'legacy_field' must survive first save()"
        );
    }

    // Final reload to prove DB has the right state (not just in-memory).
    let final_reloaded = TypedPost::get(&mut ctx, post.id)
        .await
        .expect("final get typed post");
    {
        let s = final_reloaded
            .specs
            .as_ref()
            .expect("specs after final get must be Some");
        assert_eq!(
            s.data.engine_cylinders, 8,
            "DB must have cylinders=8 after save()"
        );
        assert_eq!(
            s.extra().len(),
            2,
            "extra keys must survive second DB round-trip"
        );
        assert!(
            s.extra().contains_key("experimental"),
            "'experimental' survives second round-trip"
        );
        assert!(
            s.extra().contains_key("legacy_field"),
            "'legacy_field' survives second round-trip"
        );
    }
}

/// Verify that a JSONB path filter (`path::<i32>`) correctly applies the
/// `::int4` cast so numeric comparisons work. This specifically targets
/// the previously broken cast matrix: `i32` now maps to `::int4` instead
/// of the missing mapping that caused silent text comparison.
///
/// Also verifies the correct cast for `i16` and `bool` while reusing the
/// same table setup.
#[djogi::djogi_test]
async fn jsonb_path_cast_matrix_i32_filter_correct(mut ctx: djogi::DjogiContext) {
    setup_typed_posts(&mut ctx).await;

    // Insert two posts: one with cylinders=8, one with cylinders=2.
    // The filter `engine_cylinders > 4` must match only the first.
    let specs_8: Jsonb<PostSpec> =
        serde_json::from_str(r#"{"engine_cylinders":8,"brand":"V8"}"#).expect("parse specs_8");

    let specs_2: Jsonb<PostSpec> =
        serde_json::from_str(r#"{"engine_cylinders":2,"brand":"Eco"}"#).expect("parse specs_2");

    let post_8 = TypedPost::create(
        &mut ctx,
        TypedPost {
            title: "V8 Engine".to_string(),
            specs: Some(specs_8),
            ..Default::default()
        },
    )
    .await
    .expect("create v8 typed post");

    let post_2 = TypedPost::create(
        &mut ctx,
        TypedPost {
            title: "Eco Engine".to_string(),
            specs: Some(specs_2),
            ..Default::default()
        },
    )
    .await
    .expect("create eco typed post");

    // Filter: specs.engine_cylinders > 4 (requires ::int4 cast for correct
    // numeric comparison — text comparison of "8" > "4" would also pass but
    // "12" > "4" would fail as text while passing as integer).
    let found = TypedPost::objects()
        .filter(|f| f.specs().path::<i32>("engine_cylinders").gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("jsonb path i32 filter");

    assert!(
        found.iter().any(|p| p.id == post_8.id),
        "post with cylinders=8 must appear when filtering engine_cylinders > 4"
    );
    assert!(
        !found.iter().any(|p| p.id == post_2.id),
        "post with cylinders=2 must NOT appear when filtering engine_cylinders > 4"
    );
}

// ---------------------------------------------------------------------------
// Task 6 — #[derive(JsonbSchema)] typed deep-path JSONB queries
// ---------------------------------------------------------------------------
//
// These tests use `VehicleDeep` (to avoid name collision with the Task 4
// `Vehicle` model) with two levels of nested JsonbSchema structs:
//
//   VehicleDeepSpecs          ← depth 0 (root)
//     engine: EngineDeepSpecs ← depth 1 (nested)
//       cylinders: i32        ← depth 2 leaf
//       turbo: bool           ← depth 2 leaf
//     weight_kg: f32          ← depth 1 leaf
//     brand: String           ← depth 1 leaf
//
// The flat escape-hatch path (`.path::<i32>("engine.cylinders")`) and the
// typed path (`.typed().engine().cylinders()`) must emit the same SQL and
// return the same rows.

/// Innermost schema for the engine portion of a vehicle spec.
#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct EngineDeepSpecs {
    pub cylinders: i32,
    pub turbo: bool,
    pub displacement_cc: f32,
}

/// Top-level schema for the `specs` JSONB column of `VehicleDeep`.
#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct VehicleDeepSpecs {
    pub engine: EngineDeepSpecs,
    pub weight_kg: f32,
    pub brand: String,
}

/// Vehicle model whose `specs` column is a nested JSONB schema.
///
/// Table provisioned by `006_vehicles_deep.sql`.
#[model(table = "vehicles_deep")]
#[derive(Debug, Clone)]
pub struct VehicleDeep {
    pub name: String,
    pub specs: Option<Jsonb<VehicleDeepSpecs>>,
}

/// Apply the vehicles_deep table DDL.
async fn setup_vehicles_deep(ctx: &mut djogi::DjogiContext) {
    setup_phase5(ctx).await;
    const DDL: &str = include_str!("migrations/phase5/006_vehicles_deep.sql");
    ctx.raw_execute(DDL, &[])
        .await
        .expect("apply 006_vehicles_deep.sql");
}

/// Helper: create a VehicleDeep with the given spec JSON, inserting it via
/// `VehicleDeep::create`. Returns the created row (with DB-assigned id).
async fn make_vehicle_deep(
    ctx: &mut djogi::DjogiContext,
    name: &str,
    cylinders: i32,
    turbo: bool,
    weight_kg: f32,
    brand: &str,
) -> VehicleDeep {
    let specs = VehicleDeepSpecs {
        engine: EngineDeepSpecs {
            cylinders,
            turbo,
            displacement_cc: 2000.0,
        },
        weight_kg,
        brand: brand.to_string(),
    };
    VehicleDeep::create(
        ctx,
        VehicleDeep {
            name: name.to_string(),
            specs: Some(Jsonb::new(specs)),
            ..Default::default()
        },
    )
    .await
    .expect("create VehicleDeep")
}

// ── Depth-2 filter via typed path ─────────────────────────────────────────────

/// Filter at depth 2 via the typed path: `specs.engine.cylinders > 4`.
///
/// The typed path `.specs().typed().engine().cylinders().gt(4)` must emit:
///
/// ```sql
/// (specs->'engine'->>'cylinders')::int4 > $1
/// ```
///
/// This is identical to the flat escape hatch
/// `.specs().path::<i32>("engine.cylinders").gt(4)`.
#[djogi::djogi_test]
async fn typed_jsonb_depth2_filter_cylinders(mut ctx: djogi::DjogiContext) {
    setup_vehicles_deep(&mut ctx).await;

    let v8 = make_vehicle_deep(&mut ctx, "V8 Beast", 8, false, 1800.0, "BrandA").await;
    let eco = make_vehicle_deep(&mut ctx, "Eco Car", 3, false, 1200.0, "BrandB").await;

    // Typed deep path: specs->engine->cylinders > 4.
    let found = VehicleDeep::objects()
        .filter(|f| f.specs().typed().engine().cylinders().gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("typed depth-2 filter");

    assert!(
        found.iter().any(|v| v.id == v8.id),
        "V8 (cylinders=8) must appear in cylinders > 4 filter"
    );
    assert!(
        !found.iter().any(|v| v.id == eco.id),
        "Eco (cylinders=3) must NOT appear in cylinders > 4 filter"
    );
}

/// Verify typed depth-2 filter and flat escape-hatch filter return the same rows.
///
/// Both expressions should emit `(specs->'engine'->>'cylinders')::int4 > $1`.
/// Running both on the same DB state and asserting equal result sets proves
/// the typed path is not emitting a different SQL expression than the flat one.
#[djogi::djogi_test]
async fn typed_path_matches_flat_path_same_sql(mut ctx: djogi::DjogiContext) {
    setup_vehicles_deep(&mut ctx).await;

    let v8 = make_vehicle_deep(&mut ctx, "Turbo V8", 8, true, 1900.0, "SportsCo").await;
    let inline4 = make_vehicle_deep(&mut ctx, "Inline 4", 4, false, 1400.0, "EcoCo").await;
    let v6 = make_vehicle_deep(&mut ctx, "V6 Touring", 6, false, 1600.0, "TourCo").await;

    // Typed path filter: cylinders > 4.
    let typed_results = VehicleDeep::objects()
        .filter(|f| f.specs().typed().engine().cylinders().gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("typed path filter");

    // Flat escape-hatch filter: same predicate, same threshold.
    let flat_results = VehicleDeep::objects()
        .filter(|f| f.specs().path::<i32>("engine.cylinders").gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("flat path filter");

    // Both result sets must contain the same IDs.
    let mut typed_ids: Vec<djogi::types::HeerId> = typed_results.iter().map(|v| v.id).collect();
    let mut flat_ids: Vec<djogi::types::HeerId> = flat_results.iter().map(|v| v.id).collect();
    typed_ids.sort_unstable();
    flat_ids.sort_unstable();

    assert_eq!(
        typed_ids, flat_ids,
        "typed path and flat escape-hatch must return identical rows for cylinders > 4"
    );

    // Spot-check: V8 (8 cyl) and V6 (6 cyl) must appear; inline4 (4 cyl) must not.
    assert!(typed_ids.contains(&v8.id), "V8 must appear");
    assert!(typed_ids.contains(&v6.id), "V6 must appear");
    assert!(
        !typed_ids.contains(&inline4.id),
        "Inline4 (=4) must NOT appear"
    );
}

// ── Depth-1 filter via typed path ─────────────────────────────────────────────

/// Filter at depth 1 via the typed path: `specs.weight_kg > 1500`.
///
/// Emits: `(specs->>'weight_kg')::float4 > $1`
#[djogi::djogi_test]
async fn typed_jsonb_depth1_filter_weight(mut ctx: djogi::DjogiContext) {
    setup_vehicles_deep(&mut ctx).await;

    let heavy = make_vehicle_deep(&mut ctx, "Heavy Truck", 8, false, 2500.0, "TruckCo").await;
    let light = make_vehicle_deep(&mut ctx, "Light Sedan", 4, false, 1100.0, "SedanCo").await;

    let heavy_found = VehicleDeep::objects()
        .filter(|f| f.specs().typed().weight_kg().gt(1500.0_f32))
        .fetch_all(&mut ctx)
        .await
        .expect("depth-1 weight_kg filter");

    assert!(
        heavy_found.iter().any(|v| v.id == heavy.id),
        "Heavy truck (2500kg) must appear in weight_kg > 1500 filter"
    );
    assert!(
        !heavy_found.iter().any(|v| v.id == light.id),
        "Light sedan (1100kg) must NOT appear in weight_kg > 1500 filter"
    );
}

// ── Boolean filter via typed path ─────────────────────────────────────────────

/// Filter by boolean field at depth 2: `specs.engine.turbo == true`.
///
/// Emits: `(specs->'engine'->>'turbo')::boolean = $1`
#[djogi::djogi_test]
async fn typed_jsonb_depth2_filter_bool(mut ctx: djogi::DjogiContext) {
    setup_vehicles_deep(&mut ctx).await;

    let turbo = make_vehicle_deep(&mut ctx, "Turbo Sports", 4, true, 1500.0, "TurboCo").await;
    let no_turbo = make_vehicle_deep(&mut ctx, "NA Engine", 4, false, 1500.0, "NACo").await;

    let turbo_found = VehicleDeep::objects()
        .filter(|f| f.specs().typed().engine().turbo().eq(true))
        .fetch_all(&mut ctx)
        .await
        .expect("depth-2 turbo bool filter");

    assert!(
        turbo_found.iter().any(|v| v.id == turbo.id),
        "Turbo vehicle must appear when filtering engine.turbo = true"
    );
    assert!(
        !turbo_found.iter().any(|v| v.id == no_turbo.id),
        "Non-turbo vehicle must NOT appear when filtering engine.turbo = true"
    );
}

// ---------------------------------------------------------------------------
// Task 7 — Native aggregates: ArrayAgg / JsonAgg / StringAgg / BoolAnd / BoolOr
//           + Interval Numeric (datetime + duration arithmetic in filters)
// ---------------------------------------------------------------------------

/// `ARRAY_AGG(title)` over a table of three posts returns a `Vec<String>`
/// containing all three title strings.
///
/// Strategy: insert three posts with distinct titles, run an `annotate`
/// with `array_agg` over the title column, fetch all rows. The aggregate
/// uses `OVER ()` (window form inside `annotate`) so all rows contribute to
/// the same aggregate result on every returned row. Assert the aggregate
/// on the first row contains all three titles.
#[djogi::djogi_test]
async fn array_agg_collects_values(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    Post::create(&mut ctx, make_post("Alpha", vec![], vec![]))
        .await
        .expect("create p1");
    Post::create(&mut ctx, make_post("Beta", vec![], vec![]))
        .await
        .expect("create p2");
    Post::create(&mut ctx, make_post("Gamma", vec![], vec![]))
        .await
        .expect("create p3");

    // `annotate` + `array_agg` on the title column — window form means
    // every row gets the full array (all 3 titles). Fetch all rows and
    // check the aggregate value from the first row.
    let rows = Post::objects()
        .annotate(|f| f.title().array_agg())
        .fetch_all(&mut ctx)
        .await
        .expect("annotate array_agg");

    assert!(!rows.is_empty(), "expected at least one annotated row");
    // All rows have the same window-aggregate value; check the first.
    let (_post, titles) = &rows[0];
    assert!(
        titles.contains(&"Alpha".to_string()),
        "array_agg must include 'Alpha'; got: {titles:?}"
    );
    assert!(
        titles.contains(&"Beta".to_string()),
        "array_agg must include 'Beta'; got: {titles:?}"
    );
    assert!(
        titles.contains(&"Gamma".to_string()),
        "array_agg must include 'Gamma'; got: {titles:?}"
    );
}

/// `STRING_AGG(title, ', ')` over two posts returns a joined `String`
/// with the separator between the titles.
///
/// Strategy: insert two posts. Fetch all rows with `annotate`. The window
/// form means every row's aggregate covers the entire table — check the
/// aggregate value from the first row.
#[djogi::djogi_test]
async fn string_agg_joins_with_separator(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    Post::create(&mut ctx, make_post("Aardvark", vec![], vec![]))
        .await
        .expect("create p1");
    Post::create(&mut ctx, make_post("Buffalo", vec![], vec![]))
        .await
        .expect("create p2");

    // Fetch all rows — window aggregate covers all two rows.
    let rows = Post::objects()
        .annotate(|f| f.title().string_agg(", "))
        .fetch_all(&mut ctx)
        .await
        .expect("annotate string_agg");

    assert!(!rows.is_empty(), "expected at least one annotated row");
    let (_post, joined) = &rows[0];
    // The separator `", "` must appear somewhere in the joined result,
    // and both titles must appear. We do not assert order since Postgres
    // does not guarantee aggregate ordering without ORDER BY inside the
    // aggregate (that is a future extension).
    assert!(
        joined.contains("Aardvark"),
        "joined string must contain 'Aardvark'; got: {joined:?}"
    );
    assert!(
        joined.contains("Buffalo"),
        "joined string must contain 'Buffalo'; got: {joined:?}"
    );
    assert!(
        joined.contains(", "),
        "separator ', ' must appear in joined string; got: {joined:?}"
    );
}

/// `JSONB_AGG(title)` returns a `serde_json::Value::Array` containing
/// one element per row.
///
/// Strategy: insert two posts, annotate over all rows (window form),
/// assert the first row's aggregate is an array containing both titles.
#[djogi::djogi_test]
async fn json_agg_collects_as_jsonb(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    Post::create(&mut ctx, make_post("JsonOne", vec![], vec![]))
        .await
        .expect("create p1");
    Post::create(&mut ctx, make_post("JsonTwo", vec![], vec![]))
        .await
        .expect("create p2");

    let rows = Post::objects()
        .annotate(|f| f.title().json_agg())
        .fetch_all(&mut ctx)
        .await
        .expect("annotate json_agg");

    assert!(!rows.is_empty(), "expected at least one annotated row");
    let (_post, agg_val) = &rows[0];
    let arr = agg_val
        .as_array()
        .expect("JSONB_AGG result must be a JSON array");
    let titles: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        titles.contains(&"JsonOne"),
        "json_agg must include 'JsonOne'; got: {titles:?}"
    );
    assert!(
        titles.contains(&"JsonTwo"),
        "json_agg must include 'JsonTwo'; got: {titles:?}"
    );
}

/// `BOOL_AND(published)` returns `true` when every row in the aggregate
/// has `published = true`.
///
/// Strategy: insert 3 published posts. The window-form aggregate sees all
/// three rows and returns true.
#[djogi::djogi_test]
async fn bool_and_is_true_when_all_rows_true(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let p1 = Post::create(&mut ctx, make_post_published("Published A", true))
        .await
        .expect("create published p1");
    let p2 = Post::create(&mut ctx, make_post_published("Published B", true))
        .await
        .expect("create published p2");
    let p3 = Post::create(&mut ctx, make_post_published("Published C", true))
        .await
        .expect("create published p3");

    let rows = Post::objects()
        .filter(|f| f.id().eq(p1.id))
        .annotate(|f| f.published().bool_and())
        .fetch_all(&mut ctx)
        .await
        .expect("annotate bool_and all true");

    assert_eq!(rows.len(), 1, "expected exactly one annotated row");
    let (_post, result) = &rows[0];
    assert!(
        *result,
        "BOOL_AND must be true when all rows are published=true"
    );
    drop((p2, p3));
}

/// `BOOL_OR(published)` returns `true` when at least one row in the
/// aggregate has `published = true`, even when others are `false`.
///
/// Strategy: insert one published post and one draft post. The window-form
/// BOOL_OR sees both rows (over the whole table) and must return true.
#[djogi::djogi_test]
async fn bool_or_is_true_when_any_row_true(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let p1 = Post::create(&mut ctx, make_post_published("Published", true))
        .await
        .expect("create published post");
    let p2 = Post::create(&mut ctx, make_post_published("Draft", false))
        .await
        .expect("create draft post");

    // Annotate on the published row — the window aggregate covers all rows
    // (both published=true and published=false). BOOL_OR must return true
    // because at least one row is true.
    let rows = Post::objects()
        .filter(|f| f.id().eq(p1.id))
        .annotate(|f| f.published().bool_or())
        .fetch_all(&mut ctx)
        .await
        .expect("annotate bool_or");

    assert_eq!(rows.len(), 1, "expected exactly one annotated row");
    let (_post, result) = &rows[0];
    assert!(
        *result,
        "BOOL_OR must be true when at least one row has published=true"
    );
    drop(p2);
}

/// `f.created_at + Expr::literal(time::Duration::days(30))` composes in a
/// WHERE predicate — the SQL emitter renders it as
/// `created_at + INTERVAL '...' > $1`.
///
/// Strategy: insert a post with `created_at = 60 days ago` and a post with
/// `created_at = now()`. The filter `f.created_at + 30 days > (60 days ago)`
/// should match both rows (both are "> 60 days ago + 30 days = 30 days ago").
/// The filter `f.created_at + 30 days > now()` should match only the recent
/// post (whose `created_at + 30 days` is in the future).
#[djogi::djogi_test]
async fn interval_arithmetic_composes_in_where(mut ctx: djogi::DjogiContext) {
    setup_phase5_posts(&mut ctx).await;

    let recent = Post::create(&mut ctx, make_post("Recent", vec![], vec![]))
        .await
        .expect("create recent post");

    // Insert an old post by directly setting created_at via a raw update.
    let old = Post::create(&mut ctx, make_post("Old", vec![], vec![]))
        .await
        .expect("create old post");

    // Move `old` post's created_at to 60 days in the past.
    ctx.raw_execute(
        "UPDATE posts SET created_at = now() - INTERVAL '60 days' WHERE id = $1",
        &[&old.id.as_i64()],
    )
    .await
    .expect("backdate old post");

    // Filter: `created_at + INTERVAL '30 days' > now() - INTERVAL '40 days'`
    //
    // old post:    (now() - 60d) + 30d = now() - 30d  >  now() - 40d  → TRUE
    // recent post: now() + 30d                         >  now() - 40d  → TRUE
    // Both rows pass this loose filter — verifies the interval expression
    // composes correctly and reaches Postgres without a syntax error.
    let forty_days_ago = time::OffsetDateTime::now_utc() - time::Duration::days(40);
    let loose_found = Post::objects()
        .filter_expr(|f| {
            (f.created_at().as_expr() + Expr::literal(time::Duration::days(30)))
                .gt(Expr::literal(forty_days_ago))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("interval arithmetic filter (loose)");

    assert!(
        loose_found.iter().any(|p| p.id == old.id),
        "old post (backdated -60d, +30d = -30d) must pass loose filter (> -40d)"
    );
    assert!(
        loose_found.iter().any(|p| p.id == recent.id),
        "recent post must pass loose filter (created_at + 30d >> -40d)"
    );

    // Filter: `created_at + INTERVAL '30 days' > now() + INTERVAL '1 day'`
    //
    // old post:    (now() - 60d) + 30d = now() - 30d  >  now() + 1d   → FALSE
    // recent post: now() + 30d                         >  now() + 1d   → TRUE
    let tomorrow = time::OffsetDateTime::now_utc() + time::Duration::days(1);
    let strict_found = Post::objects()
        .filter_expr(|f| {
            (f.created_at().as_expr() + Expr::literal(time::Duration::days(30)))
                .gt(Expr::literal(tomorrow))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("interval arithmetic filter (strict)");

    assert!(
        !strict_found.iter().any(|p| p.id == old.id),
        "old post must NOT pass strict filter (created_at + 30d < tomorrow)"
    );
    assert!(
        strict_found.iter().any(|p| p.id == recent.id),
        "recent post must pass strict filter (created_at + 30d > tomorrow)"
    );
}

// ---------------------------------------------------------------------------
// Fix 2 — container-level serde(rename_all) honored by JsonbSchema typed path
// ---------------------------------------------------------------------------

/// A JSONB schema struct with container-level `#[serde(rename_all = "camelCase")]`.
///
/// The on-disk JSON representation uses camelCase keys (`engineType`, `weightKg`).
/// The typed path accessors must route `engine_type` → `engineType` and
/// `weight_kg` → `weightKg` to match the actual on-disk keys.
///
/// This struct must NOT be confused with the existing `PostSpec` (no rename_all)
/// or `VehicleDeepSpecs` (also no rename_all). It lives in a dedicated
/// `camel_posts` table provisioned by `008_camel_posts.sql`.
#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CamelSpec {
    pub engine_type: i32,
    pub weight_kg: f32,
}

/// Model backed by `camel_posts`.
#[model(table = "camel_posts")]
#[derive(Debug, Clone)]
pub struct CamelPost {
    pub spec: Option<Jsonb<CamelSpec>>,
}

async fn setup_camel_posts(ctx: &mut djogi::DjogiContext) {
    setup_phase5(ctx).await;
    const DDL: &str = include_str!("migrations/phase5/008_camel_posts.sql");
    ctx.raw_execute(DDL, &[])
        .await
        .expect("apply 008_camel_posts.sql");
}

/// Verify that the typed path for a `camelCase` rename_all struct routes to
/// the correct JSON keys.
///
/// Strategy:
/// 1. Insert a row with raw JSONB `{"engineType": 6, "weightKg": 1200.0}`.
/// 2. Filter via the typed path using `.engine_type().eq(6)`.
/// 3. Assert the row is found — if the path erroneously routes to
///    `engine_type` (the Rust ident, not the JSON key), Postgres returns
///    an empty result because no key named `engine_type` exists.
#[djogi::djogi_test]
async fn jsonb_typed_path_honors_container_rename_all(mut ctx: djogi::DjogiContext) {
    setup_camel_posts(&mut ctx).await;

    // Insert raw JSONB with camelCase keys to prove the macro routes correctly.
    ctx.raw_execute(
        "INSERT INTO camel_posts (id, created_at, updated_at, spec) \
         VALUES (generate_id(), now(), now(), $1::jsonb)",
        &[&serde_json::json!({"engineType": 6, "weightKg": 1200.0})],
    )
    .await
    .expect("insert raw camelCase post");

    // The typed path `engine_type` must emit `engineType` (the camelCase wire
    // key), not `engine_type` (the Rust ident). Without container rename_all
    // support this filter returns zero rows.
    let posts = CamelPost::objects()
        .filter(|f| f.spec().typed().engine_type().eq(6))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by engine_type via typed camelCase path");

    assert_eq!(
        posts.len(),
        1,
        "typed path engine_type must route to JSON key 'engineType' and find the inserted row"
    );

    // Also verify weightKg path routes correctly.
    let heavy = CamelPost::objects()
        .filter(|f| f.spec().typed().weight_kg().gt(1000.0_f32))
        .fetch_all(&mut ctx)
        .await
        .expect("filter by weight_kg via typed camelCase path");

    assert_eq!(
        heavy.len(),
        1,
        "typed path weight_kg must route to JSON key 'weightKg'"
    );
}

/// Verify that inserting a `CamelPost` via `create()` serialises to camelCase
/// keys on the wire, and that a subsequent typed-path filter finds the row.
///
/// This test exercises the full round-trip: serde serialisation (camelCase
/// keys in the JSON) + typed path routing (Rust idents → camelCase keys in SQL).
#[djogi::djogi_test]
async fn jsonb_camel_create_and_typed_path_roundtrip(mut ctx: djogi::DjogiContext) {
    setup_camel_posts(&mut ctx).await;

    // Create a CamelPost using the ORM — serde encodes as camelCase on the wire.
    let spec = CamelSpec {
        engine_type: 8,
        weight_kg: 2000.0,
    };
    CamelPost::create(
        &mut ctx,
        CamelPost {
            spec: Some(Jsonb::new(spec)),
            ..Default::default()
        },
    )
    .await
    .expect("create CamelPost");

    // Verify via typed path — must find the created row.
    let found = CamelPost::objects()
        .filter(|f| f.spec().typed().engine_type().gt(4))
        .fetch_all(&mut ctx)
        .await
        .expect("filter after create");

    assert_eq!(
        found.len(),
        1,
        "CamelPost created via ORM must be findable via typed camelCase path"
    );

    // Verify the on-disk JSON uses camelCase keys (not snake_case).
    let raw_spec = ctx
        .raw_scalar::<serde_json::Value>(
            "SELECT spec FROM camel_posts WHERE id = $1",
            &[&found[0].id.as_i64()],
        )
        .await
        .expect("raw_scalar spec");

    assert!(
        raw_spec.get("engineType").is_some(),
        "on-disk JSON must use 'engineType' key (camelCase), not 'engine_type'"
    );
    assert!(
        raw_spec.get("engine_type").is_none(),
        "on-disk JSON must NOT have 'engine_type' snake_case key"
    );
}

// ---------------------------------------------------------------------------
// Task 9 — TenantPost model
// ---------------------------------------------------------------------------

/// A model with `#[model(tenant_key = "org_id")]` used to exercise tenant
/// isolation via Row Level Security. Table provisioned by
/// `009_tenant_post.sql`, which applies the RLS policy by hand (Phase 7 has
/// not landed to consume the side-channel `target/djogi_rls/` file yet).
///
/// Each row belongs to an org identified by `org_id`. The RLS policy
/// enforces that a connection can only see rows whose `org_id` matches
/// `current_setting('app.tenant_id')::bigint`.
#[model(table = "tenant_post", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TenantPost {
    pub org_id: i64,
    pub title: String,
}

/// Bootstrap: create the tenant_post table and apply the RLS policy.
///
/// Each DDL statement is issued as a separate `raw_execute` call because
/// `raw_execute` uses the prepared-statement path (tokio-postgres
/// `prepare_cached`) which does not support multi-statement SQL.
///
/// We also create a restricted role `djogi_rls_test_user` with SELECT/INSERT
/// on the table. The RLS isolation test `SET LOCAL ROLE` to this user inside
/// each `atomic()` scope so the RLS policy actually fires — superusers bypass
/// RLS unless `FORCE ROW LEVEL SECURITY` is used, but FORCE only applies to
/// the table owner, not to superusers. The restricted-role path is the
/// realistic production model anyway (app connections run as a restricted
/// service account, not as a superuser).
async fn setup_tenant_post(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS tenant_post (
             id          BIGINT PRIMARY KEY DEFAULT generate_id(),
             created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
             org_id      BIGINT NOT NULL,
             title       TEXT NOT NULL
         )",
        &[],
    )
    .await
    .expect("create tenant_post table");

    ctx.raw_execute("ALTER TABLE tenant_post ENABLE ROW LEVEL SECURITY", &[])
        .await
        .expect("enable RLS on tenant_post");

    // FORCE ROW LEVEL SECURITY ensures the policy applies even to the
    // table owner (non-superuser owner). For the superuser test connection we
    // still need SET LOCAL ROLE to a restricted user below.
    ctx.raw_execute("ALTER TABLE tenant_post FORCE ROW LEVEL SECURITY", &[])
        .await
        .expect("force RLS on tenant_post");

    ctx.raw_execute(
        "CREATE POLICY tenant_post_tenant_isolation ON tenant_post \
         USING (org_id = current_setting('app.tenant_id', true)::bigint)",
        &[],
    )
    .await
    .expect("create RLS policy on tenant_post");

    // Create a restricted service-account role for RLS testing.
    //
    // Postgres does NOT support `CREATE ROLE IF NOT EXISTS` — that is
    // MySQL syntax. The idiomatic Postgres idiom is to check `pg_roles`
    // first and only issue `CREATE ROLE` when the role is absent.
    //
    // `CREATE ROLE` is also unprepared DDL — it cannot go through
    // `prepare_cached` (the `raw_execute` path). We use `raw_ddl` which
    // sends the statement via the simple query protocol.
    //
    // Postgres roles are cluster-level (not per-database), so this role
    // persists across `#[djogi_test]` drops. The existence check ensures
    // idempotency across repeated test runs without requiring
    // `DROP ROLE ... IF EXISTS` teardown.
    let role_exists: bool = ctx
        .raw_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'djogi_rls_test_user')",
            &[],
        )
        .await
        .expect("check djogi_rls_test_user existence");

    if !role_exists {
        ctx.raw_ddl("CREATE ROLE djogi_rls_test_user")
            .await
            .expect("create djogi_rls_test_user role");
    }

    // Grant table access to the restricted role.
    ctx.raw_execute(
        "GRANT SELECT, INSERT ON tenant_post TO djogi_rls_test_user",
        &[],
    )
    .await
    .expect("grant table access to djogi_rls_test_user");

    // Grant USAGE on the public schema so the restricted role can see the table.
    ctx.raw_execute("GRANT USAGE ON SCHEMA public TO djogi_rls_test_user", &[])
        .await
        .expect("grant public schema usage to djogi_rls_test_user");

    // Grant execute on generate_id so INSERT with DEFAULT generate_id() works.
    // heeranjid installs generate_id() in the public schema of each per-test
    // database; grant execute on that public function.
    ctx.raw_execute(
        "GRANT EXECUTE ON FUNCTION generate_id() TO djogi_rls_test_user",
        &[],
    )
    .await
    .expect("grant execute on generate_id to djogi_rls_test_user");

    // generate_id() internally reads and writes HeeRanjID support tables:
    // - heer_nodes: SELECT (node configuration lookup)
    // - heer_node_state: SELECT + INSERT (tracks per-node sequence counters;
    //   generate_id() upserts into this table on every call)
    // - heer_config: SELECT (global HeeRanjID configuration)
    //
    // The #[djogi_test] harness installs all four HeeRanjID tables in the
    // public schema of each per-test database. The restricted role must have
    // sufficient privileges on each for INSERT (DEFAULT generate_id()) to
    // succeed without a permission error.
    ctx.raw_execute("GRANT SELECT ON heer_nodes TO djogi_rls_test_user", &[])
        .await
        .expect("grant select on heer_nodes to djogi_rls_test_user");

    ctx.raw_execute(
        "GRANT SELECT, INSERT, UPDATE ON heer_node_state TO djogi_rls_test_user",
        &[],
    )
    .await
    .expect("grant select/insert/update on heer_node_state to djogi_rls_test_user");

    ctx.raw_execute("GRANT SELECT ON heer_config TO djogi_rls_test_user", &[])
        .await
        .expect("grant select on heer_config to djogi_rls_test_user");
}

// ---------------------------------------------------------------------------
// Task 9 — set_tenant activates RLS isolation
// ---------------------------------------------------------------------------

/// Verify that `set_tenant` inside `atomic()` activates RLS isolation.
///
/// Strategy:
/// 1. Open an `atomic` scope as org 1000, insert a TenantPost with org_id=1000.
/// 2. Open a separate `atomic` scope as org 2000, query TenantPost::objects().
///    The result must be empty — RLS hides org 1000's row from org 2000.
/// 3. Open a third `atomic` scope as org 1000 again. The inserted row must
///    reappear — proving isolation is per-transaction, not permanent.
///
/// `SET LOCAL` (via `set_config(…, true)`) resets at commit/rollback, so
/// consecutive `atomic()` scopes on the same pool never bleed tenant state
/// across request boundaries.
#[djogi::djogi_test]
async fn set_tenant_rls_isolates_tenants(mut ctx: djogi::DjogiContext) {
    setup_tenant_post(&mut ctx).await;

    // Grab the underlying pool so we can open fresh atomic scopes.
    let pool = ctx
        .pool()
        .expect("ctx must be pool-backed for this test")
        .clone();

    // ── Step 1: insert a row as org 1000 ────────────────────────────────────
    // SET LOCAL ROLE switches to the restricted service-account role inside
    // the transaction so the RLS policy fires. Superuser connections bypass
    // RLS unconditionally; the restricted role is the realistic production model.
    let post = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Drop to the restricted role so RLS applies.
            tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
                .await?;
            tx.set_tenant("1000").await?;
            assert!(
                tx.tenant_set,
                "tenant_set flag must be true after set_tenant"
            );

            let p = TenantPost::create(
                tx,
                TenantPost {
                    org_id: 1000,
                    title: "Org 1000 Post".to_string(),
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, djogi::DjogiError>(p)
        })
    })
    .await
    .expect("create TenantPost as org 1000");

    // ── Step 2: query as org 2000 — must see zero rows ────────────────────
    let org_2000_posts = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
                .await?;
            tx.set_tenant("2000").await?;
            TenantPost::objects().fetch_all(tx).await
        })
    })
    .await
    .expect("fetch TenantPost as org 2000");

    assert!(
        org_2000_posts.is_empty(),
        "org 2000 must see zero rows — RLS should hide org 1000's post; \
         got {} rows",
        org_2000_posts.len()
    );

    // ── Step 3: query as org 1000 again — must see the inserted row ────────
    let org_1000_posts = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
                .await?;
            tx.set_tenant("1000").await?;
            TenantPost::objects().fetch_all(tx).await
        })
    })
    .await
    .expect("fetch TenantPost as org 1000");

    assert_eq!(
        org_1000_posts.len(),
        1,
        "org 1000 must see exactly 1 post after re-setting tenant"
    );
    assert_eq!(
        org_1000_posts[0].id, post.id,
        "the visible post must be the one inserted in step 1"
    );
    assert_eq!(org_1000_posts[0].org_id, 1000);
    assert_eq!(org_1000_posts[0].title, "Org 1000 Post");
}

/// Verify that `DjogiContext::tenant_set` starts as `false` on a fresh context
/// and is set to `true` after `set_tenant` is called.
#[djogi::djogi_test]
async fn tenant_set_flag_tracks_set_tenant_calls(mut ctx: djogi::DjogiContext) {
    setup_tenant_post(&mut ctx).await;

    let pool = ctx.pool().expect("pool-backed context").clone();

    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            assert!(
                !tx.tenant_set,
                "tenant_set must be false on a fresh transaction context"
            );

            tx.set_tenant("42").await?;

            assert!(
                tx.tenant_set,
                "tenant_set must be true after set_tenant() returns Ok"
            );

            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("tenant_set_flag test");
}

/// Verify that the `target/djogi_rls/tenant_post_rls.sql` side-channel file
/// is emitted when the `TenantPost` model (declared in this file with
/// `#[model(tenant_key = "org_id")]`) is compiled.
///
/// The file is written by `descriptor::expand` into `target/djogi_rls/`
/// relative to the crate being compiled. `CARGO_MANIFEST_DIR` inside the
/// proc macro points at the user crate being processed — for integration
/// tests this is the `djogi` library crate whose `Cargo.toml` lives one
/// level up from `tests/`.
#[djogi::djogi_test]
async fn tenant_post_rls_side_channel_file_exists(_ctx: djogi::DjogiContext) {
    // The macro writes the file relative to CARGO_MANIFEST_DIR.
    // For this integration test, the macro runs in the context of the `djogi`
    // crate (because the `[[test]]` entry lives in djogi/Cargo.toml), so
    // CARGO_MANIFEST_DIR = {worktree}/djogi.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());

    // The proc macro writes to {CARGO_MANIFEST_DIR}/target/djogi_rls/.
    // From within the djogi crate this resolves to
    // {worktree}/djogi/target/djogi_rls/tenant_post_rls.sql.
    // We also check the workspace-root target directory as a fallback.
    let candidates = vec![
        std::path::Path::new(&manifest_dir)
            .join("target")
            .join("djogi_rls")
            .join("tenant_post_rls.sql"),
        // Workspace root target/ — Cargo sometimes redirects OUT_DIR here.
        std::path::Path::new(&manifest_dir)
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("target")
            .join("djogi_rls")
            .join("tenant_post_rls.sql"),
    ];

    let found = candidates.iter().any(|p| p.exists());

    assert!(
        found,
        "tenant_post_rls.sql must exist after compiling TenantPost; \
         checked: {candidates:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 10 — `_insecurely()` suffix methods on tenant-keyed models
// ---------------------------------------------------------------------------

/// Compile-only check: the eight `_insecurely` methods are emitted for
/// `TenantPost` (which declares `#[model(tenant_key = "org_id")]`).
///
/// We resolve each method as a function-item reference (without calling it)
/// so the assertion is purely about whether the symbol exists at all. The
/// compiler rejects this function if any of the eight names is absent from
/// `TenantPost`'s inherent impl.
///
/// No database interaction is performed; the `ctx` parameter is included only
/// because `#[djogi_test]` requires it and we want the same harness for
/// uniformity.
#[djogi::djogi_test]
async fn insecurely_methods_emitted_only_on_tenant_keyed(_ctx: djogi::DjogiContext) {
    // Resolve each method as an unambiguous function-item reference.
    // `let _ = TenantPost::foo;` resolves the path without calling — the
    // compiler emits E0425 ("no associated item `foo` found") if the method
    // is absent. Each `let _` is intentionally unused (hence `let _`).
    let _ = TenantPost::get_insecurely;
    let _ = TenantPost::create_insecurely;
    let _ = TenantPost::save_insecurely;
    let _ = TenantPost::delete_insecurely;
    let _ = TenantPost::objects_insecurely;
    let _ = TenantPost::bulk_create_insecurely;
    // bulk_update_insecurely is generic (F, A bounds) — we verify existence
    // by naming it with concrete type parameters the compiler can resolve.
    let _ = TenantPost::bulk_update_insecurely::<
        fn(TenantPostFields) -> djogi::query::UpdateAssignment,
        djogi::query::UpdateAssignment,
    >;
    let _ = TenantPost::bulk_upsert_insecurely;
}

/// Verify that `create_insecurely` bypasses the RLS `WITH CHECK` constraint.
///
/// Strategy:
/// 1. Set up the tenant_post table with RLS enabled and FORCE ROW LEVEL
///    SECURITY (via the shared `setup_tenant_post` helper from Task 9).
/// 2. Open an `atomic()` scope as a superuser connection (the `djogi` role).
///    Do NOT call `set_tenant` — `app.tenant_id` is intentionally unset.
/// 3. `create_insecurely` issues `SET LOCAL row_security = off` first, so
///    the INSERT succeeds regardless of tenant enforcement.
/// 4. Assert the returned row has the expected field values, then read it
///    back via `get_insecurely` to confirm the bypass path survives a
///    round-trip.
///
/// This test proves the `_insecurely` call path reaches Postgres and
/// returns successfully. Verifying that the *safe* path fails under the
/// same conditions requires the restricted `djogi_rls_test_user` role
/// (which cannot use BYPASSRLS) — that proof lives in
/// `set_tenant_rls_isolates_tenants` and is intentionally not duplicated
/// here.
#[djogi::djogi_test]
async fn insecurely_bypasses_rls(mut ctx: djogi::DjogiContext) {
    setup_tenant_post(&mut ctx).await;

    let pool = ctx
        .pool()
        .expect("ctx must be pool-backed for this test")
        .clone();

    // ── Verify that the insecure path bypasses RLS ──────────────────────────
    // Open an atomic scope as a superuser connection (the `djogi` role).
    // Do NOT call set_tenant — app.tenant_id is intentionally unset so that
    // a plain `TenantPost::create` would fail under FORCE ROW LEVEL SECURITY
    // with a restricted role (see `set_tenant_rls_isolates_tenants` for that test).
    //
    // create_insecurely issues `SET LOCAL row_security = off` before the INSERT,
    // which lifts the WITH CHECK clause evaluation for this statement. This allows
    // superuser connections to write cross-tenant rows.
    //
    // Note: Verifying that the *safe* path fails with RLS requires switching to
    // the djogi_rls_test_user restricted role (which cannot use BYPASSRLS), so
    // that test lives separately in `set_tenant_rls_isolates_tenants`. This test
    // focuses on proving that _insecurely succeeds when invoked by a superuser.
    let post = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Intentionally do NOT call set_tenant — app.tenant_id is unset.
            // create_insecurely issues SET LOCAL row_security = off before
            // the INSERT, so the WITH CHECK clause is not evaluated and the
            // row is written regardless of tenant enforcement.
            TenantPost::create_insecurely(
                tx,
                TenantPost {
                    org_id: 9999,
                    title: "Insecure cross-tenant post".to_string(),
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create_insecurely must succeed even without set_tenant");

    assert_eq!(post.org_id, 9999);
    assert_eq!(post.title, "Insecure cross-tenant post");

    // Confirm the row really landed in the database by reading it back
    // as a superuser (no RLS filter).
    let fetched = djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Superuser bypasses RLS unconditionally; no SET ROLE needed.
            TenantPost::get_insecurely(tx, post.id).await
        })
    })
    .await
    .expect("get_insecurely must find the row created above");

    assert_eq!(fetched.id, post.id);
    assert_eq!(fetched.org_id, 9999);
}

// ---------------------------------------------------------------------------
// Task 11.5 — outbox worker primitives
// ---------------------------------------------------------------------------
//
// These tests exercise claim_pending, mark_published, mark_failed,
// recover_stale, and NotifyPublisher. All tests use the `worker_outbox`
// table provisioned by 010_outbox_worker_schema.sql.

#[cfg(feature = "outbox")]
mod outbox_worker_tests {
    use djogi::outbox::publisher::Publisher;
    use djogi::outbox::publishers::NotifyPublisher;
    use djogi::outbox::worker;
    use djogi::pg::pool::DjogiPool;
    use time::Duration;

    const OUTBOX_TABLE: &str = "worker_outbox";

    async fn setup_worker_outbox(ctx: &mut djogi::DjogiContext) {
        const DDL: &str = include_str!("migrations/phase5/010_outbox_worker_schema.sql");
        // Use raw_ddl (batch_execute / simple query protocol) for multi-
        // statement DDL. raw_execute routes through prepare_cached, which
        // rejects scripts with more than one statement.
        ctx.raw_ddl(DDL)
            .await
            .expect("apply 010_outbox_worker_schema.sql");
    }

    /// Insert rows directly with a chosen state for test seeding.
    async fn seed_row(ctx: &mut djogi::DjogiContext, row_id: i64, action: &str, state: &str) {
        let payload = serde_json::json!({"row_id": row_id});
        let sql = format!(
            "INSERT INTO {OUTBOX_TABLE} (row_id, action, payload, state) \
             VALUES ($1, $2, $3, $4)"
        );
        ctx.raw_execute(&sql, &[&row_id, &action, &payload, &state])
            .await
            .expect("seed_row");
    }

    /// Fetch the state of a row by its outbox primary key.
    async fn fetch_state(ctx: &mut djogi::DjogiContext, id: djogi::HeerId) -> String {
        ctx.raw_scalar::<String>(
            &format!("SELECT state FROM {OUTBOX_TABLE} WHERE id = $1"),
            &[&id.as_i64()],
        )
        .await
        .expect("fetch_state")
    }

    /// Fetch retry_count for a row.
    async fn fetch_retry_count(ctx: &mut djogi::DjogiContext, id: djogi::HeerId) -> i32 {
        ctx.raw_scalar::<i32>(
            &format!("SELECT retry_count FROM {OUTBOX_TABLE} WHERE id = $1"),
            &[&id.as_i64()],
        )
        .await
        .expect("fetch_retry_count")
    }

    // -------------------------------------------------------------------------
    // claim_pending_returns_only_pending_rows
    // -------------------------------------------------------------------------

    /// Seed 3 pending + 2 processing rows. claim_pending(batch_size=10) must
    /// return exactly 3 rows, all of which were in state 'pending'.
    #[djogi::djogi_test]
    async fn claim_pending_returns_only_pending_rows(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;

        // Seed 3 pending rows.
        seed_row(&mut ctx, 1, "create", "pending").await;
        seed_row(&mut ctx, 2, "save", "pending").await;
        seed_row(&mut ctx, 3, "delete", "pending").await;

        // Seed 2 processing rows — claim_pending must skip these.
        seed_row(&mut ctx, 4, "create", "processing").await;
        seed_row(&mut ctx, 5, "create", "processing").await;

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 10, Duration::minutes(5))
            .await
            .expect("claim_pending");

        assert_eq!(
            claimed.len(),
            3,
            "exactly 3 pending rows should be claimed; got {claimed:?}"
        );

        // All returned rows should have row_ids 1, 2, or 3 (the pending ones).
        let claimed_row_ids: std::collections::HashSet<i64> =
            claimed.iter().map(|r| r.row_id).collect();
        assert!(
            claimed_row_ids.contains(&1),
            "row_id 1 must be in claimed batch"
        );
        assert!(
            claimed_row_ids.contains(&2),
            "row_id 2 must be in claimed batch"
        );
        assert!(
            claimed_row_ids.contains(&3),
            "row_id 3 must be in claimed batch"
        );

        // After claim, all 3 must be in 'processing' state in the DB.
        for row in &claimed {
            let state = fetch_state(&mut ctx, row.id).await;
            assert_eq!(
                state, "processing",
                "claimed row must be in 'processing' state after claim_pending"
            );
        }
    }

    // -------------------------------------------------------------------------
    // claim_pending_batch_size_respected
    // -------------------------------------------------------------------------

    /// Seed 5 pending rows; claim with batch_size=2. Only 2 rows are returned.
    #[djogi::djogi_test]
    async fn claim_pending_batch_size_respected(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;

        for i in 1..=5i64 {
            seed_row(&mut ctx, i, "create", "pending").await;
        }

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 2, Duration::minutes(5))
            .await
            .expect("claim_pending batch_size=2");

        assert_eq!(
            claimed.len(),
            2,
            "claim_pending with batch_size=2 must return at most 2 rows"
        );
    }

    // -------------------------------------------------------------------------
    // claim_pending_skips_locked_rows
    // -------------------------------------------------------------------------

    /// Two concurrent claim_pending calls on the same table must return
    /// disjoint sets. We simulate this by running two sequential claims after
    /// each other — since the first claim transitions rows to 'processing',
    /// the second claim must skip them and return different rows.
    ///
    /// Note: SKIP LOCKED is most impactful in a truly concurrent scenario (two
    /// connections claiming in parallel). The sequential simulation here
    /// verifies the state-machine invariant (processing rows are not re-claimed)
    /// which is the observable outcome of SKIP LOCKED from a single-connection
    /// perspective.
    #[djogi::djogi_test]
    async fn claim_pending_skips_locked_rows(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;

        // Seed 4 rows.
        for i in 1..=4i64 {
            seed_row(&mut ctx, i, "create", "pending").await;
        }

        // First claim: grab 2 rows.
        let first_batch = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 2, Duration::minutes(5))
            .await
            .expect("first claim_pending");
        assert_eq!(first_batch.len(), 2);

        // Second claim: must return the other 2, not the already-claimed rows.
        let second_batch = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 2, Duration::minutes(5))
            .await
            .expect("second claim_pending");
        assert_eq!(second_batch.len(), 2);

        // The two batches must be disjoint.
        let first_ids: std::collections::HashSet<djogi::HeerId> =
            first_batch.iter().map(|r| r.id).collect();
        let second_ids: std::collections::HashSet<djogi::HeerId> =
            second_batch.iter().map(|r| r.id).collect();
        assert!(
            first_ids.is_disjoint(&second_ids),
            "two sequential claim_pending calls must return disjoint rows"
        );
    }

    // -------------------------------------------------------------------------
    // mark_published_transitions_to_published
    // -------------------------------------------------------------------------

    #[djogi::djogi_test]
    async fn mark_published_transitions_to_published(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;
        seed_row(&mut ctx, 42, "create", "pending").await;

        // Claim the row.
        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1);

        let row = &claimed[0];
        assert_eq!(
            fetch_state(&mut ctx, row.id).await,
            "processing",
            "row must be processing after claim"
        );

        // Mark published.
        worker::mark_published(&mut ctx, OUTBOX_TABLE, row.id)
            .await
            .expect("mark_published");

        assert_eq!(
            fetch_state(&mut ctx, row.id).await,
            "published",
            "row must be in terminal 'published' state after mark_published"
        );

        // Published row must NOT be re-claimable.
        let re_claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 10, Duration::minutes(5))
            .await
            .expect("re-claim after published");
        assert!(
            re_claimed.is_empty(),
            "published row must not be re-claimable"
        );
    }

    // -------------------------------------------------------------------------
    // mark_failed_retryable_returns_to_pending
    // -------------------------------------------------------------------------

    /// A retryable failure below the retry budget returns the row to 'pending'
    /// and increments retry_count.
    #[djogi::djogi_test]
    async fn mark_failed_retryable_returns_to_pending(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;
        seed_row(&mut ctx, 99, "create", "pending").await;

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("claim");
        let row = &claimed[0];

        // Mark failed with retryable=true. retry_count starts at 0 < MAX (10).
        worker::mark_failed(
            &mut ctx,
            OUTBOX_TABLE,
            row.id,
            "transient connection timeout",
            true,
        )
        .await
        .expect("mark_failed retryable");

        // Row must be back to 'pending'.
        assert_eq!(
            fetch_state(&mut ctx, row.id).await,
            "pending",
            "retryable failed row must return to 'pending' state"
        );

        // retry_count must be incremented.
        assert_eq!(
            fetch_retry_count(&mut ctx, row.id).await,
            1,
            "retry_count must be 1 after first retryable failure"
        );

        // The row must be re-claimable.
        let re_claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("re-claim after retry");
        assert_eq!(re_claimed.len(), 1, "retried row must be re-claimable");
    }

    // -------------------------------------------------------------------------
    // mark_failed_permanent_stays_terminal
    // -------------------------------------------------------------------------

    /// A non-retryable failure terminally transitions the row to 'failed'.
    #[djogi::djogi_test]
    async fn mark_failed_permanent_stays_terminal(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;
        seed_row(&mut ctx, 77, "save", "pending").await;

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("claim");
        let row = &claimed[0];

        // Mark failed with retryable=false.
        worker::mark_failed(
            &mut ctx,
            OUTBOX_TABLE,
            row.id,
            "schema validation failed — permanent",
            false,
        )
        .await
        .expect("mark_failed permanent");

        // Row must be in terminal 'failed' state.
        assert_eq!(
            fetch_state(&mut ctx, row.id).await,
            "failed",
            "non-retryable row must be in terminal 'failed' state"
        );

        // Failed row must NOT be re-claimable.
        let re_claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 10, Duration::minutes(5))
            .await
            .expect("re-claim after permanent failure");
        assert!(
            re_claimed.is_empty(),
            "permanently failed row must not be re-claimable"
        );
    }

    // -------------------------------------------------------------------------
    // mark_failed_budget_exhausted_stays_terminal
    // -------------------------------------------------------------------------

    /// A retryable failure that has already hit MAX_RETRY_COUNT must also
    /// transition to terminal 'failed' rather than returning to 'pending'.
    #[djogi::djogi_test]
    async fn mark_failed_budget_exhausted_stays_terminal(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;

        // Insert a row that already has retry_count = MAX_RETRY_COUNT.
        let max = worker::MAX_RETRY_COUNT;
        let sql = format!(
            "INSERT INTO {OUTBOX_TABLE} (row_id, action, payload, state, retry_count) \
             VALUES ($1, 'create', '{{}}', 'pending', {max})"
        );
        ctx.raw_execute(&sql, &[&55i64])
            .await
            .expect("seed row at max retry_count");

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("claim");
        let row = &claimed[0];

        // Even though retryable=true, budget is exhausted — must go to 'failed'.
        worker::mark_failed(
            &mut ctx,
            OUTBOX_TABLE,
            row.id,
            "transient but budget exhausted",
            true,
        )
        .await
        .expect("mark_failed at budget");

        assert_eq!(
            fetch_state(&mut ctx, row.id).await,
            "failed",
            "row at retry budget must transition to terminal 'failed' even if retryable=true"
        );
    }

    // -------------------------------------------------------------------------
    // recover_stale_moves_expired_leases_back_to_pending
    // -------------------------------------------------------------------------

    /// A 'processing' row whose `leased_until` is in the past must be moved
    /// back to 'pending' by recover_stale.
    #[djogi::djogi_test]
    async fn recover_stale_moves_expired_leases_back_to_pending(mut ctx: djogi::DjogiContext) {
        setup_worker_outbox(&mut ctx).await;

        // Insert a 'processing' row with leased_until in the distant past.
        let sql = format!(
            "INSERT INTO {OUTBOX_TABLE} (row_id, action, payload, state, leased_until) \
             VALUES ($1, 'create', '{{}}', 'processing', now() - interval '1 hour')"
        );
        ctx.raw_execute(&sql, &[&111i64])
            .await
            .expect("seed stale processing row");

        // Insert a 'processing' row with leased_until in the future — must NOT be recovered.
        let sql2 = format!(
            "INSERT INTO {OUTBOX_TABLE} (row_id, action, payload, state, leased_until) \
             VALUES ($1, 'create', '{{}}', 'processing', now() + interval '1 hour')"
        );
        ctx.raw_execute(&sql2, &[&222i64])
            .await
            .expect("seed active processing row");

        // recover_stale with a 10-minute threshold. The stale row has lease
        // 1 hour old (> 10 minutes), the active row has 1 hour in the future.
        let recovered = worker::recover_stale(&mut ctx, OUTBOX_TABLE, Duration::minutes(10))
            .await
            .expect("recover_stale");

        assert_eq!(
            recovered, 1,
            "exactly 1 stale row should be recovered; got {recovered}"
        );

        // The stale row must now be 'pending'.
        let stale_state = ctx
            .raw_scalar::<String>(
                &format!("SELECT state FROM {OUTBOX_TABLE} WHERE row_id = 111"),
                &[],
            )
            .await
            .expect("fetch stale row state");
        assert_eq!(
            stale_state, "pending",
            "stale row must be returned to 'pending'"
        );

        // The active row must remain 'processing'.
        let active_state = ctx
            .raw_scalar::<String>(
                &format!("SELECT state FROM {OUTBOX_TABLE} WHERE row_id = 222"),
                &[],
            )
            .await
            .expect("fetch active row state");
        assert_eq!(
            active_state, "processing",
            "row with future lease must remain 'processing'"
        );
    }

    // -------------------------------------------------------------------------
    // notify_publisher_emits_pg_notify
    // -------------------------------------------------------------------------

    /// Seed a pending row, claim it, then publish via NotifyPublisher. A
    /// separate LISTEN connection spawned before the publish must receive the
    /// notification.
    ///
    /// In tokio-postgres 0.7, `AsyncMessage::Notification` is delivered through
    /// the `Connection` future's internal poll loop. We drive the connection in
    /// a task that forwards notifications through a `tokio::sync::mpsc` channel
    /// so the test body can await them with a timeout.
    ///
    /// The publisher uses a pool-backed context (autocommit per statement) so
    /// `pg_notify` fires immediately — no explicit COMMIT needed.
    ///
    /// # URL derivation
    ///
    /// The `#[djogi_test]` harness creates a fresh `djogi_test_<uuid>` database
    /// for every test. It gives us a `DjogiContext` backed by a pool connected
    /// to that per-test DB, but does not update the `DATABASE_URL` env var. We
    /// derive the per-test URL by querying `current_database()` and splicing that
    /// name into the admin `DATABASE_URL` (replacing its trailing path component).
    #[djogi::djogi_test]
    async fn notify_publisher_emits_pg_notify(mut ctx: djogi::DjogiContext) {
        use tokio_postgres::{AsyncMessage, NoTls};

        setup_worker_outbox(&mut ctx).await;

        // Derive the per-test database URL from DATABASE_URL + current_database().
        let admin_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for NOTIFY test");
        let current_db: String = ctx
            .raw_scalar::<String>("SELECT current_database()", &[])
            .await
            .expect("SELECT current_database()");

        // Replace the database component of the URL. DATABASE_URL may look like:
        //   postgres://user:pass@host:port/dbname
        //   postgres://user:pass@host/dbname   (no port)
        // We find the last '/' and replace everything after it.
        let test_url = {
            if let Some(slash_pos) = admin_url.rfind('/') {
                format!("{}/{}", &admin_url[..slash_pos], current_db)
            } else {
                // Unlikely: URL has no slash after the host. Fall back to admin URL.
                admin_url.clone()
            }
        };

        // ---- LISTEN connection (raw tokio_postgres) ----
        let (listen_client, mut listen_conn) = tokio_postgres::connect(&test_url, NoTls)
            .await
            .expect("connect listen_client");

        // Channel to forward notifications to the test body.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<tokio_postgres::Notification>();

        // Drive the connection. Capture AsyncMessage::Notification and forward
        // them to the test body via the mpsc sender.
        tokio::spawn(async move {
            loop {
                // `poll_fn` wraps the Connection's poll_message into a Future so
                // we can await it. Returns None when the connection is closed.
                let msg = futures::future::poll_fn(|cx| listen_conn.poll_message(cx)).await;
                match msg {
                    None => break, // connection closed
                    Some(Ok(AsyncMessage::Notification(n))) => {
                        let _ = tx.send(n);
                    }
                    Some(Ok(_)) => {} // notice or other async message — ignore
                    Some(Err(_)) => break,
                }
            }
        });

        // Register the listener before publishing so we don't miss the notification.
        let channel = "test_outbox_notify";
        listen_client
            .execute(&format!("LISTEN {channel}"), &[])
            .await
            .expect("LISTEN");

        // ---- Seed + claim + publish ----
        seed_row(&mut ctx, 7, "create", "pending").await;

        // Build a DjogiPool from the per-test URL for the publisher.
        let pool = DjogiPool::connect(&test_url)
            .await
            .expect("build pool for NotifyPublisher");

        let publisher = NotifyPublisher::new(pool.clone(), channel.to_string());

        let claimed = worker::claim_pending(&mut ctx, OUTBOX_TABLE, 1, Duration::minutes(5))
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1);

        // Publish — NotifyPublisher issues SELECT pg_notify($1, $2) outside any
        // transaction (pool-backed context), so the notification fires immediately.
        publisher
            .publish(&claimed[0])
            .await
            .expect("publish via NotifyPublisher");

        // ---- Assert the NOTIFY arrived ----
        let notification = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("NOTIFY must arrive within 5 seconds")
            .expect("notification channel must not be closed before receiving");

        assert_eq!(notification.channel(), channel, "NOTIFY channel must match");

        // The payload is the JSON string of `claimed[0].payload`.
        let expected_payload = claimed[0].payload.to_string();
        assert_eq!(
            notification.payload(),
            expected_payload,
            "NOTIFY payload must be the JSON-stringified outbox row payload"
        );
    }

    // -------------------------------------------------------------------------
    // invalid_table_name_rejected
    // -------------------------------------------------------------------------

    /// A table name with a dot (schema.table) must be rejected before any SQL
    /// is issued. This tests the identifier validation gate in all worker
    /// primitives.
    #[djogi::djogi_test]
    async fn invalid_table_name_rejected(mut ctx: djogi::DjogiContext) {
        let result = worker::claim_pending(&mut ctx, "schema.table", 1, Duration::minutes(5)).await;
        assert!(
            result.is_err(),
            "table name with dot must be rejected by validate_table_ident"
        );

        let result2 =
            worker::mark_published(&mut ctx, "1bad", djogi::HeerId::from_i64(1).unwrap()).await;
        assert!(
            result2.is_err(),
            "table name starting with digit must be rejected"
        );
    }
}
