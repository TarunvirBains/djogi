//! Proc macros for the Djogi framework.
//!
//! Provides:
//!
//! - `#[model(table = "...")]` — the attribute macro that does field
//!   injection and derives all `Model` impls.
//! - `reverse_one_to_many!` / `reverse_one_to_one!` — function-like
//!   macros emitting reverse-relation accessor methods on the target
//!   model plus an `inventory::submit!` registration record.
//!
//! `#[derive(Model)]` is a no-op stub kept for potential future use.

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
/// //     pub fn cars<'a, E>(&'a self, executor: E)
/// //         -> impl Future<Output = Result<Vec<Vehicle>, DjogiError>> + Send + 'a
/// //     where E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send + 'a,
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
/// //     pub fn profile<'a, E>(&'a self, executor: E)
/// //         -> impl Future<Output = Result<Option<Profile>, DjogiError>> + Send + 'a
/// //     where E: sqlx::Executor<'a, Database = sqlx::Postgres> + Send + 'a,
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
