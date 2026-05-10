# Inventory-on-dylib spike

Date: 2026-05-10
Spike worktree: `.claude/worktrees/agent-a77d4c74b4a99e771`
Spike target dirs: `/home/tarunvir/lihaaf-spike-target`,
`/home/tarunvir/lihaaf-spike-fixture-target`,
`/home/tarunvir/lihaaf-spike-target-noprefer`,
`/home/tarunvir/lihaaf-spike-dlopen-target`
Toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`, `cargo 1.95.0`,
LLVM 22.1.2, Linux x86_64 (WSL2 6.6.87.2-microsoft-standard).
`inventory` crate: `0.3.24` (workspace pin: `inventory = "0.3"`).

## Verdict

**`GO: cargo rustc --crate-type=dylib override works AND inventory propagates across dylib boundary`**

The override path is viable. Lihaaf and Phase 9 can build djogi as a
dylib via a per-invocation `cargo rustc -p djogi --lib --crate-type=dylib`
plus `RUSTFLAGS="-C prefer-dynamic"` (when the consumer is also link-time,
not `dlopen`). No change to `djogi/Cargo.toml` is required; existing
`cargo build` consumers (KindNudge, application crates) continue to
receive the rlib at zero extra build cost.

Two operational notes:

1. **`prefer-dynamic` is required when the fixture is linked at compile
   time.** Without it, the dylib statically embeds stdlib and the
   fixture's own stdlib rlibs collide with rustc's "cannot satisfy
   dependencies so `std` only shows up once" error. Both djogi and the
   fixture must be built with `-C prefer-dynamic`. With the flag, both
   share the rust toolchain's `libstd-<hash>.so` from
   `<sysroot>/lib/rustlib/<target>/lib/`.
2. **For `dlopen`-based plugin loading (Phase 9 / `rhai-dylib`), the
   no-`prefer-dynamic` dylib is more deployment-friendly** because it
   bakes stdlib in (single self-contained `.so`, no `LD_LIBRARY_PATH`
   gymnastics). If the runtime executable is also Rust and built with
   the matching toolchain, prefer-dynamic remains a fine choice and
   saves disk.

## Q1 results — building djogi as a dylib

| # | Invocation                                                    | Manifest change | Profile | Worked | Build wall (clean) | Output path                                      | Output size |
|---|---------------------------------------------------------------|-----------------|---------|--------|--------------------|--------------------------------------------------|-------------|
| 1a | `cargo rustc -p djogi --lib --release --crate-type=dylib`     | none            | release | yes    | 20.80 s            | `<target>/release/libdjogi.so`                   | 22 760 544 B (~21.7 MiB) |
| 1b | `cargo rustc -p djogi --lib --release --crate-type=dylib` with `RUSTFLAGS="-C prefer-dynamic"` | none | release | yes    | 21.06 s (after RUSTFLAGS-triggered full rebuild) | `<target>/release/libdjogi.so`                   | 21 329 800 B (~20.3 MiB) |
| 1c | `cargo rustc -p djogi --lib --crate-type=dylib` with `RUSTFLAGS="-C prefer-dynamic"` (debug) | none | debug   | yes    | 16.90 s            | `<target>/debug/libdjogi.so`                     | 118 090 136 B (~112.6 MiB) |
| 2  | `cargo build -p djogi --release --lib` with `RUSTFLAGS="-C prefer-dynamic --crate-type=dylib"` | none            | release | **no** | n/a (failed in <1 s) | n/a                                              | n/a |
| 3  | `cargo build -p djogi --release --lib` after adding `[lib] crate-type = ["lib", "dylib"]` to `djogi/Cargo.toml`, with `RUSTFLAGS="-C prefer-dynamic"` | yes             | release | yes    | 22.15 s (full build of fixture crate too)        | `<fixture-target>/release/deps/libdjogi.so` and `libdjogi.rlib` (both produced) | dylib 21 329 800 B; rlib 19 322 456 B |

Path 2 fails because `RUSTFLAGS` applies to *every* crate in the build
graph, including build scripts (`bin` crates) and proc-macro crates
(must be `proc-macro` crate-type), and including stdlib (which can't be
rebuilt as `dylib` from a stable toolchain). The first errors are
`error: cannot mix bin crate type with others` and
`#[panic_handler] function required, but not found`. Path 2 is not a
viable invocation.

