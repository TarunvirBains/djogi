//! Authentication substrate for Djogi (Phase 5.5).
//!
//! This module is introduced in Phase 5.5 Task 1 with a minimal `AuthContext`
//! stub sufficient to attach auth state to a [`DjogiContext`]. Task 2 extends
//! this module with the `DjogiAuth` trait and `AuthError` enum; Task 4 adds
//! the `PasswordHash` typed field.
//!
//! # Module layout
//!
//! - [`AuthContext`] — value-typed auth state attached to a `DjogiContext`.
//! - [`DjogiAuth`] — core authentication trait; implement to plug in a
//!   custom provider.
//! - [`AuthError`] — authentication and authorization failure modes.

use heeranjid::HeerId;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

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

pub mod error;
pub use error::AuthError;

pub mod password;
pub use password::PasswordHash;

mod context_ext;

/// Core authentication trait for Djogi.
///
/// `authenticate` resolves an opaque bearer token into an [`AuthContext`].
/// `verify` authorizes a specific action against a resolved context — the
/// default impl is authenticate-only semantics; override for real
/// authorization.
///
/// # Pluggability
///
/// The trait is **not sealed**. Third-party providers are first-class: any
/// crate can `impl DjogiAuth` for its own type. There is no `__sealed`
/// module or blanket-impl gate. The `action: &dyn Any` on [`verify`] lets
/// apps pass typed `Action` enums without adding a generic parameter to the
/// trait — a generic would break object safety and prevent using
/// `Arc<dyn DjogiAuth>` as a runtime-swappable provider.
///
/// # Object safety
///
/// `DjogiAuth` is object-safe. The only non-trivial requirement is that both
/// methods return `Pin<Box<dyn Future<...> + Send>>` rather than `impl Future`
/// — the boxing is the cost of object safety.
///
/// # Example
///
/// ```ignore
/// struct MyProvider;
///
/// impl djogi::auth::DjogiAuth for MyProvider {
///     fn authenticate<'a>(
///         &'a self,
///         token: &'a str,
///     ) -> std::pin::Pin<Box<dyn std::future::Future<
///         Output = Result<djogi::auth::AuthContext, djogi::auth::AuthError>,
///     > + Send + 'a>> {
///         let _ = token;
///         Box::pin(async { Err(djogi::auth::AuthError::InvalidToken) })
///     }
/// }
///
/// // Object-safe: usable as a trait object.
/// let _provider: std::sync::Arc<dyn djogi::auth::DjogiAuth> =
///     std::sync::Arc::new(MyProvider);
/// ```
///
/// [`verify`]: DjogiAuth::verify
pub trait DjogiAuth: Send + Sync + 'static {
    /// Resolve a bearer token (opaque to the trait) into an [`AuthContext`].
    ///
    /// The token format is entirely up to the provider — it may be a JWT, an
    /// opaque session token, an API key, or any other credential type. The
    /// framework passes it through without inspection.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::InvalidToken`] when the token cannot be resolved
    /// to a known session. Returns [`AuthError::ExpiredSession`] for a valid
    /// token that has passed its expiry. Returns [`AuthError::Provider`] for
    /// provider-internal failures (network errors, key-fetch failures, etc.).
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<AuthContext, AuthError>> + Send + 'a>>;

    /// Authorize a specific action against a resolved [`AuthContext`].
    ///
    /// The default implementation returns `Ok(())` — authenticate-only
    /// semantics. Override this method to add real authorization logic.
    ///
    /// `action: &dyn Any` accepts typed `Action` enums from the app. The
    /// implementation downcasts via `action.downcast_ref::<MyAction>()` to
    /// recover the concrete type. Using `Any` here avoids a generic type
    /// parameter on the trait, which would break object safety.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::Denied`] when the action is not permitted.
    /// Returns [`AuthError::MissingAuth`] when the context carries
    /// insufficient information to evaluate the action.
    fn verify<'a>(
        &'a self,
        ctx: &'a AuthContext,
        action: &'a dyn std::any::Any,
    ) -> Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + 'a>> {
        let _ = (ctx, action);
        Box::pin(async { Ok(()) })
    }
}
