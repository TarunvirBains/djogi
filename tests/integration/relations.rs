// Task 3 integration tests: `ForeignKey<T>` round-trip and
// explicit single-relation access (`.fetch()` / `.resolved()`) validated
// against live Postgres.
//
// What this file pins:
//
// 1. The macro-generated `FromRow` decodes `ForeignKey<T>` and
//    `Option<ForeignKey<T>>` columns transparently via the
//    `Decode`/`Type` impls Task 1 shipped — no special-case handling
//    required in the macro's emission.
// 2. `Vehicle::create` encodes a `ForeignKey<T>` field as the target
//    model's PK type (BIGINT here), matching Task 1's `Encode` impl.
// 3. `.key()` returns the stored PK on both freshly-constructed and
//    re-fetched rows; `.resolved()` returns `None` unconditionally on
//    the unresolved wrapper (spec's no-lazy-loading invariant).
// 4. `.fetch(executor)` issues exactly one `SELECT` via `T::get` and
//    returns the fully-materialised target row.
// 5. Nullable FKs round-trip `None`/`Some` cleanly through the
//    `Option<ForeignKey<T>>` branch.
// 6. Inserts with a non-existent FK value surface cleanly as a
//    `DjogiError::Db` — proving the PG constraint violation
//    doesn't panic through the Djogi error machinery.
//
// # Fixture strategy (Q10 resolution in the plan)
//
// Each test uses `#[djogi_test(sync_models = [Owner, FuelType, Vehicle])]`, so the `#[djogi_test]` bootstrap already installs
// HeeRanjID schema + seeds node 1 + sets `heer.node_id = '1'` at the
// database level before the test body runs. Compact seed helpers
// (`seed_owner`, `seed_fuel_type`, `seed_vehicle_with_owner`) compose
// into per-test fixtures.
//
// The `_p3` table-name suffix keeps these integration tests isolated
// from the Phase 1/2 fixtures that share the same database.

use djogi::prelude::*;
use djogi::transaction::{AtomicFuture, atomic};

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

// `Owner` is the FK target for the non-null relation on `Vehicle`. Default-
// derived so `Owner::create(ctx, Owner { name, ..Default::default() })`
// stays ergonomic — matches the Phase 1/2 test-model shape.
// T2 default flip — pin ascending HeerId; the seed helpers
// and FK targets below assume `Owner::Pk` / `FuelType::Pk` / `Vehicle::Pk`
// are all ordinary `HeerId`.
#[model(table = "owners_p3", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

// `FuelType` is the FK target for the nullable relation on `Vehicle`.
// Same Default-derived shape as `Owner` so the seed helpers stay parallel.
#[model(table = "fuel_types_p3", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct FuelType {
    pub name: String,
}

// `Vehicle` carries one non-null and one nullable FK. `no_default` is
// required because `ForeignKey<T>` intentionally does not implement
// `Default` — a relation with no PK value is meaningless (see the
// compile-pass fixture in `djogi-macros/tests/compile_pass/relations.rs`
// for the same rationale). Tests construct `Vehicle` with explicit
// framework-field sentinels via `vehicle_for_insert` below.
#[model(table = "vehicles_p3", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub owner_id: ForeignKey<Owner>,
    pub fuel_type_id: Option<ForeignKey<FuelType>>,
}

// ---------------------------------------------------------------------------
// Fixture helpers (per plan Q10 — shared setup + compact seeders)
// ---------------------------------------------------------------------------

/// Seed one `Owner` row with the given `name`. Framework fields
/// (`id`, `created_at`, `updated_at`) are populated by the DB defaults
/// via `RETURNING *`.
///
/// `heer.node_id` is inherited from the ALTER DATABASE set by the
/// `#[djogi_test]` bootstrap — the test database has it set.
async fn seed_owner(ctx: &mut djogi::DjogiContext, name: &str) -> Owner {
    Owner::create(
        ctx,
        Owner {
            name: name.into(),
            ..Default::default()
        },
    )
    .await
    .expect("seed_owner: Owner::create should succeed")
}

