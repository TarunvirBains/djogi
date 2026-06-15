//! Adopter-crate isolation fixture for macro path-routing.
//!
//! See `../Cargo.toml` for the why-this-is-its-own-crate explanation.
//! In short: this crate's `[dependencies]` table contains exactly one
//! entry — `djogi` — so any macro-emitted path that resolves against
//! `::sassi::*` / `::heeranjid::*` / `::time::*` / `::uuid::*` /
//! `::inventory::*` / `::serde::*` / `::tokio::*` / `::tokio_postgres::*` /
//! `::postgres_types::*` / `::bytes::*` / `::futures::*` directly fails
//! to compile here with E0433.
//!
//! # What this fixture exercises
//!
//! The `#[model]` annotations below run the full `model::expand`
//! pipeline (inject, descriptor, crud, stubs, filter, from_row,
//! cacheable, ...). Each pass emits its own paths through
//! `::djogi::__private::*` / `::djogi::types::*` / `::djogi::cache::*`
//! / `::djogi::query::*` / `::djogi::SassiBootHook` / etc. A future
//! regression that introduces a stray `::sassi::*` / `::heeranjid::*` /
//! `::time::*` etc. into any model emission pass exercised here
//! surfaces here — not in the ordinary
//! `djogi-macros/tests/compile_pass/*.rs` lihaaf fixtures, because
//! `djogi-macros/Cargo.toml` lists those crates as `[dev-dependencies]`
//! so lihaaf's compile_pass bucket compiles against a richer dep
//! graph than a real adopter has.
//!
//! The fixture also invokes `djogi::primary_key!` and
//! `#[derive(DjogiEnum)]`, because those macros emit their own
//! `postgres_types` / `bytes` / `inventory` / `serde` paths outside the
//! `#[model]` pipeline. `cargo check --all-targets` also compiles the
//! `#[djogi::djogi_test]` function below, so the test-harness macro's
//! `tokio` / `futures` paths are covered without running a database
//! test.
//!
//! # The two model shapes
//!
//! - `DefaultRow` exercises the default-`updated_at` watermark branch
//!   of `model::cacheable::expand` (no `watermark_field = ...` on
//!   `#[model]`). The emitted `DeltaSyncCacheable::Watermark` resolves
//!   to the framework-injected `updated_at: ::djogi::types::DateTime`
//!   field.
//!
//! - `WatermarkedRow` exercises the explicit-watermark branch with a
//!   user-declared field (`expires_at: DateTime`). The emitted
//!   `DeltaSyncCacheable::Watermark` resolves to the user-declared
//!   field's type. `no_default` is required because
//!   `time::OffsetDateTime` does not implement `Default`; every field
//!   on a non-`Default` model must be initialised explicitly by the
//!   adopter.
//!
//! Every fixture binary needs `fn main` so the rustc invocation
//! produces a linkable artifact (this requirement carried over from
//! the trybuild era and still holds under lihaaf).
//!
//! Spec anchor:
//!   Plan: cluster-8delta-granular
//!   §3 commit T7.4 — compile-fixture bullet.
//!
//! GitHub: djogi#124.

use djogi::prelude::*;

// ── CustomPrimaryKey — `djogi::primary_key!` path routing ───────────────

djogi::primary_key! {
    pub struct AdopterIsolationId(i64);
    sql_type = "BIGINT";
    default_sql = "0";
    bulk_sql = "SELECT 0::bigint AS id FROM generate_series(1, $1)";
}

// ── IsolationState — `#[derive(DjogiEnum)]` path routing ────────────────

#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[djogi_enum(name = "adopter_isolation_state", rename_all = "snake_case")]
pub enum IsolationState {
    Active,
    Archived,
}

impl Default for IsolationState {
    fn default() -> Self {
        IsolationState::Active
    }
}

// ── DefaultRow — default-`updated_at` watermark branch ──────────────────
//
// Also exercises u64 fields: the macro-emitted bind shim must route through
// `::djogi::__private::rust_decimal::Decimal`, not `::rust_decimal::Decimal`
// directly. If the shim named `::rust_decimal::Decimal`, this fixture would
// fail to compile here (rust_decimal is not in this crate's [dependencies]).

#[model(table = "adopter_isolation_default_rows")]
#[derive(Debug, Clone)]
pub struct DefaultRow {
    pub label: String,
    // u64 field: emits a `Decimal::from(v)` bind shim that MUST route through
    // `::djogi::__private::rust_decimal::Decimal`, not `::rust_decimal::Decimal`.
    // Compiling here (where rust_decimal is NOT a direct dep) proves isolation.
    pub counter: u64,
    pub opt_counter: Option<u64>,
}