For Path 1, the dylib is also hard-linked to
`<target>/release/deps/libdjogi.so` (same inode, same byte content),
which is where downstream rustc invocations naturally find it via
`-L <target>/release/deps`.

For Path 3, cargo writes BOTH the dylib AND the rlib because `[lib]
crate-type = ["lib", "dylib"]` requests both. The rlib carries no
extra cost on consumers who only want the rlib (cargo will read the
rmeta they want), but every `cargo build` of djogi itself does extra
work to emit both artifacts (the LLVM passes are similar but the
linker runs twice).

The 20–22 s clean-build figures are dominated by the dependency graph
(~140 crates including tokio-postgres, deadpool, sassi, heeranjid, the
darling/serde/figment chain). On an incremental rebuild touching only
djogi/src, the dylib re-emit drops to ~10–11 s. Both wall figures are
release; debug shaves a few seconds off (16.9 s clean) but the dylib
is ~5× larger.

## Q2 results — inventory propagation across the dylib boundary

### Submission setup

The only `inventory::submit!` inside `djogi/src/` (in
`relation/registry.rs:675`) is `#[cfg(test)]`-gated, so a release dylib
of djogi has zero pre-registered items for any inventory type — there
is nothing to enumerate without a real adopter model crate. To prove
cross-boundary propagation conclusively, the spike adds one
non-test-gated `inventory::submit!` for `SassiBootHook` inside
`djogi/src/cache/boot.rs` (right after the `inventory::collect!`
declaration). The added submission registers a no-op `_lihaaf_spike_register`
function. **This change is throwaway and is not committed.** A note
flagging it for removal sits next to the submission.

The fixture additionally submits its own `_fixture_register` to verify
that the fixture binary's `.init_array` constructors deposit into the
same registry that djogi.so owns.

### Path 1a/1b results (cargo rustc + manual rustc fixture link)

Built djogi with `cargo rustc -p djogi --lib --release --crate-type=dylib`
(plus `RUSTFLAGS="-C prefer-dynamic"`), then linked a manual fixture
with `rustc -C prefer-dynamic --extern djogi=<libdjogi.so> -L <deps>`.

Two fixture variants:

- `fixture-manual.rs` — only iterates, no fixture-side submission.
  Expected count ≥ 1.
- `fixture-manual-bidir.rs` — submits `_fixture_register` and iterates.
  Expected count ≥ 2.

Run with
`LD_LIBRARY_PATH=<target>/release:<target>/release/deps:<sysroot>/lib/rustlib/<target>/lib`:

```
$ ./fixture-manual
LIHAAF_SPIKE_TOTAL=1
LIHAAF_SPIKE_VERDICT=DJOGI_SUBMISSION_VISIBLE
EXIT=0

$ ./fixture-manual-bidir
LIHAAF_SPIKE_TOTAL=2
LIHAAF_SPIKE_EXPECTED=2
LIHAAF_SPIKE_VERDICT=BOTH_SUBMISSIONS_VISIBLE
EXIT=0
```

### Path 1c results (debug)

Same fixture rebuilt against the debug dylib via the debug deps dir:

```
$ ./fixture-manual-bidir-debug
LIHAAF_SPIKE_TOTAL=2
LIHAAF_SPIKE_EXPECTED=2
LIHAAF_SPIKE_VERDICT=BOTH_SUBMISSIONS_VISIBLE
EXIT=0
```

Behavior is consistent across release and debug.

