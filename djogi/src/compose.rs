//! Composition primitives — `Auditable` and `SoftDeletable`.
//! These are the runtime trait surfaces a model picks up when adopters
//! opt in via `#[model(auditable)]` (supersedes the legacy
//! `#[derive(Auditable)]` per spec line 1037, locked 2026-05-03) or
//! `#[model(soft_deletable)]` (supersedes the legacy
//! `#[derive(SoftDeletable)]` for symmetry with the auditable surface
//! and to de-risk automatic default-filter composition).
//! Initial landing covered trait shapes only; the full macro emissions
//! are the source of truth for behavior.
//! Downstream code that only needs to *bound a generic* on "models with
//! audit fields" or "models with soft-delete semantics" can import
//! these traits today.
//! # Why two traits, no methods beyond the field accessors?
//! §D6 (lines 149–157 of the v3 plan) settles the audit-field
//! shape: `created_by: Option<String>` populated from
//! `AuthContext.user_id` at create time when `ctx.auth().is_some()`, and
//! left as `None` otherwise (no warn-on-null). The trait exposes the
//! getter as `Option<&str>` so callers do not pay a `String` clone to
//! observe the audit user.
//! `SoftDeletable` mirrors the same pattern for `deleted_at:
//! Option<DateTime>`. Models implementing the trait acquire a default
//! filter that excludes rows where `deleted_at IS NOT NULL`; adopter-side
//! bypass goes through the `_insecurely()` audit-warning shape — same
//! `set_tenant` precedent already in [`crate::DjogiContext`].
//! That filter and bypass live in the query layer;
//! this module is intentionally bound surface only.
//! # Sealing model — convention-sealed, not compile-enforced
//! V3 spec line 758 directs this module to "convention-seal per
//! `decisions.md` row 78 pattern". The intent — per the explicit
//! [CHECK] callout on : "convention-sealed = doc comment
//! plus trait visibility, no compile-enforced seal" — is doc-only seal.
//! No `private::Sealed` supertrait, no `__seal::Sealed` re-export. The
//! reasons:
//! 1. The traits are *user-implementable in shape* — adopter macros
//!    (`#[model(auditable)]` / `#[model(soft_deletable)]`)
//!    emit `impl Auditable for UserModel` / `impl SoftDeletable for
//! UserModel` directly. If we sealed them via a supertrait, the
//!    macro emission would need to route through
//!    `::djogi::__private::compose::Sealed` (the [`crate::hooks`]
//!    precedent). This module explicitly defers macro work, and threading a
//!    seal across two follow-up commits adds churn for no protection
//!    benefit at this stage.
//! 2. The framework's harder seals (`Model` via [`crate::model::__sealed`],
//!    `HasHooks` via [`crate::hooks`], `App` via the apps-seal token,
//!    `PrimaryKey` via `PkSealToken`) defend an SQL-injection or
//!    correctness boundary — a hand-rolled `impl Model` could smuggle
//!    `table_name()` strings into the emitter. `Auditable` and
//!    `SoftDeletable` carry no such boundary: the only methods are
//!    field getters that return `Option<&str>` and `Option<DateTime>`.
//!    A hostile hand-rolled impl can lie about its audit user, but
//!    every other read of the column already routes through the same
//!    `FromPgRow` decode the macros emit, so the lie never leaves the
//!    in-memory copy.
//! 3. `decisions.md` row "Apps seal enforcement" already documents that
//!    "True hard-sealing of a proc-macro-emitted trait is not achievable
//!    in stable Rust" — every public path the macro emission needs is
//!    downstream-reachable too. The supertrait approach buys a cosmetic
//!    barrier, not a real one. We document the convention here and move
//!    on.
//!    If a future phase decides `Auditable` / `SoftDeletable` need
//!    compile-enforced seals (e.g. because a security review surfaces a
//!    threat the field-getter shape cannot mitigate), the upgrade path is
//!    straightforward: add a `pub(crate) mod private { pub trait Sealed
//! {} }` supertrait, re-export through `crate::__private::compose` for
//!    macro emission, and follow the [`crate::hooks::HasHooks`] precedent
//!    at `djogi/src/hooks.rs:171`. Until then, the convention seal is
//!    load-bearing through doc comments alone.
//! # Phase / spec anchors
//! - line 221 — "`djogi/src/compose.rs` — runtime helpers
//!   `Auditable` / `SoftDeletable` traits (sealed; convention-sealed per
//!   `decisions.md` row 78 pattern)."
//! - v3 §D6 lines 149–157 — `created_by` nullable, AuthContext-
//!   driven population, no warn on null.
//! - `feedback_macro_path_routing.md` — runtime trait module routes
//!   directly through `crate::types::DateTime`, **not** via
//!   `crate::__private::time` (the `__private` re-export exists for
//!   macro-emitted code, not for hand-written framework modules).

