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
//! ## Collision detection
//!
//! rustc covers the trait-layer half; the registry walker
//! [`validate_relation_accessor_collisions`] covers the cross-suffix
//! half.
//!
//! Each macro invocation emits a per-relation **trait** plus its impl
//! (GH issue #39 — coherence rule, see `reverse_relation.rs` and
//! `many_to_many.rs` module docs). The trait name embeds the macro kind
//! suffix:
//!
//! - `reverse_one_to_many!` / `reverse_one_to_one!` → `{Receiver}{Method}ReverseRelation`
//! - `many_to_many!`                                → `{Source}{Relation}ManyToManyRelation`
//!
//! That suffix split means rustc only catches **same-suffix**
//! collisions:
//!
//! - Two `reverse_one_to_many!`s with the same `(Receiver, method)` (or
//!   one `reverse_one_to_many!` and one `reverse_one_to_one!`) emit the
//!   same `…ReverseRelation` trait twice → E0428 / E0119, build fails.
//!   The compile-fail fixture
//!   `tests/compile_fail/reverse_relation_duplicate_accessor.rs` pins
//!   that surface, as does `many_to_many_collision.rs` for the M2M
//!   same-suffix case.
//! - A `reverse_one_to_many!` (or `reverse_one_to_one!`) and a
//!   `many_to_many!` that all want to expose the same accessor name on
//!   the same source emit DIFFERENT trait names (`…ReverseRelation` vs
//!   `…ManyToManyRelation`). Both compile cleanly. The collision only
//!   manifests downstream as an "ambiguous method call" error at every
//!   call site that has both traits in scope — the diagnostic points at
//!   the call site instead of at the macro invocations, and there is no
//!   guarantee any call site exercises the ambiguity.
//!
//! [`validate_relation_accessor_collisions`] closes that gap. It walks
//! a sequence of [`ReverseRelationMarker`]s (typically
//! `inventory::iter::<ReverseRelationMarker>()`), groups them by
//! `(source, accessor_name)`, and returns
//! [`RelationRegistryError::AccessorCollisions`] for any group whose
//! members disagree on `kind`, `target`, or `via`. Adopters are expected
//! to call it once during startup or in a CI gate test so cross-suffix
//! collisions surface at the macro invocations rather than at an
//! arbitrary call site.
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
/// [`crate::ident::const_assert_user_supplied_ident`]. `source` and
/// `target` are Rust type names: they are validated at the macro call
/// site via `debug_assert_ident!` (cheap) because Rust's own tokenizer
/// already constrains the shapes reachable into a `syn::Ident`.
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

/// Errors produced by relation-registry validators.
///
/// Currently surfaces a single failure mode — accessor collisions that
/// rustc cannot catch because the colliding macros emit different trait
/// suffixes. Held under `#[non_exhaustive]` so future relation-graph
/// invariants (orphaned through-side markers, self-referential M2M with
/// inconsistent FK ordering, etc.) can land as new variants without a
/// breaking change.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum RelationRegistryError {
    /// One or more `(source, accessor_name)` pairs were registered by
    /// multiple [`ReverseRelationMarker`]s that disagree on `kind`,
    /// `target`, or `via`.
    ///
    /// Carries every conflicting group so a single call to
    /// [`validate_relation_accessor_collisions`] reports every cross-kind
    /// collision in one pass instead of forcing an iterative
    /// fix-rebuild-revalidate loop.
    #[error("relation-accessor collisions detected:\n{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join(""))]
    AccessorCollisions(Vec<RelationAccessorCollision>),
}

