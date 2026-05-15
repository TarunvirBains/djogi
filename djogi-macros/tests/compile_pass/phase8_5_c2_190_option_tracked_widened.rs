// Phase 8.5 Cluster 2 — djogi#190 Option<Tracked<T>> with widened types.
//
// Verifies that `save()` compiles correctly for models that contain
// `Option<Tracked<T>>` fields where `T` is a narrow/unsigned integer type
// (i8/u8/u16/u32/u64).
//
// Save behaviour for Option<Tracked<T>>:
// - The field is emitted UNCONDITIONALLY on every save() call. Unlike
//   `Tracked<T>` (which skips the field when not dirty) or `Tracked<Option<T>>`
//   (which detects any assignment including None ↔ Some transitions),
//   `Option<Tracked<T>>` cannot distinguish None→Some(clean) or Some→None
//   transitions from a dirty-check on the inner Tracked alone. Emitting
//   unconditionally is always correct and prevents silent data loss.
// - If full dirty-tracking of optional fields is needed — i.e. the save should
//   only include the field when the Option state OR the inner value has changed
//   — use `Tracked<Option<T>>` instead (Tracked as the outermost wrapper).
//
// This fixture exercises all five narrow/unsigned types in both:
//   - `Option<Tracked<T>>` — nullable unconditionally-emitted widened field
//   - `Tracked<T>` — non-nullable dirty-tracked widened field (pre-existing, regression guard)
// The fixture also includes a direct-typed `Option<Tracked<String>>` to
// confirm the non-widened path still compiles.

use djogi::prelude::*;

// ── Option<Tracked<T>> with widened types ────────────────────────────────────

#[model(table = "opt_tracked_widened", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OptTrackedWidened {
    // Option<Tracked<narrow/unsigned>> — the cases that previously failed.
    pub maybe_signed_byte: Option<Tracked<i8>>,
    pub maybe_unsigned_byte: Option<Tracked<u8>>,
    pub maybe_unsigned_short: Option<Tracked<u16>>,
    pub maybe_unsigned_int: Option<Tracked<u32>>,
    pub maybe_unsigned_long: Option<Tracked<u64>>,
    // Option<Tracked<direct>> — regression guard for the non-widened path.
    pub maybe_label: Option<Tracked<String>>,
    // Non-optional Tracked for the direct comparison.
    pub required_count: Tracked<u32>,
    pub label: String,
}

// ── Mixed widened and non-widened Tracked variants ───────────────────────────

#[model(table = "opt_tracked_mixed", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OptTrackedMixed {
    // Mix of Tracked<T>, Option<Tracked<T>>, Option<T>, and plain T.
    pub tracked_u8: Tracked<u8>,
    pub opt_tracked_u16: Option<Tracked<u16>>,
    pub opt_u32: Option<u32>,
    pub plain_u64: u64,
    pub label: String,
}

fn main() {}
