//! Lifecycle hooks for `Model` CRUD operations.
//!
//! Six methods, each defaulted to a no-op that returns `Ok(())`. Adopters
//! `impl ModelHooks for MyModel` selectively — methods they don't override
//! stay no-op.
//!
//! T1.2 adds the sealed [`HasHooks`] marker trait that the macro layer
//! emits in T1.3 (`#[model(hooks)]`) to gate monomorphic dispatch in
//! T1.4–T1.6. Until the macro layer lands, `HasHooks` has zero impls and
//! [`ModelHooks`] remains the public adopter trait — nothing dispatches
//! against either yet.
//!
//! # Async-fn-in-trait via `impl Future + Send`
//!
//! Each method returns `impl Future<Output = Result<(), DjogiError>> + Send`
//! rather than going through `BoxFuture` / `Pin<Box<...>>` / the
//! `async-trait` macro. The default body desugars to a state machine the
//! compiler elides at call sites that keep the no-op default — there is no
//! heap allocation, no virtual dispatch, and no `'static` escape on `Self`.
//! T1.8 verifies the zero-overhead claim with `cargo asm`.
//!
//! # Receiver shape
//!
//! `before_*` methods take `&mut self` so the hook body can mutate the
//! model before it is written to the database (e.g. setting `created_by`,
//! normalising a slug). `after_*` methods take `&self` because the row is
//! already persisted — there is nothing to mutate that the database would
//! pick up. Every method takes `&mut DjogiContext` so the hook body
//! inherits the surrounding tenant scope, [`AuthContext`], and the
//! `on_commit` queue (Phase 8 §D1).
//!
//! # Sequencing and error semantics
//!
//! The CRUD callers wire hooks into the canonical
//! `before → DB → outbox → after → on_commit drain` sequence (Phase 8 §D3).
//! Returning `Err` from any hook aborts the operation: the surrounding
//! transaction (if one is open) rolls back via the standard `?` propagation
//! path, and no `after_*` hook fires for an aborted operation (Phase 8 §D4).
//!
//! [`AuthContext`]: crate::auth::AuthContext

use crate::{DjogiContext, DjogiError};
use std::future::Future;

/// Lifecycle hooks an adopter implements for a `Model` to participate in
/// CRUD-time side effects.
///
/// All six methods default to a no-op that returns `Ok(())`. Adopters
/// override only the methods they care about — the rest stay no-op and
/// remain zero-cost at the call site. See the module-level docs for the
/// `before → DB → outbox → after → on_commit drain` sequence and the
/// `Err`-aborts-operation contract.
///
/// `Send` is required on every returned future because Djogi's CRUD
/// terminals are themselves `Send` futures driven by the multi-threaded
/// Tokio runtime — a hook future that is not `Send` would refuse to compile
/// at the call site rather than at the trait definition.
pub trait ModelHooks: Sized {
    /// Fired before a row is inserted into the database.
    ///
    /// `&mut self` lets the body mutate the model — typical uses set
    /// audit columns (`created_by`, `created_at` overrides) or normalise
    /// derived fields (slug, search vector source) before the INSERT
    /// statement composes its `RETURNING` clause.
    fn before_create(
        &mut self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }

    /// Fired after a row is successfully inserted, BEFORE the surrounding
    /// transaction commits.
    ///
    /// `&self` because the row is already persisted — mutations here
    /// would not round-trip back to the database. Use `on_commit`
    /// callbacks (queued via the `ctx`) for side effects that should fire
    /// only after the commit succeeds.
    fn after_create(
        &self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }

    /// Fired before a row is updated by `save()`.
    ///
    /// Mirrors [`before_create`](Self::before_create) for the update path.
    /// Typical uses bump `updated_by`, refresh derived columns, or run a
    /// validation pass that depends on the in-memory state of `self`.
    fn before_save(
        &mut self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }

    /// Fired after a row is successfully updated, BEFORE the surrounding
    /// transaction commits. Mirrors [`after_create`](Self::after_create).
    fn after_save(
        &self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }

