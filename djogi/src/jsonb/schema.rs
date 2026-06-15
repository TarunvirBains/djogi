//! `JsonbSchema` — typed compile-time path tree for JSONB columns.
//! # What
//! [`JsonbSchema`] is a marker trait that opts a struct into the typed
//! JSONB path API. Implementing it (via `#[derive(JsonbSchema)]`) causes
//! the proc macro to emit a `{T}Path<M>` struct with one method per field:
//! - Scalar fields (from the cast-matrix allowlist: `i16`, `i32`, `i64`,
//! `f32`, `f64`, `bool`, `String`, `time::OffsetDateTime`, etc.) return a
//! [`JsonbPathRef<M, FieldType>`](crate::jsonb::JsonbPathRef) that carries
//! the accumulated JSONB path and exposes `eq` / `gt` / `lt` /... comparisons.
//! - Nested fields whose types also implement `JsonbSchema` return the nested
//! type's `Path<M>`, extending the path accumulator by one segment.
//! # Why
//! The flat escape hatch (`FieldRef<M, Jsonb<T>>::path::<V>("a.b.c")`) is
//! correct but bypasses the type system — a string typo produces a silent
//! runtime miss. `JsonbSchema` brings the path into the type system so
//! `f.specs.typed().engine.cylinders.gt(4)` compiles only when `VehicleSpecs`
//! has an `engine: EngineSpecs` field and `EngineSpecs` has a `cylinders: i32`
//! field.
//! # Deref vs `.typed()` — decision
//! The plan considered a `Deref<Target = T::Path<M>>` impl on
//! `FieldRef<M, Jsonb<T>>`. Deref requires returning `&T::Path<M>`, which
//! means storing the path tree inside `FieldRef` or using a thread-local. Both
//! approaches add runtime storage to a `Copy` ZST, harming the zero-overhead
//! design goal.
//! The explicit `.typed()` method avoids any storage:
//! ```ignore
//! f.specs.typed().engine.cylinders.gt(4)
//! ```
//! The extra `.typed()` call also makes the schema boundary visible to readers,
//! and the flat escape hatch (`f.specs().path::<i32>("engine.cylinders")`) is
//! still available for dynamic paths or types not covered by the allowlist.
//! # How to opt in
//! ```rust
//! use djogi::JsonbSchema;
//! use serde::{Serialize, Deserialize};
//!
//! #[derive(JsonbSchema, Serialize, Deserialize, Default)]
//! pub struct EngineSpecs {
//!  pub cylinders: i32,
//!  pub displacement_cc: f32,
//! }
//!
//! #[derive(JsonbSchema, Serialize, Deserialize, Default)]
//! pub struct VehicleSpecs {
//!  pub engine: EngineSpecs,
//!  pub weight_kg: f32,
//! }
//! ```
//! Then, with a model that has `specs: Jsonb<VehicleSpecs>`, you can write:
//! ```ignore
//! Vehicle::objects()
//! .filter(|f| f.specs().typed().engine.cylinders.gt(4))
//! .fetch_all(&mut ctx).await?
//! ```
//! This emits: `(specs->'engine'->>'cylinders')::int4 > $1`

use crate::model::Model;
use std::collections::HashMap;
use std::sync::Mutex;

// ── String path interner ─────────────────────────────────────────────────────
// Typed path construction (`{T}Path<M>` methods) must produce
// `JsonbPathRef<M, V>` which carries a `&'static str` for the dotted path.
// Since the set of unique paths in a compiled binary is bounded (one per
// leaf field per schema type), leaking each unique dotted string once is
// acceptable — the total allocation is proportional to the number of
// distinct path queries in the program, not the number of rows.
// The interner is a `Mutex<HashMap>` rather than a thread-local because
// `JsonbPathRef` must be `Send` (it escapes the filter closure into the
// async executor). A thread-local would give a different reference for the
// same path string on each thread, which would be incorrect for
// multi-threaded use.
// # Performance
// The lock is taken once per unique `(column, path_segments)` combination
// at filter-build time. Identical paths on subsequent calls are served by
// the interner without allocation (they hit the HashMap entry and return the
// already-leaked `&'static str`). For single-threaded access (the overwhelm-
// ingly common case for filter closures), the lock is always uncontended.
static PATH_INTERNER: Mutex<Option<HashMap<String, &'static str>>> = Mutex::new(None);

