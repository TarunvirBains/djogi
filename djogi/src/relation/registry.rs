//! Inventory-based registry for reverse / M2M accessor macros.
//!
//! # What
//!
//! Each `djogi::reverse_one_to_many!(...)` / `djogi::reverse_one_to_one!(...)`
//! invocation expands to (among other things) a
//! [`::inventory::submit!`] block that registers a
//! [`ReverseRelationMarker`] record carrying enough metadata to identify
//! the accessor: its kind, the source model it lives on, the method
//! name, the target model, and the via column.
//!
//! # Why
//!
//! The reverse / M2M macros are **single-declaration**: one line per
//! direction, no field-side metadata on the source struct. That choice
//! keeps the macros readable (`reverse_one_to_many!(Owner, cars ->
//! Vehicle by owner_id);`) but leaves the framework without a
//! compile-time list of registered reverses unless the registration is
//! recorded explicitly. The `inventory` crate solves that: every
//! invocation emits a static record into a link-time-collected slice,
//! walkable at startup.
//!
//! Phase 4.5's projection generator — the first real consumer — walks
//! these records to discover which reverse accessors live on each
//! model without re-parsing source or inspecting method tables. Phase
//! 3 itself does not consume them; shipping the registry now is
//! forward-compatibility infrastructure.
//!
//! # How
//!
//! At link time, every `ReverseRelationMarker` submitted via `inventory::
//! submit!` is appended to a crate-level static slice. User code walks
//! them with `inventory::iter::<ReverseRelationMarker>()`:
//!
//! ```ignore
//! for marker in inventory::iter::<ReverseRelationMarker> {
//!     println!("{} has {} accessor pointing at {}",
//!         marker.source, marker.name, marker.target);
//! }
//! ```
//!
//! # Where
//!
//! - `ReverseRelationMarker` — this module.
//! - `djogi_macros::reverse_one_to_many!` — emits the `inventory::
//!   submit!` block.
//! - `djogi_macros::reverse_one_to_one!` — same, with
//!   `RelationKind::O2O`.
//! - A future `djogi_macros::many_to_many!` (Task 7's M2M half, out
//!   of scope here) will emit `RelationKind::M2M` records using the
//!   same marker shape.

/// Kind discriminator for registered relation accessors.
///
/// `#[non_exhaustive]` so a future phase can add new relation flavors
/// (e.g. polymorphic reverses, self-referential through-accessors)
/// without breaking downstream pattern matches on the enum. Matching
/// code must include a `_ => …` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RelationKind {
    /// `reverse_one_to_many!` — forward FK's reverse side. The
    /// generated accessor returns `Vec<Source>`.
    FK,
    /// `reverse_one_to_one!` — unique-backed reverse of a
    /// `OneToOneField` / `ForeignKey` + `UNIQUE` pair. The generated
    /// accessor returns `Option<Source>`.
    O2O,
    /// `many_to_many!` — through-model-backed M2M. The generated
    /// accessor returns a `Vec<{Source}{Name}View>` JOIN row. Emitted
    /// by a future sibling macro; carried here so Phase 4.5's
    /// projection generator can walk all three kinds in one pass.
    M2M,
}

/// One registered reverse / M2M accessor.
///
/// The fields are all `&'static str` so records can live in a
/// `const`-initialised static slot and survive the whole-program
/// lifetime. Keeping them string-typed (rather than `TypeId` or
/// `&'static ModelDescriptor`) lets the registry stay independent of
/// `#[model]` expansion order: a marker submitted by a macro-expanded
/// `reverse_one_to_many!` in crate `A` can reference types declared in
/// crate `B` without forcing ordering between the two expansions.
///
/// Phase 4.5's projection generator is the first real consumer — it
/// maps `source`/`target` strings to the correspondingly-registered
/// `ModelDescriptor` entries (which carry the same `&'static str`
/// identifier). Phase 3 itself does not walk these records; they are
/// forward-compatibility infrastructure.
///
/// `#[doc(hidden)]` because the struct is populated by macro expansion,
/// not by hand. The field shape is load-bearing for macro emission;
/// downstream code should reach this type through
/// `inventory::iter::<ReverseRelationMarker>` rather than constructing
/// records directly.
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct ReverseRelationMarker {
    /// Whether the accessor was registered by `reverse_one_to_many!`,
    /// `reverse_one_to_one!`, or (future) `many_to_many!`.
    pub kind: RelationKind,
    /// Type name of the model the accessor method is attached to.
    /// Example: for `reverse_one_to_many!(Owner, cars -> Vehicle by
    /// owner_id)` this is `"Owner"`.
    pub source: &'static str,
    /// Method name the macro emitted on `source`. Example: `"cars"`.
    pub name: &'static str,
    /// Type name of the model the accessor returns rows of. Example:
    /// `"Vehicle"`.
    pub target: &'static str,
    /// Column name on the `target` table that carries the FK pointing
    /// back at `source`. Example: `"owner_id"`. For M2M markers, this
    /// is the through-model's FK column pointing at `source`.
    pub via: &'static str,
}

::inventory::collect!(ReverseRelationMarker);

#[cfg(test)]
mod tests {
    use super::*;

    // Submit a marker from the test module so `inventory::iter` has at
    // least one entry to verify. `inventory::submit!` macros expand to
    // private items carrying the `ReverseRelationMarker` value into the
    // link-time-collected slice.
    ::inventory::submit! {
        ReverseRelationMarker {
            kind: RelationKind::FK,
            source: "TestSource",
            name: "test_accessor",
            target: "TestTarget",
            via: "test_via_id",
        }
    }

    #[test]
    fn relation_kind_is_copy() {
        // `Copy` is part of the contract — records are copied through
        // `inventory::iter` and into `ReverseRelationMarker` fields by
        // value. A regression that drops `Copy` would force a more
        // invasive consumer shape.
        fn assert_copy<T: Copy>() {}
        assert_copy::<RelationKind>();
        assert_copy::<ReverseRelationMarker>();
    }

    #[test]
    fn relation_kind_variants_are_distinct() {
        assert_ne!(RelationKind::FK, RelationKind::O2O);
        assert_ne!(RelationKind::O2O, RelationKind::M2M);
        assert_ne!(RelationKind::FK, RelationKind::M2M);
    }

    #[test]
    fn inventory_collects_test_marker() {
        // Walk every submitted marker; at least the one this module
        // submitted must be present. The check filters by the unique
        // source/name pair to stay robust if other crates later submit
        // markers into the same collector (Phase 4.5 integration
        // tests, downstream apps).
        let mut seen = false;
        for marker in ::inventory::iter::<ReverseRelationMarker> {
            if marker.source == "TestSource" && marker.name == "test_accessor" {
                assert_eq!(marker.kind, RelationKind::FK);
                assert_eq!(marker.target, "TestTarget");
                assert_eq!(marker.via, "test_via_id");
                seen = true;
            }
        }
        assert!(
            seen,
            "inventory::iter<ReverseRelationMarker> did not surface the test marker — \
             either linkage dropped it or the submit! block expanded without registering."
        );
    }
}