    /// Fired before a row is deleted.
    ///
    /// `&mut self` is preserved for symmetry with the other `before_*`
    /// methods even though most delete-time hooks read state rather than
    /// write it — the receiver shape stays uniform across the trait so
    /// that a generic dispatcher (T1.3) can call any method by name
    /// without reasoning about per-method receiver kinds.
    fn before_delete(
        &mut self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }

    /// Fired after a row is successfully deleted, BEFORE the surrounding
    /// transaction commits. Mirrors [`after_create`](Self::after_create).
    fn after_delete(
        &self,
        ctx: &mut DjogiContext,
    ) -> impl Future<Output = Result<(), DjogiError>> + Send {
        let _ = ctx;
        async { Ok(()) }
    }
}

/// Sealed marker trait — the type-level gate the `#[model(hooks)]` macro
/// (T1.3) emits to opt a model into hook dispatch.
///
/// `HasHooks` carries no methods, no associated types, and no lifetime
/// parameters: it is purely a witness that lets the CRUD terminals
/// (T1.4–T1.6) branch monomorphically between the no-op fast path and the
/// hook-dispatch path. Without an `impl HasHooks for M`, the generic
/// `<M as HasHooks>::…` call sites collapse to dead code that LLVM
/// removes regardless of LTO settings (T1.8 verifies this with
/// `cargo asm`).
///
/// # Sealed via `private::Sealed`
///
/// The supertrait `private::Sealed` lives in a module-private inner
/// module so adopter code working only against the public surface
/// (`djogi::hooks::HasHooks`, `djogi::ModelHooks`) cannot name the seal
/// and therefore cannot write `impl HasHooks for MyType` from the public
/// path. The single emitter is the `#[model(hooks)]` proc macro, which
/// routes through `::djogi::__private::hooks::Sealed` (re-exported below)
/// per the macro-path-routing convention.
///
/// **Seal-by-convention caveat.** The `__private::hooks` re-export means
/// the `Sealed` trait is reachable cross-crate by name — downstream code
/// that deliberately reaches into `__private` (in violation of the
/// "we reserve the right to break that code in any future release without
/// notice" contract documented at the crate root) could still hand-roll
/// an `impl HasHooks` chain. This matches the convention used by
/// [`crate::visage_boundary`] for `DjogiVisageOf` / `VisageSealed` and
/// by [`crate::primary_key`] for the `PkSealToken` witness — Rust has no
/// way to mark a trait "implementable only inside this crate" when its
/// supertrait must be reachable from a separate proc-macro-emitting
/// crate, and we accept the convention-level seal as the trade-off.
pub trait HasHooks: ModelHooks + private::Sealed {}

mod private {
    /// Closed-world seal — only the `#[model(hooks)]` macro emits the
    /// `impl Sealed for M` that completes the `HasHooks` bound. Module-
    /// private so downstream crates cannot name the supertrait, but
    /// re-exported for macro-emitted code through
    /// `::djogi::__private::hooks::Sealed`.
    pub trait Sealed {}
}

/// Macro-callable re-exports.
///
/// The `#[model(hooks)]` proc macro (T1.3) emits paths through
/// `::djogi::__private::hooks::*` — never `::djogi::hooks::__seal::*`
/// directly — so that this module is the single coupling point between
/// the emitted code and the framework's seal machinery. Adopter code that
/// only writes `impl ModelHooks for M` and `#[model(hooks)]` never names
/// any symbol in this module.
#[doc(hidden)]
pub mod __seal {
    /// Const-witness seal. The macro evaluates `<() as MarkerSeal>::SEAL`
    /// (a const, not a value-construction) inside the generated impl
    /// block as a compile-time assertion that `djogi` is reachable —
    /// the trait is implemented for every `T`, so the const always
    /// resolves, but only macro-emitted code reaches for it.
    ///
    /// The point is not to gate compilation on the const itself — it is
    /// to give the macro a stable, name-routed handle into `djogi` that
    /// future T1.3 work can extend (e.g. with debug-mode assertions about
    /// the surrounding `impl ModelHooks` shape) without changing the
    /// macro's emitted token tree.
    pub trait MarkerSeal {
        const SEAL: ();
    }
    impl<T> MarkerSeal for T {
        const SEAL: () = ();
    }