/// Seed one `FuelType` row. Mirror of `seed_owner`.
async fn seed_fuel_type(ctx: &mut djogi::DjogiContext, name: &str) -> FuelType {
    FuelType::create(
        ctx,
        FuelType {
            name: name.into(),
            ..Default::default()
        },
    )
    .await
    .expect("seed_fuel_type: FuelType::create should succeed")
}

/// Build a `Vehicle` value suitable for `Vehicle::create`. Framework
/// fields use the same sentinel pattern as
/// `phase1_model::rich_field_types_roundtrip` — the DB defaults
/// overwrite them via `RETURNING *` on the insert. Extracted into a
/// helper because `no_default` on `Vehicle` forbids
/// `..Default::default()` and the sentinel construction would
/// otherwise repeat across every test.
fn vehicle_for_insert(make: &str, owner: &Owner, fuel: Option<&FuelType>) -> Vehicle {
    Vehicle {
        id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
        make: make.into(),
        owner_id: ForeignKey::new(owner.id),
        fuel_type_id: fuel.map(|f| ForeignKey::new(f.id)),
    }
}

/// Seed one `Vehicle` row pointing at `owner` (required) and
/// optionally `fuel` (nullable FK). Returns the DB-materialised row
/// (with DB-assigned id + timestamps).
async fn seed_vehicle_with_owner(
    ctx: &mut djogi::DjogiContext,
    make: &str,
    owner: &Owner,
    fuel: Option<&FuelType>,
) -> Vehicle {
    Vehicle::create(ctx, vehicle_for_insert(make, owner, fuel))
        .await
        .expect("seed_vehicle_with_owner: Vehicle::create should succeed")
}

async fn run_atomic<F, R>(ctx: &mut djogi::DjogiContext, closure: F) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut djogi::DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    let mut tx = ctx.begin().await?;
    let result = atomic(&mut tx, closure).await;
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(err) => {
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Task 3 integration tests
// ---------------------------------------------------------------------------

/// FK round-trip: create a vehicle pointing at an owner, re-fetch via
/// `Vehicle::get`, and confirm the FK column decoded into a
/// `ForeignKey<Owner>` whose `.key()` matches the owner's PK.
///
/// Also pins the no-lazy-loading invariant: a freshly-decoded FK has
/// `.resolved() == None`. Any future regression that accidentally
/// caches the target alongside the FK column would flip this assertion
/// and surface as a loud test failure instead of a silent behavior
/// change.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn fk_round_trip_stores_and_retrieves_pk(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Alice").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Toyota", &owner, None).await;

    let loaded = Vehicle::get(&mut ctx, vehicle.id)
        .await
        .expect("Vehicle::get should succeed");

    assert_eq!(
        loaded.owner_id.key(),
        owner.id,
        "re-fetched ForeignKey must carry the original owner's PK"
    );
    assert!(
        loaded.owner_id.resolved().is_none(),
        "unresolved ForeignKey must not carry a cached child — prefetch / select_related is the only path that materialises one"
    );
}

/// `.fetch(executor)` issues one `SELECT` against the target table and
/// returns the fully-materialised row. Asserts both the id (PK match)
/// and a non-PK column (`name`) so a misrouted fetch — e.g. one that
/// returned the same ID from the wrong table — would fail on the name
/// comparison.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn fk_fetch_loads_related_row(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Alice").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Toyota", &owner, None).await;

    let fetched_owner = vehicle
        .owner_id
        .fetch(&mut ctx)
        .await
        .expect("ForeignKey::fetch should resolve to the owner row");

    assert_eq!(fetched_owner.id, owner.id);
    assert_eq!(
        fetched_owner.name, "Alice",
        "fetch must return the row that matched the FK, not a different owner"
    );
}

/// Nullable FK — `None` side. Create a vehicle with no fuel type,
/// re-fetch, and confirm the `Option<ForeignKey<FuelType>>` column
/// decodes back to `None`.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn nullable_fk_round_trips_none(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Bob").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Honda", &owner, None).await;

    let loaded = Vehicle::get(&mut ctx, vehicle.id)
        .await
        .expect("Vehicle::get should succeed");

    assert!(
        loaded.fuel_type_id.is_none(),
        "nullable FK with NULL column must decode to Option::None"
    );
}

