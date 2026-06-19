//! Core type aliases and re-exports used in `#[model]` struct definitions.
//! Every `#[model]` struct pulls these in via `djogi::prelude::*`, so users
//! never need per-field imports for the framework's canonical types. The
//! aliases (`DateTime`, `Date`) pin the project to the `time` crate — the
//! framework explicitly avoids `chrono` (see CLAUDE.md "Dependencies
//! excluded").
//! # Naming aliases
//! `HeerIdRecencyBiased` / `RanjIdRecencyBiased` are spec-§3.5a-mandated
//! public aliases over heeranjid's internal `HeerIdDesc` / `RanjIdDesc`.
//! User-facing code (guide examples, `#[model(pk = X)]` callers) should
//! reach for the `RecencyBiased` names; internal plumbing keeps the
//! `Desc` spelling to stay lockstep with the upstream crate.

pub use heeranjid::{HeerId, HeerIdDesc, RanjId, RanjIdDesc, RanjPrecision};

// Postgres typed-surface newtypes (typed PG types).
// `Interval` ships now; follow-on dispatches in the
// follow-on dispatches add additional newtypes alongside.
pub use crate::pg_types::Interval;

// G0 — Range<T> + RangeBound<T> substrate.
// `Range<T>` is the typed Postgres range value carried by columns
// typed `int4range` / `int8range` / `numrange` / `tstzrange` /
// `tsrange` / `daterange`. The substrate lives at
// `crate::pg_types::Range` / `crate::pg_types::RangeBound`; the
// re-export here surfaces them at the canonical adopter-facing path
// (`djogi::types::Range`, and `djogi::Range` via the crate root).
// The `RangeSubtype` trait is intentionally **not** re-exported.
// Adopters should not need it for ordinary use; the wire codec
// reaches it via trait-bound dispatch and adopter code interacts
// only with the `Range<T>` value itself.
pub use crate::pg_types::{Range, RangeBound};

// Network types (, `network` feature).
// `MacAddr` carries a 6-byte EUI-48 MAC address (Postgres `MACADDR`
// columns); `CidrAddr` carries an `(IpAddr, u8)` network address +
// prefix (Postgres `CIDR` columns). The `INET` column type is reached
// through `std::net::IpAddr` directly (no djogi-side newtype) — the
// `postgres-types` crate carries the native ToSql/FromSql impl, and
// the host-address case is what adopters reaching for `INET` today
// want. A future-work follow-up can add
// `InetAddr { addr, prefix }` symmetric with `CidrAddr` if adopter
// demand for non-host INET surfaces.
// The `CidrAddrError` / `MacAddrParseError` types are re-exported so
// adopters who handle construction errors from `CidrAddr::new` /
// `str::parse::<MacAddr>` can name the error type at the canonical
// `djogi::types::*` path.
#[cfg(feature = "network")]
pub use crate::pg_types::{
    CidrAddr, CidrAddrError, InetAddr, InetAddrError, MacAddr, MacAddrParseError,
};

// Public naming — spec §3.5a. Internals keep `HeerIdDesc` / `RanjIdDesc`
// to match heeranjid; user-facing surfaces (guides, `#[model(pk = X)]`)
// use the `RecencyBiased` aliases so the name describes intent rather
// than bit layout.
pub use heeranjid::HeerIdDesc as HeerIdRecencyBiased;
pub use heeranjid::RanjIdDesc as RanjIdRecencyBiased;

/// `TIMESTAMPTZ` — alias for `time::OffsetDateTime`.
pub type DateTime = time::OffsetDateTime;

/// `DATE` — alias for `time::Date`.
pub type Date = time::Date;

// Sassi cache-surface re-exports for macro-emitted code (1).
// Per `feedback_macro_path_routing.md`, proc-macro-emitted impls must
// route through `::djogi::*` paths only — never `::sassi::*` directly.
// The macro emits `impl ::djogi::types::Cacheable for {Model}` and the
// matching `DeltaSyncCacheable` impl through `sassi-codegen` with
// `sassi_path = ::djogi::types`. `MonotonicWatermark` is the watermark
// trait the emitted `DeltaSyncCacheable` impl bounds against;
// `BasicPredicate` is the lifted predicate algebra surface carried by
// `Q::Portable(...)` in macro-emission contexts.
// The adopter-facing surface lives at `djogi::cache::*` (see
// `crate::cache`) — this re-export is purely a macro-routing target.
// `Cacheable` is imported via the module path `sassi::cacheable` so the
// re-export carries the TRAIT only — `sassi::Cacheable` glob-resolves to
// both the trait and `sassi_macros::Cacheable` (the derive). The macro emits
// `impl ::djogi::types::Cacheable for ...`; that path must point at the
// trait, not the derive. (See the matching note in `crate::cache`.)
pub use sassi::cacheable::Cacheable;
pub use sassi::{BasicPredicate, DeltaSyncCacheable, MonotonicWatermark};

// Sassi predicate inspection re-exports.
// Macro-emitted `impl Model::__djogi_emit_field_predicate` blocks live in
// adopter crates and must spell Sassi types through `::djogi::types::*`
// (not `::sassi::*` directly), per `feedback_macro_path_routing.md`. PR2d
// emits the override; PR2a adds the re-exports so the macro emission
// compiles when PR2d lands.
// - `Field<T, V>` — the Sassi field accessor `__make_djogi_field` constructs
// internally and `DjogiField::__portable_field` returns. Lives at
// `sassi::Field` (re-exported from `sassi::cacheable::Field`).
// - `FieldPredicate<T>` — the per-field predicate payload Sassi attaches to
// `BasicPredicate::Field(_)` leaves; the SQL walker downcasts its
// `value_as::<V>()` payload to dispatch on concrete Rust types.
// - `LookupOp` — the predicate operator marker; PR2d's match arms key off
// `(field.field_name(), field.op())`.
// - `IntoBasicPredicate<T>` — the Sassi-side conversion trait that
// `PortablePredicate<T>` implements so adopters can pass the wrapper
// straight into `PunnuScope::filter_basic`.
pub use sassi::Field;
pub use sassi::predicate::{FieldPredicate, IntoBasicPredicate, LookupOp};