/// One detected accessor collision — every marker that shares a
/// `(source, name)` pair with at least one disagreement on `kind`,
/// `target`, or `via`.
///
/// Returned (in a `Vec`) inside
/// [`RelationRegistryError::AccessorCollisions`]. The `Display` impl
/// renders a diagnostic block listing every conflicting marker; the
/// fields are `pub` so consumers that want to format the diagnostic
/// themselves (e.g. emit a build-script `cargo:warning=...` line, or
/// route into a structured `tracing` event) can read them directly.
#[derive(Debug, Clone)]
pub struct RelationAccessorCollision {
    /// Source model name — the receiver that the accessor method is
    /// attached to. Shared by every marker in [`Self::markers`].
    pub source: &'static str,
    /// Accessor method name. Shared by every marker in [`Self::markers`].
    pub name: &'static str,
    /// Every marker that registered this `(source, name)` pair. The
    /// vec contains at least two elements; otherwise the validator
    /// would not have flagged it as a collision.
    pub markers: Vec<ReverseRelationMarker>,
}

impl std::fmt::Display for RelationAccessorCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "  - `{}::{}` is registered by {} markers, but they disagree on kind / target / via:",
            self.source,
            self.name,
            self.markers.len(),
        )?;
        for m in &self.markers {
            writeln!(
                f,
                "      kind={:?}, target={}, via={}",
                m.kind(),
                m.target(),
                m.via(),
            )?;
        }
        writeln!(
            f,
            "    fix: rename one of the accessors so each `(source, name)` pair is \
             unique, or align the macro invocations on a single kind/target/via.",
        )
    }
}

/// Stable total order over [`RelationKind`] discriminants for
/// diagnostic-ordering purposes only.
///
/// `RelationKind` is `#[non_exhaustive]` and intentionally does not
/// derive `Ord` — adopters pattern-match on the variants and adding
/// `Ord` would constrain the public ordering semantics to whatever the
/// derive picks. Internal sorts (specifically the within-group marker
/// sort in [`validate_relation_accessor_collisions`]) just need *some*
/// stable key, so we project to `u8` here.
///
/// The match is intentionally exhaustive (no `_` arm). Within the
/// defining crate `#[non_exhaustive]` does not relax exhaustiveness,
/// so any future variant added here surfaces as a compile error and
/// forces the maintainer to choose its sort position deliberately —
/// which is what we want, since every diagnostic snapshot test
/// downstream depends on the ordering being stable.
const fn kind_order(k: RelationKind) -> u8 {
    match k {
        RelationKind::FK => 0,
        RelationKind::O2O => 1,
        RelationKind::M2M => 2,
    }
}