### Path 3 results (manifest + cargo build)

Built the `lihaaf-spike-fixture` crate (a tiny Cargo crate that depends
on djogi and sassi by path) with djogi modified to declare
`[lib] crate-type = ["lib", "dylib"]` and `RUSTFLAGS="-C prefer-dynamic"`:

```
$ ./fixture
LIHAAF_SPIKE_TOTAL=2
LIHAAF_SPIKE_EXPECTED_AT_LEAST=2
LIHAAF_SPIKE_VERDICT=GO
EXIT=0
```

Both djogi-internal and fixture-internal submissions visible.

### Symbol-table inspection

The dylib's `.init_array` section carries the `inventory::submit!`
constructor:

```
$ objdump -h /home/tarunvir/lihaaf-spike-target/release/libdjogi.so | grep init_array
 19 .init_array   00000010  0000000000643c58  0000000000643c58  00641c58  2**3
```

`0x10 = 16 bytes` = two function pointers (rust runtime init + the
inventory ctor for `_lihaaf_spike_register`). `nm -D` shows zero
`inventory`-named exported symbols, which is correct: the inventory
machinery uses `.init_array` ctor pointers (no public symbols), and
`T::registry()` is a reachable `pub fn` only via Rust's name-mangled
mangling inside the dylib — fixture rustc resolves it through the
crate metadata, not via dlsym.

### dlopen viability (Phase 9 plugin loader)

The `dlopen-test` Cargo crate calls `libloading::Library::new(...)` on
`libdjogi.so`:

```
$ ./dlopen-test /home/tarunvir/lihaaf-spike-target/release/libdjogi.so
DLOPEN_PATH=/home/tarunvir/lihaaf-spike-target/release/libdjogi.so
DLOPEN_VERDICT=OK
EXIT=0

$ ./dlopen-test /home/tarunvir/lihaaf-spike-target-noprefer/release/libdjogi.so
DLOPEN_PATH=/home/tarunvir/lihaaf-spike-target-noprefer/release/libdjogi.so
DLOPEN_VERDICT=OK
EXIT=0
```

dlopen succeeds against both prefer-dynamic and non-prefer-dynamic
dylibs. The non-prefer dylib opens cleanly without `LD_LIBRARY_PATH`
because it bakes stdlib in.

This confirms that **`.init_array` constructors run at dlopen time**,
which matches the inventory crate's README: *"Elements brought in by a
dynamically loaded library are registered at the time that dlopen
occurs."* For Phase 9 plugin loading, an adopter could ship a model
crate as a dylib that, when `dlopen`'d by the Rhai shell, deposits its
`#[derive(Model)]`-generated `inventory::submit!` entries into djogi's
registry.

A caveat for the Phase 9 case (not exercised by this spike): the
dlopen-loader binary must itself link djogi at compile time (so that
the registry static lives somewhere reachable), and the dlopen'd
plugin must reference the exact same djogi dylib (not a re-statically-
linked copy). Otherwise the plugin's submissions go to a different
`static REGISTRY` and the loader can't see them. This is the standard
shared-library-with-singleton constraint and is solved by the same
`-C prefer-dynamic` discipline used throughout.

### Toolchain breadth

The spike ran against `rustc 1.95.0 (59807616e 2026-04-14)` only.
Available alternates on this box are `nightly-1.94.0` and a
hardpinned `1.95.0` — both either older than djogi's MSRV (`1.95`,
nightly is `1.94.0`) or identical to stable. No useful alternate
toolchain. The spike does NOT verify behavior on macOS, Windows, or
musl. The inventory crate uses platform-specific `#[link_section]`
attributes (`.init_array` on Linux/Android/BSD,
`__DATA,__mod_init_func,mod_init_funcs` on macOS/iOS,
`.CRT$XCU` on Windows). All three rely on the same OS-loader
"run constructors when the binary/library loads" mechanism. The
inventory README claims platform support including
"Linux, macOS, iOS, FreeBSD, Android, Windows, and various
others" — so we expect parity, but lihaaf and Phase 9 should add
a per-platform smoke test if any of them target macOS or Windows.

