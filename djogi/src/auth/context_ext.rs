//! `DjogiContext` extensions for Phase 5.5 auth integration.
//!
//! Adds builder-style methods for attaching an [`AuthContext`] to a context
//! and the internal [`DjogiContext::ensure_tenant_set`] helper that wires
//! auto-tenant-scope onto Phase 5's existing `set_tenant`.
//!
//! Phase 4's `djogi/src/context.rs` carries the struct definition; this
//! file owns every method Phase 5.5 adds to it (Task 1 + Task 11).

use super::AuthContext;
use crate::{DjogiContext, DjogiError};

impl DjogiContext {
    /// Attach an [`AuthContext`] to this context.
    ///
    /// Builder-style (consuming). When `auth.tenant_id.is_some()` AND the
    /// next CRUD/QuerySet operation targets a tenant-keyed model (per
    /// `ModelDescriptor::tenant_key`), the auto-`set_tenant` integration
    /// (Phase 5.5 Task 10) calls [`Self::ensure_tenant_set`] transparently.
    pub fn with_auth(mut self, auth: AuthContext) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Return the attached [`AuthContext`], if any.
    pub fn auth(&self) -> Option<&AuthContext> {
        self.auth.as_ref()
    }

    /// Attach an `AuthContext` while emitting a `tracing::warn!` at the
    /// call site. Bypass is searchable via the `_insecurely` suffix and the
    /// log message text.
    ///
    /// `_insecurely` variants are intended only for code with manually-
    /// established safety invariants (tests, migrations, admin tooling,
    /// service-account flows). Calling this inside a request handler is a
    /// design smell — see Phase 5.5 plan amendment Q11.
    #[track_caller]
    pub fn with_auth_insecurely(mut self, auth: AuthContext) -> Self {
        tracing::warn!(
            user_id = ?auth.user_id,
            caller = %std::panic::Location::caller(),
            "auth guard bypassed via with_auth_insecurely",
        );
        self.auth = Some(auth);
        self
    }

    /// Attach an [`AuthContext`] by mutation (does not consume `self`).
    ///
    /// Mirror of [`Self::with_auth`] for use inside
    /// [`crate::transaction::atomic`] closures, which expose a
    /// `&mut DjogiContext` rather than an owned context.
    ///
    /// ```ignore
    /// djogi::transaction::atomic(&pool, |ctx| Box::pin(async move {
    ///     ctx.set_auth(AuthContext::new(user_id).with_tenant("org_a"));
    ///     TenantPost::objects().fetch_all(ctx).await
    /// })).await
    /// ```
    pub fn set_auth(&mut self, auth: AuthContext) {
        self.auth = Some(auth);
    }

    /// Clear any attached [`AuthContext`] on this context. No-op if none
    /// was set.
    pub fn clear_auth(&mut self) {
        self.auth = None;
    }

    /// Mutating sibling of [`Self::with_auth_insecurely`]. Emits the same
    /// `tracing::warn!` with caller location. `#[track_caller]` reports
    /// the user's call site, not this wrapper.
    #[track_caller]
    pub fn set_auth_insecurely(&mut self, auth: AuthContext) {
        tracing::warn!(
            user_id = ?auth.user_id,
            caller = %std::panic::Location::caller(),
            "auth guard bypassed via set_auth_insecurely",
        );
        self.auth = Some(auth);
    }

    /// Internal helper: ensure the `app.tenant_id` GUC is set for the
    /// current context. No-op if [`Self::tenant_set`] is already true;
    /// otherwise delegates to the existing [`Self::set_tenant`] (Phase 5
    /// Task 9).
    ///
    /// Invoked by the auto-tenant integration (Phase 5.5 Task 10) before
    /// every CRUD dispatch on a tenant-keyed model when
    /// `ctx.auth().and_then(|a| a.tenant_id.as_ref())` is `Some`.
    pub(crate) async fn ensure_tenant_set(&mut self, tenant_id: &str) -> Result<(), DjogiError> {
        if self.tenant_set {
            return Ok(());
        }
        self.set_tenant(tenant_id).await?;
        Ok(())
    }
}
