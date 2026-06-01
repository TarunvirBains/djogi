//! The `PasswordHash` typed field.
//! Stores a PHC-format password hash as `TEXT`. The PHC string is
//! self-describing — it contains the algorithm identifier, parameters
//! (memory cost, iterations, parallelism), salt, and hash separated by
//! `$`. This lets Djogi migrate between algorithms or param sets
//! without schema changes: the `verify` path dispatches on the
//! algorithm prefix inside the stored string.
//! `PasswordHash::hash(plaintext)` is only available when the
//! `auth-argon2` feature is enabled; it produces Argon2id hashes with
//! the `argon2` crate's default params. `PasswordHash::verify(plaintext)`
//! always returns `bool` (never a `Result`) — any parse failure or
//! mismatch returns `false`, preventing timing leaks through error
//! variants.
//! # Storage
//! Postgres column type: `TEXT`. Size is variable (Argon2id PHC strings
//! are typically 90–150 bytes) but always single-line and ASCII-safe.
//! # Substrate
//! Implements `postgres_types::ToSql` and `postgres_types::FromSql`
//! transparently via delegation to `String` — the same pattern
//! `Tracked<T>` uses (`djogi/src/tracked.rs`).

use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};

#[cfg(feature = "auth-argon2")]
use crate::auth::AuthError;

/// A stored password hash in PHC format.
/// Construct via [`PasswordHash::hash`] (requires `auth-argon2` feature),
/// verify via [`PasswordHash::verify`], or reconstruct from a stored
/// PHC string via [`PasswordHash::from_phc`] when reading from a row
/// manually (normal ORM reads go through `FromSql` automatically).
/// # `Default`
/// `PasswordHash::default` returns `PasswordHash("")` — an empty PHC
/// string that [`Self::verify`] always rejects. The empty value exists
/// so models that carry a `password_hash` field can derive `Default`
/// (required by the `#[model]` pattern's `..Default::default` struct-
/// literal ergonomics). The empty hash is **not a usable credential**:
/// a row whose `password_hash` stayed at the default cannot be logged
/// into by any input. Always set a real hash via [`Self::hash`] before
/// persisting a user row, and prefer explicit `save`/`create` shapes
/// over blanket `..Default::default` in production code paths.
#[derive(Debug, Clone, Default)]
pub struct PasswordHash(String);

impl crate::descriptor::DjogiSqlType for PasswordHash {
    const SQL_TYPE: &'static str = "TEXT";
}

impl PasswordHash {
    /// Hash a plaintext password using Argon2id with the `argon2` crate's
    /// default parameters.
    /// Returns `AuthError::Provider(..)` if hashing fails for any reason
    /// (all failure paths in the `argon2` crate are infrastructure errors
    /// OS randomness unavailable, allocation failure). On success the
    /// returned `PasswordHash` carries a PHC string beginning with
    /// `$argon2id$`.
    #[cfg(feature = "auth-argon2")]
    pub fn hash(plaintext: &str) -> Result<Self, AuthError> {
        use argon2::password_hash::SaltString;
        use argon2::password_hash::rand_core::OsRng;
        use argon2::{Argon2, PasswordHasher};
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let phc = argon2
            .hash_password(plaintext.as_bytes(), &salt)
            .map_err(|e| AuthError::Provider(Box::new(e)))?
            .to_string();
        Ok(Self(phc))
    }

    /// Verify `plaintext` against the stored PHC hash.
    /// Returns `true` if the hash algorithm parses and the plaintext
    /// matches; `false` otherwise (including parse failures and
    /// mismatches — so callers cannot distinguish "wrong password"
    /// from "malformed stored hash" through the return value). The
    /// `argon2` crate's `verify_password` is constant-time.
    /// Without the `auth-argon2` feature, always returns `false`
    /// Djogi does not bundle a non-Argon2 verifier today.
    pub fn verify(&self, plaintext: &str) -> bool {
        #[cfg(feature = "auth-argon2")]
        {
            use argon2::password_hash::PasswordHash as Phc;
            use argon2::{Argon2, PasswordVerifier};
            if let Ok(parsed) = Phc::new(&self.0) {
                return Argon2::default()
                    .verify_password(plaintext.as_bytes(), &parsed)
                    .is_ok();
            }
            false
        }
        #[cfg(not(feature = "auth-argon2"))]
        {
            let _ = plaintext;
            false
        }
    }

    /// Return the raw PHC string.
    pub fn as_phc(&self) -> &str {
        &self.0
    }

    /// Construct from an existing PHC string — for code paths that
    /// read `TEXT` columns manually rather than through `FromSql`.
    pub fn from_phc(phc: impl Into<String>) -> Self {
        Self(phc.into())
    }
}

impl ToSql for PasswordHash {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.0.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        <String as ToSql>::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl<'a> FromSql<'a> for PasswordHash {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        <String as FromSql>::from_sql(ty, raw).map(Self)
    }

    fn accepts(ty: &Type) -> bool {
        <String as FromSql>::accepts(ty)
    }
}

#[cfg(all(test, feature = "auth-argon2"))]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trips() {
        let h = PasswordHash::hash("s3cret").unwrap();
        assert!(h.verify("s3cret"));
        assert!(!h.verify("wrong"));
        assert!(h.as_phc().starts_with("$argon2id$"));
    }

    #[test]
    fn password_hash_verify_returns_false_on_malformed_phc() {
        let h = PasswordHash::from_phc("not-a-real-phc-string");
        assert!(!h.verify("anything"));
    }

    #[test]
    fn password_hash_is_clone_debug() {
        let h = PasswordHash::hash("s3cret").unwrap();
        let h2 = h.clone();
        assert_eq!(h.as_phc(), h2.as_phc());
        assert!(format!("{h:?}").contains("PasswordHash"));
    }
}
