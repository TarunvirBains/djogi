//! Authentication substrate for Djogi (Phase 5.5).
//!
//! This module is introduced in Phase 5.5 Task 1 with a minimal `AuthContext`
//! stub sufficient to attach auth state to a [`DjogiContext`]. Task 2 extends
//! this module with the `DjogiAuth` trait and `AuthError` enum; Task 4 adds
//! the `PasswordHash` typed field.
//!
//! See `docs/superpowers/plans/2026-04-19-phase5-5-auth-v3.md` for the full
//! Phase 5.5 design.

use heeranjid::HeerId;
use std::collections::HashMap;

/// Value-typed auth context attached to a [`crate::DjogiContext`] via
/// [`crate::DjogiContext::with_auth`].
///
/// The full shape (with `ext: HashMap<String, String>` and additional
/// builders) is documented in Critical Design Decision #2 of the Phase 5.5
/// plan. Task 1 ships the fields and builders the Task 1 integration test
/// exercises; Task 2 fills out the rest.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: HeerId,
    pub tenant_id: Option<String>,
    pub scopes: Vec<String>,
    pub ext: HashMap<String, String>,
}

impl AuthContext {
    /// Construct a minimal `AuthContext` with the given `user_id`.
    ///
    /// `tenant_id`, `scopes`, and `ext` default to `None`, empty, and empty
    /// respectively. Use the builder methods to attach additional state.
    pub fn new(user_id: HeerId) -> Self {
        Self {
            user_id,
            tenant_id: None,
            scopes: Vec::new(),
            ext: HashMap::new(),
        }
    }

    /// Set the tenant identifier for this auth context (consuming builder).
    pub fn with_tenant(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self
    }

    /// Set the scope list for this auth context (consuming builder).
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = String>) -> Self {
        self.scopes = scopes.into_iter().collect();
        self
    }

    /// Return `true` if `scope` is present in the scope list.
    ///
    /// Comparison is byte-exact (no case folding or wildcard expansion).
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

mod context_ext;
