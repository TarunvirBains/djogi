//! Proc macros for the Djogi framework.
//!
//! Provides:
//!
//! - `#[model(table = "...")]` — the attribute macro that does field
//!   injection and derives all `Model` impls.
//! - `reverse_one_to_many!` / `reverse_one_to_one!` — function-like
//!   macros emitting reverse-relation accessor methods on the target
//!   model plus an `inventory::submit!` registration record.
//! - `many_to_many!` — function-like macro emitting one direction of
//!   a many-to-many relation: the `ManyToMany<Target>` trait impl,
//!   a named inherent accessor on the source type, and an
//!   `inventory::submit!` registration record.
//!
//! `#[derive(Model)]` is a no-op stub kept for potential future use.

mod many_to_many;
mod model;
mod reverse_relation;

use proc_macro::TokenStream;

/// The primary Djogi macro. Annotate any struct with `#[model(table = "...")]`
/// to inject framework fields and derive CRUD, `FromRow`, and model descriptor.
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[model(table = "posts")]
/// #[derive(Debug, Clone)]
/// pub struct Post {
///     pub title: String,
///     pub published: bool,
/// }
/// ```
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    model::expand(attr.into(), item.into()).into()
}

/// No-op stub — field injection requires `#[model]` (attribute macro).
/// Kept as a placeholder for future derive-based extensions.
///
/// NOTE: Only `field` is listed as a helper attribute here, not `model`.
/// Listing `model` as a helper would shadow the `#[model]` proc_macro_attribute
/// and cause ambiguous resolution (Post-Review Fix #4).
#[proc_macro_derive(Model, attributes(field))]
pub fn derive_model(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// Emit a reverse one-to-many accessor on a model.
///
/// Invocation form:
///
/// ```ignore
/// djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);
/// // expands to (roughly):
/// //
/// // impl Owner {
/// //     pub fn cars<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Vec<Vehicle>, DjogiError>> + Send + 'ctx
/// //     { ... filters Vehicle by owner_id ... }
/// // }
/// ```
///
/// The macro also emits an `inventory::submit!` registration carrying a
/// `ReverseRelationMarker` record — Phase 4.5's projection generator
/// walks those markers to discover every registered reverse accessor.
///
/// See [`djogi_macros::reverse_relation`] module docs for the full
/// expansion shape, the terminology note on "source" vs "target", and
/// the rationale for function-like (not derive) form.
#[proc_macro]
pub fn reverse_one_to_many(input: TokenStream) -> TokenStream {
    reverse_relation::expand(
        input.into(),
        reverse_relation::AccessorKindOpaque::ONE_TO_MANY,
    )
    .into()
}

/// Emit a reverse one-to-one accessor on a model.
///
/// Invocation form:
///
/// ```ignore
/// djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
/// // expands to (roughly):
/// //
/// // impl User {
/// //     pub fn profile<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Option<Profile>, DjogiError>> + Send + 'ctx
/// //     { ... returns .first() match on Profile.user_id ... }
/// // }
/// ```
///
/// Intended for reverses of `OneToOneField<Receiver>` (or a
/// `ForeignKey<Receiver>` + `UNIQUE` pair on the foreign side) — the
/// `.first()` terminal is correct when the schema guarantees at most
/// one matching row. If the schema does not enforce uniqueness, prefer
/// `reverse_one_to_many!` to surface the fact that multiple rows are
/// possible.
///
/// Also emits an `inventory::submit!` marker with
/// `RelationKind::O2O`.
#[proc_macro]
pub fn reverse_one_to_one(input: TokenStream) -> TokenStream {
    reverse_relation::expand(
        input.into(),
        reverse_relation::AccessorKindOpaque::ONE_TO_ONE,
    )
    .into()
}

/// Emit one direction of a many-to-many relation — the
/// `ManyToMany<Target>` trait impl, the named inherent accessor on the
/// source type, and an inventory marker for Phase 4.5.
///
/// Invocation form:
///
/// ```ignore
/// djogi::many_to_many!(
///     Person, Group,
///     through  = PersonGroup,
///     this_fk  = person_id,
///     that_fk  = group_id,
///     relation = "groups"
/// );
/// // expands to (roughly):
/// //
/// // impl djogi::relation::ManyToMany<Group> for Person {
/// //     type Through = PersonGroup;
/// //     const RELATION: &'static str = "groups";
/// //     fn this_fk() -> &'static str { "person_id" }
/// //     fn that_fk() -> &'static str { "group_id" }
/// //     async fn related(...) { ... }
/// //     async fn add_related(...) { ... }
/// //     async fn remove_related(...) { ... }
/// // }
/// //
/// // impl Person {
/// //     pub fn groups<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Vec<Group>, DjogiError>> + Send + 'ctx
/// //     { <Self as ManyToMany<Group>>::related(self, ctx) }
/// // }
/// //
/// // inventory::submit! { ReverseRelationMarker { kind: M2M, ... } }
/// ```
///
/// See [`djogi_macros::many_to_many`] module docs (crate-internal) for
/// the full expansion shape, the rationale for emitting one direction
/// per call, and the seal story for the identifier arguments.
#[proc_macro]
pub fn many_to_many(input: TokenStream) -> TokenStream {
    many_to_many::expand(input.into()).into()
}