/// Walk a sequence of [`ReverseRelationMarker`]s and surface any
/// `(source, accessor_name)` pair claimed by markers that disagree on
/// `kind`, `target`, or `via`.
///
/// # What this catches that rustc misses
///
/// The reverse / M2M macros emit per-relation traits whose names embed
/// the macro kind:
///
/// - `reverse_one_to_many!` / `reverse_one_to_one!` →
///   `{Receiver}{Method}ReverseRelation`
/// - `many_to_many!` →
///   `{Source}{Relation}ManyToManyRelation`
///
/// rustc only catches **same-suffix** trait redefinitions (E0428 / E0119);
/// a `reverse_one_to_many!` and a `many_to_many!` competing for the
/// same `.cars()` accessor on `Owner` produce `OwnerCarsReverseRelation`
/// and `OwnerCarsManyToManyRelation`, both of which compile, and the
/// collision only manifests as an "ambiguous method call" at every
/// downstream call site that has both traits in scope. This validator
/// closes the gap: callers route the inventory iterator through it once
/// at startup (or in a CI gate) and the diagnostic points at the macro
/// invocations rather than at an arbitrary call site.
///
/// # Tolerance for legitimate duplicates
///
/// A group whose members all share the same `(kind, target, via)`
/// triple is treated as an intentional duplicate (e.g. a registry-merge
/// tool concatenating two inventories that happen to overlap on a
/// shared marker). Such groups never appear from the macro layer alone:
/// every reverse / M2M macro invocation emits both a unique trait impl
/// and a unique inventory record, so two truly identical markers also
/// imply two identical trait impls and rustc rejects the build with
/// E0428 before the markers ever reach this validator. The tolerance
/// is defensive future-proofing, not a deliberate macro escape hatch.
///
/// # Diagnostic ordering
///
/// Collisions are reported in deterministic `(source, name)` order so
/// the diagnostic is stable between runs — important for integrating
/// the validator into reproducible CI gates and snapshot-style tests.
///
/// # Example
///
/// ```ignore
/// // Typical adopter call site — startup or a CI gate test:
/// djogi::relation::registry::validate_relation_accessor_collisions(
///     ::inventory::iter::<djogi::relation::registry::ReverseRelationMarker>(),
/// )?;
/// ```
pub fn validate_relation_accessor_collisions<'a, I>(markers: I) -> Result<(), RelationRegistryError>
where
    I: IntoIterator<Item = &'a ReverseRelationMarker>,
{
    use std::collections::BTreeMap;

    // BTreeMap (vs HashMap) keeps the diagnostic ordering deterministic
    // so the same input always produces the same error message — a
    // requirement for snapshot-style tests and reproducible CI gates.
    // Marker populations are tiny (tens for a typical app, hundreds at
    // most), so the O(log n) overhead is dwarfed by the stability win.
    let mut by_pair: BTreeMap<(&'static str, &'static str), Vec<ReverseRelationMarker>> =
        BTreeMap::new();
    for marker in markers {
        by_pair
            .entry((marker.source(), marker.name()))
            .or_default()
            .push(*marker);
    }

    let mut collisions: Vec<RelationAccessorCollision> = Vec::new();
    for ((source, name), mut group) in by_pair {
        if group.len() < 2 {
            continue;
        }
        // Probe whether every marker in the group is an exact duplicate
        // of the first. The macros never emit identical duplicates (each
        // invocation emits a unique trait impl), but a future
        // registry-merge consumer might legitimately concatenate
        // overlapping inventories; tolerate identical duplicates so that
        // case keeps working. Disagreements on ANY of kind/target/via
        // are flagged.
        let head = &group[0];
        let identical = group.iter().all(|m| {
            m.kind() == head.kind() && m.target() == head.target() && m.via() == head.via()
        });
        if identical {
            continue;
        }
        // Sort markers within the group on a stable key so the
        // diagnostic is deterministic regardless of inventory link
        // order (across both `inventory::iter` walks and arbitrary
        // input orders). Insertion order can shift across builds
        // because the link-time-collected slice depends on linker
        // ordering and codegen unit shuffling — sorting in-place
        // anchors the diagnostic.
        group.sort_by(|l, r| {
            (kind_order(l.kind()), l.target(), l.via()).cmp(&(
                kind_order(r.kind()),
                r.target(),
                r.via(),
            ))
        });
        collisions.push(RelationAccessorCollision {
            source,
            name,
            markers: group,
        });
    }

    if collisions.is_empty() {
        Ok(())
    } else {
        Err(RelationRegistryError::AccessorCollisions(collisions))
    }
}

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
/// supported construction path routes user-supplied identifier strings
/// through the shared [`crate::ident::const_assert_user_supplied_ident`]
/// validator before the record reaches the inventory slice.
#[doc(hidden)]
pub mod __macro_support {
    use super::{RelationKind, ReverseRelationMarker};
    use crate::ident::{const_assert_plain_ident, const_assert_user_supplied_ident};