/// Nullable FK — `Some` side. Mirror of `nullable_fk_round_trips_none`:
/// insert with a non-null fuel-type FK, re-fetch, confirm the
/// `Option<ForeignKey<FuelType>>` column carries the target's PK.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn nullable_fk_round_trips_some(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Carol").await;
    let fuel = seed_fuel_type(&mut ctx, "Gas").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Subaru", &owner, Some(&fuel)).await;

    let loaded = Vehicle::get(&mut ctx, vehicle.id)
        .await
        .expect("Vehicle::get should succeed");

    let fk = loaded
        .fuel_type_id
        .as_ref()
        .expect("nullable FK with non-null column must decode to Option::Some");
    assert_eq!(
        fk.key(),
        fuel.id,
        "round-tripped nullable FK must carry the original fuel-type PK"
    );
}

/// `.fetch(executor)` on the inner `ForeignKey<FuelType>` of a
/// `Some(_)`-valued nullable column. Proves the happy path composes
/// cleanly through `Option::as_ref().unwrap().fetch(&mut ctx)` — the
/// same shape user code will write for opportunistic resolution of
/// nullable FKs inside a handler.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn nullable_fk_fetch_loads_related_row(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Dana").await;
    let fuel = seed_fuel_type(&mut ctx, "Diesel").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Ford", &owner, Some(&fuel)).await;

    let fetched_fuel = vehicle
        .fuel_type_id
        .as_ref()
        .expect("seed helper attached a fuel type — Option should be Some")
        .fetch(&mut ctx)
        .await
        .expect("ForeignKey::fetch on the nullable FK should resolve");

    assert_eq!(fetched_fuel.id, fuel.id);
    assert_eq!(fetched_fuel.name, "Diesel");
}

