//! Built-in [`PrimaryKey`] implementations.
//!
//! Covers the four HeeRanjId variants (`HeerId`, `HeerIdDesc`, `RanjId`,
//! `RanjIdDesc`) plus `Serial` (`i32`). Each variant implements
//! [`PrimaryKey`], four of the five also implement [`PrimaryKeyDbGen`];
//! `Serial` deliberately does not. Absence of [`PrimaryKeyDbGen`] on
//! `i32` is load-bearing — Task 5's `bulk_create` dispatches on the
//! bound, so `pk = Serial` models get a clean compile error at
//! `bulk_create` call sites instead of a runtime failure.
//!
//! # Single round-trip contract
//!
//! [`PrimaryKeyDbGen::generate_many`] issues exactly one query against
//! the HeeRanjId `generate_ids` / `generate_ranjids` SQL functions the
//! schema installs. The node_id is resolved by
//! `current_heer_node_id()` / `current_heer_ranj_node_id()` inside
//! Postgres, so callers need no per-request session setup beyond the
//! standard `djogi_test` fixtures which run `install_schema` +
//! `seed_default_node`.
//!
//! Desc variants post-process each ascending row with
//! `heerid_to_desc(id)` / `ranjid_to_desc(id)` in the SAME query — the
//! XOR transform is `IMMUTABLE` so Postgres folds it into the batch.

use crate::context::DjogiContext;
use crate::descriptor::PkType;
use crate::error::DjogiError;
use crate::primary_key::{PrimaryKey, PrimaryKeyDbGen};
use crate::types::{HeerId, HeerIdDesc, RanjId, RanjIdDesc, RanjPrecision};

/// Clamp `usize` to `i32` for the Postgres `INTEGER` parameter on the
/// `generate_ids` / `generate_ranjids` functions.
///
/// Returns `Ok(0)` for `n == 0` so an empty batch is a no-op instead of
/// a round-trip. Any other failure (`n > i32::MAX as usize`) surfaces as
/// `DjogiError::Db` — mirrors the error channel the raw_query path uses.
fn checked_count(n: usize) -> Result<i32, DjogiError> {
    i32::try_from(n).map_err(|_| {
        DjogiError::Db(crate::error::DbError::other(format!(
            "bulk id allocation rejected: count {n} exceeds i32::MAX"
        )))
    })
}

// ── HeerId ─────────────────────────────────────────────────────────────

impl PrimaryKey for HeerId {
    const KIND: PkType = PkType::HeerId;
    const SQL_TYPE: &'static str = "BIGINT";
    const DEFAULT_SQL: Option<&'static str> = Some("heerid_next()");

    fn sentinel() -> Self {
        // `HeerId(0,0,0)` is always valid — timestamp/node/sequence all
        // fit in their bit-widths — and matches the pre-T1
        // `__heerid_default()` helper exactly.
        HeerId::new(0, 0, 0).expect("HeerId(0,0,0) is always valid as a sentinel")
    }
}

impl PrimaryKeyDbGen for HeerId {
    async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError> {
        let row = ctx.query_one("SELECT heerid_next()", &[]).await?;
        Ok(row.try_get::<_, HeerId>(0)?)
    }

    async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError> {
        let count = checked_count(n)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        // Explicit 3-arg overload so the parameter-type inference
        // unambiguously picks `generate_ids(integer, integer, boolean)`
        // rather than the 2-arg `(integer, boolean)` variant.
        let rows = ctx
            .query_all(
                "SELECT id FROM generate_ids(current_heer_node_id(), $1::integer, true)",
                &[&count],
            )
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<_, HeerId>(0).map_err(DjogiError::from))
            .collect()
    }
}

// NOTE: no inherent `impl HeerId { fn generate(...) }` block.
// Rust's orphan rule forbids inherent items on foreign types; the
// `PrimaryKeyDbGen` trait in scope via `djogi::prelude::*` makes
// `HeerId::generate(&mut ctx)` resolve directly, so the wrapper would
// be redundant even if the rule allowed it.

// ── HeerIdDesc ─────────────────────────────────────────────────────────

impl PrimaryKey for HeerIdDesc {
    const KIND: PkType = PkType::HeerIdDesc;
    const SQL_TYPE: &'static str = "BIGINT";
    const DEFAULT_SQL: Option<&'static str> = Some("heerid_next_desc()");

