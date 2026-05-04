//! Composition derive macros — Phase 8 §T2.
//!
//! This module groups the `#[derive(Auditable)]` (T2.2) and the
//! forthcoming `#[derive(SoftDeletable)]` (T2.3) proc macro
//! implementations. Each submodule exposes a single `expand(input)
//! -> TokenStream` entry point that the registrations in
//! [`crate::lib`](../lib.rs) forward to.
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
//! Derives in this module are designed to compose with the
//! [`#[model(hooks)]`](crate::model) attribute. Adopter usage:
//!
//! ```rust,ignore
//! use djogi::prelude::*;
//!
//! #[derive(Auditable)]
//! #[model(table = "posts", hooks)]
//! #[derive(Debug, Clone)]
//! pub struct Post {
//!     pub title: String,
//!     pub created_by: Option<String>, // adopter declares the field
//! }
//!
//! impl djogi::hooks::ModelHooks for Post {
//!     async fn before_create(
//!         &mut self,
//!         ctx: &mut djogi::DjogiContext,
//!     ) -> Result<(), djogi::DjogiError> {
//!         if let Some(auth) = ctx.auth() {
//!             self.created_by = Some(auth.user_id().to_string());
//!         }
//!         Ok(())
//!     }
//! }
//! ```
//!
//! T2.4 will introduce a macro-side helper that synthesises the
//! `before_create` body so the adopter no longer has to hand-write the
//! `auth()` capture; for T2.2 the derive is getter-only.

pub mod auditable;
pub mod soft_deletable;