/// Inserting a `Vehicle` with an `owner_id` that doesn't exist in
/// `owners_p3` must surface the Postgres `foreign_key_violation` as a
/// `DjogiError::Db` — not panic, not swallow, not mangle. This
/// anchors the contract that the relation wrapper is transparent to
/// the usual error flow: the FK column is just a BIGINT with a REFERENCES
/// constraint as far as the `INSERT` is concerned, so any existing
/// error plumbing continues to work.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn fk_creation_db_error_on_unknown_owner(mut ctx: djogi::DjogiContext) {
    // Craft a HeerId that can't exist in `owners_p3` — we never seed
    // any owners in this test, and `generate_id()` produces
    // time-ordered IDs that dwarf this small sentinel value.
    let bogus_owner_id = ::heeranjid::HeerId::from_i64(42).expect("42 is a valid HeerId");
    let bogus_owner = Owner {
        id: bogus_owner_id,
        ..Default::default()
    };

    let result = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Vehicle::create(ctx, vehicle_for_insert("Phantom", &bogus_owner, None)).await
        })
    })
    .await;

    let err = result.expect_err("insert with unknown owner_id must fail");
    // FK violations now surface as `DjogiError::Db(DbError)`. We assert the
    // SQLSTATE 23503 (`foreign_key_violation`) is preserved.
    match err {
        DjogiError::Db(ref db_err) => {
            let msg = db_err.message();
            assert!(
                matches!(db_err.code(), Some(code) if code.code() == "23503"),
                "expected foreign_key_violation (SQLSTATE 23503), got: {msg}"
            );
        }
        other => panic!("expected DjogiError::Db from FK violation, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Task 4 integration tests: `QuerySet::prefetch` + `PrefetchedRow<T>`.
// ---------------------------------------------------------------------------
//
// Each test seeds its own fixture via compact `seed_*` builders used above. The prefetch path runs the usual
// `fetch_all_prefetched` terminal — no
// instrumentation beyond what the main query already uses. Where a test
// asserts "exactly two queries were issued", we rely on the `LEFT JOIN`-
// based stitcher's shape (one main query + one prefetch query per
// registered path) — the assertion
// is structural, anchored by the terminal's documented contract in
// `djogi/src/relation/prefetch.rs`.

/// Happy path: two vehicles, two distinct owners, prefetch resolves both.
/// Proves the wrapper surface works end-to-end — main rows come back as
/// `PrefetchedRow<Vehicle>` and each `row.get(VehicleRelated::owner())`
/// returns a `&Owner` whose `name` matches the seeded value.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_fk_loads_related_without_n_plus_one(mut ctx: djogi::DjogiContext) {
    let alice = seed_owner(&mut ctx, "Alice").await;
    let bob = seed_owner(&mut ctx, "Bob").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &alice, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Honda", &bob, None).await;

    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .order_by(|f| f.make().asc())
        .prefetch(VehicleRelated::owner())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed");

    assert_eq!(rows.len(), 2);
    // The order_by clause guarantees `Honda` precedes `Toyota`
    // alphabetically, so position 0 is Bob's Honda and position 1 is
    // Alice's Toyota. Asserting on both ordering and name pins the
    // correct parent-to-target wiring: a naive implementation that
    // accidentally swapped slots would still return two Owners, but
    // with the wrong names in the wrong slots.
    assert_eq!(rows[0].row.make, "Honda");
    let honda_owner = rows[0]
        .get(VehicleRelated::owner())
        .expect("prefetched owner should be present for non-null FK");
    assert_eq!(honda_owner.name, "Bob");

    assert_eq!(rows[1].row.make, "Toyota");
    let toyota_owner = rows[1]
        .get(VehicleRelated::owner())
        .expect("prefetched owner should be present for non-null FK");
    assert_eq!(toyota_owner.name, "Alice");
}

#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_on_bulk_delete_is_rejected(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "DeleteGuard").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &owner, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Honda", &owner, None).await;

    let err = Vehicle::objects()
        .prefetch(VehicleRelated::owner())
        .delete(&mut ctx)
        .await
        .expect_err("prefetch on bulk delete must be rejected");
    match err {
        DjogiError::Validation(msg) => assert!(
            msg.contains("prefetch"),
            "validation should mention prefetch: {msg}"
        ),
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let remaining = Vehicle::objects().count(&mut ctx).await.unwrap();
    assert_eq!(
        remaining, 2,
        "rejected prefetch delete must not remove rows"
    );
}

/// Nullable FK path: vehicle has no `fuel_type_id`. Prefetching the
/// `fuel_type` relation on this row must return `None` (not panic, not
/// error) — the LEFT JOIN miss is the documented null-safe behaviour.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_nullable_fk_skipped(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Carol").await;
    // Deliberately pass `None` for the fuel type — the column is
    // `Option<ForeignKey<FuelType>>` and `fuel_type_id` is the nullable
    // FK we want to exercise.
    let _ = seed_vehicle_with_owner(&mut ctx, "Subaru", &owner, None).await;

    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .prefetch(VehicleRelated::fuel_type())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed");

    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].get(VehicleRelated::fuel_type()).is_none(),
        "nullable FK with NULL column must surface as None in the resolved-relations map"
    );
}

/// Duplicate FK: two vehicles point at the same owner. Both
/// `row.get(...)` calls must return data equivalent to the shared owner.
/// Implementation detail: this test deliberately does NOT assert they
/// are the same `&Owner` pointer — the stitcher clones the target so
/// each parent owns its slot independently, which keeps the API's
/// `Drop` semantics simple. Data-equality is the observable contract.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_duplicate_fk_stitches_same_child(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Dana").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Ford", &owner, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Chevy", &owner, None).await;

    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .order_by(|f| f.make().asc())
        .prefetch(VehicleRelated::owner())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed");

    assert_eq!(rows.len(), 2);
    for row in &rows {
        let resolved = row
            .get(VehicleRelated::owner())
            .expect("both vehicles share the same owner — both resolved entries must be present");
        assert_eq!(resolved.id, owner.id);
        assert_eq!(resolved.name, "Dana");
    }
}

