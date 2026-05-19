// Phase 8.5 Cluster 2 issue #189 — opt-in HeerId / RanjId structural CHECK.
//
// # What this file pins
//
// 1. **Default-off (compile pass).** A HeerId-PK model and a RanjId-PK model
//    without `#[model(strict_ids)]` / `#[field(strict_id_check)]` compile
//    cleanly and project NO extra CHECK on the framework `id` column.
//    Verified via the pg_constraint catalog: no `<table>_id_check` constraint
//    is emitted.
//
// 2. **Model-wide opt-in (catalog assertion).** A HeerId-PK model with
//    `#[model(strict_ids)]` projects `CHECK ("id" >= 0)` on the id column,
//    landing in pg_constraint as `<table>_id_check`. Same applies to a
//    RanjId-PK model with `#[model(strict_ids)]` — the projected CHECK
//    enforces UUIDv8 + RFC 4122 variant via `pg_catalog.substring` calls
//    against the canonical text form.
//
// 3. **Field-level opt-in (catalog assertion).** Applying
//    `#[field(strict_id_check)]` to a single bare HeerId field on an
//    otherwise-unprotected model projects the CHECK on exactly that column.
//
// 4. **FK propagation under model-wide opt-in.** When `#[model(strict_ids)]`
//    is on, every FK column gets the strict CHECK applied against the
//    resolved target PK type (BIGINT for HeerId-PK target; the projection
//    silently skips Serial-PK targets — not tested here since Serial PK is
//    rare in the HeerId-default world).
//
// 5. **OOB rejection (raw bypass).** With opt-in on a HeerId column, a raw
//    `INSERT … VALUES (-1, …)` is rejected by the CHECK. With opt-in on a
//    RanjId column, a raw `INSERT … VALUES ('00000000-0000-4000-8000-…', …)`
//    (UUIDv4) is rejected by the CHECK. Both rejection paths cite the
//    `<table>_<column>_check` constraint name in the error.
//
// 6. **Round-trip preservation.** A valid HeerId / RanjId round-trips
//    cleanly through `Model::create` + `Model::get` even with strict checks
//    enabled — the CHECK does not change typed-surface behaviour for
//    well-formed IDs.
//
// # Why a tests/internal target
//
// Point (5) requires constructing Postgres values outside the type's
// representable range (negative i64 for HeerId, UUIDv4 / nil UUID for
// RanjId). These are unreachable through the typed surface — `HeerId::from_i64`
// rejects every negative, and `RanjId::from_uuid` rejects every non-v8 /
// non-RFC4122 UUID. The only way to exercise OOB write rejection is via
// `raw_execute`, which lives behind the bypass attribute. Tests that reach
// for raw access live under `tests/internal/`; adopter-shaped integration
// tests stay raw-free per CLAUDE.md.
//
// # Spec anchors
//
// - GH #189 — opt-in HeerId / RanjId structural CHECK.
// - `docs/spec/migrations.md` §10.6.3 — Opt-in HeerId / RanjId Structural
//   CHECK (djogi#189).
// - `docs/spec/decisions.md` — "HeerId / RanjId structural CHECK
//   (djogi#189)".
// - `djogi/src/migrate/projection.rs::strict_id_check_expr` — the helper.

use djogi::prelude::*;

// ── (1) Default-off model — should carry no strict-ID CHECK ──────────────────

#[model(table = "phase8_5_c2_189_default_off", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189DefaultOff {
    pub label: String,
}

// ── (2) Model-wide opt-in on a HeerId-PK model ───────────────────────────────

#[model(
    table = "phase8_5_c2_189_strict_heer",
    pk = HeerId,
    strict_ids,
    no_default,
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189StrictHeer {
    pub label: String,
}

// ── (3) Model-wide opt-in on a RanjId-PK model ───────────────────────────────

#[model(
    table = "phase8_5_c2_189_strict_ranj",
    pk = RanjId,
    strict_ids,
    no_default,
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189StrictRanj {
    pub label: String,
}

// ── (4) Field-level opt-in on a bare HeerId user column ──────────────────────

#[model(table = "phase8_5_c2_189_field_optin", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189FieldOptin {
    /// Explicit per-field opt-in — equivalent to `#[model(strict_ids)]`
    /// but scoped to this one column. Hardens externally-written
    /// reference IDs without affecting the rest of the table.
    #[field(strict_id_check)]
    pub external_owner: ::djogi::types::HeerId,
    pub label: String,
}

