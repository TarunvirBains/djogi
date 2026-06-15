//! `DjogiContext` extensions for auth integration.
//! Adds builder-style and mutating methods for attaching an [`AuthContext`] to a
//! context, plus the internal [`DjogiContext::ensure_tenant_set`] helper that
//! wires auto-tenant-scope onto the existing `set_tenant`.

use super::AuthContext;
use crate::{DjogiContext, DjogiError};

/// Emit the standard "auth guard bypassed via X" warning. Captured here so
/// the consuming-builder and mutating bypass methods both emit a consistent
/// message — log scrapers can grep for the specific method name on either
/// site, and the user's call site is recorded via the caller's
/// `#[track_caller]` attribute (which the helper preserves by being marked
/// `#[track_caller]` itself).
#[track_caller]
fn warn_auth_bypass(auth: &AuthContext, method: &'static str) {
    tracing::warn!(
     user_id = ?auth.user_id,
     caller = %std::panic::Location::caller(),
     "auth guard bypassed via {method}",
    );
}

impl DjogiContext {
    /// Attach an [`AuthContext`] to this context (consuming builder).
    /// When `auth.tenant_id.is_some()` AND the next CRUD/QuerySet operation
    /// targets a tenant-keyed model (per `ModelDescriptor::tenant_key`), the
    /// auto-`set_tenant` integration calls [`Self::ensure_tenant_set`]
    /// transparently.
    pub fn with_auth(mut self, auth: AuthContext) -> Self {
        self.set_auth(auth);
        self
    }

    /// Return the attached [`AuthContext`], if any.
    pub fn auth(&self) -> Option<&AuthContext> {
        self.auth.as_ref()
    }

    /// Attach an `AuthContext` while emitting a `tracing::warn!` at the
    /// call site (consuming builder). Bypass is searchable via the
    /// `_insecurely` suffix and the log message text.
    /// `_insecurely` variants are intended only for code with manually-
    /// established safety invariants (tests, migrations, admin tooling,
    /// service-account flows). Calling this inside a request handler is a
    /// design smell.
    #[track_caller]
    pub fn with_auth_insecurely(mut self, auth: AuthContext) -> Self {
        warn_auth_bypass(&auth, "with_auth_insecurely");
        self.auth = Some(auth);
        self
    }

    /// Attach an [`AuthContext`] by mutation (does not consume `self`).
    /// Mirror of [`Self::with_auth`] for use inside
    /// [`crate::transaction::atomic`] closures, which expose a
    /// `&mut DjogiContext` rather than an owned context.
    /// ```ignore
    /// djogi::transaction::atomic(&mut ctx, |ctx| Box::pin(async move {
    ///  ctx.set_auth(AuthContext::new(user_id).with_tenant("org_a"));
    ///  TenantPost::objects().fetch_all(ctx).await
    /// })).await
    /// ```
    pub fn set_auth(&mut self, auth: AuthContext) {
        self.auth = Some(auth);
    }

    /// Mutating sibling of [`Self::with_auth_insecurely`]. Emits the same
    /// `tracing::warn!` with caller location. `#[track_caller]` reports
    /// the user's call site, not this wrapper.
    #[track_caller]
    pub fn set_auth_insecurely(&mut self, auth: AuthContext) {
        warn_auth_bypass(&auth, "set_auth_insecurely");
        self.auth = Some(auth);
    }

    /// Explicitly opt out of the "cross-tenant context" warn emitted when
    /// `auth.tenant_id.is_none()` on a tenant-keyed model (consuming builder).
    /// ```ignore
    /// let ctx = DjogiContext::from_pool(pool).with_no_tenant_scope();
    /// ```
    /// For `atomic(&mut ctx, |ctx|...)` closures where the closure has
    /// `&mut DjogiContext`, use [`Self::set_no_tenant_scope`] instead.
    /// Intended for admin / batch / migration flows that legitimately want
    /// queries to span tenants without a `tenant_id` attached. A plain
    /// `.with_auth(AuthContext::new(uid))` on a tenant-keyed model without
    /// this opt-out will emit a `tracing::warn!` on every CRUD / terminal
    /// call — that warn is by design: bypass is always searchable.
    pub fn with_no_tenant_scope(mut self) -> Self {
        self.set_no_tenant_scope();
        self
    }

    /// Mutating form of [`Self::with_no_tenant_scope`] for use inside
    /// `atomic(&mut ctx, |ctx|...)` closures.
    pub fn set_no_tenant_scope(&mut self) {
        self.tenant_scope_suppressed = true;
    }

    /// Internal helper: ensure the `app.tenant_id` GUC matches `tenant_id`
    /// for the current context. No-op when the currently-applied tenant id
    /// already equals `tenant_id`; otherwise delegates to
    /// [`Self::set_tenant`] to re-issue `SET LOCAL`.
    /// Invoked by the auto-tenant integration before every CRUD dispatch on
    /// a tenant-keyed model when
    /// `ctx.auth().and_then(|a| a.tenant_id.as_ref())` is `Some`.
    /// **Why the per-tenant comparison, not a plain `tenant_set` bool:**
    /// `SET LOCAL app.tenant_id = 'org_a'` persists for the lifetime of the
    /// open transaction. If auth changes inside one `atomic()` scope from
    /// `org_a` to `org_b`, a bool short-circuit would leave queries running
    /// under `org_a` — a silent cross-tenant read. Comparing against
    /// `applied_tenant_id` forces a re-issue of `SET LOCAL` whenever the
    /// requested tid differs.
    pub(crate) async fn ensure_tenant_set(&mut self, tenant_id: &str) -> Result<(), DjogiError> {
        if self.applied_tenant_id.as_deref() == Some(tenant_id) {
            return Ok(());
        }
        self.set_tenant(tenant_id).await?;
        Ok(())
    }
}
