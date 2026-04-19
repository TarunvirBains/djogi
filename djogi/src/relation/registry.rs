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
//! The inventory marker provides metadata for tooling and future
//! collision checks; runtime collision detection across reverse-relation
//! accessors is out of scope for this commit. The macro emits a plain
//! inherent method, so duplicate accessors with the same method name on
//! the same receiver type already fail to compile via rustc's
//! duplicate-definition error (see
//! `tests/compile_fail/reverse_relation_duplicate_accessor.rs`). Detecting
//! cross-macro-kind collisions (e.g. a `reverse_one_to_many!` and a
//! `many_to_many!` both emitting `.cars()` on the same source) lands in
//! a follow-up that walks `inventory::iter::<ReverseRelationMarker>`
//! during startup registration.
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
//!         marker.source(), marker.name(), marker.target());
//! }
//! ```
//!
//! # Where
//!
//! - `ReverseRelationMarker` — this module.
//! - [`__macro_support::__make_reverse_relation_marker`] — the sole
//!   validated constructor; the only supported caller is macro-emitted
//!   code in `djogi-macros/src/reverse_relation.rs`.
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
///
/// The enum itself stays public so consumers walking
/// `inventory::iter::<ReverseRelationMarker>` can match on the kind,
/// but fabrication of a full [`ReverseRelationMarker`] still goes
/// through the validated constructor in [`__macro_support`].
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
/// # Seal
///
/// The fields are `pub(crate)` so downstream code cannot
/// `inventory::submit!` a fabricated marker with arbitrary
/// `source` / `name` / `target` / `via` strings. The `name` and `via`
/// strings both flow into emitted SQL / Rust identifier positions; a
/// hostile (or simply buggy) downstream marker carrying SQL
/// metacharacters in either slot would be walked by future tooling as
/// if it were macro-emitted. The only supported construction path is
/// [`__macro_support::__make_reverse_relation_marker`], which routes
/// both `name` and `via` through
/// [`crate::ident::assert_plain_ident`]. `source` and `target` are
/// Rust type names: they are validated at the macro call site via
/// `debug_assert_ident!` (cheap) because Rust's own tokenizer already
/// constrains the shapes reachable into a `syn::Ident`.
///
/// Accessors [`source`](Self::source), [`name`](Self::name),
/// [`target`](Self::target), [`via`](Self::via), and
/// [`kind`](Self::kind) expose the data read-only.
///
/// `#[doc(hidden)]` because the struct is populated by macro expansion,
/// not by hand. Downstream code should reach this type through
/// `inventory::iter::<ReverseRelationMarker>` rather than constructing
/// records directly.
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct ReverseRelationMarker {
    pub(crate) kind: RelationKind,
    pub(crate) source: &'static str,
    pub(crate) name: &'static str,
    pub(crate) target: &'static str,
    pub(crate) via: &'static str,
}

impl ReverseRelationMarker {
    /// Whether the accessor was registered by `reverse_one_to_many!`,
    /// `reverse_one_to_one!`, or (future) `many_to_many!`.
    #[inline]
    pub fn kind(&self) -> RelationKind {
        self.kind
    }

    /// Type name of the model the accessor method is attached to.
    /// Example: for `reverse_one_to_many!(Owner, cars -> Vehicle by
    /// owner_id)` this is `"Owner"`.
    #[inline]
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// Method name the macro emitted on `source()`. Example: `"cars"`.
    #[inline]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Type name of the model the accessor returns rows of. Example:
    /// `"Vehicle"`.
    #[inline]
    pub fn target(&self) -> &'static str {
        self.target
    }

    /// Column name on the `target()` table that carries the FK
    /// pointing back at `source()`. Example: `"owner_id"`. For M2M
    /// markers, this is the through-model's FK column pointing at
    /// `source()`.
    #[inline]
    pub fn via(&self) -> &'static str {
        self.via
    }
}

::inventory::collect!(ReverseRelationMarker);

/// Macro-only entry point for constructing [`ReverseRelationMarker`]
/// values. **Not** part of the stable public API.
///
/// `djogi-macros` emits a call into this module from the
/// `reverse_one_to_many!` / `reverse_one_to_one!` / (future)
/// `many_to_many!` expansion inside every `inventory::submit!` block.
/// The items here are `pub` only so cross-crate codegen can reach
/// them; the double-underscore prefix and `#[doc(hidden)]` marker
/// signal that downstream code must not call these directly.
///
/// Mirrors the seal patterns in [`crate::relation::__macro_support`]
/// (for `RelationPath::new`) and [`crate::query::field::__macro_support`]
/// (for `FieldRef::new`): fields are `pub(crate)` and the only
/// supported construction path routes identifier strings through the
/// shared [`crate::ident::assert_plain_ident`] validator before the
/// record reaches the inventory slice.
#[doc(hidden)]
pub mod __macro_support {
    use super::{RelationKind, ReverseRelationMarker};
    use crate::ident::const_assert_plain_ident;

