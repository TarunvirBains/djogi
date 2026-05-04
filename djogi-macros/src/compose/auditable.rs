//! `#[model(auditable)]` — emit the `Auditable` trait impl plus the
//! `__djogi_auditable_populate` helper invoked from `Model::create`.
//!
//! Phase 8 §T2.4.
//!
//! # 2026-05-03 design pivot
//!
//! T2.2 (commit 939b9ab) shipped `#[derive(Auditable)]` as the opt-in
//! surface; T2.4 supersedes it with `#[model(auditable)]` per spec
//! line 1037 (locked 2026-05-03, lens). Proc macros cannot observe
//! sibling derives — `#[derive(Auditable)]` could not deterministically
//! signal to `#[derive(Model)]` / `#[model(...)]`. Rather than a
//! "stack two macros" idiom, the single `#[model(auditable)]`
//! attribute solves it cleanly: the model expansion emits the
//! `Auditable` trait impl AND the populator AND wires the
//! `before_create` hook in one pass.
//!
//! # What this module emits
//!
//! Given a struct `MyModel` with `pub created_by: Option<String>`:
//!
//! ```rust,ignore
//! impl ::djogi::Auditable for MyModel {
//!     fn created_by(&self) -> ::std::option::Option<&str> {
//!         self.created_by.as_deref()
//!     }
//! }
//!
//! impl MyModel {
//!     /// Auditable population — invoked by the macro-emitted
//!     /// `Model::create` body before any user
//!     /// `ModelHooks::before_create`. Reads `AuthContext.user_id`
//!     /// (Display impl) when present.
//!     /// Phase 8 §D6.
//!     #[doc(hidden)]
//!     pub(crate) fn __djogi_auditable_populate(
//!         &mut self,
//!         ctx: &mut ::djogi::DjogiContext,
//!     ) {
//!         if self.created_by.is_none() {
//!             self.created_by = ctx.auth()
//!                 .map(|a| ::std::format!("{}", a.user_id));
//!         }
//!     }
//! }
//! ```
//!
//! No fields are added, removed, or renamed. The adopter still
//! declares `pub created_by: Option<String>` themselves (Path B per
//! Phase 8 v3 line 866 — preserved across the T2.2→T2.4 pivot).
//!
//! # Path B — adopter declares the `created_by` field
//!
//! Phase 8 v3 line 866 settled the field-injection question on Path B:
//! the adopter declares `pub created_by: Option<String>` on the
//! struct. The macro emits only the trait impl + populator. The T2.4
//! pivot does not change this: the surface flipped from a derive to
//! an attribute, but field injection still does not happen. When the
//! field is missing, the emitted `self.created_by.as_deref()` /
//! `self.created_by.is_none()` produces an actionable rustc
//! diagnostic (`error[E0609]: no field "created_by" on type ...`).
//!
//! # Composition with `#[model(hooks)]`
//!
//! `#[model(auditable)]` and `#[model(hooks)]` compose orthogonally.
//! The composition populator runs BEFORE any user
//! `ModelHooks::before_create`, so user hooks can inspect or override
//! the populated `created_by` value (spec line 990). The ordering is
//! enforced by `crud.rs::create_body`:
//!
//! ```text
//! #auto_set_tenant
//! #create_value_binding
//! #auditable_populate     ← T2.4: composition populator
//! #before_create_call     ← T1.4: user hook
//! #sequence_upsert_preamble  ← T1 BLOCK-1 fix
//! ... INSERT, outbox, after_create ...
//! ```
//!
//! # Display vs Debug for `user_id`
//!
//! The populator emits `format!("{}", a.user_id)` — Display — not
//! `format!("{:?}", a.user_id)` — Debug. Per spec line 1064, Debug
//! shape is unstable. `HeerId` implements `Display` per the framework
//! cols established in Phase 1.5; `RanjId` likewise. The Display
//! shape is the canonical string representation.
//!
//! # No warn-on-null
//!
//! When `ctx.auth()` returns `None` (framework-internal contexts:
//! seeds, migrations), `created_by` stays `None`. NO `tracing::warn!`
//! per spec line 1049 — production stability axis: framework-internal
//! contexts are expected to run without auth, and warning would be
//! operational noise. Adopters who want stricter behaviour write a
//! `before_create` hook that errors when `created_by.is_none()`
//! (criterion-4 escape hatch on top of a criterion-2 default).
//!
//! # User-set value preserved
//!
//! The `if self.created_by.is_none()` guard inside
//! `__djogi_auditable_populate` is load-bearing (spec line 1062): a
//! user-set `created_by` is never clobbered. The composition
//! populator runs before user `before_create`, so the user's hook
//! can also override the populated value if it sees fit.
//!
//! # Sealing
//!
//! [`djogi::Auditable`] is convention-sealed (doc only — see
//! `djogi/src/compose.rs` module docs for the full rationale). The
//! emitted impl therefore routes through the public re-export
//! `::djogi::Auditable`, not through `::djogi::__private::*`. T1.3's
//! `HasHooks` impl uses the `__private` route because `HasHooks` is
//! supertrait-sealed via `__private::hooks::Sealed`; `Auditable` is
//! not, so the canonical public path is correct. See
//! `feedback_macro_path_routing.md` for the full routing convention.

use crate::model::attrs::ModelAttrs;
use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// Emit the `Auditable` trait impl + `__djogi_auditable_populate`
/// helper when `#[model(auditable)]` is set.
///
/// Returns an empty [`TokenStream`] without invoking `quote!` when
/// the flag is absent so opt-out models pay zero macro-output
/// overhead. The companion call inside `create_body` —
/// `value.__djogi_auditable_populate(ctx);` — is gated on the same
/// flag in `crud.rs`, so non-auditable models also pay zero
/// dispatch cost at create time.
///
/// The two emissions are kept together (one trait impl + one
/// inherent impl) so a single `#[model(auditable)]` toggle controls
/// the whole surface. Splitting them across two emit functions
/// would invite drift if someone added a third emission later.
pub fn expand(model_ident: &Ident, model_attrs: &ModelAttrs) -> TokenStream {
    if !model_attrs.auditable {
        return TokenStream::new();
    }
    quote! {
        // Trait impl — `Auditable` getter exposing the adopter-declared
        // `created_by: Option<String>` as `Option<&str>`. Borrowed —
        // no allocation, no copy.
        impl ::djogi::Auditable for #model_ident {
            fn created_by(&self) -> ::std::option::Option<&str> {
                self.created_by.as_deref()
            }
        }

        // Populator helper — invoked from `Model::create` between
        // `auto_set_tenant` and the user `before_create` hook (Phase 8
        // §D6). The `is_none()` guard is load-bearing: a user-set
        // `created_by` is never clobbered. Spec line 1062.
        //
        // `#[doc(hidden)] pub(crate)` because the helper is a
        // macro-call surface, not adopter API. `pub(crate)` would
        // collide with the consumer's crate boundary if the model is
        // declared in a downstream crate; the actual visibility
        // emitted is `pub` so the macro-emitted call site (in
        // `crud.rs::create_body`, expanded into the same downstream
        // crate) can reach it. `#[doc(hidden)]` keeps it out of
        // adopter rustdoc.
        impl #model_ident {
            #[doc(hidden)]
            pub fn __djogi_auditable_populate(
                &mut self,
                ctx: &mut ::djogi::DjogiContext,
            ) {
                if self.created_by.is_none() {
                    self.created_by = ctx
                        .auth()
                        .map(|a| ::std::format!("{}", a.user_id));
                }
            }
        }
    }
}