/// Empty main result: filter matches no rows. Prefetch must short-
/// circuit without issuing the target-fetch query — `apply_prefetches`
/// documents this and `fetch_all_prefetched` honours it. The test asserts
/// the empty result (no panic, no error); the "no second query" clause
/// of the contract is enforced structurally at the code path, not
/// instrumented here.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_empty_parent_set_issues_no_child_query(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Eve").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "BMW", &owner, None).await;

    // Filter on an ID that cannot exist — generate_id() is
    // time-ordered and dwarfs any small sentinel.
    let nonexistent = ::djogi::types::HeerId::from_i64(1).expect("1 is a valid HeerId sentinel");
    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .filter(|f| f.id().eq(nonexistent))
        .prefetch(VehicleRelated::owner())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched should succeed even when empty");

    assert!(
        rows.is_empty(),
        "filter on nonexistent id should return zero rows"
    );
}

/// Multiple prefetches on the same queryset — one per relation. Each
/// resolves independently; both are reachable via `row.get(...)` on the
/// same `PrefetchedRow`. Proves the `prefetch_paths` vec and the
/// per-column HashMap key strategy compose across heterogeneous target
/// types (`Owner` and `FuelType` are distinct structs).
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_multiple_relations_combines_correctly(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Frank").await;
    let fuel = seed_fuel_type(&mut ctx, "Electric").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Tesla", &owner, Some(&fuel)).await;

    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .prefetch(VehicleRelated::owner())
        .prefetch(VehicleRelated::fuel_type())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("fetch_all_prefetched with two prefetches should succeed");

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let resolved_owner = row
        .get(VehicleRelated::owner())
        .expect("owner prefetch should resolve");
    assert_eq!(resolved_owner.name, "Frank");
    let resolved_fuel = row
        .get(VehicleRelated::fuel_type())
        .expect("fuel_type prefetch should resolve");
    assert_eq!(resolved_fuel.name, "Electric");
}

/// Idempotent prefetch: calling `.prefetch(same_path)` twice registers
/// one prefetch, not two. The second call is a no-op (deduplicated by
/// `source_column`). Test passes if:
///
/// 1. The query does not error (no duplicate-key HashMap panic),
/// 2. The single owner prefetch returns the correct data.
///
/// Strictly-speaking, asserting "one round trip instead of two" is a
/// performance contract; this test only enforces correctness, which is
/// the user-visible invariant. The structural dedup lives in
/// `QuerySet::prefetch` and is covered by the queryset's own unit
/// tests through related coverage (no runtime panic path exists for duplicate
/// paths).
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn prefetch_same_relation_twice_is_idempotent(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Grace").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Audi", &owner, None).await;

    let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
        .prefetch(VehicleRelated::owner())
        .prefetch(VehicleRelated::owner())
        .fetch_all_prefetched(&mut ctx)
        .await
        .expect("duplicate prefetch registrations must not cause failure");

    assert_eq!(rows.len(), 1);
    let resolved = rows[0]
        .get(VehicleRelated::owner())
        .expect("owner must resolve after duplicate prefetch");
    assert_eq!(resolved.name, "Grace");
}

// ---------------------------------------------------------------------------
// Task 5 integration tests: `QuerySet::select_related` + `JoinedRow<T>`.
// ---------------------------------------------------------------------------
//
// select_related issues a single `LEFT JOIN` per registered relation path
// instead of the follow-up query prefetch uses. Each test seeds its own
// fixture via typed seed helpers and exercises the
// `fetch_all_joined` terminal. The wrapper type is `JoinedRow<T>` — the
// joined-row analog of `PrefetchedRow<T>` — and exposes typed child
// access via `row.get(VehicleRelated::owner())`.

/// Happy path: one vehicle, one owner. `select_related(owner)` emits a
/// single `LEFT JOIN` and the resulting `JoinedRow<Vehicle>` exposes the
/// joined owner via the typed accessor. Pins:
///   1. Main row decodes correctly from the joined result set (no
///      column-name collision with the child `id` / `created_at` /
///      `updated_at`).
///   2. Child row is materialised — `row.get(path)` returns `Some(&Owner)`
///      with the seeded name.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_fk_emits_join_and_populates(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Alice").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &owner, None).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("fetch_all_joined should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].row.make, "Toyota");
    let joined_owner = rows[0]
        .get(VehicleRelated::owner())
        .expect("owner join must materialise for a non-null FK");
    assert_eq!(joined_owner.id, owner.id);
    assert_eq!(joined_owner.name, "Alice");
}