// ── WatermarkedRow — explicit-watermark branch ──────────────────────────

#[model(
    table = "adopter_isolation_watermarked_rows",
    watermark_field = "expires_at",
    no_default
)]
#[derive(Debug, Clone)]
pub struct WatermarkedRow {
    pub label: String,
    // `DateTime` reaches `time::OffsetDateTime` via `djogi::prelude::*`
    // → `djogi::types::DateTime`. The macro must emit a path that
    // resolves through `::djogi::types`, not `::time::*` directly —
    // the latter would fail to compile here because `time` is not
    // listed in this fixture's `[dependencies]`.
    pub expires_at: DateTime,
}

// ── CustomPkEnumRow — custom-PK + DjogiEnum model field ─────────────────

#[model(table = "adopter_isolation_custom_pk_enum_rows", pk = AdopterIsolationId)]
#[derive(Debug, Clone)]
pub struct CustomPkEnumRow {
    pub label: String,
    pub state: IsolationState,
}

// ── `#[djogi_test]` path routing ────────────────────────────────────────

#[djogi::djogi_test]
async fn djogi_test_macro_path_routes_through_djogi(ctx: DjogiContext) {
    let _ = ctx;
}

// ── Trait-bound surface checks (compile-time resolution only) ───────────

/// Witnesses that `#[model]` auto-emits an `impl Cacheable`
/// reachable through the macro-routing path `::djogi::types::Cacheable`.
fn _accept_cacheable<T: ::djogi::types::Cacheable + 'static>() {}

/// Witnesses that the default-watermark model resolves
/// `DeltaSyncCacheable::Watermark = DateTime` (i.e. the macro found
/// the framework-injected `updated_at: DateTime` field).
fn _accept_delta_sync_default<T>()
where
    T: ::djogi::types::DeltaSyncCacheable<Watermark = ::djogi::types::DateTime>,
{
}

/// Witnesses that the explicit-watermark model resolves
/// `DeltaSyncCacheable::Watermark` to the user-declared field's type.
/// Same constraint shape as `_accept_delta_sync_default` — the
/// distinction is which `#[model]` branch produced the impl.
fn _accept_delta_sync_explicit<T>()
where
    T: ::djogi::types::DeltaSyncCacheable<Watermark = ::djogi::types::DateTime>,
{
}

/// Witnesses that `SassiBootHook` is reachable from the djogi crate
/// root — the path the macro-emitted `inventory::submit!` block names
/// (`::djogi::SassiBootHook`).
fn _use_boot_hook_type() -> Option<::djogi::SassiBootHook> {
    None
}

/// Witnesses that `Punnu<T>` is reachable through `djogi::cache::*`
/// for both `Cacheable` types — adopters never reach into `::sassi::*`
/// to construct a Punnu.
fn _build_punnu_default() -> ::djogi::cache::Punnu<DefaultRow> {
    ::djogi::cache::Punnu::<DefaultRow>::builder().build()
}

fn _build_punnu_watermarked() -> ::djogi::cache::Punnu<WatermarkedRow> {
    ::djogi::cache::Punnu::<WatermarkedRow>::builder().build()
}

/// Witnesses that `DjogiDeltaSyncMeta::WATERMARK_COLUMN` resolves
/// through `djogi::cache` for both branches. The macro emits this impl
/// alongside `DeltaSyncCacheable` so the fetcher can
/// generate `WHERE <col> >= $since` SQL without runtime field-name
/// reflection.
fn _watermark_column_default() -> &'static str {
    <DefaultRow as ::djogi::cache::DjogiDeltaSyncMeta>::WATERMARK_COLUMN
}

fn _watermark_column_watermarked() -> &'static str {
    <WatermarkedRow as ::djogi::cache::DjogiDeltaSyncMeta>::WATERMARK_COLUMN
}

fn main() {
    let _ = <AdopterIsolationId as ::djogi::primary_key::PrimaryKey>::sentinel();
    _accept_cacheable::<DefaultRow>();
    _accept_cacheable::<WatermarkedRow>();
    _accept_cacheable::<CustomPkEnumRow>();
    _accept_delta_sync_default::<DefaultRow>();
    _accept_delta_sync_explicit::<WatermarkedRow>();
    let _ = _use_boot_hook_type();
    let _ = _build_punnu_default();
    let _ = _build_punnu_watermarked();
    let _ = CustomPkEnumRow::objects().filter(|f| f.state().eq(IsolationState::Active));
    // Sanity-check the column-name constants resolve to the expected
    // strings. The values are compile-time constants; the assertion is
    // really exercising that the trait impls are emitted.
    assert_eq!(_watermark_column_default(), "updated_at");
    assert_eq!(_watermark_column_watermarked(), "expires_at");
}
