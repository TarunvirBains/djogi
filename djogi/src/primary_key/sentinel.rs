//! Sentinel-value documentation.
//!
//! Phase 7-Zero-2 ships two complementary sentinel surfaces:
//!
//! 1. **`T::ZERO`** — the canonical wire-zero `pub const` declared
//!    upstream in heeranjid 0.3.5+ (closing heeranjid#30). Reach for
//!    this directly when the concrete PK type is known at the call
//!    site. It works in const-position contexts and matches the stdlib
//!    idiom (`Duration::ZERO`, `Ipv4Addr::*`). On the wire it is the
//!    all-zero bit pattern, provably outside the image of every
//!    upstream generator (timestamps in HeerId/Desc are always
//!    non-zero; the all-zero UUID is the RFC 4122 §4.1.7 nil UUID,
//!    structurally invalid as UUIDv8 and rejected by `RanjId::from_uuid`).
//!
//! 2. **`<T as PrimaryKey>::sentinel()`** — the polymorphic-context
//!    entry point. The macro-emitted `Default` impl calls this at
//!    runtime; generic helpers and macro expansions reach for it when
//!    the concrete `T` isn't known. Each built-in implementation
//!    delegates to `T::ZERO`, so the runtime values match: both forms
//!    produce the same wire bytes.
//!
//! # Why two surfaces
//!
//! The const form is ergonomic at the call site but unreachable from
//! generic / macro contexts where `T` is bound by trait. The trait
//! function has the opposite shape: usable polymorphically but
//! unavailable in const position. Shipping both lets adopter code
//! pick the right tool for the call site.
//!
//! # Bit-pattern note (post-0.3.5 adoption)
//!
//! Pre-0.3.5 djogi reconstructed the sentinel via `T::new(0, 0, 0)`,
//! which on `HeerIdDesc` / `RanjId` / `RanjIdDesc` does NOT yield the
//! all-zero wire pattern (HeerIdDesc XORs with a flip mask;
//! Ranj{Id,IdDesc}::new encodes UUIDv8 version/variant bits). The
//! pre- and post-adoption sentinel values therefore differ for three
//! of the four PK types. This is intentional and safe:
//!
//! - The sentinel is purely an INSERT placeholder. `Model::create`
//!   ignores the placeholder and replaces the `id` field via
//!   `RETURNING *` before the row lands.
//! - Nothing in the framework (or any user code, by design) compares
//!   PK values against the sentinel — the placeholder is never
//!   observable beyond construction.
//! - Both old (`T::new(0,0,0)`) and new (`T::ZERO`) values are
//!   provably outside the image of every upstream generator, so
//!   either form is a valid sentinel.
//!
//! Anchoring on `T::ZERO` aligns djogi with the stdlib const-sentinel
//! idiom and gives users a single canonical name for the value.
//!
//! # Historical note
//!
//! Earlier drafts of Phase 7-Zero-2 T1 proposed a djogi-side `pub
//! const SENTINEL: Self`. Two stacked problems blocked it:
//!
//! 1. **Orphan rule.** `HeerId`, `HeerIdDesc`, `RanjId`, and `RanjIdDesc`
//!    are defined in the `heeranjid` crate, so the `djogi` crate
//!    cannot open an inherent `impl HeerId { ... }` block (E0118).
//! 2. **No const constructor (heeranjid 0.3.0–0.3.4).** Even if the
//!    inherent impl were legal, those releases exposed no `const fn`
//!    constructor (the inner field is private), so a `const
//!    SENTINEL` expression had nothing to evaluate at compile time.
//!
//! Heeranjid 0.3.5 closed problem (2) upstream by adding `pub const
//! ZERO` directly on the four ID types — eliminating the need for a
//! djogi-side const wrapper. The trait function `sentinel()` stays
//! for the polymorphic / macro-expansion path; both surfaces agree
//! on the wire bytes.
//!
//! This module is intentionally empty of code. It exists so adopters
//! grepping for `sentinel` have one authoritative place to read the
//! rationale before changing the trait shape.