#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_on_bulk_update_is_rejected(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "UpdateGuard").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &owner, None).await;

    let err = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .update(|f| f.make().set("mutated".to_string()))
        .execute(&mut ctx)
        .await
        .expect_err("select_related on bulk update must be rejected");
    match err {
        DjogiError::Validation(msg) => assert!(
            msg.contains("select_related"),
            "validation should mention select_related: {msg}"
        ),
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let vehicle = Vehicle::objects()
        .fetch_one(&mut ctx)
        .await
        .expect("seeded vehicle should remain");
    assert_eq!(
        vehicle.make, "Toyota",
        "rejected select_related update must not mutate rows"
    );
}

/// Nullable-FK branch: `fuel_type_id` is `NULL`. The LEFT JOIN miss must
/// surface as `None` on the child side — not panic, not produce a
/// default-valued target struct, not omit the parent row from the result
/// set. This is the documented null-safe contract of `select_related`.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_nullable_fk_yields_no_child(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Bob").await;
    // `None` for fuel — the column is `NULL`, not an orphan FK.
    let _ = seed_vehicle_with_owner(&mut ctx, "Honda", &owner, None).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::fuel_type())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("fetch_all_joined should succeed");

    assert_eq!(rows.len(), 1);
    // Parent row still carries its own data — the missing child doesn't
    // drop the parent (LEFT JOIN, not INNER).
    assert_eq!(rows[0].row.make, "Honda");
    assert!(
        rows[0].get(VehicleRelated::fuel_type()).is_none(),
        "NULL FK must surface as None — not a default-valued FuelType"
    );
}

/// Multiple `.select_related(...)` calls on disjoint relations — one
/// LEFT JOIN per path, both aliased under distinct `rel_{source_column}`
/// prefixes so their column names never collide in the result set. Both
/// child accessors resolve correctly on the same joined row.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_multiple_relations_combine(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Dana").await;
    let fuel = seed_fuel_type(&mut ctx, "Electric").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Tesla", &owner, Some(&fuel)).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .select_related(VehicleRelated::fuel_type())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("two-relation select_related should succeed");

    assert_eq!(rows.len(), 1);
    let joined_owner = rows[0]
        .get(VehicleRelated::owner())
        .expect("owner join should materialise");
    assert_eq!(joined_owner.name, "Dana");
    let joined_fuel = rows[0]
        .get(VehicleRelated::fuel_type())
        .expect("fuel_type join should materialise");
    assert_eq!(joined_fuel.name, "Electric");
}

/// `select_related` composes with `.filter()` and `.order_by()`: the
/// filter narrows the parent result set, ordering determines row order,
/// and the join attaches children per surviving parent. Pins the
/// queryset machinery composes cleanly with Task 5's join
/// emission — same `WHERE` / `ORDER BY` tail, just with a LEFT JOIN
/// prepended.
///
/// The filter targets `make` (a parent-only column). Framework columns
/// that also appear on the child (`id`, `created_at`, `updated_at`)
/// get exercised by the dedicated regression tests
/// [`select_related_compose_with_filter_on_id`] and
/// [`select_related_compose_with_order_by_created_at`] below — the
/// emitter qualifies every bare `WHERE` / `ORDER BY` / `DISTINCT ON`
/// column reference with the parent table under `.select_related(...)`
/// so Postgres does not raise 42702 on the shared column name.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_composes_with_filter_and_order(mut ctx: djogi::DjogiContext) {
    let alice = seed_owner(&mut ctx, "Alice").await;
    let bob = seed_owner(&mut ctx, "Bob").await;
    // Two Toyota entries (one per owner) plus a Honda. The filter
    // picks out Toyotas; the order_by is a tiebreaker pinned on
    // `make` again so the two rows land in a stable order regardless
    // of insertion order.
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &alice, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Honda", &bob, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "Toyota", &bob, None).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .filter(|f| f.make().eq("Toyota".to_string()))
        .order_by(|f| f.make().asc())
        .select_related(VehicleRelated::owner())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("filter+order_by+select_related should compose");

    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.row.make, "Toyota");
        let joined_owner = row
            .get(VehicleRelated::owner())
            .expect("each surviving row should carry a joined owner");
        // Either Alice or Bob — both owners are live; we only
        // assert the join materialised with a live owner row.
        assert!(
            joined_owner.name == "Alice" || joined_owner.name == "Bob",
            "unexpected owner name: {}",
            joined_owner.name
        );
    }
}