// ── (5) FK propagation under model-wide opt-in ───────────────────────────────

#[model(table = "phase8_5_c2_189_fk_target", pk = HeerId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189FkTarget {
    pub label: String,
}

#[model(
    table = "phase8_5_c2_189_fk_source",
    pk = HeerId,
    strict_ids,
    no_default,
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189FkSource {
    /// FK to a HeerId-PK target. `#[model(strict_ids)]` propagates the
    /// opt-in to every FK column; the projection resolves the FK
    /// target's semantic family to HeerId and emits the structural
    /// CHECK. Column name is `owner_id` per the djogi convention
    /// (`{target}_id` for FK columns; `Related::owner()` method strips
    /// the suffix).
    pub owner_id: ForeignKey<Phase85C2189FkTarget>,
    pub label: String,
}

// ── (6) Custom-PK target — FK propagation must skip the CHECK ────────────────
//
// djogi#189 (post-review hardening): a `PkType::Custom { sql_type: "BIGINT", .. }`
// PK shares the SQL carrier with HeerId but is NOT a HeerRanjID identifier.
// `#[model(strict_ids)]` on a model whose FK targets such a Custom PK must
// NOT emit `col >= 0` against the FK column — that would constrain the
// adopter's custom ID value domain at the DB layer without their consent.
// The projection layer's family-based dispatch (`type_to_pk_family`)
// catches this case and silently skips the CHECK.

// `Phase85C2189CustomBigintId` — custom BIGINT-shaped PK simulating an
// adopter Snowflake-style or app-scoped ID. `default_sql` is the Postgres
// builtin `txid_current()` (returns `BIGINT`, exists in every Postgres
// install Djogi targets) so `sync_models` can CREATE TABLE without
// depending on an adopter-installed generator function — this test only
// inspects the catalog, never INSERTs, so duplicate `txid_current()`
// values across rows are irrelevant. The production adopter would
// supply a real bulk-allocator here.
djogi::primary_key! {
    pub struct Phase85C2189CustomBigintId(i64);
    sql_type = "BIGINT";
    default_sql = "txid_current()";
}

#[model(table = "phase8_5_c2_189_custom_bigint_target", pk = Phase85C2189CustomBigintId, no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189CustomBigintTarget {
    pub label: String,
}

#[model(
    table = "phase8_5_c2_189_custom_bigint_fk_source",
    pk = HeerId,
    strict_ids,
    no_default,
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Phase85C2189CustomBigintFkSource {
    /// FK to a Custom-BIGINT PK target. `#[model(strict_ids)]`
    /// propagates the opt-in flag to this FK at the macro layer, but
    /// the projection layer's family-based dispatch resolves the
    /// target's PK family to `StrictIdFamily::None` (Custom — not
    /// HeerRanjID) and silently skips the CHECK. The FK reference
    /// itself still works (the column SQL type is BIGINT, matching the
    /// target's PK).
    pub owner_id: ForeignKey<Phase85C2189CustomBigintTarget>,
    pub label: String,
}

// ── Catalog assertions — default-off baseline ─────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189DefaultOff])]
async fn default_off_emits_no_strict_id_check(mut ctx: djogi::DjogiContext) {
    // Pre-189 backward-compat invariant: a HeerId-PK model without any
    // opt-in carries no CHECK on the `id` column. The pg_constraint
    // catalog walk filters to CHECK constraints whose name follows the
    // `<table>_id_check` shape used by `migrate::sql::check_constraint_name`.
    let id_checks: Vec<String> = ctx
        .raw_rows(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_default_off'::regclass \
             AND contype = 'c' \
             AND conname LIKE 'phase8_5_c2_189_default_off_id%' \
             ORDER BY conname",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert!(
        id_checks.is_empty(),
        "default-off model must not project a strict-ID CHECK on id; found: {id_checks:?}"
    );
}

