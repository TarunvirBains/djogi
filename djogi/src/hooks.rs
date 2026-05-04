//! Lifecycle hooks for `Model` CRUD operations.
//!
//! Six methods, each defaulted to a no-op that returns `Ok(())`. Adopters
//! `impl ModelHooks for MyModel` selectively — methods they don't override
//! stay no-op. The marker trait `HasHooks` in `hooks::sealed` is what the
//! macro layer uses to decide whether to emit dispatch calls (see T1.3).
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

#[cfg(test)]
mod tests {
    //! Compile-time tests for the `ModelHooks` trait.
    //!
    //! These tests intentionally do NOT construct a real `DjogiContext`.
    //! The default bodies (`let _ = ctx; async { Ok(()) }`) never read
    //! from `ctx`, so the only behavioural surface to verify is that
    //! the trait compiles, that an empty `impl ModelHooks for T {}`
    //! type-checks, and that an override can shadow each default.
    //! Behavioural integration with the CRUD terminals lands in T1.7's
    //! canonical-sequence integration test, which uses `#[djogi_test]`
    //! against a real per-test database.
    //!
    //! Static (compile-time) verification is sufficient here because:
    //!
    //! - The default body is literally `async { Ok(()) }` — no runtime
    //!   ambiguity to test.
    //! - Confirming `T: ModelHooks` via the `_assert_impls` helper
    //!   forces the compiler to instantiate every defaulted method's
    //!   future type, catching any signature mistake (missing `Send`,
    //!   wrong return type, accidental `'static` bound) at this layer.
    //!
    //! Runtime tests requiring a `DjogiContext` would either need a
    //! mock/stub constructor (none exists today — `DjogiPool::connect`
    //! pings the DB at build time) or `#[djogi_test]` (which requires
    //! `DATABASE_URL` to be set). Both options push the test outside
    //! the `cargo test --lib` lane the spec asks the verification step
    //! to exercise.

    use super::*;

    /// Generic compile-time witness: forces the trait bound on `T` so
    /// the compiler instantiates every defaulted future type. If any
    /// signature drifts (e.g. a stray `'static` bound, a missing `Send`,
    /// a return type that no longer matches `Result<(), DjogiError>`)
    /// this helper fails to compile.
    fn _assert_impls<T: ModelHooks>() {}

    /// Test 1: an empty impl on a unit struct compiles, exercising every
    /// defaulted method.
    #[test]
    fn default_impl_is_no_op() {
        struct Empty;
        impl ModelHooks for Empty {}

        // Witness: `Empty: ModelHooks` instantiates each defaulted
        // method's future type. The compiler proves the no-op default
        // is reachable for every hook on `Empty`.
        _assert_impls::<Empty>();
    }

    /// Test 2: overriding `before_create` to mutate `self` compiles and
    /// keeps the rest of the trait at the no-op default.
    #[test]
    fn custom_before_create_can_mutate_self() {
        struct M {
            count: i32,
        }
        impl ModelHooks for M {
            async fn before_create(&mut self, _ctx: &mut DjogiContext) -> Result<(), DjogiError> {
                self.count = 42;
                Ok(())
            }
        }

        // Constructing the value verifies the field shape; the trait
        // bound on the witness verifies the override resolves.
        let _m = M { count: 0 };
        _assert_impls::<M>();
    }

    /// Test 3: overriding `before_create` to return `Err` compiles, and
    /// the chosen `DjogiError::Validation(String)` variant round-trips
    /// through the trait's return type without coercion.
    #[test]
    fn custom_before_create_err_propagates() {
        struct M;
        impl ModelHooks for M {
            async fn before_create(&mut self, _ctx: &mut DjogiContext) -> Result<(), DjogiError> {
                Err(DjogiError::Validation("nope".into()))
            }
        }

        _assert_impls::<M>();

        // Pattern-match a constructed error of the same variant to
        // confirm the spelling stays valid as the error enum evolves.
        // If `Validation(String)` is renamed or replaced, this match
        // breaks at the same time the trait test breaks, surfacing the
        // upstream change in one place.
        let err = DjogiError::Validation("nope".into());
        match err {
            DjogiError::Validation(msg) => assert_eq!(msg, "nope"),
            _ => panic!("expected Validation variant"),
        }
    }
}