// ── Segment-slice interner ────────────────────────────────────────────────────
// The `__new_from_slice` method on derived `{T}Path<M>` types needs a
// `&'static [&'static str]` slice of path segments. Without interning,
// every call to a nested accessor would `Box::leak` a fresh slice, producing
// unbounded allocation growth proportional to query volume rather than
// schema shape — a memory leak in long-running web servers.
// `intern_path_slice` caches slices keyed by their content (as a
// `Vec<&'static str>`), so the same path traversal reuses the identical
// slice on every call. Growth is bounded by unique paths, same as
// `PATH_INTERNER` above.
static PATH_SLICE_INTERNER: Mutex<Option<HashMap<Vec<&'static str>, &'static [&'static str]>>> =
    Mutex::new(None);

/// Intern a dotted JSONB path string.
/// Joins `segments` with `.` to form the dotted path string, then either
/// returns the previously-interned `&'static str` for that string, or
/// leaks a new `Box<str>` and stores the reference in the global interner.
/// # Panics
/// Panics if the interner mutex is poisoned (which only happens if a previous
/// holder panicked while holding the lock — an unrecoverable situation).
/// # Validity
/// Each segment must satisfy the plain-identifier rules enforced by
/// `crate::jsonb::path::is_plain_ident` — the derive macro only emits calls
/// to `intern_path` with literal field name strings, so this is guaranteed
/// at derive time. The resulting dotted string is validated by
/// `JsonbPathRef::new` anyway.
#[doc(hidden)]
pub fn intern_path(segments: &[&'static str]) -> &'static str {
    let dotted = segments.join(".");
    let mut guard = PATH_INTERNER
        .lock()
        .expect("djogi: PATH_INTERNER mutex poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&existing) = map.get(&dotted) {
        return existing;
    }
    // Leak the string to produce a `&'static str`.
    let leaked: &'static str = Box::leak(dotted.clone().into_boxed_str());
    map.insert(dotted, leaked);
    leaked
}

/// Intern a `&'static [&'static str]` segment slice.
/// Returns the previously-interned slice for the given segment sequence, or
/// leaks a new `Box<[&'static str]>` on first encounter and caches it.
/// # Why this is needed
/// `{T}Path<M>::__new` carries a `&'static [&'static str]` base_path. Without
/// interning, every call to a nested accessor would `Box::leak` a fresh slice,
/// producing unbounded allocation growth proportional to query-build volume.
/// With interning, the slice is allocated once per unique segment sequence,
/// bounding growth by the number of distinct JSONB paths in the program.
/// # Panics
/// Panics if the interner mutex is poisoned.
/// # Pointer stability
/// Two calls with equal segment sequences return the same `&'static` pointer
/// pointer equality can be used in tests to confirm caching is active.
#[doc(hidden)]
pub fn intern_path_slice(segments: &[&'static str]) -> &'static [&'static str] {
    let key: Vec<&'static str> = segments.to_vec();
    let mut guard = PATH_SLICE_INTERNER
        .lock()
        .expect("djogi: PATH_SLICE_INTERNER mutex poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&existing) = map.get(&key) {
        return existing;
    }
    let leaked: &'static [&'static str] = Box::leak(key.clone().into_boxed_slice());
    map.insert(key, leaked);
    leaked
}

/// Marker trait for structs that participate in the typed JSONB deep-path API.
/// Implementing this trait (via `#[derive(JsonbSchema)]`) causes the proc macro
/// to emit a `{T}Path<M>` struct providing one method per field. Scalar fields
/// (from the cast-matrix allowlist) return a
/// [`JsonbPathRef<M, V>`](crate::jsonb::JsonbPathRef) ready for comparison;
/// nested fields return the nested type's `Path<M>` with the path accumulator
/// extended by one segment.
/// # Implementing manually
/// Manual implementation is possible but strongly discouraged — the derive
/// handles path-accumulator bookkeeping correctly. If you implement manually,
/// the `Path<M>` type must accept `(base_column: &'static str,
/// base_path: &'static [&'static str])` and propagate those through every
/// nested accessor.
/// # The `Path` associated type
/// `type Path<M: Model>` is generic over the model so that `JsonbPathRef<M, V>`
/// nodes carry the model marker — mismatching a `VehicleSpecsPath<Post>` with
/// a `Vehicle`-targeted `QuerySet` is a compile error, not a runtime bug.
/// # Bounded-leak guarantee
/// All interning (both string paths via [`intern_path`] and segment slices via
/// [`intern_path_slice`]) is bounded by the product of (distinct JSONB schema
/// types x field counts x nesting depth) across the compiled program — typically
/// hundreds of entries for a real app. Every unique path is leaked at most once,
/// so memory growth reaches zero at steady state once every unique path has been
/// observed for the first time.
pub trait JsonbSchema {
    /// The typed path tree emitted by `#[derive(JsonbSchema)]`.
    /// Carry `M` in a `PhantomData<fn() -> M>` field so the path tree is
    /// always `Send + Sync`, even when `M` is not — the tree never owns or
    /// borrows a value of type `M`, it only tags the path.
    type Path<M: Model>: Sized;

    /// Construct the root of the typed path tree for column `base_column`
    /// with no accumulated path segments.
    /// Called by the `FieldRef<M, Jsonb<T>>::typed()` bridge to build
    /// the tree starting at the column root. Nested calls pass the extended
    /// base path down.
    fn root_path<M: Model>(base_column: &'static str) -> Self::Path<M>;

    /// Internal: construct a nested path node from a pre-interned segment slice.
    /// This method is an implementation detail of the `#[derive(JsonbSchema)]`
    /// macro expansion. It is `#[doc(hidden)]` and not intended for direct use.
    /// The derive macro emits calls to this from parent node accessor methods.
    /// The `base_path` slice must be a pointer returned by
    /// [`intern_path_slice`] — it is `&'static` and bounded by unique paths,
    /// never allocated more than once per unique segment sequence.
    #[doc(hidden)]
    fn __new_from_slice<M: Model>(
        base_column: &'static str,
        base_path: &'static [&'static str],
    ) -> Self::Path<M>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── intern_path_slice — pointer-equality caching ─────────────────────

    #[test]
    fn intern_path_slice_same_input_returns_same_pointer() {
        let a = intern_path_slice(&["engine", "cylinders"]);
        let b = intern_path_slice(&["engine", "cylinders"]);
        // Pointer equality proves the second call hit the cache and did NOT
        // allocate a second Box::leak'd slice — the BLOCKER memory-leak fix
        // is confirmed by this assertion.
        assert!(
            std::ptr::eq(a, b),
            "Expected intern_path_slice to return the same &'static pointer on repeated \
    calls, but got two distinct allocations. This means Fix 1 (path-slice \
    interning) is broken."
        );
    }

    #[test]
    fn intern_path_slice_different_inputs_return_different_pointers() {
        let a = intern_path_slice(&["engine", "displacement_cc"]);
        let b = intern_path_slice(&["engine", "turbo"]);
        // Different paths must produce different slices.
        assert!(
            !std::ptr::eq(a, b),
            "Different segment sequences must produce different slices"
        );
    }

    #[test]
    fn intern_path_slice_single_segment() {
        let a = intern_path_slice(&["weight_kg"]);
        let b = intern_path_slice(&["weight_kg"]);
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn intern_path_slice_empty_segments() {
        let a = intern_path_slice(&[]);
        let b = intern_path_slice(&[]);
        assert!(std::ptr::eq(a, b));
    }

    // ── intern_path — existing string interner (regression) ──────────────

    #[test]
    fn intern_path_same_input_returns_same_pointer() {
        let a = intern_path(&["engine", "cylinders"]);
        let b = intern_path(&["engine", "cylinders"]);
        // ptr::eq on &str compares the pointer + length. Same static string
        // has same address.
        assert_eq!(a.as_ptr(), b.as_ptr());
    }
}
