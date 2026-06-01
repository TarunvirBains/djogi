//! `RelationPath<Source, Target>` — the typed zero-sized handle that
//! names a relation from one model to another at the type level.
//! Produced exclusively by the macro-emitted `{Source}Related` accessor
//! struct. User code receives a `RelationPath<Vehicle, Owner>` from
//! `VehicleRelated::owner` and hands it to a future
//! `QuerySet::prefetch(...)` / `QuerySet::select_related(...)` call
//! (Tasks 4 + 5). The path is a ZST — all information is carried
//! at the type level plus three `&'static str` / discriminant fields for
//! use by the SQL emitters.
//! # Why the `Source` / `Target` generics
//! The compile-time source/target pinning means `prefetch` / `select_related`
//! cannot be called with a relation path from the wrong model: a
//! `RelationPath<Vehicle, Owner>` only matches `QuerySet<Vehicle>`. Attempting
//! to use it on `QuerySet<Post>` fails at the type level, long before any SQL
//! would be emitted. This is the feature — relations are a place where
//! mismatched targets are a nasty silent source of wrong results.
//! # Sealed constructor
//! [`RelationPath::new`] is `pub(crate)` — the type cannot be constructed from
//! downstream crates. Proc-macro-emitted `{Source}Related` methods reach it via
//! the `#[doc(hidden)] pub` helper [`__private::__make_relation_path`] in
//! `djogi::relation`, which validates the identifier strings before
//! instantiating the path. That helper exists only so `#[derive(Model)]` in
//! `djogi-macros` can emit user-crate code that calls it; it is not part of
//! the stable public API and its name carries the framework-internal
//! double-underscore convention.
//! The seal closes the SQL-injection vector that existed while `__new` was
//! `pub` with `#[doc(hidden)]`: downstream code could previously fabricate a
//! `RelationPath` whose `source_column` / `target_table` strings contained
//! quotes, spaces, or SQL metacharacters, and those strings flowed straight
//! into `SqlAccumulator::push_sql` in the prefetch / select_related emitters.
//! With the constructor `pub(crate)` and the macro helper routing both
//! identifier args through [`crate::ident::assert_plain_ident`] (Postgres
//! unquoted-identifier grammar plus reserved-keyword rejection), no value
//! of `RelationPath` can carry an injection payload or a malformed
//! identifier that would reach SQL emission.
//! # scope
//! `RelationKind` lists `ForeignKey` and `OneToOne` — the two relation
//! shapes landed by Tasks 1 + 2. Task 7 adds a `ManyToMany` variant; the
//! enum is `#[non_exhaustive]` so growing it is non-breaking for downstream
//! matches.

use crate::model::Model;
use std::marker::PhantomData;

/// Cardinality discriminator for a [`RelationPath`].
/// The downstream prefetch / select_related SQL planner reads this to
/// pick the right strategy: `ForeignKey` and `OneToOne` both run a
/// single-row-per-parent join, but `OneToOne`'s uniqueness constraint
/// lets the emitter elide a `DISTINCT` that `ForeignKey` may need.
/// Future `ManyToMany` variants take a completely different path
/// (`JOIN` through the through-model).
/// `#[non_exhaustive]` so 's `ManyToMany` variant lands
/// without breaking any consumer that already matches on the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RelationKind {
    /// Many-to-one — parent row holds an FK column pointing at the target.
    /// Emitted for fields declared as `ForeignKey<T>` or `Option<ForeignKey<T>>`.
    ForeignKey,
    /// One-to-one — same shape as `ForeignKey` at the SQL level, but the
    /// FK column carries a `UNIQUE` constraint so at most one parent
    /// references each target row. Emitted for `OneToOneField<T>` and
    /// `Option<OneToOneField<T>>`.
    OneToOne,
}

/// Typed handle describing "the relation on `Source` that points at `Target`".
/// Produced by the macro-emitted `{Source}Related` accessor struct; never
/// constructed by user code. A `RelationPath<Vehicle, Owner>` carries:
/// - `source_column` — the column on `Source`'s table that stores the
///   target's primary key (e.g. `"owner_id"`);
/// - `target_table` — `Target::table_name` at macro-expansion time;
/// - `kind` — whether the relation is a `ForeignKey` or `OneToOne`.
///   The struct is a ZST plus three `&'static` members and one enum
///   discriminant; it costs a handful of bytes to pass around and a free
///   type-level proof that `Source` and `Target` line up. Tasks
///   4 + 5 (`prefetch` / `select_related`) consume these handles.
///   Does not yet expose Getters for the `Source`/`Target` markers
///   the type-level proof is all that downstream code needs. If a later
///   phase grows a reflective API over the marker, it should add explicit
///   type-level accessors here rather than expose the `PhantomData` field
///   directly.
#[derive(Debug, Clone, Copy)]
pub struct RelationPath<Source: Model, Target: Model> {
    pub(crate) source_column: &'static str,
    pub(crate) target_table: &'static str,
    pub(crate) kind: RelationKind,
    // `PhantomData<fn -> (Source, Target)>` — covariant in both generics
    // without implying ownership of a Source or Target value. Matches the
    // variance choice in `ForeignKey<T>` / `OneToOneField<T>` and keeps
    // `RelationPath` `Send`/`Sync` regardless of the model types' own
    // auto-trait fate.
    _markers: PhantomData<fn() -> (Source, Target)>,
}

