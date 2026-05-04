//! Composition macros — Phase 8 §T2.
//!
//! This module groups the composition surfaces. T2.4 pivoted the
//! `Auditable` opt-in from a derive macro to the `#[model(auditable)]`
//! attribute (spec line 1037, locked 2026-05-03); T2.3's
//! `#[derive(SoftDeletable)]` derive remains.
//!
//! Each submodule exposes a single `expand(...)` entry point. The
//! `auditable` submodule's `expand` takes a model identifier + parsed
//! `ModelAttrs` and is called from `model::expand_inner`; the
//! `soft_deletable` submodule's `expand` takes a raw `TokenStream`
//! (derive input) and is called from `crate::lib::derive_soft_deletable`.
//!
//! # Path-routing convention
//!
//! Every macro emitted from this module routes the trait impl through
//! the public re-export `::djogi::Auditable` / `::djogi::SoftDeletable`
//! — **not** through `::djogi::__private::*`. The two composition
//! traits are convention-sealed (doc only — see
//! [`djogi::compose`] module docs); a sealed-supertrait route would
//! require `__private::compose::Sealed` plumbing, which T2.1
//! deliberately did not ship. See `feedback_macro_path_routing.md` for
//! the full routing rule.
//!
//! # Composition with `#[model(hooks)]` (T1)
//!
//! `#[model(auditable)]` and `#[model(hooks)]` compose orthogonally.
//! The composition populator runs BEFORE any user
//! `ModelHooks::before_create`, so user hooks can inspect or override
//! the populated `created_by` value. Adopter usage:
//!
//! ```rust,ignore
//! use djogi::prelude::*;
//!
//! #[model(table = "posts", auditable, hooks)]
//! #[derive(Debug, Clone)]
//! pub struct Post {
//!     pub title: String,
//!     pub created_by: Option<String>, // adopter declares the field
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