    /// Construct a [`ReverseRelationMarker`] from macro-emitted
    /// identifier strings. The only supported caller is the
    /// `::inventory::submit!` block that the reverse-relation
    /// macros expand in the user's crate.
    ///
    /// Panics (at const-eval time — `inventory::submit!` wraps the
    /// returned value in a `static` initializer) if `name` or `via`
    /// violates any rule in
    /// [`crate::ident::const_assert_plain_ident`]: empty, over 63
    /// bytes, leading digit, a non-identifier byte, or a reserved
    /// Postgres keyword. `name` names a Rust method emitted on the
    /// receiver type and `via` names a Postgres column; both must
    /// therefore satisfy the shared unquoted-identifier rule.
    /// `source` and `target` are Rust type names reached via
    /// `syn::Ident` in the macro, so the Rust tokenizer has already
    /// rejected obviously malformed inputs at parse time; they are
    /// passed through unmodified.
    ///
    /// `const fn` because `inventory::submit!` expands to
    /// `static __INVENTORY: Node = Node { value: &{ <expr> }, ... };`
    /// — the value expression must be const-evaluable or the build
    /// fails with `E0015` (non-const in const context). Mirrors the
    /// `const fn RelationPath::new` seal; both sit behind the shared
    /// validator family.
    #[doc(hidden)]
    pub const fn __make_reverse_relation_marker(
        kind: RelationKind,
        source: &'static str,
        name: &'static str,
        target: &'static str,
        via: &'static str,
    ) -> ReverseRelationMarker {
        const_assert_plain_ident(name, "reverse_relation_name");
        const_assert_plain_ident(via, "reverse_relation_via");
        ReverseRelationMarker {
            kind,
            source,
            name,
            target,
            via,
        }
    }

    /// Validate a macro-emitted identifier in const context without
    /// constructing a marker.
    ///
    /// `many_to_many!` needs this for `that_fk`: unlike `relation` and
    /// `this_fk`, it does not flow through the stored
    /// [`ReverseRelationMarker`] fields, but it still becomes a
    /// SQL-facing `&'static str` through `ManyToMany::that_fk()`.
    #[doc(hidden)]
    pub const fn __const_assert_plain_ident(value: &'static str, role: &'static str) {
        const_assert_plain_ident(value, role);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn try_make(
            name: &'static str,
            via: &'static str,
        ) -> std::thread::Result<ReverseRelationMarker> {
            std::panic::catch_unwind(|| {
                __make_reverse_relation_marker(RelationKind::FK, "Owner", name, "Vehicle", via)
            })
        }

        fn try_const_assert(value: &'static str) -> std::thread::Result<()> {
            std::panic::catch_unwind(|| __const_assert_plain_ident(value, "test_role"))
        }

        #[test]
        fn accepts_plain_identifiers() {
            let marker = __make_reverse_relation_marker(
                RelationKind::FK,
                "Owner",
                "cars",
                "Vehicle",
                "owner_id",
            );
            assert_eq!(marker.name(), "cars");
            assert_eq!(marker.via(), "owner_id");
            assert_eq!(marker.source(), "Owner");
            assert_eq!(marker.target(), "Vehicle");
            assert_eq!(marker.kind(), RelationKind::FK);
        }

        #[test]
        fn rejects_bad_name() {
            // Method name with a leading digit would panic at the
            // shared validator — the same shape that would sneak
            // through to identifier positions in macro-emitted code.
            assert!(try_make("1bad", "owner_id").is_err());
        }

        #[test]
        fn rejects_bad_via() {
            // A via column carrying SQL metacharacters is the
            // injection shape the seal prevents — symmetric with
            // the `FieldRef` / `RelationPath` seals.
            assert!(try_make("cars", "col) OR 1=1 --").is_err());
        }

        #[test]
        fn rejects_reserved_via_keyword() {
            // `select` is a reserved Postgres keyword; assert_plain_ident
            // rejects it so emitted SQL cannot grow a `JOIN select ON ...`
            // clause from downstream fabrication.
            assert!(try_make("cars", "select").is_err());
        }

        #[test]
        fn const_assert_wrapper_rejects_reserved_keywords() {
            assert!(try_const_assert("owner_id").is_ok());
            assert!(try_const_assert("select").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Submit a marker from the test module so `inventory::iter` has at
    // least one entry to verify. `inventory::submit!` macros expand to
    // private items carrying the `ReverseRelationMarker` value into the
    // link-time-collected slice. Route construction through the sealed
    // `__macro_support` constructor — the same path macro-emitted code
    // takes.
    ::inventory::submit! {
        crate::relation::registry::__macro_support::__make_reverse_relation_marker(
            RelationKind::FK,
            "TestSource",
            "test_accessor",
            "TestTarget",
            "test_via_id",
        )
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
            if marker.source() == "TestSource" && marker.name() == "test_accessor" {
                assert_eq!(marker.kind(), RelationKind::FK);
                assert_eq!(marker.target(), "TestTarget");
                assert_eq!(marker.via(), "test_via_id");
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