    fn sentinel() -> Self {
        HeerIdDesc::new(0, 0, 0).expect("HeerIdDesc(0,0,0) is always valid as a sentinel")
    }
}

impl PrimaryKeyDbGen for HeerIdDesc {
    async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError> {
        let row = ctx.query_one("SELECT heerid_next_desc()", &[]).await?;
        Ok(row.try_get::<_, HeerIdDesc>(0)?)
    }

    async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError> {
        let count = checked_count(n)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        // `heerid_to_desc(bigint)` is `IMMUTABLE PARALLEL SAFE`, so
        // Postgres evaluates it per-row inside the batch with no extra
        // round-trip. Same ascending call the Rust helper
        // `generate_heerids` uses; only the projection changes.
        let rows = ctx
            .query_all(
                "SELECT heerid_to_desc(id) \
                 FROM generate_ids(current_heer_node_id(), $1::integer, true)",
                &[&count],
            )
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<_, HeerIdDesc>(0).map_err(DjogiError::from))
            .collect()
    }
}

// ── RanjId ─────────────────────────────────────────────────────────────

impl PrimaryKey for RanjId {
    const KIND: PkType = PkType::RanjId;
    const SQL_TYPE: &'static str = "UUID";
    const DEFAULT_SQL: Option<&'static str> = Some("ranjid_next()");

    fn sentinel() -> Self {
        RanjId::new(0, RanjPrecision::Microseconds, 0, 0)
            .expect("RanjId all-zero sentinel is always valid")
    }
}

impl PrimaryKeyDbGen for RanjId {
    async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError> {
        let row = ctx.query_one("SELECT ranjid_next()", &[]).await?;
        Ok(row.try_get::<_, RanjId>(0)?)
    }

    async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError> {
        let count = checked_count(n)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let rows = ctx
            .query_all(
                "SELECT id FROM generate_ranjids(current_heer_ranj_node_id(), $1::integer, true)",
                &[&count],
            )
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<_, RanjId>(0).map_err(DjogiError::from))
            .collect()
    }
}

// ── RanjIdDesc ─────────────────────────────────────────────────────────

impl PrimaryKey for RanjIdDesc {
    const KIND: PkType = PkType::RanjIdDesc;
    const SQL_TYPE: &'static str = "UUID";
    const DEFAULT_SQL: Option<&'static str> = Some("ranjid_next_desc()");

    fn sentinel() -> Self {
        RanjIdDesc::new(0, RanjPrecision::Microseconds, 0, 0)
            .expect("RanjIdDesc all-zero sentinel is always valid")
    }
}

impl PrimaryKeyDbGen for RanjIdDesc {
    async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError> {
        let row = ctx.query_one("SELECT ranjid_next_desc()", &[]).await?;
        Ok(row.try_get::<_, RanjIdDesc>(0)?)
    }

    async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError> {
        let count = checked_count(n)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let rows = ctx
            .query_all(
                "SELECT ranjid_to_desc(id) \
                 FROM generate_ranjids(current_heer_ranj_node_id(), $1::integer, true)",
                &[&count],
            )
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<_, RanjIdDesc>(0).map_err(DjogiError::from))
            .collect()
    }
}

// ── Serial (i32) ───────────────────────────────────────────────────────
//
// `Serial` deliberately does not implement `PrimaryKeyDbGen`. Task 5's
// `bulk_create` signature will bound on that trait, so `pk = Serial`
// models get a clean compile error at the call site instead of a runtime
// failure. The semantic: lookup / reference tables declared `pk = Serial`
// are insert-by-row, not bulk, on purpose — Postgres' `SERIAL` sequence
// returns one row at a time via `RETURNING id`.

impl PrimaryKey for i32 {
    const KIND: PkType = PkType::Serial;
    const SQL_TYPE: &'static str = "INTEGER";
    const DEFAULT_SQL: Option<&'static str> = None;

    fn sentinel() -> Self {
        0
    }
}

// Live-database coverage for `PrimaryKeyDbGen::generate_many` lives in
// `tests/integration/phase7_zero2_primary_key_generate.rs` so it follows
// the existing project convention — every test that calls
// `setup_test_db()` is a named integration target in `djogi/Cargo.toml`
// rather than a `cfg(test)` unit test inside the library source.
//
// Unit-level coverage for the non-DB parts of `PrimaryKey` lives in the
// `tests` module of `djogi/src/primary_key/mod.rs`.
