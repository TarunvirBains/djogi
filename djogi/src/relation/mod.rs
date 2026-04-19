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
/// [`__make_relation_path`](__private::__make_relation_path), which rejects
/// any identifier that does not match `[A-Za-z0-9_]+` — a character class
/// Postgres accepts for unquoted identifiers. Even if a downstream crate
/// finds and calls the helper, it cannot smuggle SQL through.
#[doc(hidden)]
pub mod __private {
    use super::path::{RelationKind, RelationPath};
    use crate::model::Model;

    /// Panic message format for the identifier validation. A panic (rather
    /// than `Result`) is appropriate here because reaching the helper with
    /// a bad identifier indicates either a broken proc-macro emission or a
    /// downstream caller deliberately bypassing the seal — both are
    /// framework-bug or misuse cases that the caller cannot recover from.
    #[inline]
    fn assert_plain_ident(value: &'static str, role: &'static str) {
        // Empty strings would produce broken SQL (`SELECT FROM  p LEFT JOIN  t`)
        // and cannot be legitimate identifiers.
        assert!(
            !value.is_empty(),
            "djogi::relation: macro-emitted {role} must not be empty — this is a framework bug"
        );
        // Postgres unquoted identifiers match `[A-Za-z_][A-Za-z0-9_$]*` per
        // the SQL grammar, but djogi restricts further to `[A-Za-z0-9_]+` so
        // the check stays trivial to reason about and rejects the `$`
        // edge case entirely. Both user column names and table names
        // emitted by `#[derive(Model)]` already conform to this narrower
        // class — there is no legitimate macro path that fails the check.
        for byte in value.bytes() {
            let ok = byte.is_ascii_alphanumeric() || byte == b'_';
            assert!(
                ok,
                "djogi::relation: macro-emitted {role} {value:?} contains a non-identifier \
                 character — either the proc-macro emission is broken or downstream code \
                 bypassed the RelationPath seal"
            );
        }
    }

    /// Construct a [`RelationPath<Source, Target>`] from macro-emitted
    /// identifier strings. The only supported caller is the
    /// `{Source}Related::relation_name()` method that `#[derive(Model)]`
    /// emits in the user's crate.
    ///
    /// Panics if `source_column` or `target_table` contain any character
    /// outside `[A-Za-z0-9_]` — see [`assert_plain_ident`] for the
    /// rationale. The check is the runtime half of the seal; the
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
}