impl<Source: Model, Target: Model> RelationPath<Source, Target> {
    /// Construct a relation path. Crate-private — downstream code goes through
    /// the macro-emitted `{Source}Related::relation_name` accessor, which
    /// reaches this constructor via
    /// [`__private::__make_relation_path`](super::__private::__make_relation_path)
    /// after validating the identifier strings.
    /// The seal prevents the SQL-injection fabrication vector documented on
    /// the module header: arbitrary `&'static str` inputs can no longer reach
    /// the SQL emitter via a downstream-constructed `RelationPath`.
    /// `const fn` so the macro helper can call this in `const` contexts if a
    /// later phase needs `const`-promoted relation paths; matches the ZST
    /// nature of the struct.
    pub(crate) const fn new(
        source_column: &'static str,
        target_table: &'static str,
        kind: RelationKind,
    ) -> Self {
        Self {
            source_column,
            target_table,
            kind,
            _markers: PhantomData,
        }
    }

    /// Name of the column on `Source`'s table that holds `Target`'s PK.
    /// Used by the prefetch / select_related SQL emitters (Tasks
    /// 4 + 5) to name the join condition's left-hand side.
    #[inline]
    pub fn source_column(&self) -> &'static str {
        self.source_column
    }

    /// `Target::table_name`, snapshotted at macro expansion time.
    /// The SQL emitters use this as the join condition's right-hand-side
    /// table.
    #[inline]
    pub fn target_table(&self) -> &'static str {
        self.target_table
    }

    /// Relation cardinality — drives SQL strategy selection in later tasks.
    #[inline]
    pub fn kind(&self) -> RelationKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::descriptor::ModelDescriptor;
    use crate::model::Model;
    use crate::types::HeerId;
    use std::future::Future;

    // Minimal inert `Model` stubs — enough to satisfy the `Model` bounds on
    // `RelationPath`'s generics. The CRUD methods panic if called; the unit
    // tests only exercise the static metadata (`source_column`,
    // `target_table`, `kind`), which never drives a CRUD path.
    struct Src;
    struct Dst;

    macro_rules! dummy_model {
        ($ty:ty, $table:literal) => {
            impl crate::model::__sealed::Sealed for $ty {}
            impl Model for $ty {
                type Pk = HeerId;
                type Fields = ();
                fn table_name() -> &'static str {
                    $table
                }
                fn pk_value(&self) -> &Self::Pk {
                    unreachable!()
                }
                fn descriptor() -> &'static ModelDescriptor {
                    unreachable!()
                }
                fn get(
                    _ctx: &mut crate::context::DjogiContext,
                    _id: Self::Pk,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn create(
                    _ctx: &mut crate::context::DjogiContext,
                    _v: Self,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn save<'ctx>(
                    &'ctx mut self,
                    _ctx: &'ctx mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
                fn delete(
                    self,
                    _ctx: &mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn refresh_from_db<'ctx>(
                    &'ctx self,
                    _ctx: &'ctx mut crate::context::DjogiContext,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
            }
        };
    }

    dummy_model!(Src, "srcs");
    dummy_model!(Dst, "dsts");

    #[test]
    fn relation_path_holds_source_column_and_target_table() {
        let p: RelationPath<Src, Dst> =
            RelationPath::new("dst_id", "dsts", RelationKind::ForeignKey);
        assert_eq!(p.source_column(), "dst_id");
        assert_eq!(p.target_table(), "dsts");
        assert_eq!(p.kind(), RelationKind::ForeignKey);
    }

    #[test]
    fn relation_path_one_to_one_kind_round_trips() {
        // OneToOne paths are the other variant ships
        // pin the discriminant so Task 7's `ManyToMany` addition
        // doesn't accidentally renumber the existing variants.
        let p: RelationPath<Src, Dst> = RelationPath::new("dst_id", "dsts", RelationKind::OneToOne);
        assert_eq!(p.kind(), RelationKind::OneToOne);
    }

    #[test]
    fn relation_path_is_zst_shaped() {
        // ZST invariant: `RelationPath` costs only its three `&'static` /
        // enum fields — the `PhantomData` marker contributes nothing.
        // A size mismatch here would signal the marker picked up a
        // concrete `(Source, Target)` pair somewhere.
        use std::mem::size_of;
        assert_eq!(
            size_of::<RelationPath<Src, Dst>>(),
            size_of::<(&'static str, &'static str, RelationKind)>()
        );
    }
}