// ── Catalog assertions — model-wide opt-in ────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189StrictHeer])]
async fn strict_heer_id_has_nonneg_check(mut ctx: djogi::DjogiContext) {
    // The model-wide `#[model(strict_ids)]` sets `strict_id_check: true`
    // on the framework `id` column. The projection layer resolves
    // FieldSqlType::BigInt → "BIGINT" and emits `<col> >= 0`.
    let defs: Vec<String> = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(oid)::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_strict_heer'::regclass \
             AND contype = 'c' \
             AND conname = 'phase8_5_c2_189_strict_heer_id_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert_eq!(
        defs.len(),
        1,
        "strict_ids HeerId model must project exactly one id_check constraint; got: {defs:?}"
    );
    let def = &defs[0];
    assert!(
        def.contains(">= 0"),
        "HeerId structural CHECK must constrain id to non-negative: {def}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2189StrictRanj])]
async fn strict_ranj_id_has_uuidv8_check(mut ctx: djogi::DjogiContext) {
    // RanjId is UUIDv8 + RFC 4122 variant. The projection emits a CHECK
    // that extracts position 15 (version nibble) and position 20
    // (variant high nibble) from the canonical text form via
    // `pg_catalog.substring` and constrains them to `'8'` and
    // `('8','9','a','b')` respectively.
    let defs: Vec<String> = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(oid)::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_strict_ranj'::regclass \
             AND contype = 'c' \
             AND conname = 'phase8_5_c2_189_strict_ranj_id_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert_eq!(
        defs.len(),
        1,
        "strict_ids RanjId model must project exactly one id_check constraint; got: {defs:?}"
    );
    let def = &defs[0];
    // Postgres's `pg_get_constraintdef` rewrites the source SQL —
    // `pg_catalog.substring(...)` becomes bare `"substring"(...)`
    // (the catalog qualification is dropped because `pg_catalog` is
    // implicit), and `IN (a, b, ...)` becomes `= ANY (ARRAY[...])`.
    // The semantics are unchanged; the assertions match the
    // post-normalisation rendering.
    assert!(
        def.contains("substring") && def.contains(", 15, 1)"),
        "RanjId structural CHECK must extract the version nibble at position 15: {def}"
    );
    assert!(
        def.contains(", 20, 1)"),
        "RanjId structural CHECK must extract the variant nibble at position 20: {def}"
    );
    assert!(
        def.contains("= '8'"),
        "RanjId structural CHECK must constrain the version nibble to '8' (UUIDv8): {def}"
    );
    // Variant check — the RFC 4122 variant nibble must be one of {8, 9, a, b}.
    // Postgres renders `IN (...)` as `= ANY (ARRAY[...])`; both spellings are
    // semantically identical so accept either.
    assert!(
        def.contains("'8'") && def.contains("'9'") && def.contains("'a'") && def.contains("'b'"),
        "RanjId structural CHECK must constrain the variant nibble to RFC 4122 ('8','9','a','b'): {def}"
    );
}

// ── Catalog assertions — field-level opt-in ───────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189FieldOptin])]
async fn field_optin_emits_check_only_on_marked_column(mut ctx: djogi::DjogiContext) {
    // `#[field(strict_id_check)]` on `external_owner` (HeerId) must
    // project a `<table>_external_owner_check`. The `id` column (also
    // HeerId, but without opt-in) gets no CHECK.
    let constraint_names: Vec<String> = ctx
        .raw_rows(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_field_optin'::regclass \
             AND contype = 'c' \
             ORDER BY conname",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert!(
        constraint_names.contains(&"phase8_5_c2_189_field_optin_external_owner_check".to_string()),
        "field-level opt-in must emit CHECK on `external_owner`; got: {constraint_names:?}"
    );
    assert!(
        !constraint_names.contains(&"phase8_5_c2_189_field_optin_id_check".to_string()),
        "field-level opt-in must NOT emit CHECK on the unmarked `id` column; got: {constraint_names:?}"
    );
}

// ── Catalog assertions — FK propagation ───────────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189FkTarget, Phase85C2189FkSource])]
async fn fk_propagation_emits_check_on_fk_column(mut ctx: djogi::DjogiContext) {
    // The fk-source model has `#[model(strict_ids)]`. Every FK column
    // gets the opt-in flag at descriptor emit time; the projection
    // resolves `owner` (FK to a HeerId-PK target) to BIGINT and emits
    // the HeerId structural CHECK.
    let defs: Vec<String> = ctx
        .raw_rows(
            "SELECT pg_get_constraintdef(oid)::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_fk_source'::regclass \
             AND contype = 'c' \
             AND conname = 'phase8_5_c2_189_fk_source_owner_id_check'",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert_eq!(
        defs.len(),
        1,
        "FK column on a strict_ids model must project the HeerId CHECK; got: {defs:?}"
    );
    let def = &defs[0];
    assert!(
        def.contains(">= 0"),
        "FK CHECK must use the HeerId structural bound for BIGINT-resolved targets: {def}"
    );
}

// ── Round-trip — valid IDs survive the CHECK ──────────────────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189StrictHeer])]
async fn strict_heer_id_valid_round_trip(mut ctx: djogi::DjogiContext) {
    // A valid HeerId (non-negative BIGINT) round-trips cleanly through
    // the typed surface even with `strict_id_check` on the `id` column.
    // The CHECK doesn't change behaviour for well-formed IDs.
    let row = Phase85C2189StrictHeer::create(
        &mut ctx,
        Phase85C2189StrictHeer {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            label: "valid".into(),
        },
    )
    .await
    .expect("create with valid HeerId must succeed");
    assert!(row.id.as_i64() > 0, "DB-generated HeerId must be positive");

    let fetched = Phase85C2189StrictHeer::get(&mut ctx, row.id)
        .await
        .expect("get with valid HeerId must succeed");
    assert_eq!(fetched.label, "valid");
}

#[djogi::djogi_test(sync_models = [Phase85C2189StrictRanj])]
async fn strict_ranj_id_valid_round_trip(mut ctx: djogi::DjogiContext) {
    let row = Phase85C2189StrictRanj::create(
        &mut ctx,
        Phase85C2189StrictRanj {
            id: <::djogi::types::RanjId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            label: "valid".into(),
        },
    )
    .await
    .expect("create with valid RanjId must succeed");

    let fetched = Phase85C2189StrictRanj::get(&mut ctx, row.id)
        .await
        .expect("get with valid RanjId must succeed");
    assert_eq!(fetched.label, "valid");
}

// ── OOB rejection (raw bypass) — negative HeerId rejected ─────────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189StrictHeer])]
async fn strict_heer_check_rejects_negative_id(mut ctx: djogi::DjogiContext) {
    // -1 is an externally-injected garbage HeerId — bit 63 = 1 (i.e.
    // the i64 carrier is negative). `HeerId::from_i64` would reject
    // this on the typed-decode side; the CHECK rejects it at the DB
    // layer so the bad row never lands. Raw SQL is required because
    // `Model::create` would reject the value before it hits Postgres.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_189_strict_heer (id, created_at, updated_at, label) \
             VALUES (-1, now(), now(), 'evil')",
            &[],
        )
        .await
        .expect_err("negative BIGINT must be rejected by the strict HeerId CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_189_strict_heer_id_check"),
        "rejection must cite the strict_id_check constraint: {msg}"
    );
}

// ── OOB rejection (raw bypass) — non-UUIDv8 RanjId rejected ──────────────────

#[djogi::djogi_test(sync_models = [Phase85C2189StrictRanj])]
async fn strict_ranj_check_rejects_uuid_v4(mut ctx: djogi::DjogiContext) {
    // A UUIDv4 carries version=4 at the version nibble, which the
    // strict RanjId CHECK rejects (it requires version=8). The
    // version nibble of the chosen literal (`4`) is the first char
    // of the third hyphen-separated group, position 15 in the
    // canonical text form.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_189_strict_ranj (id, created_at, updated_at, label) \
             VALUES ('00000000-0000-4000-8000-000000000000', now(), now(), 'v4')",
            &[],
        )
        .await
        .expect_err("UUIDv4 must be rejected by the strict RanjId version-nibble CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_189_strict_ranj_id_check"),
        "rejection must cite the strict_id_check constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2189StrictRanj])]
async fn strict_ranj_check_rejects_uuid_v7(mut ctx: djogi::DjogiContext) {
    // UUIDv7 is also time-ordered but is a distinct standard. RanjId
    // requires version=8 (UUIDv8 with the HeeRanjID-specific layout).
    // Mirrors the `ranjid_rejects_uuid_v7` test in HeeRanjID's own
    // suite — closes the same external-writer hole at the DB layer.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_189_strict_ranj (id, created_at, updated_at, label) \
             VALUES ('00000000-0000-7000-8000-000000000000', now(), now(), 'v7')",
            &[],
        )
        .await
        .expect_err("UUIDv7 must be rejected by the strict RanjId version-nibble CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_189_strict_ranj_id_check"),
        "rejection must cite the strict_id_check constraint: {msg}"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2189StrictRanj])]