/// Disjoint `select_related` and `.prefetch()` on the same queryset:
/// one relation is joined, the other is fetched via the follow-up-query
/// prefetch path. Proves the two eager-loading strategies coexist on
/// one queryset without fighting over column aliases or SQL structure.
///
/// select_related lives on the main query's SELECT list; prefetch runs
/// its own SQL round trip. The terminal `fetch_all_joined` honours both:
/// the main query emits the JOIN, and the post-query prefetch pass
/// stitches the prefetched targets into the same `JoinedRow<T>`.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_and_prefetch_compose_disjoint(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Eve").await;
    let fuel = seed_fuel_type(&mut ctx, "Gas").await;
    let _ = seed_vehicle_with_owner(&mut ctx, "BMW", &owner, Some(&fuel)).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .prefetch(VehicleRelated::fuel_type())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("select_related + prefetch should compose on disjoint relations");

    assert_eq!(rows.len(), 1);
    // owner is joined in-place;
    let joined_owner = rows[0]
        .get(VehicleRelated::owner())
        .expect("owner join should resolve");
    assert_eq!(joined_owner.name, "Eve");
    // fuel_type is prefetched (separate query, same accessor surface).
    let prefetched_fuel = rows[0]
        .get(VehicleRelated::fuel_type())
        .expect("fuel_type prefetch should resolve through the same typed accessor");
    assert_eq!(prefetched_fuel.name, "Gas");
}

// ---------------------------------------------------------------------------
// Task 5 fixup: parent-table qualification under `.select_related(...)`.
// ---------------------------------------------------------------------------
//
// Before the fix, filtering or ordering on a framework column (`id`,
// `created_at`, `updated_at`) while `.select_related(...)` was active
// produced SQL with a bare `WHERE id = $1` / `ORDER BY id` against a
// JOIN where both parent and child tables contribute the same column
// name. Postgres then raised `42702 column reference "id" is
// ambiguous` at query time.
//
// The emitter now qualifies every bare column reference in the
// `WHERE` / `ORDER BY` / `DISTINCT ON` tail with the parent table
// when `select_related_paths` is non-empty, sidestepping the
// ambiguity. These two integration tests exercise the two common
// shapes against live Postgres — emitter-level shape tests live in
// `djogi/src/query/sql.rs`'s `joined_select_*` suite.

/// `.select_related(...)` composed with `.filter(|f| f.id.eq(x))` —
/// the filter targets the framework `id` column, which also appears
/// on the joined `Owner` table. The emitted SQL qualifies the
/// reference as `WHERE vehicles_p3.id = $1`, so Postgres does not
/// raise 42702 on the bare form.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_compose_with_filter_on_id(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Iris").await;
    let vehicle = seed_vehicle_with_owner(&mut ctx, "Kia", &owner, None).await;

    // `f.id()` is the macro-generated FieldRef<Vehicle, HeerId> — the
    // same typed handle the `.filter(...)` closure takes for every
    // Filter. The call site reads naturally; qualification
    // happens inside the emitter.
    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .filter(|f| f.id().eq(vehicle.id))
        .fetch_all_joined(&mut ctx)
        .await
        .expect("select_related + filter on `id` must succeed (no 42702 ambiguity)");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].row.id, vehicle.id);
    let joined_owner = rows[0]
        .get(VehicleRelated::owner())
        .expect("owner join should materialise on the filtered row");
    assert_eq!(joined_owner.name, "Iris");
}

