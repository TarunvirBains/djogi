# djogi-macros — lihaaf fixture layout (Phase 8.5)

This directory wires djogi-macros's existing `tests/compile_pass/` and
`tests/compile_fail/` trybuild corpus into the
[lihaaf](https://github.com/TarunvirBains/lihaaf) v0.1 proc-macro test
harness so adopters can drive the same 237 fixtures through the
build-once-dylib + per-fixture-rustc path lihaaf exposes.

Phase 8.5 is the integration phase only. trybuild remains the
**authoritative source-of-truth** for the corpus. The trybuild
`.stderr` snapshots under `tests/compile_fail/` stay committed; lihaaf
runs in parallel as the fast-iteration path while the trybuild →
lihaaf migration is sequenced as a separate later step.

## Source of truth and integration base

| | Path | Ref |
| - | - | - |
| Standalone Lihaaf (canonical) | `~/projects/lihaaf` | post-alpha build with diagnostic extraction and long-type path normalization fixes |
| Djogi integration worktree | `~/projects/djogi/.worktrees/phase85-lihaaf-integration` | branch `phase85-lihaaf-integration` |
| Djogi integration base | `~/projects/djogi` | `main` |

The lihaaf crate is consumed as a local dev tool during Phase 8.5
branch work — there is no `[dev-dependencies] lihaaf = ...` entry in
this crate's `Cargo.toml`. The harness is invoked through `cargo run`
against a standalone lihaaf source tree that includes the post-alpha
diagnostic extraction fix and long-type note path normalization:

```bash
# From the djogi-macros crate root (this directory's grandparent).
cargo run \
    --manifest-path ~/projects/lihaaf/Cargo.toml \
    --bin cargo-lihaaf \
    -- lihaaf \
    --manifest-path $(pwd)/Cargo.toml \
    --list
```

If your shell's `cargo run` resolves the binary's working directory to
lihaaf's own package root (older cargo behaviour) rather than the
invoker's CWD, pass the fully qualified path to djogi-macros's
`Cargo.toml` to the lihaaf side of the `--` boundary as shown above —
absolute paths are portable across both behaviours.

## Fixture-discovery layout

Lihaaf reads `[package.metadata.lihaaf]` from `djogi-macros/Cargo.toml`
and walks the directories named in `fixture_dirs`. Both
fixture-discovery directories live under `tests/lihaaf/`:

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

## Snapshot divergence is intentional

Lihaaf and trybuild normalize diagnostic path prefixes differently:

- **trybuild** writes the relative crate path: `--> tests/compile_fail/<name>.rs:L:C`.
- **lihaaf** writes the placeholder form: `--> $DIR/<name>.rs:L:C`.

This is harness-format noise — the underlying diagnostic semantics are
identical, and lihaaf v0.1 spec §9.2 anticipates exactly one re-bless
when an adopter migrates off trybuild. Phase 8.5 takes the path that
keeps the snapshots **physically separate** so neither corpus
overwrites the other when the developer runs `--bless` against either
harness.

Concretely, lihaaf-owned `.stderr` snapshots written into
`tests/lihaaf/compile_fail/` are **gitignored** (see the local
`.gitignore`) and regenerated locally with:

```bash
cargo run \
    --manifest-path ~/projects/lihaaf/Cargo.toml \
    --bin cargo-lihaaf \
    -- lihaaf \
    --manifest-path $(pwd)/Cargo.toml \
    --filter compile_fail \
    --bless \
    -j 4
```

Bless writes are constrained to this directory by construction: lihaaf
computes the snapshot path from the fixture path-as-discovered
(`tests/lihaaf/compile_fail/<name>.rs`), and the snapshot writer does
not canonicalize through the symlink.

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

`tests/trybuild_tests.rs` and `tests/trybuild_spatial_tests.rs` continue
to drive the same source files with trybuild's own normalization and
committed `.stderr` snapshots. Phase 8.5 does **not** remove or rewrite
either harness — trybuild stays the publicly committed corpus and CI
gate, lihaaf is the new fast path.

## Validation reference

The historical validation from the earlier in-repo wiring commit
(reference only, not the integration base): 237 fixtures discovered;
compile_pass 99 OK in 10.7s; compile_fail bless + re-run 138 OK in
11.8s; full post-bless sweep 237 OK in 16.5s. Recapture locally on
this branch with a post-fix lihaaf checkout:

```bash
# Enumerate
cargo run --manifest-path ~/projects/lihaaf/Cargo.toml --bin cargo-lihaaf \
    -- lihaaf --manifest-path $(pwd)/Cargo.toml --list | wc -l   # expect 237

# Compile-pass sweep (no bless side-effects — compile_pass has no .stderr)
cargo run --manifest-path ~/projects/lihaaf/Cargo.toml --bin cargo-lihaaf \
    -- lihaaf --manifest-path $(pwd)/Cargo.toml --filter compile_pass -j 4

# Compile-fail bless (writes into tests/lihaaf/compile_fail/*.stderr only,
# never tests/compile_fail/*.stderr; gitignored)
cargo run --manifest-path ~/projects/lihaaf/Cargo.toml --bin cargo-lihaaf \
    -- lihaaf --manifest-path $(pwd)/Cargo.toml --filter compile_fail --bless -j 4
```