## Recommendations

- **Recommended dylib build invocation for lihaaf** (compile-time link):
  `RUSTFLAGS="-C prefer-dynamic" cargo rustc -p djogi --lib --release --crate-type=dylib`
  in a dedicated CARGO_TARGET_DIR (do NOT share with the project's
  normal `target/`, since RUSTFLAGS toggling triggers full rebuilds of
  every crate in the graph). Lihaaf's per-fixture rustc invocation
  must also pass `-C prefer-dynamic` and `--extern djogi=<libdjogi.so>`
  with `-L <deps-dir>` so transitive crates resolve.
- **Recommended dylib build invocation for Phase 9 shell**
  (`dlopen` of djogi for `rhai-dylib`):
  `cargo rustc -p djogi --lib --release --crate-type=dylib` (NO
  `-C prefer-dynamic`) so the dylib is self-contained and dlopen-able
  without sysroot library-path setup. The shell binary itself can be
  built normally (linking djogi as rlib at compile time); plugin
  crates that `dlopen` into the same shell must be built as dylibs
  and reference the exact same djogi dylib (not a re-link).
- **Whether the djogi `[lib]` PR is needed: NO** for the lihaaf use
  case and Phase 9 link-time consumers. The override path keeps
  zero-impact on KindNudge and other application consumers. The
  manifest-change path remains a viable fallback if a future use case
  needs cargo-driven dylib resolution (e.g., if lihaaf grows a Cargo
  metadata phase that needs djogi to advertise dylib in its manifest)
  — but this spike has no such use case in scope.
- **Platform caveats observed:**
  - Linux x86_64 only verified. macOS / Windows / iOS / Android
    expected to work per the inventory crate's platform support
    matrix, but lihaaf and Phase 9 should ship per-platform smoke
    tests if those targets are in their support set.
  - `LD_LIBRARY_PATH` plumbing is required for the prefer-dynamic
    dylib at run time (must include both the deps dir holding
    `libdjogi.so` and the rust toolchain's
    `<sysroot>/lib/rustlib/<target>/lib/` for `libstd-<hash>.so`).
    Lihaaf's harness should set this automatically.
  - In a shared CARGO_TARGET_DIR, alternating between
    `cargo rustc --crate-type=dylib` (with `RUSTFLAGS="-C prefer-dynamic"`)
    and a normal `cargo build` will trigger a full rebuild of every
    crate in the graph because `RUSTFLAGS` is part of the cargo
    fingerprint. Use a dedicated lihaaf target dir to avoid thrashing
    the developer's normal `cargo test` loop.

## Reproduction

Prerequisites: this spike worktree (or any djogi worktree with the same
sassi-reference symlink and a djogi/src tree that contains at least one
non-`#[cfg(test)]` `inventory::submit!`), `rustc 1.95+`, `cargo 1.95+`,
no special toolchain features required.

