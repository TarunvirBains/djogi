//! Relation field types and (later) relation-aware query extensions.
//!
//! Phase 3 Task 1 lands the runtime wrappers only:
//!
//! - [`ForeignKey<T>`] / [`ForeignKeyResolved<T>`] — many-to-one.
//! - [`OneToOneField<T>`] / [`OneToOneFieldResolved<T>`] —
//!   unique-constrained singular relation.
//! - [`OnDelete`] — cascade enum emitted into DDL by Phase 6's
//!   migration layer.
//!
//! Later Phase 3 tasks extend this module with:
//!
//! - `path.rs` / `RelationPath<Source, Target>` — typed ZST relation
//!   handle produced by `{Source}Related::relation_name()` for prefetch
//!   / select_related (Task 2).
//! - `prefetch.rs` / `PrefetchedRow<T>` — post-prefetch wrapper + its
//!   two-query stitching loader (Task 4).
//! - `joined_row.rs` / `JoinedRow<T>` + `FromJoinedRow` — post-select_related
//!   wrapper + prefix-aware row decoder (Task 5).
//! - `select_related.rs` — single-hop LEFT JOIN SQL emission + joined-
//!   row stitching glue (Task 5).
//! - `many_to_many.rs` / `ManyToMany<Target>` trait + through-model
//!   plumbing (Task 6).
//!
//! See `docs/guide/relations.md` (Phase 3 Task 8) for the user-facing
//! guide once later tasks land.

pub mod foreign_key;
pub mod joined_row;
pub mod on_delete;
pub mod one_to_one;
pub mod path;
pub mod prefetch;
pub mod select_related;

pub use foreign_key::{ForeignKey, ForeignKeyResolved};
pub use joined_row::{FromJoinedRow, JoinedRow};
pub use on_delete::OnDelete;
pub use one_to_one::{OneToOneField, OneToOneFieldResolved};
pub use path::{RelationKind, RelationPath};
pub use prefetch::PrefetchedRow;

/// Macro-only entry points. **Not** part of the stable public API.
///
/// `djogi-macros` emits calls into this module from user-crate code that
/// `#[derive(Model)]` expands — the items here are `pub` only so cross-crate
/// codegen can reach them. The double-underscore prefix and `#[doc(hidden)]`
/// marker signal to tooling and reviewers that downstream code must not
/// call these directly; the macro is the sole supported caller.
///
/// The seal exists to close the SQL-injection vector that was reachable when
/// [`RelationPath`]'s constructor was `pub`: a downstream caller could
/// previously fabricate a path whose identifier strings carried SQL
/// metacharacters, and those strings flowed through
/// [`sqlx::QueryBuilder::push`] in the prefetch / select_related emitters.
/// Constructing a path now requires going through
/// [`__make_relation_path`](__macro_support::__make_relation_path), which
/// delegates to [`crate::ident::assert_plain_ident`] — the shared validator
/// that enforces the full Postgres unquoted-identifier rule (non-empty,
/// length ≤ 63 bytes, leading ASCII letter or underscore followed by ASCII
/// alphanumerics or underscores, not a reserved keyword). Any identifier
/// that is not a valid *unquoted* Postgres identifier — even one that is
/// safe against classic SQL-injection metacharacters — is rejected, so
/// the emitter can never produce malformed SQL like `p.123` or
/// `LEFT JOIN select ...` from hostile downstream input.
#[doc(hidden)]
pub mod __macro_support {
    use super::path::{RelationKind, RelationPath};
    use crate::ident::assert_plain_ident;
    use crate::model::Model;

    /// Construct a [`RelationPath<Source, Target>`] from macro-emitted
    /// identifier strings. The only supported caller is the
    /// `{Source}Related::relation_name()` method that `#[derive(Model)]`
    /// emits in the user's crate.
    ///
    /// Panics if `source_column` or `target_table` violates any rule in
    /// [`crate::ident::assert_plain_ident`]: empty, over 63 bytes,
    /// leading digit, a non-identifier byte, or a reserved Postgres
    /// keyword. The check is the runtime half of the seal; the
    /// compile-time half is [`RelationPath::new`] being `pub(crate)`.
    #[doc(hidden)]
    pub fn __make_relation_path<Source: Model, Target: Model>(
        source_column: &'static str,
        target_table: &'static str,
        kind: RelationKind,
    ) -> RelationPath<Source, Target> {
        assert_plain_ident(source_column, "source_column");
        assert_plain_ident(target_table, "target_table");
        RelationPath::new(source_column, target_table, kind)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::DjogiError;
        use crate::descriptor::ModelDescriptor;
        use std::future::Future;

        // Minimal inert `Model` stubs — mirrors the pattern in
        // `djogi/src/relation/path.rs`'s unit tests. Exhaustive validator
        // coverage lives in `crate::ident::tests`; this file only verifies
        // that the thin `__make_relation_path` wrapper threads both args
        // through the shared validator before constructing the path.
        struct Src;
        struct Dst;

        macro_rules! dummy_model {
            ($ty:ty, $table:literal) => {
                impl Model for $ty {
                    type Pk = crate::types::HeerId;
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
                    fn get<'a>(
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                        _id: Self::Pk,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn create<'a>(
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                        _v: Self,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn save<'a>(
                        &self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn delete<'a>(
                        self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                        async { unreachable!() }
                    }
                    fn refresh_from_db<'a>(
                        &self,
                        _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
                    ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                        async { unreachable!() }
                    }
                }
            };
        }

        dummy_model!(Src, "srcs");
        dummy_model!(Dst, "dsts");

        fn try_make(
            source_column: &'static str,
            target_table: &'static str,
        ) -> std::thread::Result<RelationPath<Src, Dst>> {
            std::panic::catch_unwind(|| {
                __make_relation_path::<Src, Dst>(
                    source_column,
                    target_table,
                    RelationKind::ForeignKey,
                )
            })
        }

        #[test]
        fn accepts_plain_identifiers() {
            assert!(try_make("owner_id", "owners").is_ok());
        }

        #[test]
        fn validates_source_column() {
            // Source column is the first argument threaded through
            // the validator; a bad one panics before target_table is
            // checked.
            assert!(try_make("123", "owners").is_err());
        }

        #[test]
        fn validates_target_table() {
            // Target table is the second argument; a bad source would
            // panic first, so a good source + bad target pins that the
            // second call site also routes through the validator.
            assert!(try_make("owner_id", "select").is_err());
        }
    }
}
