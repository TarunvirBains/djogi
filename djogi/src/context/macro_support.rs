//! Sealed macro-support surface for `DjogiContext`.
//!
//! `#[derive(Model)]` emits CRUD bodies that expand inside the *adopter's*
//! crate. Those bodies must call `DjogiContext`'s internal execution, tenant,
//! and auth helpers, but adopter code lives outside `djogi` and cannot reach
//! `pub(crate)` members. The helpers therefore live on this trait, which is
//! `pub` so macro-emitted code can call its methods, but whose supertrait
//! [`sealed::Sealed`] is unnameable and unimplementable from outside `djogi`.
//! No downstream type can implement `MacroSupportExt`, and the methods left
//! the inherent `DjogiContext` surface entirely. Macro-emitted code reaches
//! them with fully-qualified call syntax —
//! `<::djogi::DjogiContext as ::djogi::__private::MacroSupportExt>::__method(ctx, ...)`
//! — which names this `#[doc(hidden)]` trait at the call-site and is the only
//! way to reach the methods (no inherent `ctx.__method(...)` form survives).
//! This is the same seal shape and call form used by
//! `crate::__bypass::RawAccessExt` and the `VisageSealed` family.
//!
//! Carries no stability guarantee. Not part of the public API.

use tokio_postgres::Row;
use tokio_postgres::types::ToSql;

use crate::DjogiError;
use crate::context::{AuthStateSnapshot, DjogiContext};

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::context::DjogiContext {}
}

/// Macro-support helpers for `#[derive(Model)]`-emitted CRUD bodies.
///
/// Sealed via [`sealed::Sealed`]: no downstream crate can implement this
/// trait, and the methods are reachable only by naming this `#[doc(hidden)]`
/// trait out of `::djogi::__private` (the macro does so with fully-qualified
/// call syntax). Carries no stability guarantee.
#[doc(hidden)]
#[trait_variant::make(MacroSupportExt: Send)]
pub trait MacroSupportExtBase: sealed::Sealed {
    /// Execute a query and return all rows. For macro-emitted code only.
    async fn __query_all_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError>;

    /// Execute a query and return the first row, if any. For macro-emitted code only.
    async fn __query_opt_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError>;

    /// Execute a query and return exactly one row. For macro-emitted code only.
    async fn __query_one_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError>;

    /// Execute a DML statement. For macro-emitted code only.
    async fn __execute_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError>;

    /// Whether this context is backed by a transaction. For macro-emitted code only.
    fn __djogi_is_transaction_backed_for_macros(&self) -> bool;

    /// Read the tenant-scope suppression flag. For macro-emitted code only.
    fn __tenant_scope_suppressed_for_macros(&self) -> bool;

    /// Ensure `app.tenant_id` is set on this context. For macro-emitted code only.
    async fn __ensure_tenant_set_for_macros(
        &mut self,
        tenant_id: &str,
    ) -> Result<(), DjogiError>;

    /// Snapshot auth-related mutable state. For macro-emitted code only.
    fn __snapshot_auth_state_for_macros(&self) -> AuthStateSnapshot;

    /// Restore auth-related state captured by `__snapshot_auth_state_for_macros`.
    /// For macro-emitted code only.
    async fn __restore_auth_state_for_macros(
        &mut self,
        snapshot: AuthStateSnapshot,
    ) -> Result<(), DjogiError>;
}

impl MacroSupportExt for DjogiContext {
    async fn __query_all_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, DjogiError> {
        self.query_all(sql, params).await
    }

    async fn __query_opt_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Option<Row>, DjogiError> {
        self.query_opt(sql, params).await
    }

    async fn __query_one_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Row, DjogiError> {
        self.query_one(sql, params).await
    }

    async fn __execute_for_macros(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, DjogiError> {
        self.execute(sql, params).await
    }

    fn __djogi_is_transaction_backed_for_macros(&self) -> bool {
        self.is_transaction_backed()
    }

    fn __tenant_scope_suppressed_for_macros(&self) -> bool {
        // Field read, not a method call: the existing inherent shim reads
        // `self.tenant_scope_suppressed` directly. There is no accessor method.
        // `macro_support` is a child module of `context`, so it sees the
        // private field on `DjogiContext`.
        self.tenant_scope_suppressed
    }

    async fn __ensure_tenant_set_for_macros(
        &mut self,
        tenant_id: &str,
    ) -> Result<(), DjogiError> {
        self.ensure_tenant_set(tenant_id).await
    }

    fn __snapshot_auth_state_for_macros(&self) -> AuthStateSnapshot {
        self.snapshot_auth_state()
    }

    async fn __restore_auth_state_for_macros(
        &mut self,
        snapshot: AuthStateSnapshot,
    ) -> Result<(), DjogiError> {
        // Same body as the former inherent shim: re-apply the tenant GUC
        // to match the snapshot, then restore the in-memory trackers.
        match snapshot.applied_tenant_id.as_deref() {
            Some(tenant_id) => {
                self.set_tenant(tenant_id).await?;
            }
            None => {
                if self.applied_tenant_id.is_some() {
                    self.clear_tenant().await?;
                }
            }
        }
        self.restore_auth_state(snapshot);
        Ok(())
    }
}
