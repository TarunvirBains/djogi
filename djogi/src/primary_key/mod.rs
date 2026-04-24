//! Primary-key trait surface (Phase 7-Zero-2 T1).
//!
//! Three-trait split per `docs/spec/decisions.md`:
//!
//! - [`PrimaryKey`] (required) — every PK type declares its [`PkType`]
//!   discriminant and the schema-emission bits (`SQL_TYPE`, `DEFAULT_SQL`),
//!   plus a zero-valued [`sentinel`](PrimaryKey::sentinel) factory used by
//!   the macro-emitted `Default` impl.
//! - [`PrimaryKeyDbGen`] (optional) — DB-sourced bulk allocation. Every
//!   built-in variant except `Serial` implements it; its deliberate
//!   absence on `i32` is load-bearing for `bulk_create` dispatch (Task 5).
//! - [`PrimaryKeyClientGen`] (optional, custom-only) — client-side single
//!   and bulk generation. Built-in PKs never client-generate: HeeRanjId's
//!   node/sequence/epoch model requires a database round-trip.
//!
//! Post-Phase-4-retrofit discipline: every generation helper takes
//! `&mut DjogiContext`, never a raw pool. The context dispatches to the
//! pool or the active transaction without the caller caring which.
//!
//! # `sentinel()` is non-const
//!
//! The Phase 7-Zero-2 plan originally called for a `pub const SENTINEL:
//! Self` associated item so that `const _ZERO: HeerId = HeerId::SENTINEL;`
//! compiled. HeeRanjId 0.3 ships `HeerId::new` / `HeerId::from_i64` as
//! non-const fns and exposes no public const constructor (the inner
//! `i64` field is private). Exposing a `const SENTINEL` from `djogi`
//! would require either a foreign-type inherent const (forbidden by the
//! orphan rule) or `mem::transmute` through the `#[repr(transparent)]`
//! layout (brittle — a future `#[repr(Rust)]` change silently breaks
//! us). The plan's documented fallback is a non-const factory method;
//! that is what ships here. Macro-emitted `Default` impls call
//! `<T as ::djogi::primary_key::PrimaryKey>::sentinel()` at runtime
//! instead of a const-context read.

use crate::context::DjogiContext;
use crate::descriptor::PkType;
use crate::error::DjogiError;

pub mod builtins;
pub mod sentinel;

/// Contract every primary-key type must satisfy.
///
/// Implementations map the type to its [`PkType`] discriminant, the
/// Postgres column type, the optional `DEFAULT` clause, and the zero
/// value the macro-emitted `Default` impl uses for the `id` field.
pub trait PrimaryKey: Sized + 'static {
    /// Runtime discriminant the [`ModelDescriptor`](crate::descriptor::ModelDescriptor) carries.
    const KIND: PkType;

    /// Postgres column type, e.g. `"BIGINT"` / `"UUID"` / `"INTEGER"`.
    const SQL_TYPE: &'static str;

    /// Column `DEFAULT` clause, e.g. `"generate_id()"` / `"heerid_next_desc()"`.
    /// `None` when no server-side default is installed — for example
    /// `Serial`, where the column is a plain `INTEGER`.
    const DEFAULT_SQL: Option<&'static str>;

    /// Zero-valued instance used by the macro-emitted `Default` impl's
    /// `id` initialiser. The value is never written to the database:
    /// `create()` replaces it via `RETURNING id` before the row lands.
    fn sentinel() -> Self;
}

/// Optional DB-backed bulk-allocation path.
///
/// Every built-in PK variant except `Serial` implements this. The
/// absence on `i32` is intentional: `bulk_create` for `pk = Serial`
/// models is a compile error today (Task 5 wires the dispatch), which
/// matches HeeRanjId's design — client-side bulk allocation requires
/// coordinated node/sequence state that only the database owns.
#[allow(async_fn_in_trait)]
pub trait PrimaryKeyDbGen: PrimaryKey {
    /// Allocate exactly one ID in one database round-trip.
    async fn generate(ctx: &mut DjogiContext) -> Result<Self, DjogiError>;

    /// Allocate `n` IDs in **one** database round-trip. Implementations
    /// must not issue `n` separate queries — a plain
    /// `SELECT id FROM generate_ids(...)` returns the full set.
    async fn generate_many(ctx: &mut DjogiContext, n: usize) -> Result<Vec<Self>, DjogiError>;
}

/// Optional client-side generation path.
///
/// Custom-only. Built-in PKs never client-generate because HeeRanjId's
/// timestamp / node_id / sequence layout requires the database to own
/// the monotonic state. Adopter-defined PK types that can produce an ID
/// locally (UUIDv4, ULID, deterministic hash, etc.) opt in by
/// implementing this trait in addition to [`PrimaryKey`].
pub trait PrimaryKeyClientGen: PrimaryKey {
    /// Produce a single ID without touching the database.
    fn generate_client() -> Self;
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtins_all_implement_primary_key_with_expected_kind() {
        use crate::descriptor::PkType;
        use crate::primary_key::PrimaryKey;
        use crate::types::{HeerId, HeerIdDesc, RanjId, RanjIdDesc};

        assert!(matches!(<HeerId as PrimaryKey>::KIND, PkType::HeerId));
        assert!(matches!(
            <HeerIdDesc as PrimaryKey>::KIND,
            PkType::HeerIdDesc
        ));
        assert!(matches!(<RanjId as PrimaryKey>::KIND, PkType::RanjId));
        assert!(matches!(
            <RanjIdDesc as PrimaryKey>::KIND,
            PkType::RanjIdDesc
        ));
        assert!(matches!(<i32 as PrimaryKey>::KIND, PkType::Serial));
    }
}
