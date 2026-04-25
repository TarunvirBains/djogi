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
//! # Const-position sentinels via heeranjid 0.3.5+
//!
//! The trait function `<T as PrimaryKey>::sentinel()` is the
//! polymorphic-context entry point. When the caller knows the concrete
//! PK type, **prefer the upstream `T::ZERO` const** (added in heeranjid
//! 0.3.5):
//!
//! ```ignore
//! // Inside #[model(no_default)] constructor helpers:
//! Widget {
//!     id: HeerId::ZERO,                         // const-position OK
//!     created_at: DateTime::UNIX_EPOCH,
//!     // ...
//! }
//! ```
//!
//! `T::ZERO` is the wire-zero bit pattern, declared `pub const` on each
//! of HeerId / HeerIdDesc / RanjId / RanjIdDesc upstream. The
//! `PrimaryKey::sentinel()` impls in this crate delegate to that const,
//! so the trait fn returns the same wire bytes the const exposes.
//!
//! Use the trait fn when writing code polymorphic over PK type
//! (e.g. inside macro expansions or generic helpers); reach for the
//! const directly otherwise.
//!
//! See [`sentinel`] for the bit-pattern note documenting why the
//! 0.3.5 adoption changed the sentinel value (vs. the pre-0.3.5
//! `T::new(0, 0, 0)` form) on three of the four PK types and why
//! that change is safe.
//!
//! # Historical note
//!
//! The Phase 7-Zero-2 plan originally called for a djogi-side `pub
//! const SENTINEL: Self` associated item. Heeranjid 0.3.0–0.3.4
//! exposed neither `pub const ZERO` nor a `const fn` constructor (the
//! inner field is private), so a djogi-side const would have required
//! either a foreign-type inherent const (forbidden by the orphan rule)
//! or `mem::transmute` through the `#[repr(transparent)]` layout
//! (brittle). The fallback was the non-const `sentinel()` factory.
//! Heeranjid 0.3.5 (closing heeranjid#30) added the `pub const ZERO`
//! upstream, which is now the canonical const-position sentinel.

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
    /// `create()` replaces it via `RETURNING *` before the row lands.
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
