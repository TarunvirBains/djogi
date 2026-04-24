//! Sentinel-value documentation.
//!
//! Earlier drafts of Phase 7-Zero-2 T1 proposed shipping zero-valued
//! [`PrimaryKey`](crate::primary_key::PrimaryKey) constants as inherent
//! `const SENTINEL: Self` items on [`HeerId`](crate::types::HeerId) and
//! friends. Two compounded problems force the fallback design:
//!
//! 1. **Orphan rule.** `HeerId`, `HeerIdDesc`, `RanjId`, and `RanjIdDesc`
//!    are defined in the `heeranjid` crate, so the `djogi` crate cannot
//!    open an inherent `impl HeerId { ... }` block — inherent items must
//!    live in the defining crate (E0118).
//! 2. **No const constructor.** Even if the inherent impl were legal,
//!    heeranjid 0.3's public surface has no `const fn` constructor for
//!    these types — `HeerId::new` / `HeerId::from_i64` are non-const
//!    and the inner `i64` is private, so a `const SENTINEL` expression
//!    has nothing to evaluate at compile time.
//!
//! The shipped design therefore promotes the sentinel factory onto the
//! [`PrimaryKey`](crate::primary_key::PrimaryKey) trait as
//! [`PrimaryKey::sentinel`](crate::primary_key::PrimaryKey::sentinel) —
//! a non-const associated function. The macro-emitted `Default` impl
//! calls it at runtime; the result is never written to the database
//! because `create()` replaces the `id` field via `RETURNING id` before
//! the row lands.
//!
//! This module is intentionally empty of code. It exists so adopters
//! grepping for `sentinel` have one authoritative place to read the
//! rationale before changing the trait shape.
