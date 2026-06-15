# Linker Spike Results — Issue #370: Adopter/CLI Descriptor-Provider Boundary

## Purpose

Determine how the Rust linker handles `inventory::submit!` registrations across crate boundaries. Specifically: does referencing ONE model type's descriptor force ALL models in that crate to survive linking?

## Fixture Structure

```
crates/tracker/ — two models: Elephant, Herd (both #[model])
crates/billing/ — one model: Invoice (#[model])

bin_one_ref/  — depends on tracker + billing; references ONLY Elephant::descriptor
bin_all_ref/  — depends on tracker + billing; references BOTH Elephant + Herd descriptors
bin_no_ref/  — depends on billing only; references NOTHING from billing
```

Each bin iterates `inventory::iter::<ModelDescriptor>()` and prints all registered model descriptors, grouped by crate (filtered via table_name prefix).

## Raw Output

### bin_one_ref (debug) — references ONLY Elephant from tracker

```
Total descriptors: 2

tracker crate models (table_name starts with 'tracker_'):
 Elephant -> tracker_elephant
 Herd -> tracker_herd
billing crate models (table_name starts with 'billing_'):
 (none)

All descriptors:
 Elephant -> tracker_elephant
 Herd -> tracker_herd
```

**Observation:** Both Elephant AND Herd appear, despite only referencing Elephant.
The billing crate contributes zero descriptors (it is a dependency but unreferenced).

### bin_all_ref (debug) — references BOTH Elephant and Herd from tracker

```
Total descriptors: 2

tracker crate models (table_name starts with 'tracker_'):
 Elephant -> tracker_elephant
 Herd -> tracker_herd
billing crate models (table_name starts with 'billing_'):
 (none)

All descriptors:
 Elephant -> tracker_elephant
 Herd -> tracker_herd
```

**Observation:** Same result as bin_one_ref. Both tracker models present. Billing absent.

### bin_no_ref (debug) — references NOTHING from billing

```
Total descriptors: 0

billing crate models (table_name starts with 'billing_'):
 (none — billing crate was dropped by linker)

All descriptors:
 (none)
```

**Observation:** Zero descriptors. The billing crate was entirely dropped by the linker.

### bin_one_ref (release + fat LTO, CARGO_PROFILE_RELEASE_LTO=fat)

```
Total descriptors: 2

tracker crate models (table_name starts with 'tracker_'):
 Elephant -> tracker_elephant
 Herd -> tracker_herd
billing crate models (table_name starts with 'billing_'):
 (none)

All descriptors:
 Elephant -> tracker_elephant
 Herd -> tracker_herd
```

**Observation:** Identical to debug. Fat LTO does not change the behavior.

### bin_all_ref (release + fat LTO, CARGO_PROFILE_RELEASE_LTO=fat)

```
Total descriptors: 2

tracker crate models (table_name starts with 'tracker_'):
 Elephant -> tracker_elephant
 Herd -> tracker_herd
billing crate models (table_name starts with 'billing_'):
 (none)

All descriptors:
 Elephant -> tracker_elephant
 Herd -> tracker_herd
```

**Observation:** Identical to debug. Fat LTO does not change the behavior.

### bin_no_ref (release + fat LTO, CARGO_PROFILE_RELEASE_LTO=fat)

```
Total descriptors: 0

billing crate models (table_name starts with 'billing_'):
 (none — billing crate was dropped by linker)

All descriptors:
 (none)
```

**Observation:** Identical to debug. Fat LTO does not change the behavior.

## CI Profile Settings

The djogi CI (`.github/workflows/ci.yml`) uses:
- `cargo build --workspace` — debug profile, no LTO
- `cargo test --workspace` — debug profile, no LTO
- No `[profile.release]` or LTO settings in workspace Cargo.toml
- MSRV job runs `cargo check --workspace` only (no tests)

The fixture was tested against:
1. Debug build (matches CI exactly)
2. Release with default settings (no LTO, default codegen-units=16)
3. Release with fat LTO (via CARGO_PROFILE_RELEASE_LTO=fat env var)

**All configurations produced identical results.**

## Answers

### Does ONE reference per crate force ALL models in that crate?

**YES.** Referencing a single model type's `descriptor` method (e.g., `<Elephant as Model>::descriptor`) causes the entire tracker crate object code to be linked, including ALL `inventory::submit!` statics from that crate. Herd's descriptor appears alongside Elephant's even though only Elephant is referenced.

### Does this hold under release/LTO?

**YES.** Fat LTO produces identical results to debug builds. The linker still includes the entire crate's object code when any symbol from it is referenced.

### How many types must `djogi_main!` require: one per crate or every model type?

**One type per crate is sufficient.** Referencing a single model type from each adopter crate forces all inventory submissions in that crate to survive linking. There is no need to enumerate every model type individually.

The `djogi_main!` macro only needs to require the adopter to provide **one representative type per model crate** (e.g., via a single reference to any model's descriptor method from that crate).

### Is a per-crate `link_anchor!()` needed as fallback?

**NO, not for the standard case.** The "one type per crate" pattern is robust across all tested build profiles (debug, release, release + fat LTO). A `link_anchor!()` macro would be unnecessary overhead.

There is one edge case where it MIGHT matter: if an adopter crate contains models but the adopter's binary never references any model from that specific crate (the bin_no_ref scenario). In that case, the linker drops the entire crate and its inventory submissions vanish. This is expected and correct behavior — if you don't use a crate, its models shouldn't register.

For the `djogi_main!` contract: requiring one type per crate covers this edge case naturally. If the adopter lists a crate in `djogi_main!`, at least one type from it is referenced, ensuring all its inventory survives.

## Mechanism Explanation

`inventory::submit!(ModelDescriptor {... })` expands to a static variable placed in a special linker section (`.rodata.inventory`). The `inventory::iter::<T>()` function iterates over this section at runtime using linker-defined start/end symbols.

The key insight is that the Rust linker operates at the **crate level**, not the individual static level, under normal conditions:

1. When any symbol from a crate (e.g., `tracker`) is referenced by the final binary, the entire crate's object code is linked into the binary.
2. This includes ALL `inventory::submit!` statics in that crate, regardless of whether each specific model type is individually referenced.
3. If NO symbol from a crate is referenced, the entire crate is dropped — its inventory items never appear.

Fat LTO does not change this because LTO operates after the initial crate-level linking decision. Once a crate's object code is included (because one of its symbols was referenced), LTO may optimize within that code but doesn't selectively drop individual inventory statics — they're all in the same linker section and the section boundaries are preserved.

## Implications for `djogi_main!` Design

The macro should accept a list of **representative types**, one per model crate:

```rust
// The adopter provides one type per crate containing models.
// This forces all inventory submissions in each crate to survive linking.
djogi_main!(tracker::Elephant, billing::Invoice);
```

This is simpler than requiring every model type individually and provably sufficient under all tested Rust compilation profiles.
