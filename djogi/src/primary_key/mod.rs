//! Primary-key trait surface.
//! Three-trait split per `docs/spec/decisions.md`:
//! - [`PrimaryKey`] (required) — every PK type declares its [`PkType`]
//! discriminant and the schema-emission bits (`SQL_TYPE`, `DEFAULT_SQL`),
//! plus a zero-valued [`sentinel`](PrimaryKey::sentinel) factory used by
//! the macro-emitted `Default` impl.
//! - [`PrimaryKeyDbGen`] (optional) — DB-sourced bulk allocation. Every
//! built-in variant except `Serial` implements it; its deliberate
//! absence on `i32` is load-bearing for `bulk_create` dispatch.
//! - [`PrimaryKeyClientGen`] (optional, custom-only) — client-side single
//! and bulk generation. Built-in PKs never client-generate: HeeRanjId's
//! node/sequence/epoch model requires a database round-trip.
//! Every generation helper takes `&mut DjogiContext`, never a raw pool.
//! The context dispatches to the pool or the active transaction without
//! the caller caring which.
//! # Const-position sentinels via heeranjid 0.3.5+
//! The trait function `<T as PrimaryKey>::sentinel` is the
//! polymorphic-context entry point. When the caller knows the concrete
//! PK type, **prefer the upstream `T::ZERO` const** (added in heeranjid
//! 0.3.5):
//! ```ignore
//! // Inside #[model(no_default)] constructor helpers:
//! Widget {
//!     id: HeerId::ZERO,                         // const-position OK
//!     created_at: DateTime::UNIX_EPOCH,
//!     // ...
//! }
//! ```
//! `T::ZERO` is the wire-zero bit pattern, declared `pub const` on each
//! of HeerId / HeerIdDesc / RanjId / RanjIdDesc upstream. The
//! `PrimaryKey::sentinel` impls in this crate delegate to that const,
//! so the trait fn returns the same wire bytes the const exposes.
//! Use the trait fn when writing code polymorphic over PK type
//! (e.g. inside macro expansions or generic helpers); reach for the
//! const directly otherwise.
//! Heeranjid 0.3.5's `T::ZERO` consts replaced the older
//! `T::new(0, 0, 0)` reconstruction and are now the canonical
//! const-position sentinel values.
//! # Historical note
//! Earlier heeranjid releases did not expose a const sentinel; 0.3.5
//! shipped `T::ZERO`, while `sentinel` remains the polymorphic entry point.

use crate::context::DjogiContext;
use crate::descriptor::PkType;
use crate::error::{DbError, DjogiError};

pub mod builtins;

/// Hidden seal witness type for [`PrimaryKey`].
/// `#[doc(hidden)] pub` because the trait associated const
/// `__DJOGI_PK_SEAL` has this type and the trait itself is public
/// macro emission in downstream crates needs to be able to name the
/// type. The struct has no public constructor; the sole value lives
/// in [`crate::__private::pk_seal::TOKEN`] which routes through the
/// off-limits `__private` namespace. Nothing in the public
/// `djogi::primary_key` path surfaces a constructor — the previous
/// public `__DJOGI_PK_SEAL_TOKEN` re-export is gone.
/// Downstream code reaching the value through `djogi::__private` is
/// explicitly violating the framework boundary; same convention as
/// `VisageSealed` in [`crate::__private`].
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PkSealToken {
    _private: (),
}

impl PkSealToken {
    /// `pub(crate)` constructor so the [`crate::__private::pk_seal`]
    /// module can mint the witness without exposing a public path on
    /// the type itself. The empty private field still keeps
    /// downstream code from naming `PkSealToken { ... }` directly.
    pub(crate) const fn __new() -> Self {
        Self { _private: () }
    }
}

/// Convert a batch size to the Postgres `INTEGER` parameter used by
/// bulk primary-key allocation helpers. Returns an error when `n`
/// exceeds `i32::MAX` (the database `generate_ids` / `generate_ranjids`
/// functions take an `INTEGER` count).
pub fn checked_count(n: usize) -> Result<i32, DjogiError> {
    i32::try_from(n).map_err(|_| {
        DjogiError::Db(DbError::other(format!(
            "djogi::primary_key!: bulk generate rejected — count {n} exceeds i32::MAX"
        )))
    })
}

/// Error used when a bulk primary-key allocation query returns a
/// different number of rows than requested.
pub fn bulk_row_count_mismatch_err(got: usize, want: usize, label: &str) -> DjogiError {
    DjogiError::Db(DbError::other(format!(
        "djogi::primary_key!: {label} returned {got} rows for n={want}"
    )))
}

/// Contract every primary-key type must satisfy.
/// Implementations map the type to its [`PkType`] discriminant, the
/// Postgres column type, the optional `DEFAULT` clause, and the zero
/// value the macro-emitted `Default` impl uses for the `id` field.
/// # Sealing
/// Sealed via a hidden `PkSealToken` witness — the only paths to a
/// valid impl are the built-in HeerId / RanjId / Serial impls in
/// [`builtins`] and the [`djogi::primary_key!`] macro. Hand-rolled
/// `impl PrimaryKey for …` in downstream crates fail at the
/// [`__DJOGI_PK_SEAL`](Self::__DJOGI_PK_SEAL) const because the token
/// type has no public constructor and the public `djogi::primary_key`
/// path no longer re-exports either the type or its sole instance
/// macro-emitted code reaches them through
/// [`crate::__private::pk_seal`] (a doc-hidden path under the
/// off-limits `__private` namespace). Matches the
/// [`crate::apps::App`] sealing convention and the `VisageSealed`
/// convention in [`crate::__private`].
pub trait PrimaryKey: Sized + 'static {
    /// Hidden seal witness — only macro-emitted and built-in impls
    /// can name the value (see [`crate::__private::pk_seal`]).
    #[doc(hidden)]
    const __DJOGI_PK_SEAL: PkSealToken;

    /// Runtime discriminant the [`ModelDescriptor`](crate::descriptor::ModelDescriptor) carries.
    const KIND: PkType;

    /// Postgres column type, e.g. `"BIGINT"` / `"UUID"` / `"INTEGER"`.
    const SQL_TYPE: &'static str;

    /// Column `DEFAULT` clause, e.g. `"heerid_next"` / `"heerid_next_desc"`.
    /// `None` when no server-side default is installed — for example
    /// `Serial`, where the column is a plain `INTEGER`.
    const DEFAULT_SQL: Option<&'static str>;

    /// Zero-valued instance used by the macro-emitted `Default` impl's
    /// `id` initialiser. The value is never written to the database:
    /// `create` replaces it via `RETURNING *` before the row lands.
    fn sentinel() -> Self;
}

/// Optional DB-backed bulk-allocation path.
/// Every built-in PK variant except `Serial` implements this. The
/// absence on `i32` is intentional: `bulk_create` for `pk = Serial`
/// models is a compile error today, which
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