    /// Re-export of the closed-world `Sealed` supertrait. The macro
    /// emits `impl ::djogi::__private::hooks::Sealed for M {}` alongside
    /// `impl ::djogi::hooks::HasHooks for M {}` so the supertrait bound
    /// is satisfied without exposing the seal at the public crate root.
    pub use super::private::Sealed;
}

#[cfg(test)]
mod tests {
    //! Runtime tests for the `ModelHooks` trait.
    //!
    //! Each test runs against a fresh per-test Postgres database via
    //! `#[djogi_test]`. The `DjogiContext` is the real one CRUD terminals
    //! receive, so a passing test here proves both that the trait compiles
    //! and that the awaited futures behave: the default body returns
    //! `Ok(())`, an override can mutate `self` through `before_create`, and
    //! an override returning `Err(DjogiError::Validation(_))` round-trips
    //! the variant without coercion.
    //!
    //! Behavioural integration with the CRUD terminals (the
    //! `before → DB → outbox → after → on_commit drain` sequence) lands in
    //! T1.7; these tests cover the trait surface in isolation.
    //!
    //! The crate root carries `#[cfg(test)] extern crate self as djogi;`
    //! so the absolute `::djogi::*` paths emitted by `#[djogi_test]` resolve
    //! to the current crate when the macro is used from inside the `djogi`
    //! crate's own unit-test code.

    use super::*;
    use djogi_macros::djogi_test;

    /// Test 1: every defaulted method on an empty `impl ModelHooks for T`
    /// awaits to `Ok(())`.
    #[djogi_test]
    async fn default_impl_is_no_op(mut ctx: DjogiContext) {
        struct Empty;
        impl ModelHooks for Empty {}

        let mut empty = Empty;

        empty
            .before_create(&mut ctx)
            .await
            .expect("default before_create returns Ok(())");
        empty
            .after_create(&mut ctx)
            .await
            .expect("default after_create returns Ok(())");
        empty
            .before_save(&mut ctx)
            .await
            .expect("default before_save returns Ok(())");
        empty
            .after_save(&mut ctx)
            .await
            .expect("default after_save returns Ok(())");
        empty
            .before_delete(&mut ctx)
            .await
            .expect("default before_delete returns Ok(())");
        empty
            .after_delete(&mut ctx)
            .await
            .expect("default after_delete returns Ok(())");
    }

    /// Test 2: overriding `before_create` mutates `self` before the
    /// awaited future resolves.
    #[djogi_test]
    async fn custom_before_create_can_mutate_self(mut ctx: DjogiContext) {
        struct M {
            count: i32,
        }
        impl ModelHooks for M {
            async fn before_create(&mut self, _ctx: &mut DjogiContext) -> Result<(), DjogiError> {
                self.count = 42;
                Ok(())
            }
        }

        let mut m = M { count: 0 };
        m.before_create(&mut ctx)
            .await
            .expect("override returns Ok(())");
        assert_eq!(m.count, 42, "override should have mutated self.count");
    }

    /// Test 3: overriding `before_create` to return `Err` propagates the
    /// `DjogiError::Validation(String)` variant unchanged through the
    /// trait's return type.
    #[djogi_test]
    async fn custom_before_create_err_propagates(mut ctx: DjogiContext) {
        struct M;
        impl ModelHooks for M {
            async fn before_create(&mut self, _ctx: &mut DjogiContext) -> Result<(), DjogiError> {
                Err(DjogiError::Validation("nope".into()))
            }
        }

        let mut m = M;
        let result = m.before_create(&mut ctx).await;
        let Err(DjogiError::Validation(msg)) = result else {
            panic!("expected Err(DjogiError::Validation(_)), got {result:?}");
        };
        assert_eq!(msg, "nope");
    }
}