use crate::model::Model;
use crate::types::DateTime;

/// Marker trait emitted by `#[model(auditable)]`
/// (supersedes the legacy `#[derive(Auditable)]` per spec line 1037).
/// A model carrying this bound declares `created_by: Option<String>`
/// itself (Path B per) and the
/// `#[model(auditable)]` attribute emits the trait impl plus an
/// inherent `__djogi_auditable_populate` helper invoked from
/// [`Model::create`](crate::model::Model::create) before the user
/// `before_create` hook. When [`ctx.auth()`](crate::context::DjogiContext::auth)
/// is `Some`, the helper captures `format!("{}", auth.user_id)`
/// (Display, not Debug — Debug shape is unstable per spec line 1064)
/// into the field unless the user already set a value; otherwise the
/// field stays `None`. No warn-on-null per §D6.
/// The single accessor returns a borrowed `&str` to keep audit reads
/// allocation-free in hot paths (request-side rendering, audit-log
/// emission).
/// # Example bound
/// ```ignore
/// fn render_audit_line<M: djogi::Auditable>(m: &M) -> String {
/// match m.created_by() {
/// Some(user) => format!("created by {user}"),
/// None       => "created by system".to_string(),
/// }
/// }
/// ```
pub trait Auditable: Model {
    /// Returns the user that created this row, or `None` when the row
    /// was created without an authenticated [`crate::auth::AuthContext`]
    /// (system jobs, migration replay, seed data).
    fn created_by(&self) -> Option<&str>;
}

/// Marker trait emitted by `#[model(soft_deletable)]`
/// (supersedes the legacy `#[derive(SoftDeletable)]` for the same
/// proc-macros-cannot-observe-sibling-derives constraint that drove
/// the auditable pivot).
/// A model carrying this bound declares `deleted_at: Option<DateTime>`
/// itself (Path B per) and the
/// `#[model(soft_deletable)]` attribute emits the trait impl.
/// `objects()` on a soft-deletable model excludes deleted rows by default
/// via an automatic `deleted_at IS NULL` filter; the explicit bypass is the
/// macro-emitted `objects_including_deleted()`. The manual
/// [`QuerySet::not_deleted()`](crate::query::QuerySet::not_deleted) helper
/// remains for explicit re-application on a bypassed queryset and reads the
/// column name through `<M as SoftDeletable>::COLUMN`.
/// This trait is the bound surface used by code that needs to talk
/// generically about "models with soft-delete semantics" — for example,
/// the visage layer's "include trashed rows" toggle.
/// # Example bound
/// ```ignore
/// fn purge_window<M: djogi::SoftDeletable>(m: &M) -> Option<i64> {
/// m.deleted_at()
/// .map(|dt| (djogi::DateTime::now_utc() - dt).whole_seconds())
/// }
/// ```
pub trait SoftDeletable: Model {
    /// SQL column name for the soft-delete timestamp. Defaults to
    /// `"deleted_at"`.
    /// Reading via `<M as SoftDeletable>::COLUMN` from generic code
    /// lets [`QuerySet::not_deleted()`](crate::query::QuerySet::not_deleted)
    /// and any future `SoftDeletable` consumer key off the trait
    /// surface instead of a hard-coded string. 6 keeps the
    /// canonical `"deleted_at"` value as the trait default so today's
    /// macro-emitted impls inherit it without per-model override
    /// emission. A future per-model column-rename path (e.g.
    /// `#[model(soft_deletable(column = "trashed_at"))]`) can override
    /// the const at the `impl` level — non-breaking extension.
    const COLUMN: &'static str = "deleted_at";