async fn strict_ranj_check_rejects_non_rfc4122_variant(mut ctx: djogi::DjogiContext) {
    // A UUID with version=8 but variant high bits != 10 is structurally
    // invalid — `RanjId::from_uuid` would reject it. The CHECK's variant
    // clause (position 20 must be in {8, 9, a, b}) catches this case at
    // the DB layer. Variant nibble of '0' at position 20 means high
    // bits = 00 (NCS variant), not RFC 4122.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_189_strict_ranj (id, created_at, updated_at, label) \
             VALUES ('00000000-0000-8000-0000-000000000000', now(), now(), 'wrong-variant')",
            &[],
        )
        .await
        .expect_err("non-RFC4122 variant must be rejected by the strict RanjId variant CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_189_strict_ranj_id_check"),
        "rejection must cite the strict_id_check constraint: {msg}"
    );
}

// ── OOB rejection (raw bypass) — FK column propagation ────────────────────────

// ── Catalog assertion — Custom-PK target FK skip ─────────────────────────────

#[djogi::djogi_test(
    sync_models = [Phase85C2189CustomBigintTarget, Phase85C2189CustomBigintFkSource]
)]
async fn fk_to_custom_bigint_target_skips_strict_check(mut ctx: djogi::DjogiContext) {
    // djogi#189 (post-review hardening). The source model has
    // `#[model(strict_ids)]` and an FK to a Custom-BIGINT PK target.
    // The macro propagates the opt-in flag to the FK descriptor (it
    // cannot inspect the target's PK family at parse time), but the
    // projection layer resolves the family to `StrictIdFamily::None`
    // (Custom) and emits NO CHECK on the FK column. The catalog walk
    // confirms the constraint does not exist.
    //
    // The `sync_models` macro stubs the `phase8_5_c2_189_custom_bigint_id_next`
    // function via the typed surface; we only assert the constraint
    // catalog state here, not the function existence.
    let id_checks: Vec<String> = ctx
        .raw_rows(
            "SELECT conname::text FROM pg_constraint \
             WHERE conrelid = 'phase8_5_c2_189_custom_bigint_fk_source'::regclass \
             AND contype = 'c' \
             AND conname LIKE 'phase8_5_c2_189_custom_bigint_fk_source_owner_id%' \
             ORDER BY conname",
            &[],
        )
        .await
        .expect("catalog query must succeed")
        .into_iter()
        .map(|row| row.try_get::<_, String>(0).unwrap())
        .collect();
    assert!(
        id_checks.is_empty(),
        "FK to a Custom-BIGINT PK target must NOT carry the strict-ID CHECK \
         (Custom is not a HeerRanjID family); found: {id_checks:?}"
    );

    // The FK column itself must still exist and target the correct table
    // with the correct SQL type (BIGINT, inherited from Custom.sql_type).
    let col_type: String = ctx
        .raw_rows(
            "SELECT format_type(atttypid, atttypmod)::text \
             FROM pg_attribute \
             WHERE attrelid = 'phase8_5_c2_189_custom_bigint_fk_source'::regclass \
             AND attname = 'owner_id'",
            &[],
        )
        .await
        .expect("attribute query must succeed")
        .into_iter()
        .next()
        .expect("owner_id column must exist")
        .try_get::<_, String>(0)
        .unwrap();
    assert_eq!(
        col_type, "bigint",
        "FK column SQL type must still inherit from Custom.sql_type; \
         the family-based skip applies only to the strict-ID CHECK"
    );
}

