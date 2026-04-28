//! Built-in [`PrimaryKey`] implementations.
//!
//! Covers the four HeeRanjId variants (`HeerId`, `HeerIdDesc`, `RanjId`,
//! `RanjIdDesc`) plus `Serial` (`i32`). Each variant implements
//! [`PrimaryKey`], four of the five also implement [`PrimaryKeyDbGen`];
//! `Serial` deliberately does not. Absence of [`PrimaryKeyDbGen`] on
//! `i32` is load-bearing: `pk = Serial` models get a clean compile
//! error at `bulk_create` call sites instead of a runtime failure.
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
use crate::primary_key::{PrimaryKey, PrimaryKeyDbGen, checked_count};
use crate::types::{HeerId, HeerIdDesc, RanjId, RanjIdDesc};

macro_rules! impl_heeranjid_pk {
    ($ty:ty, HeerId, $default_sql:literal, $single_sql:literal, $batch_sql:literal) => {
        impl_heeranjid_pk!(@impl $ty, HeerId, "BIGINT", $default_sql, $single_sql, $batch_sql);
    };
    ($ty:ty, HeerIdDesc, $default_sql:literal, $single_sql:literal, $batch_sql:literal) => {
        impl_heeranjid_pk!(@impl $ty, HeerIdDesc, "BIGINT", $default_sql, $single_sql, $batch_sql);
    };
    ($ty:ty, RanjId, $default_sql:literal, $single_sql:literal, $batch_sql:literal) => {
        impl_heeranjid_pk!(@impl $ty, RanjId, "UUID", $default_sql, $single_sql, $batch_sql);
    };
    ($ty:ty, RanjIdDesc, $default_sql:literal, $single_sql:literal, $batch_sql:literal) => {
        impl_heeranjid_pk!(@impl $ty, RanjIdDesc, "UUID", $default_sql, $single_sql, $batch_sql);
    };
    (@impl $ty:ty, $kind_tag:ident, $sql_type:literal, $default_sql:literal, $single_sql:literal, $batch_sql:literal) => {
        impl PrimaryKey for $ty {
            const __DJOGI_PK_SEAL: crate::primary_key::PkSealToken =
                crate::primary_key::__DJOGI_PK_SEAL_TOKEN;
            const KIND: PkType = PkType::$kind_tag;
            const SQL_TYPE: &'static str = $sql_type;
            const DEFAULT_SQL: Option<&'static str> = Some($default_sql);

            fn sentinel() -> Self {
                <$ty>::ZERO
            }
        }

        impl PrimaryKeyDbGen for $ty {
            async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError> {
                let row = ctx.query_one($single_sql, &[]).await?;
                Ok(row.try_get::<_, $ty>(0)?)
            }

            async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError> {
                let count = checked_count(n)?;
                if count == 0 {
                    return Ok(Vec::new());
                }
                let rows = ctx.query_all($batch_sql, &[&count]).await?;
                rows.into_iter()
                    .map(|row| row.try_get::<_, $ty>(0).map_err(DjogiError::from))
                    .collect()
            }
        }
    };
}

impl_heeranjid_pk!(
    HeerId,
    HeerId,
    "heerid_next()",
    "SELECT heerid_next()",
    "SELECT id FROM generate_ids(current_heer_node_id(), $1::integer, true)"
);

// NOTE: no inherent `impl HeerId { fn generate(...) }` block.
// Rust's orphan rule forbids inherent items on foreign types; the
// `PrimaryKeyDbGen` trait in scope via `djogi::prelude::*` makes
// `HeerId::generate(&mut ctx)` resolve directly, so the wrapper would
// be redundant even if the rule allowed it.

impl_heeranjid_pk!(
    HeerIdDesc,
    HeerIdDesc,
    "heerid_next_desc()",
    "SELECT heerid_next_desc()",
    "SELECT heerid_to_desc(id) \
     FROM generate_ids(current_heer_node_id(), $1::integer, true)"
);

impl_heeranjid_pk!(
    RanjId,
    RanjId,
    "ranjid_next()",
    "SELECT ranjid_next()",
    "SELECT id FROM generate_ranjids(current_heer_ranj_node_id(), $1::integer, true)"
);

impl_heeranjid_pk!(
    RanjIdDesc,
    RanjIdDesc,
    "ranjid_next_desc()",
    "SELECT ranjid_next_desc()",
    "SELECT ranjid_to_desc(id) \
     FROM generate_ranjids(current_heer_ranj_node_id(), $1::integer, true)"
);

// ── Serial (i32) ───────────────────────────────────────────────────────
//
// `Serial` deliberately does not implement `PrimaryKeyDbGen`. The
// `bulk_create` signature bounds on that trait, so `pk = Serial`
// models get a clean compile error at the call site instead of a
// runtime failure. Lookup / reference tables declared `pk = Serial`
// are insert-by-row on purpose: Postgres' `SERIAL` sequence returns one
// row at a time via `RETURNING id`.

impl PrimaryKey for i32 {
    const __DJOGI_PK_SEAL: crate::primary_key::PkSealToken =
        crate::primary_key::__DJOGI_PK_SEAL_TOKEN;
    const KIND: PkType = PkType::Serial;
    const SQL_TYPE: &'static str = "INTEGER";
    const DEFAULT_SQL: Option<&'static str> = None;

    fn sentinel() -> Self {
        0
    }
}

// Live-database coverage for `PrimaryKeyDbGen::generate_many` follows
// the existing project convention: every test that calls
// `setup_test_db()` is a named integration target in `djogi/Cargo.toml`
// rather than a `cfg(test)` unit test inside the library source.
//
// Unit-level coverage for the non-DB parts of `PrimaryKey` lives in the
// `tests` module of `djogi/src/primary_key/mod.rs`.