    /// Returns the soft-delete timestamp for this row, or `None` if the
    /// row is live.
    fn deleted_at(&self) -> Option<DateTime>;
}

#[cfg(test)]
#[allow(clippy::manual_async_fn)]
// `Model`'s CRUD methods return `impl Future + Send` rather than using
// `async fn` syntax (pinned to `Send` explicitly). The inert stub below
// mirrors that trait shape, which trips `clippy::manual_async_fn` under
// Rust 1.93+. Mirror the allow used by `crate::query::field::tests`
// for the same reason.
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::context::DjogiContext;
    use crate::descriptor::ModelDescriptor;
    use std::future::Future;

    // Inert `Model` stub — same shape as the canonical test stub in
    // `crate::query::field::tests`. Exists purely so we can attach an
    // `impl Auditable` / `impl SoftDeletable` block and verify the trait
    // signatures compile against a real `Model`. The trait method bodies
    // are unreachable; this is a compile-time smoke test, not a runtime
    // assertion.
    struct DummyAuditable {
        created_by: Option<String>,
    }

    struct DummySoftDeletable {
        deleted_at: Option<DateTime>,
    }

    macro_rules! impl_inert_model {
        ($ty:ty) => {
            impl crate::model::__sealed::Sealed for $ty {}
            impl Model for $ty {
                type Pk = crate::types::HeerId;
                type Fields = ();
                fn table_name() -> &'static str {
                    "dummy"
                }
                fn pk_value(&self) -> &Self::Pk {
                    unreachable!()
                }
                fn descriptor() -> &'static ModelDescriptor {
                    unreachable!()
                }
                fn get(
                    _ctx: &mut DjogiContext,
                    _id: Self::Pk,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn create(
                    _ctx: &mut DjogiContext,
                    _v: Self,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn save<'ctx>(
                    &'ctx mut self,
                    _ctx: &'ctx mut DjogiContext,
                ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
                fn delete(
                    self,
                    _ctx: &mut DjogiContext,
                ) -> impl Future<Output = Result<(), DjogiError>> + Send {
                    async { unreachable!() }
                }
                fn refresh_from_db<'ctx>(
                    &'ctx self,
                    _ctx: &'ctx mut DjogiContext,
                ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
                    async { unreachable!() }
                }
            }
        };
    }

    impl_inert_model!(DummyAuditable);
    impl_inert_model!(DummySoftDeletable);

    impl Auditable for DummyAuditable {
        fn created_by(&self) -> Option<&str> {
            self.created_by.as_deref()
        }
    }

    impl SoftDeletable for DummySoftDeletable {
        fn deleted_at(&self) -> Option<DateTime> {
            self.deleted_at
        }
    }

    /// Compile-time smoke test — proves the trait signatures don't
    /// reference `Self::Pk` in surprising ways and that a hand-written
    /// `impl Auditable` / `impl SoftDeletable` block satisfies the
    /// `crate::Model` super-bound when the model is otherwise inert.
    /// Required by .1 line 793.
    #[test]
    fn traits_compile_with_dummy_models() {
        let a = DummyAuditable {
            created_by: Some("user-42".to_string()),
        };
        assert_eq!(a.created_by(), Some("user-42"));

        let none_user = DummyAuditable { created_by: None };
        assert_eq!(none_user.created_by(), None);

        let live = DummySoftDeletable { deleted_at: None };
        assert_eq!(live.deleted_at(), None);

        let trashed = DummySoftDeletable {
            deleted_at: Some(DateTime::UNIX_EPOCH),
        };
        assert_eq!(trashed.deleted_at(), Some(DateTime::UNIX_EPOCH));

        // Confirm the traits can be used as bounds — this exercises the
        // `Auditable: crate::Model` super-bound at compile time.
        fn assert_auditable<M: Auditable>(_: &M) {}
        fn assert_soft_deletable<M: SoftDeletable>(_: &M) {}
        assert_auditable(&a);
        assert_soft_deletable(&live);
    }
}
