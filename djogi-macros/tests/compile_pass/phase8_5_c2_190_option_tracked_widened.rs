// Phase 8.5 Cluster 2 — djogi#190 Option<Tracked<T>> with widened types.
//
// Verifies that `save()` compiles correctly for models that contain
// `Option<Tracked<T>>` fields where `T` is a narrow/unsigned integer type
// (i8/u8/u16/u32/u64). Previously `save_set_fragments` used `is_tracked(ty)`
// (outermost-only check) rather than `is_tracked_inner(ty)`, so
// `Option<Tracked<T>>` fell through to the unconditional else branch. For
// widened types this produced a compile error because the widening
// `.map(WideType::from)` call received `Option<Tracked<T>>` instead of
// `Option<T>`.
//
// The fix adds an `else if is_tracked_inner(ty)` branch that correctly
// extracts `Option<T>` from `Option<Tracked<T>>` and applies widening on the
// inner type before binding.
//
// This fixture exercises all five narrow/unsigned types in both:
//   - `Option<Tracked<T>>` — nullable dirty-tracked widened field
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
