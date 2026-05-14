# Draft rustdoc for `djogi/src/__bypass.rs`

Raw SQL escape hatches for deliberate framework bypasses.

This module is public but hidden from generated documentation. It exists for
code that must consciously step outside djogi's typed surface: pin tests,
framework-internal substrate code, sibling workspace crates, and adopter code
with a documented typed-surface gap. Public callers should normally reach it
through `#[djogi::deliberately_bypass_convention_with_raw_sql]`, not by writing
the hidden imports directly.

Raw SQL in djogi is treated culturally the way `unsafe` is in Rust. It is not
banned, but it must be visible in review. Ordinary code should prefer
`Model::create`, `Model::save`, `Model::delete`, `Model::objects()`,
`QuerySet`, `djogi::transaction::atomic`, and the migration APIs. Importing this
module is an explicit opt-out from that convention.

The `RawAccessExt` and `RawPoolAccessExt` traits expose the raw methods and
pool-level escape hatches that are intentionally absent from ordinary
`DjogiContext` method lookup. The traits are sealed; downstream crates can use
the provided implementations, but they cannot implement the raw surface for
their own types.

In this repository's tests, do not import `djogi::__bypass` directly. Use:

```rust
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): explain the typed-surface gap.
async fn test(mut ctx: DjogiContext) {
    // raw calls are available here
}
```

Pin tests that exercise a raw API directly use `JUSTIFICATION (PIN)` instead of
an issue number. The xtask validators enforce that test bypasses are attached
to a justification comment and that ordinary integration tests do not reference
this module directly.

Framework-internal modules may import the crate-local traits directly:

```rust
use crate::__bypass::RawAccessExt as DjogiRawAccessExt;
use crate::__bypass::RawPoolAccessExt as DjogiRawPoolAccessExt;
```

Sibling workspace crates and deliberate adopter opt-outs use the public bypass
attribute plus a justification comment; the attribute injects the hidden public
path inside the decorated item.

Every reach for raw SQL should remain auditable. If a call exists because the
typed surface cannot express a production-useful SQL shape, track that gap in
djogi's issue tracker and prefer adding a typed API over normalizing raw SQL at
call sites.
