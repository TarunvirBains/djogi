//! Sealed boundary marker trait for visage projections.
//!
//! `impl DjogiVisageOf<M> for V` asserts that `V` is a visage (or the
//! model itself) projecting fields from model `M`. The reflexive blanket
//! `impl<M: Model> DjogiVisageOf<M> for M` lets model-scoped code that
//! expects "something that is-or-projects-M" accept either the raw model
//! or one of its visages uniformly.
//!
//! # Why a seal?
//!
//! Emitted visages live in user crates and must be pairable with their
//! source model at the type level — traversal combinators and filter entry
//! points take `V: DjogiVisageOf<M>` bounds so a visage of `User` cannot
//! accidentally plug into a query over `Post`.
//! The seal prevents hostile downstream code from `impl`-ing
//! `DjogiVisageOf<Post> for UserPublic` and smuggling a mismatched
//! visage past the bound. The `private::Sealed<M>` supertrait is the
//! closed-world gate: only the proc macro (via
//! `::djogi::__private::Sealed`) and the reflexive blanket below reach
//! into it.
//!
//! # Path routing
//!
//! The sealed sub-trait is re-exported through `djogi::__private::Sealed`
//! so the `#[model]` proc macro can emit the two impls (`Sealed<M>` +
//! `DjogiVisageOf<M>`) without the user's crate needing a direct
//! `djogi::visage_boundary` import. This matches the macro-path-routing
//! convention (`feedback_macro_path_routing.md`): macro output goes
//! through `::djogi::*` paths only.

/// Sealed marker: `V` is a visage of model `M`.
///
/// Implemented automatically by `#[model]`-generated code for every
/// emitted visage struct (`UserPublic`, `UserSelfView`, `UserAdmin`,
/// `UserExport`). A reflexive blanket also makes every `M: Model` a
/// visage of itself, so generic code bounded on `V: DjogiVisageOf<M>`
/// composes uniformly across both shapes.
///
/// The trait is sealed via `private::Sealed<M>` — downstream crates
/// cannot add their own impls.
pub trait DjogiVisageOf<M: crate::model::Model>: private::Sealed<M> {}

// Reflexive blanket: every Model is a visage of itself.
impl<M: crate::model::Model> DjogiVisageOf<M> for M {}

/// Closed-world seal. Re-exported through `::djogi::__private::VisageSealed`
/// so macro-emitted visage code can satisfy the bound; **not part of the
/// public API surface.** The module is `#[doc(hidden)] pub` because the
/// proc macro emits a cross-crate path through it; downstream code must
/// reach the trait only via the `__private` re-export.
///
/// # Do not implement this trait
///
/// This seal is enforced by convention, not by the compiler — Rust has no
/// way to mark a trait "implementable only inside this crate" when its
/// supertrait must be reachable from a separate proc-macro-emitting
/// crate. Any code that hand-implements `djogi::__private::VisageSealed`
/// (or `djogi::visage_boundary::private::Sealed`) for a type outside the
/// `#[model]` macro's emission is breaking the contract; we reserve the
/// right to break that code in any future release without notice. The
/// existing `apps::__DJOGI_APPS_SEAL_TOKEN` and `model::__sealed::Sealed`
/// surfaces follow the same convention.
#[doc(hidden)]
pub mod private {
    /// Seal marker — only `#[model]`-emitted code and the reflexive
    /// blanket below are expected to satisfy this trait. See the parent
    /// module's "Do not implement this trait" note.
    pub trait Sealed<M> {}

    // Reflexive: every Model is sealed for itself.
    impl<M: crate::model::Model> Sealed<M> for M {}
}