#[djogi::djogi_test(sync_models = [Phase85C2189FkTarget, Phase85C2189FkSource])]
async fn strict_fk_rejects_negative_owner_id(mut ctx: djogi::DjogiContext) {
    // Negative BIGINT into the FK column. The FK reference is satisfied
    // by inserting a parent row first, but the FK column carries the
    // strict HeerId CHECK from `#[model(strict_ids)]`. Postgres
    // evaluates CHECK constraints before FK constraints, so the
    // structural CHECK fires first — exactly the protection we want
    // against externally-injected malformed IDs that happen to also
    // collide with a real parent ID.
    //
    // Note: we use a value that is structurally invalid AND has no
    // matching parent row. The CHECK fires first; if it didn't, the FK
    // would. Either way the bad row is rejected, but the assertion
    // verifies the CHECK is the proximate cause.
    let err = ctx
        .raw_execute(
            "INSERT INTO phase8_5_c2_189_fk_source (id, created_at, updated_at, owner_id, label) \
             VALUES (1, now(), now(), -1, 'evil')",
            &[],
        )
        .await
        .expect_err("negative FK BIGINT must be rejected by the strict HeerId CHECK");

    let msg = format!("{err:?}");
    assert!(
        msg.contains("phase8_5_c2_189_fk_source_owner_id_check"),
        "rejection must cite the FK column's strict_id_check constraint: {msg}"
    );
}