```bash
# 0. Ensure sassi-reference symlink exists in the worktree
ln -s ../../../../sassi <worktree>/sassi-reference

# 1. Add a one-line non-test inventory::submit! inside djogi/src/cache/boot.rs
#    right after `inventory::collect!(SassiBootHook);` (the spike's marker).
#    Without this, the release dylib has zero pre-registered SassiBootHook
#    items and Q2 cannot demonstrate cross-boundary propagation.
#    See djogi/src/cache/boot.rs in this worktree for the exact 5-line block.

# 2. Build djogi as a dylib (Path 1, prefer-dynamic, release)
RUSTFLAGS="-C prefer-dynamic" \
CARGO_TARGET_DIR=/home/tarunvir/lihaaf-spike-target \
  cargo rustc -p djogi --lib --release --crate-type=dylib

# 3. Locate the sassi rlib hash (varies per build)
SASSI_RLIB=$(ls /home/tarunvir/lihaaf-spike-target/release/deps/libsassi-*.rlib | head -1)

# 4. Build the spike fixture (manual rustc, bi-directional)
cd /tmp/lihaaf-spike
rustc --edition 2024 \
  -C prefer-dynamic \
  -L /home/tarunvir/lihaaf-spike-target/release/deps \
  --extern djogi=/home/tarunvir/lihaaf-spike-target/release/libdjogi.so \
  --extern sassi="$SASSI_RLIB" \
  fixture-manual-bidir.rs -o fixture-manual-bidir

# 5. Run with library paths set
LD_LIBRARY_PATH=/home/tarunvir/lihaaf-spike-target/release:/home/tarunvir/lihaaf-spike-target/release/deps:/home/tarunvir/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/lib \
  ./fixture-manual-bidir

# Expected:
#   LIHAAF_SPIKE_TOTAL=2
#   LIHAAF_SPIKE_EXPECTED=2
#   LIHAAF_SPIKE_VERDICT=BOTH_SUBMISSIONS_VISIBLE
#   exit code 0

# 6. (optional) dlopen check — build a no-prefer-dynamic dylib and dlopen it
CARGO_TARGET_DIR=/home/tarunvir/lihaaf-spike-target-noprefer \
  cargo rustc -p djogi --lib --release --crate-type=dylib
cd /tmp/lihaaf-spike/dlopen-test
CARGO_TARGET_DIR=/home/tarunvir/lihaaf-spike-dlopen-target \
  cargo build --release
./target/release/dlopen-test /home/tarunvir/lihaaf-spike-target-noprefer/release/libdjogi.so
# Expected: DLOPEN_VERDICT=OK, exit 0

# 7. Cleanup (the spike target dirs and /tmp/lihaaf-spike are throwaway)
rm -rf /home/tarunvir/lihaaf-spike-target \
       /home/tarunvir/lihaaf-spike-target-noprefer \
       /home/tarunvir/lihaaf-spike-fixture-target \
       /home/tarunvir/lihaaf-spike-dlopen-target \
       /tmp/lihaaf-spike
```

## Spike artifacts (in this worktree)

- `djogi/src/cache/boot.rs` — augmented with a 5-line `inventory::submit!`
  marker block. **Throwaway. Revert before merging anything from this
  worktree.** Search for `LIHAAF SPIKE` to find the marker.
- This research note (`docs/research/2026-05-10-inventory-on-dylib-spike.md`).

The Cargo.toml-modification for Path 3 was applied during the run and
then reverted; `git status` on this worktree should show only the
boot.rs change and the new research note.

## Open questions / what was NOT tested

- **macOS, Windows, musl, iOS, Android targets.** Inventory's README
  claims support; this spike did not exercise it.
- **Symbol-table behavior under LTO.** This spike used the workspace's
  default profile.release settings (no extra LTO flags). Phase 9 might
  want to verify that thin/fat LTO does not strip the `.init_array`
  constructor or merge it across crates in a way that breaks
  propagation.
- **`#[derive(Model)]`-generated submissions through the macro
  expansion path** (vs. the hand-written submission in this spike).
  The expansion path uses `::djogi::__private::inventory::submit!`,
  not `inventory::submit!` directly; it routes through the same
  macro and should behave identically, but lihaaf's first real
  fixture (an actual `#[model]` struct compiled into a separate crate
  that the fixture links) is the conclusive end-to-end test.
- **Multiple dylibs depending on djogi.** If two plugin crates both
  call `inventory::submit!` and both are built as dylibs and both are
  linked into the same fixture, do their submissions all reach
  djogi's single static registry? This is the standard inventory
  guarantee and we have no reason to doubt it, but lihaaf's spec
  may want to lock it down with a fixture before users discover it.
