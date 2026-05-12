# djogi-macros — lihaaf fixture harness (CI-primary, Phase 8.5)

This directory wires djogi-macros's existing `tests/compile_pass/` and
`tests/compile_fail/` trybuild corpus into the
[lihaaf](https://github.com/TarunvirBains/lihaaf) v0.1 proc-macro test
harness. **lihaaf is the primary CI gate for macro fixtures** as of Phase 8.5.

## CI posture

| Harness | CI role | How it runs |
| - | - | - |
| **lihaaf** | **Primary gate** | `cargo install lihaaf --version 0.1.0-alpha.2 --locked` → `cargo lihaaf --manifest-path djogi-macros/Cargo.toml -j 2` |
| trybuild | Manual parity only | `cargo test -p djogi-macros --features trybuild-tests --test trybuild_tests -- --test-threads=2` |

`cargo test --workspace` (and therefore default CI) does NOT run the
trybuild test binaries. They are gated behind `required-features =
["trybuild-tests"]` in `djogi-macros/Cargo.toml`. lihaaf runs instead.

## Source of truth

| | Path |
| - | - |
| Standalone lihaaf (canonical) | `~/projects/lihaaf` (local), `TarunvirBains/lihaaf` on GitHub, published at `lihaaf = "0.1.0-alpha.2"` on crates.io |
| Trybuild snapshots (committed) | `djogi-macros/tests/compile_fail/*.stderr` |
| Lihaaf snapshots (committed) | `djogi-macros/tests/lihaaf/compile_fail/*.stderr` |

## Committed snapshots

lihaaf-owned `.stderr` snapshots under `tests/lihaaf/compile_fail/` **are
committed** so `cargo lihaaf` in CI succeeds without `--bless`. They are
separate from the trybuild snapshots under `tests/compile_fail/` — the two
corpora normalize path prefixes differently:

- **trybuild** → `tests/compile_fail/<name>.rs:L:C`
- **lihaaf** → `$DIR/<name>.rs:L:C`

This harness-format difference is intentional. The diagnostic semantics are
identical; only the path normalization format differs.

### Re-blessing snapshots

Re-bless locally whenever macro diagnostics change (new error, span change,
error message edit), then commit the updated `.stderr` files alongside the
macro change:

```bash
# From the repo root (manifest-path is relative to cwd):
cargo lihaaf --manifest-path djogi-macros/Cargo.toml \
    --filter compile_fail --bless -j 4
```

If `cargo-lihaaf` is not on PATH, install it first:

```bash
cargo install lihaaf --version 0.1.0-alpha.2 --locked
```

### rust-src requirement

3 of the 138 compile_fail snapshots reference stdlib source via `$RUST`
(e.g. `$RUST/lib/rustlib/src/rust/library/alloc/src/string.rs`). These
lines appear only when `rust-src` is installed. CI always installs
rust-src; local dev should too. If you bless locally without rust-src,
those 3 fixtures will produce shorter output and the CI run will fail
with `SNAPSHOT_DIFF` on the `$RUST`-containing lines.

## Fixture layout

Lihaaf reads `[package.metadata.lihaaf]` from `djogi-macros/Cargo.toml`
and walks the directories named in `fixture_dirs`:

| Directory | Backing | Why this shape |
| - | - | - |
| `tests/lihaaf/compile_pass` | Directory symlink → `../compile_pass` | Compile-pass fixtures have no `.stderr` siblings, so a single directory symlink keeps the corpus in one on-disk location with zero risk of conflicting writes. |
| `tests/lihaaf/compile_fail/` | Real directory of per-file `.rs` symlinks → `../../compile_fail/<name>.rs` | Compile-fail fixtures DO have `.stderr` siblings. The per-file symlink layout means lihaaf's snapshot writes land in `tests/lihaaf/compile_fail/<name>.stderr` — a *real* file in this directory — and never clobber the trybuild-blessed snapshots under `tests/compile_fail/`. |

`lihaaf::snapshot::snapshot_path()` resolves to
`fixture_path.with_extension("stderr")` (no canonicalization); rustc
preserves the path-as-passed in its diagnostics; lihaaf's normalizer
rewrites `<fixture_dir>` prefixes to `$DIR`. The chain is symlink-safe
for both reads and writes provided the per-file `.rs` layout above is
preserved.

## Why `dylib_crate = "djogi"`

Fixtures `use djogi::prelude::*`. The proc-macro crate is loaded
through djogi's `pub use djogi_macros::*` re-export, so lihaaf only
needs to build the djogi dylib once per session and link the
proc-macro crate as a second `--extern`. The exact metadata shape:

```toml
[package.metadata.lihaaf]
dylib_crate     = "djogi"
extern_crates   = ["djogi", "djogi-macros"]
features        = []
dev_deps        = ["serde", "serde_json", "sassi"]
edition         = "2024"
fixture_dirs    = ["tests/lihaaf/compile_fail", "tests/lihaaf/compile_pass"]
```

`features = []` is deliberate: the spatial compile_pass fixtures gate
their bodies behind `#[cfg(feature = "spatial")]` so they compile
cleanly even when the dylib is built without the flag. Spatial-feature
parity through lihaaf is a follow-up.

## Trybuild fallback posture

`tests/trybuild_tests.rs` and `tests/trybuild_spatial_tests.rs` are
retained as opt-in manual parity checks gated behind
`--features trybuild-tests`. They are NOT removed — they serve as a
validation backstop when upgrading the CI toolchain or comparing snapshot
normalization formats. Phase 8.5 promotes lihaaf to the CI-primary role
while keeping trybuild as a committed fallback.

## Validation reference

237 fixtures total: 99 compile_pass + 138 compile_fail.

```bash
# Enumerate all 237 fixtures
cargo lihaaf --manifest-path djogi-macros/Cargo.toml --list | wc -l

# Full sweep (compile_pass + compile_fail), no bless — expect 237 OK
cargo lihaaf --manifest-path djogi-macros/Cargo.toml -j 4

# Compile-pass only (~12s)
cargo lihaaf --manifest-path djogi-macros/Cargo.toml --filter compile_pass -j 4

# Compile-fail only, re-bless (~8s)
cargo lihaaf --manifest-path djogi-macros/Cargo.toml \
    --filter compile_fail --bless -j 4
```

Baseline wall-clock (local, djogi dylib already built): compile_pass ~12s;
compile_fail ~8s; full sweep ~20s. Versus trybuild: ~15 min for a full
sweep (22 test-split functions × N fixtures each, one cargo invocation
per split). The speedup comes from the shared dylib and parallel
per-fixture rustc dispatch.
