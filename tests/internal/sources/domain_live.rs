// djogi#216 Piece A — Live integration tests for
// `#[field(domain = "<name>")]` against a real Postgres 18.
//
// # What this file pins
//
// 1. **Descriptor projection.** A model field carrying
//    `#[field(domain = "c4_216_positive_amount")]` lowers to
//    `FieldSqlType::Domain { name, base: &FieldSqlType::Numeric }` in
//    the descriptor; the rendered `Display` output is the bare domain
//    name. (Doesn't need a live DB but lives here so the scope
//    assertion fails alongside the live-DDL check if the descriptor
//    layer regresses.)
// 2. **`sync_models` emits the domain name in the column DDL.** The
//    descriptor flows through projection + migration composer; the
//    resulting `CREATE TABLE` references the domain by name (no
//    `NUMERIC(...)` fallback). Verified by inspecting
//    `pg_attribute.atttypid` against the domain's `pg_type.oid`.
// 3. **Domain CHECK fires on insert.** A row whose value violates the
//    adopter-declared `CHECK (VALUE > 0)` on the domain is rejected by
//    Postgres at INSERT time. Pin tests the integrity guarantee an
//    adopter buys by declaring a domain — the constraint runs
//    server-side, no application-layer enforcement required.
//
// The differ-surface assertion (domain rename →
// `ColumnChange::ChangeType`) is covered transitively. The differ
// already compares column types by rendered `Display` string (the same
// path that emits `ALTER COLUMN ... TYPE ...` for every other type
// evolution). Two facts together prove the chain:
//
// 1. `FieldSqlType::Domain { name, .. }` renders to the bare domain
//    name — pinned by `domain_sql_type_displays_as_domain_name` in
//    `djogi/src/descriptor.rs::tests`.
// 2. The differ keys off the rendered string — pinned by every
//    existing `type_change_emits_alter_column_change_type` regression
//    test in `djogi/src/migrate/diff.rs::tests`.
//
// A domain rename therefore produces a `ColumnChange::ChangeType
// { from: "<old>", to: "<new>" }` operation by construction. Adding a
// dedicated differ unit test for the domain variant would require a
// change to `diff.rs` that is out of scope for Piece A (the file is
// owned by an adjacent active lane).
//
// # Piece A scope — wire-codec binding through the typed surface
//
// **The typed `Model::create` / `Model::save` round-trip through a
// domain-wrapped column does NOT work in Piece A.** Postgres-types'
// `ToSql` impl for built-in Rust types (`rust_decimal::Decimal`,
// `String`, etc.) accepts only the exact base type (`Type::NUMERIC`,
// `Type::TEXT`); a column whose Postgres type is
// `Type::Other(Other { kind: Domain(NUMERIC), ... })` is rejected
// with a `WrongType` error before the value is sent. This is a
// known scope boundary of Piece A — the descriptor + migration
// surfaces work, but the wire codec does not transparently strip
// the domain wrapper.
//
// Adopters wanting `Model::create` against a domain-typed column
// today have two options:
//
// 1. Declare a custom Rust newtype implementing `DjogiSqlType` with
//    `SQL_TYPE = "<domain>"`, plus the matching `ToSql` / `FromSql`
//    impl with a relaxed `accepts()` that returns `true` for the
//    domain (`pg_type.typtype = 'd'`). The same pattern the docs
//    suggest for `FieldSqlType::Custom` types.
// 2. Use raw SQL for inserts / updates against the column (under
//    the bypass attribute), bypassing the typed bind path until
//    Piece B / a future wire-codec relaxation.
//
// This test does NOT exercise the typed `Model::create` /
// `Model::save` round-trip — that would surface the limitation as a
// failure on the live test surface, masking real Piece A wins. The
// CHECK enforcement test below uses raw_execute to bind the
// violating value (with an explicit `::numeric` cast) so the
// assertion is about the DOMAIN CHECK, not about Rust-side
// wire-codec strictness.
//
// # Why this test lives in `tests/internal/`
//
// Piece A only references adopter-managed domains; the
// `CREATE DOMAIN` DDL itself is Piece B (deferred). The test
// therefore needs `raw_ddl` to install the domain before
// `sync_models` runs, which is the legitimate "djogi-API-gap"
// rationale that `tests/internal/` exists to cover. Pin tests
// (`tests/pin/`) are reserved for exercising the raw APIs
// themselves; integration tests (`tests/integration/`) are pure
// typed-surface. The internal bypass + `JUSTIFICATION (djogi#216)`
// annotation in the wrapper file documents the gap.