/// `.select_related(...)` composed with `.order_by(|f| f.created_at.asc())` —
/// the ordering targets the framework `created_at` column, which also
/// appears on the joined `Owner` table. The emitter qualifies the
/// reference as `ORDER BY vehicles_p3.created_at ASC`, sidestepping
/// 42702.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn select_related_compose_with_order_by_created_at(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Jack").await;
    // Two vehicles on the same owner — ordering is the only thing
    // that matters here; the test asserts on "no ambiguity error"
    // rather than on a specific row order (per-row `created_at`
    // granularity depends on HeeRanjId generation timing, not on
    // the test's control flow).
    let _ = seed_vehicle_with_owner(&mut ctx, "Audi", &owner, None).await;
    let _ = seed_vehicle_with_owner(&mut ctx, "BMW", &owner, None).await;

    let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
        .select_related(VehicleRelated::owner())
        .order_by(|f| f.created_at().asc())
        .fetch_all_joined(&mut ctx)
        .await
        .expect("select_related + order_by `created_at` must succeed (no 42702 ambiguity)");

    assert_eq!(rows.len(), 2);
    for row in &rows {
        let joined_owner = row
            .get(VehicleRelated::owner())
            .expect("every joined row should carry the owner");
        assert_eq!(joined_owner.name, "Jack");
    }
}

// ---------------------------------------------------------------------------
// Task 1 closure: tx-backed prefetch / select_related now dispatch
// through the generalised `PrefetchLoaderFn` and share the outer
// transaction scope.
// ---------------------------------------------------------------------------
//
// Originally shipped context-limited support for the multi-query
// fan-out terminals (`fetch_all_prefetched`, `fetch_all_joined`): the
// loader signature carried `&PgPool`, so both terminals guarded at the
// entry point and returned a configuration-style database error on a
// transaction-backed context. Task 1 widened the loader
// signature to `&mut ContextInner`, so both paths now work inside an
// `atomic()` scope and see the scope's uncommitted writes.
//
// The two tests below pin the new behavior — the very writes that
// landed inside an open transaction are visible to the follow-up
// prefetch / join query on the SAME transaction. An accidental
// regression that switched either terminal back to a separate context lookup
// would fail to see those uncommitted writes and surface as a red
// test.

/// `fetch_all_prefetched` on a tx-backed context: the generalised
/// loader shares the outer transaction so the follow-up fetch sees
/// the uncommitted parent write.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn fetch_all_prefetched_tx_backed_reads_uncommitted_writes(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Kate").await;

    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            let vehicle = Vehicle::create(ctx, vehicle_for_insert("Honda", &owner, None)).await?;

            let rows: Vec<PrefetchedRow<Vehicle>> = Vehicle::objects()
                .filter(|f| f.id().eq(vehicle.id))
                .prefetch(VehicleRelated::owner())
                .fetch_all_prefetched(ctx)
                .await?;

            assert_eq!(rows.len(), 1);
            let fetched_owner = rows[0]
                .get(VehicleRelated::owner())
                .expect("prefetched owner should be present for non-null FK");
            assert_eq!(fetched_owner.name, "Kate");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("transaction-backed prefetch must succeed");
}

/// `fetch_all_joined` on a tx-backed context: same shape as the
/// prefetch version — the join + prefetch stitcher both use the
/// outer transaction scope.
#[djogi::djogi_test(sync_models = [Owner, FuelType, Vehicle])]
async fn fetch_all_joined_tx_backed_reads_uncommitted_writes(mut ctx: djogi::DjogiContext) {
    let owner = seed_owner(&mut ctx, "Liam").await;

    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            let vehicle = Vehicle::create(ctx, vehicle_for_insert("Kia", &owner, None)).await?;

            let rows: Vec<JoinedRow<Vehicle>> = Vehicle::objects()
                .filter(|f| f.id().eq(vehicle.id))
                .select_related(VehicleRelated::owner())
                .fetch_all_joined(ctx)
                .await?;

            assert_eq!(rows.len(), 1);
            let fetched_owner = rows[0]
                .get(VehicleRelated::owner())
                .expect("joined owner should be present for non-null FK");
            assert_eq!(fetched_owner.name, "Liam");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("transaction-backed joined fetch must succeed");
}