    /// Construct a [`ReverseRelationMarker`] from macro-emitted
    /// identifier strings. The only supported caller is the
    /// `::inventory::submit!` block that the reverse-relation
    /// macros expand in the user's crate.
    ///
    /// Panics (at const-eval time — `inventory::submit!` wraps the
    /// returned value in a `static` initializer) if `name` or `via`
    /// violates any rule in
    /// [`crate::ident::const_assert_user_supplied_ident`]: empty, over
    /// 63 bytes, leading digit, a non-identifier byte, a reserved
    /// Postgres keyword, or the framework-reserved `__djogi_*`
    /// namespace. `name` names a Rust method emitted on the receiver
    /// type and `via` names a Postgres column; both originate in adopter
    /// macro input and must therefore satisfy the shared user-supplied
    /// unquoted-identifier rule.
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
        const_assert_user_supplied_ident(name, "reverse_relation_name");
        const_assert_user_supplied_ident(via, "reverse_relation_via");
        ReverseRelationMarker {
            kind,
            source,
            name,
            target,
            via,
        }
    }

    /// Validate a framework-emitted identifier in const context without
    /// constructing a marker.
    ///
    /// This remains available for macro-support call sites that need the
    /// plain identifier contract while still allowing djogi's own
    /// `__djogi_*` namespace. User-supplied macro arguments should use
    /// [`__const_assert_user_supplied_ident`] instead.
    #[doc(hidden)]
    pub const fn __const_assert_plain_ident(value: &'static str, role: &'static str) {
        const_assert_plain_ident(value, role);
    }

    /// Validate a macro-emitted user-supplied identifier in const
    /// context without constructing a marker.
    ///
    /// `many_to_many!` needs this for `that_fk`: unlike `relation` and
    /// `this_fk`, it does not flow through the stored
    /// [`ReverseRelationMarker`] fields, but it still becomes a
    /// SQL-facing `&'static str` through `ManyToMany::that_fk()`.
    #[doc(hidden)]
    pub const fn __const_assert_user_supplied_ident(value: &'static str, role: &'static str) {
        const_assert_user_supplied_ident(value, role);
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

        fn try_const_assert_user(value: &'static str) -> std::thread::Result<()> {
            std::panic::catch_unwind(|| __const_assert_user_supplied_ident(value, "test_role"))
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
        fn rejects_reserved_djogi_prefix_in_name_and_via() {
            assert!(try_make("__djogi_cars", "owner_id").is_err());
            assert!(try_make("__DJOGI_cars", "owner_id").is_err());
            assert!(try_make("cars", "__djogi_owner_id").is_err());
            assert!(try_make("cars", "__Djogi_owner_id").is_err());
        }

        #[test]
        fn const_assert_wrapper_rejects_reserved_keywords() {
            assert!(try_const_assert("owner_id").is_ok());
            assert!(try_const_assert("select").is_err());
        }

        #[test]
        fn const_user_assert_wrapper_rejects_reserved_djogi_prefix() {
            assert!(try_const_assert_user("owner_id").is_ok());
            assert!(try_const_assert_user("__djogi_owner_id").is_err());
            assert!(try_const_assert_user("__DJOGI_owner_id").is_err());
            assert!(try_const_assert_user("_djogi_owner_id").is_ok());
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

    // ── validate_relation_accessor_collisions ────────────────────────────
    //
    // The validator's contract is small enough to verify with
    // hand-built marker fixtures rather than driven through the proc
    // macro. Every test below routes construction through the sealed
    // `__make_reverse_relation_marker` so the marker shape is
    // guaranteed identical to the one the macros emit.

    fn make(
        kind: RelationKind,
        source: &'static str,
        name: &'static str,
        target: &'static str,
        via: &'static str,
    ) -> ReverseRelationMarker {
        super::__macro_support::__make_reverse_relation_marker(kind, source, name, target, via)
    }

    #[test]
    fn validator_accepts_empty_input() {
        // A registry with no markers is trivially collision-free; the
        // validator must not allocate or panic on the empty case.
        let markers: [ReverseRelationMarker; 0] = [];
        assert!(validate_relation_accessor_collisions(markers.iter()).is_ok());
    }

    #[test]
    fn validator_accepts_unrelated_markers() {
        // Different `(source, name)` pairs do not collide regardless of
        // kind / target / via.
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::M2M, "Person", "groups", "Group", "person_id"),
            make(RelationKind::O2O, "User", "profile", "Profile", "user_id"),
        ];
        assert!(validate_relation_accessor_collisions(markers.iter()).is_ok());
    }

    #[test]
    fn validator_tolerates_identical_duplicates() {
        // Two markers identical in every field — kind, target, via —
        // are treated as an intentional duplicate. The reverse / M2M
        // macros never emit this in practice (each invocation also
        // emits a unique trait impl that rustc would E0428 on a true
        // double), but a future registry-merge consumer concatenating
        // overlapping inventories should not be punished for harmless
        // overlap.
        let m = make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id");
        let markers = [m, m];
        assert!(validate_relation_accessor_collisions(markers.iter()).is_ok());
    }

    #[test]
    fn validator_flags_cross_kind_fk_vs_m2m() {
        // The headline case from GH issue #158: a `reverse_one_to_many!`
        // and a `many_to_many!` both expose `.cars()` on `Owner`. The
        // emitted trait names — `OwnerCarsReverseRelation` and
        // `OwnerCarsManyToManyRelation` — differ, so rustc compiles
        // both. Without this validator the collision only surfaces as
        // an "ambiguous method call" error at every downstream call
        // site that has both traits in scope.
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::M2M, "Owner", "cars", "Garage", "owner_id"),
        ];
        let err = validate_relation_accessor_collisions(markers.iter())
            .expect_err("FK + M2M with the same (source, name) must collide");
        let RelationRegistryError::AccessorCollisions(collisions) = err;
        assert_eq!(collisions.len(), 1);
        let c = &collisions[0];
        assert_eq!(c.source, "Owner");
        assert_eq!(c.name, "cars");
        assert_eq!(c.markers.len(), 2);
    }

    #[test]
    fn validator_flags_cross_kind_o2o_vs_m2m() {
        // Symmetric companion to the FK + M2M test. `OwnerProfileReverseRelation`
        // and `OwnerProfileManyToManyRelation` again differ at the trait layer,
        // so this case is invisible to rustc.
        let markers = [
            make(RelationKind::O2O, "User", "profile", "Profile", "user_id"),
            make(RelationKind::M2M, "User", "profile", "Avatar", "user_id"),
        ];
        let err = validate_relation_accessor_collisions(markers.iter())
            .expect_err("O2O + M2M with the same (source, name) must collide");
        let RelationRegistryError::AccessorCollisions(collisions) = err;
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].markers.len(), 2);
    }

    #[test]
    fn validator_flags_same_kind_different_target() {
        // The reverse / M2M macros emit one inventory marker per
        // invocation alongside a unique trait impl, so in practice two
        // markers sharing kind + name + source but disagreeing on
        // target also fail at rustc (the trait names match, E0428
        // fires). The validator covers the case as a defensive belt:
        // if a future emission shape ever decoupled the trait name
        // from the (source, name) pair, this gate keeps the registry
        // honest.
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::FK, "Owner", "cars", "Truck", "owner_id"),
        ];
        let err = validate_relation_accessor_collisions(markers.iter())
            .expect_err("same (source, name) but different target must collide");
        let RelationRegistryError::AccessorCollisions(collisions) = err;
        assert_eq!(collisions.len(), 1);
    }

    #[test]
    fn validator_flags_same_kind_different_via() {
        // Same defensive gate, but for the via column — caught at the
        // trait layer today, but the validator double-checks.
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "old_owner_id"),
        ];
        let err = validate_relation_accessor_collisions(markers.iter())
            .expect_err("same (source, name, target) but different via must collide");
        let RelationRegistryError::AccessorCollisions(collisions) = err;
        assert_eq!(collisions.len(), 1);
    }

    #[test]
    fn validator_reports_multiple_collisions_in_one_pass() {
        // The validator surfaces every collision in a single
        // `Result::Err` so adopters can fix the whole registry in one
        // round instead of iteratively rebuilding to discover the
        // next conflict.
        let markers = [
            make(RelationKind::FK, "A", "x", "Vehicle", "a_id"),
            make(RelationKind::M2M, "A", "x", "Garage", "a_id"),
            make(RelationKind::O2O, "B", "y", "Vehicle", "b_id"),
            make(RelationKind::M2M, "B", "y", "Garage", "b_id"),
            make(RelationKind::FK, "C", "z", "Vehicle", "c_id"), // alone — no collision
        ];
        let err = validate_relation_accessor_collisions(markers.iter()).unwrap_err();
        let RelationRegistryError::AccessorCollisions(mut collisions) = err;
        // Sort by (source, name) so the assertion is robust against
        // future ordering changes (today they're sorted by BTreeMap
        // construction; pin both invariants explicitly).
        collisions.sort_by(|l, r| (l.source, l.name).cmp(&(r.source, r.name)));
        assert_eq!(collisions.len(), 2);
        assert_eq!((collisions[0].source, collisions[0].name), ("A", "x"));
        assert_eq!((collisions[1].source, collisions[1].name), ("B", "y"));
    }

    #[test]
    fn validator_diagnostic_ordering_is_deterministic() {
        // Diagnostic stability is a contract — snapshot-style tests
        // and reproducible CI gates depend on it. Submit the same
        // collision set twice in different input orders and assert
        // the resulting error string is identical.
        let a = [
            make(RelationKind::FK, "B", "y", "Vehicle", "b_id"),
            make(RelationKind::M2M, "B", "y", "Garage", "b_id"),
            make(RelationKind::FK, "A", "x", "Vehicle", "a_id"),
            make(RelationKind::M2M, "A", "x", "Garage", "a_id"),
        ];
        let b = [
            make(RelationKind::M2M, "A", "x", "Garage", "a_id"),
            make(RelationKind::FK, "A", "x", "Vehicle", "a_id"),
            make(RelationKind::M2M, "B", "y", "Garage", "b_id"),
            make(RelationKind::FK, "B", "y", "Vehicle", "b_id"),
        ];
        let err_a = validate_relation_accessor_collisions(a.iter())
            .unwrap_err()
            .to_string();
        let err_b = validate_relation_accessor_collisions(b.iter())
            .unwrap_err()
            .to_string();
        assert_eq!(err_a, err_b);
    }

    #[test]
    fn validator_error_display_mentions_source_name_and_kinds() {
        // The diagnostic must point at the colliding (source, name) and
        // list every marker's kind/target/via — that's what makes the
        // error actionable without a separate "where did this come
        // from" investigation. Pin the load-bearing substrings so
        // accidental refactors don't silently drop them.
        let markers = [
            make(RelationKind::FK, "Owner", "cars", "Vehicle", "owner_id"),
            make(RelationKind::M2M, "Owner", "cars", "Garage", "owner_id"),
        ];
        let msg = validate_relation_accessor_collisions(markers.iter())
            .unwrap_err()
            .to_string();
        assert!(msg.contains("Owner"), "missing source: {msg}");
        assert!(msg.contains("cars"), "missing accessor name: {msg}");
        assert!(msg.contains("FK"), "missing FK kind: {msg}");
        assert!(msg.contains("M2M"), "missing M2M kind: {msg}");
        assert!(msg.contains("Vehicle"), "missing FK target: {msg}");
        assert!(msg.contains("Garage"), "missing M2M target: {msg}");
    }

    #[test]
    fn validator_inventory_walk_compiles() {
        // Pin that the validator's signature accepts the canonical
        // adopter call shape — passing `inventory::iter::<T>()`
        // directly — and that the iterator's `&'static T` items satisfy
        // the `&'a T` bound. The result itself is whatever the live
        // inventory contains; the test is purely a type-check.
        let _ = validate_relation_accessor_collisions(::inventory::iter::<ReverseRelationMarker>());
    }
}
