//! Composition macros —.
//! This module groups the composition surfaces. The
//! `Auditable` opt-in pivoted from a derive macro to the `#[model(auditable)]`
//! attribute (spec line 1037, locked 2026-05-03); later
//! `SoftDeletable` followed by pivoting to `#[model(soft_deletable)]`
//! for the same proc-macros-cannot-observe-sibling-derives constraint
//! — both opt-ins now route through the model
//! attribute. `#[derive(Auditable)]` and `#[derive(SoftDeletable)]`
//! are removed from the current surface.
//! Each submodule exposes a single `expand(model_ident, model_attrs)`
//! entry point called from `model::expand_inner`. Both functions
//! return an empty `TokenStream` when their respective opt-in flag is
//! absent so opt-out models pay zero macro-output overhead.
//! # Path-routing convention
//! Every macro emitted from this module routes the trait impl through
//! the public re-export `::djogi::Auditable` / `::djogi::SoftDeletable`
//! **not** through `::djogi::__private::*`. The two composition
//! traits are convention-sealed (doc only — see
//! [`djogi::compose`] module docs); a sealed-supertrait route would
//! require `__private::compose::Sealed` plumbing, which
//! was deliberately not shipped. See `feedback_macro_path_routing.md` for
//! the full routing rule.
//! # Composition with `#[model(hooks)]`
//! `#[model(auditable)]` / `#[model(soft_deletable)]` and
//! `#[model(hooks)]` compose orthogonally. The composition populator
//! runs BEFORE any user `ModelHooks::before_create`, so user hooks can
//! inspect or override the populated `created_by` value. Adopter usage:
//! ```rust,ignore
//! use djogi::prelude::*;
//!
//! #[model(table = "posts", auditable, soft_deletable, hooks)]
//! #[derive(Debug, Clone)]
//! pub struct Post {
//!     pub title: String,
//!     pub created_by: Option<String>,         // adopter declares the field
//!     pub deleted_at: Option<djogi::DateTime>, // adopter declares the field
//! }
//!
//! impl djogi::hooks::ModelHooks for Post {
//!     async fn before_create(
//!         &mut self,
//!         _ctx: &mut djogi::DjogiContext,
//!     ) -> Result<(), djogi::DjogiError> {
//!         // self.created_by has already been populated from auth (or
//!         // left as the user-set value). Hook can override or
//!         // validate freely.
//!         Ok(())
//!     }
//! }
//! ```

pub mod auditable;
pub mod soft_deletable;