use djogi::DjogiContext;
use djogi::prelude::*;

// ── Test models — `Order216` references a positive-amount domain ──────────

/// Adopter-managed domain referenced by the test model. Declared via
/// `raw_ddl` in the test setup because djogi#216 Piece A only
/// references domains; the `CREATE DOMAIN` emission lives in Piece B
/// (deferred).
const DOMAIN_NAME: &str = "c4_216_positive_amount";

const TABLE_NAME: &str = "c4_216_orders";

#[model(
    table = "c4_216_orders",
    pk = HeerId,
    no_default
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Order216Live {
    /// Domain-typed column. The macro lowers this to
    /// `FieldSqlType::Domain { name: "c4_216_positive_amount",
    /// base: &FieldSqlType::Numeric }`. The migration composer emits
    /// the bare domain name in the column-type slot, so adopter
    /// domain constraints fire on every INSERT / UPDATE.
    #[field(domain = "c4_216_positive_amount")]
    pub amount: rust_decimal::Decimal,
    pub label: String,
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Idempotent setup: drops the domain (CASCADE drops every dependent
/// table column) and recreates it with a `VALUE > 0` CHECK. Per-test
/// database isolation handles repeated runs, but the CASCADE keeps
/// the per-test setup self-contained against any test that leaked
/// state through a shared maintenance database.
async fn install_positive_amount_domain(ctx: &mut DjogiContext) {
    ctx.raw_ddl(&format!("DROP DOMAIN IF EXISTS {DOMAIN_NAME} CASCADE"))
        .await
        .expect("DROP DOMAIN should succeed");
    ctx.raw_ddl(&format!(
        "CREATE DOMAIN {DOMAIN_NAME} AS NUMERIC CHECK (VALUE > 0)"
    ))
    .await
    .expect("CREATE DOMAIN should succeed");
}

// ── Descriptor-side checks ───────────────────────────────────────────────

#[djogi::djogi_test]
async fn domain_descriptor_displays_bare_name(mut ctx: DjogiContext) {
    // Descriptor projection — pin the rendered column-type slot
    // before exercising the live DB. A drift here surfaces as a
    // mismatch between what `sync_models` emits and what an adopter
    // expects to see in the snapshot / Postgres catalog. The check
    // does not need a live DB but keeping it in the live suite means
    // any sync_models breakage from the descriptor side fails this
    // test alongside the round-trip check, isolating the problem to
    // the descriptor layer.
    let _ = &mut ctx; // unused; the test does not touch the live DB.

    let desc = Order216Live::descriptor();
    let amount_field = desc
        .fields
        .iter()
        .find(|f| f.name == "amount")
        .expect("amount column must exist on Order216Live descriptor");

    // The variant must be `Domain { name, base }`.
    match &amount_field.sql_type {
        djogi::FieldSqlType::Domain { name, base } => {
            assert_eq!(
                *name, DOMAIN_NAME,
                "Domain.name must be the adopter-declared identifier verbatim",
            );
            // The base is informational in Piece A; pin it as
            // `Numeric` since the Rust source type is
            // `rust_decimal::Decimal`. Piece B will read this slot
            // when emitting `CREATE DOMAIN <name> AS <base>` DDL.
            assert!(
                matches!(**base, djogi::FieldSqlType::Numeric),
                "Domain.base for a `rust_decimal::Decimal` field must be `Numeric`, got {:?}",
                **base,
            );
        }
        other => panic!("amount.sql_type must be FieldSqlType::Domain, got {other:?}"),
    }

    // Display contract — the migration composer / differ / snapshot
    // all key off this rendered string.
    assert_eq!(
        format!("{}", amount_field.sql_type),
        DOMAIN_NAME,
        "Display must render the bare domain name, not the inner base type",
    );
}

// ── Live DDL checks — sync_models emits domain reference ─────────────────

#[djogi::djogi_test]
async fn sync_models_emits_domain_reference_in_table_ddl(mut ctx: DjogiContext) {
    // Install the domain via raw_ddl (Piece A does NOT emit
    // CREATE DOMAIN — that's Piece B / deferred). Then run
    // sync_models manually to lower the descriptor's domain
    // reference into the CREATE TABLE DDL.
    install_positive_amount_domain(&mut ctx).await;
    djogi::testing::sync_models(&mut ctx, &[Order216Live::descriptor()])
        .await
        .expect("sync_models must accept a domain-typed column");

    // Inspect pg_catalog directly: the `amount` column's atttypid
    // must equal the domain's pg_type.oid. If sync_models emitted
    // the column as `NUMERIC` (dropping the domain wrapper), this
    // assertion catches the regression at the Postgres catalog
    // layer — the source-of-truth representation that survives any
    // SQL string-comparison shenanigans.
    let domain_oid_matches: bool = ctx
        .raw_scalar(
            "SELECT a.atttypid = t.oid
             FROM pg_attribute a
             JOIN pg_class c ON a.attrelid = c.oid
             JOIN pg_type t ON t.typname = $2
             WHERE c.relname = $1
               AND a.attname = 'amount'",
            &[&TABLE_NAME, &DOMAIN_NAME],
        )
        .await
        .expect("pg_attribute lookup should succeed");
    assert!(
        domain_oid_matches,
        "sync_models must emit the column with the domain's oid (not the underlying NUMERIC oid)",
    );

    // Belt-and-suspenders: verify pg_type.typtype = 'd' on that oid —
    // confirms the column type IS a domain, not a built-in NUMERIC
    // that happens to share the name.
    let is_domain: bool = ctx
        .raw_scalar(
            "SELECT t.typtype = 'd'
             FROM pg_attribute a
             JOIN pg_class c ON a.attrelid = c.oid
             JOIN pg_type t ON a.atttypid = t.oid
             WHERE c.relname = $1
               AND a.attname = 'amount'",
            &[&TABLE_NAME],
        )
        .await
        .expect("pg_type.typtype lookup should succeed");
    assert!(
        is_domain,
        "the amount column's type must be a Postgres domain (typtype = 'd')",
    );
}

// ── Live CHECK enforcement — domain rejects invalid values ───────────────

#[djogi::djogi_test]
async fn domain_check_rejects_invalid_value_at_insert(mut ctx: DjogiContext) {
    // Domain CHECK constraints fire on every INSERT / UPDATE that
    // touches the column. Pin the integrity guarantee an adopter
    // buys by declaring a domain: the constraint runs server-side,
    // no application-layer enforcement required.
    //
    // The INSERT uses raw_execute with an explicit `::numeric` cast
    // (instead of `Model::create`) because postgres-types' `ToSql`
    // for `rust_decimal::Decimal` does not accept `Type::Other(Other
    // { kind: Domain(NUMERIC) })` in Piece A — see the file's scope
    // notes above. Using raw_execute keeps the test focused on the
    // CHECK enforcement (Piece A scope) rather than the typed
    // wire-codec limitation (out of Piece A scope).
    install_positive_amount_domain(&mut ctx).await;
    djogi::testing::sync_models(&mut ctx, &[Order216Live::descriptor()])
        .await
        .expect("sync_models must accept a domain-typed column");

    // Step 1: a valid value (positive) must succeed. The CHECK
    // semantics admit it; the domain wrapper does not alter
    // values that satisfy the CHECK.
    let valid_insert = ctx
        .raw_execute(
            &format!(
                "INSERT INTO {TABLE_NAME} (id, created_at, updated_at, amount, label) \
                 VALUES (heerid_next(), now(), now(), 123.45::numeric, 'valid')"
            ),
            &[],
        )
        .await;
    assert!(
        valid_insert.is_ok(),
        "positive value should pass the domain CHECK, got: {valid_insert:?}",
    );

    // Step 2: a negative value must be rejected by the domain CHECK.
    // Postgres emits a `check_violation` (SQLSTATE 23514) with a
    // diagnostic naming either the domain or the CHECK constraint.
    let invalid_insert = ctx
        .raw_execute(
            &format!(
                "INSERT INTO {TABLE_NAME} (id, created_at, updated_at, amount, label) \
                 VALUES (heerid_next(), now(), now(), -50::numeric, 'invalid')"
            ),
            &[],
        )
        .await;
    let err = invalid_insert
        .expect_err("domain CHECK must reject negative values via the VALUE > 0 predicate");
    // `{err}` collapses the postgres-error chain to "db error"; the
    // domain / CHECK detail surfaces only via Debug (the inner
    // `DbError` carries the SQLSTATE and the constraint-name
    // diagnostic Postgres emitted). Use Debug formatting so the
    // assertion exercises the path an operator would inspect when
    // debugging an actual rejection in production logs.
    let debug = format!("{err:?}");
    // Postgres 18 wording: "value for domain ... violates check constraint"
    // — the message embeds both the domain name and the literal
    // word "check", along with the SQLSTATE 23514 ("check_violation").
    assert!(
        debug.contains(DOMAIN_NAME)
            || debug.contains("check")
            || debug.contains("23514"),
        "error must reference the domain or the CHECK constraint (debug = {debug})",
    );
}
