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
