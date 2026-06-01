//! `#[model(soft_deletable)]` — emit the `SoftDeletable` trait impl.
//! (supersedes `#[derive(SoftDeletable)]`).
//! # 2026-05-03 design pivot
//! Commit 863c4cb shipped `#[derive(SoftDeletable)]` as the
//! opt-in surface; the current design supersedes it with `#[model(soft_deletable)]`,
//! mirroring the Auditable pivot. Proc macros cannot observe
//! sibling derives — `#[derive(SoftDeletable)]` could not
//! deterministically signal to `#[derive(Model)]` / `#[model(...)]`.
//! Automatic default-filter composition will need to know the
//! model is soft-deletable AT model-macro expansion time, so doing the
//! migration NOW is cheaper than later (otherwise composition wiring
//! would need to be unwound).
//! # What this module emits
//! Given a struct `MyModel` with `pub deleted_at: Option<djogi::DateTime>`:
//! ```rust,ignore
//! impl ::djogi::SoftDeletable for MyModel {
//!     fn deleted_at(&self) -> ::std::option::Option<::djogi::types::DateTime> {
//!         self.deleted_at
//!     }
//! }
//! ```
//! No fields are added, removed, or renamed. The adopter still
//! declares `pub deleted_at: Option<DateTime>` themselves (Path B per
//! Preserved across the design pivot).
//! The `COLUMN` const on the `SoftDeletable` trait carries the default
//! `"deleted_at"` value at the trait level; this emission inherits
//! the default and does not override the const. `QuerySet::not_deleted`
//! reads the column name through `<M as SoftDeletable>::COLUMN` rather
//! than from a hard-coded string, so future column-rename overrides are
//! one trait-const override away.
//! # Path B — adopter declares the `deleted_at` field
//! Settled the field-injection question on Path B:
//! the adopter declares `pub deleted_at: Option<djogi::DateTime>` on
//! the struct. The macro emits only the trait impl. The
//! pivot does not change this: the surface flipped from a derive to an
//! attribute, but field injection still does not happen. When the
//! field is missing, the emitted `self.deleted_at` produces an
//! actionable rustc diagnostic (`error[E0609]: no field "deleted_at"
//! on type ...`).
//! # Default-filter composition deferred
//! The current implementation ships **only** the trait impl. The runtime helper
//! `QuerySet::not_deleted` (in `djogi/src/query/queryset.rs`) is a
//! manual filter the adopter invokes per `objects` chain, now reading
//! the column name through `<M as SoftDeletable>::COLUMN`. **Automatic**
//! default-filter composition — making `Model::objects` exclude
//! soft-deleted rows by default and exposing an `_insecurely`
//! bypass — is deferred to once the `Q<T>` substrate
//! lands. Per spec line 971 (RESOLVED 2026-05-03, lens, locked):
//! substrate decisions belong with the substrate refactor.
//! # Composition with `#[model(...)]`
//! Adopter usage:
//! ```rust,ignore
//! use djogi::prelude::*;
//!
//! #[model(table = "posts", soft_deletable)]
//! #[derive(Debug, Clone)]
//! pub struct Post {
//!     pub title: String,
//!     pub deleted_at: Option<djogi::DateTime>,
//! }
//! ```
//! `#[model(soft_deletable)]` and `#[model(auditable)]` / `#[model(hooks)]`
//! compose orthogonally — the model macro produces independent impl
//! blocks for each opt-in.
//! # Sealing
//! [`djogi::SoftDeletable`] is convention-sealed (doc only — see
//! `djogi/src/compose.rs` module docs for the full rationale). The
//! emitted impl therefore routes through the public re-export
//! `::djogi::SoftDeletable`, not through `::djogi::__private::*`.
//! See `feedback_macro_path_routing.md` for the full routing
//! convention.

use crate::model::attrs::ModelAttrs;
use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// Emit the `SoftDeletable` trait impl when `#[model(soft_deletable)]`
/// is set.
/// Returns an empty [`TokenStream`] without invoking `quote!` when
/// the flag is absent so opt-out models pay zero macro-output
/// overhead. Mirrors the Auditable pattern.
/// The emitted impl inherits the trait-level
/// `const COLUMN: &'static str = "deleted_at"` default — the column
/// name lives on the trait, not in this macro emission, so
/// `QuerySet::not_deleted` and any future SoftDeletable consumer
/// reads it through `<M as SoftDeletable>::COLUMN` rather than a
/// hard-coded string. A future per-model column-rename path can
/// override the const at the impl level without changing this
/// emission shape.
pub fn expand(model_ident: &Ident, model_attrs: &ModelAttrs) -> TokenStream {
    if !model_attrs.soft_deletable {
        return TokenStream::new();
    }
    quote! {
        // Trait impl — `SoftDeletable` getter exposing the adopter-declared
        // `deleted_at: Option<DateTime>` as `Option<DateTime>` (copy
        // `OffsetDateTime` is `Copy`-bounded under the `time` crate's
        // surface).
        impl ::djogi::SoftDeletable for #model_ident {
            fn deleted_at(&self) -> ::std::option::Option<::djogi::types::DateTime> {
                self.deleted_at
            }
        }
    }
}
