//! Authentication error type.
//! `AuthError` is `#[non_exhaustive]` — downstream matches must include a
//! wildcard arm so adding variants (e.g., rate-limit-exceeded, MFA-required)
//! in later phases is not a breaking change.
//! Database/driver failures surface as `DjogiError::Db(DbError)` from the
//! Postgres substrate, not through `AuthError`. Provider-internal failures
//! that aren't driver errors wrap as `AuthError::Provider(...)`.

use thiserror::Error;

/// Authentication and authorization failure modes.
/// Returned by [`DjogiAuth::authenticate`](super::DjogiAuth::authenticate) and
/// [`DjogiAuth::verify`](super::DjogiAuth::verify), and re-raised as
/// [`DjogiError::Auth`](crate::error::DjogiError::Auth) when those calls
/// propagate through `?` inside framework operations.
/// # Matching
/// The enum is `#[non_exhaustive]`. All downstream `match` arms must end with
/// a wildcard:
/// ```ignore
/// match err {
///  AuthError::InvalidToken => { /*... */ }
///  AuthError::ExpiredSession => { /*... */ }
///  _ => { /* forward-compatible catch-all */ }
/// }
/// ```
/// # Error hierarchy
/// `AuthError` is narrowly scoped to authentication and authorization logic.
/// It does NOT carry database/driver errors (those flow through
/// `DjogiError::Db`). Use `AuthError::Provider` to wrap any provider-internal
/// failure that is not covered by a more specific variant.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The token did not parse or did not resolve to a known session or user.
    /// Returned when the opaque bearer token passed to
    /// [`DjogiAuth::authenticate`](super::DjogiAuth::authenticate) is
    /// malformed, has an invalid signature, or does not correspond to any
    /// session the provider can resolve.
    #[error("invalid token")]
    InvalidToken,

    /// A previously-valid session has expired.
    /// Distinct from `InvalidToken` so callers can give users a more
    /// specific prompt ("please log in again") rather than treating
    /// expiry as a completely unknown error.
    #[error("expired session")]
    ExpiredSession,

    /// An auth-required operation was attempted without an attached
    /// `AuthContext` on the `DjogiContext`.
    /// Raised by framework guard helpers when a route or model method
    /// that requires authentication is invoked against a context that
    /// has no auth attached via
    /// [`DjogiContext::with_auth`](crate::DjogiContext::with_auth).
    #[error("missing auth context")]
    MissingAuth,

    /// `DjogiAuth::verify` returned a denial. `reason` is an
    /// implementation-supplied explanation suitable for logging — it is NOT
    /// necessarily safe to forward to end users.
    /// # When to use
    /// Return this variant when the resolved `AuthContext` is valid (the
    /// user is authenticated) but the specific action they attempted is
    /// not permitted (the user is not authorized). This keeps the
    /// "who are you" and "can you do this" failure paths distinguishable
    /// by callers.
    #[error("authorization denied: {reason}")]
    Denied {
        /// Human-readable explanation of why the action was denied.
        /// Suitable for structured logging. Not guaranteed safe for
        /// end-user display — scrub before including in HTTP responses.
        reason: String,
    },

    /// Provider-internal error (JWT parse failure, HTTP fetch to an OIDC
    /// issuer, key-store lookup, etc.). Wraps any `Error + Send + Sync +
    /// 'static` so every provider can surface its own error hierarchy without
    /// this enum growing per-provider variants.
    /// # When to use
    /// Use this variant for failures that originate inside the provider
    /// implementation and do not map to a more specific `AuthError` variant.
    /// Database/driver errors that bubble up from a Postgres lookup should be
    /// converted to `DjogiError::Db` at the boundary instead — they do not
    /// belong here.
    #[error("provider error: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}
