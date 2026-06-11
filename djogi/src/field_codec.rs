//! Field-level codecs for at-rest transformations of protected fields.
//! Trait + compile-time registry surface.
//! # Boundary contract (§1 D3 + §1 D4)
//! Codec identifiers are `&'static str` consts declared on the trait
//! impl, and the registry is a [`phf::Set`] populated at framework
//! compile time. Runtime registration is *not* exposed: a codec that
//! could be added at adopter-runtime would also be unavailable when the
//! descriptor differ runs at macro-expansion time, and a missing codec
//! at differ-time means a protected field could ship referencing an
//! identifier that simply does not exist. That outcome is silent
//! at-rest data corruption, so the framework treats codecs as code,
//! not data — the only way to add a codec is to ship a new release of
//! Djogi with that codec wired into the static registry below. The
//! macro (`#[field(protected(codec = "<id>"))]`) calls
//! [`is_registered`] during expansion; an unknown identifier is a
//! compile error at the call site, never a runtime surprise.
//! # The trait — `classify_transition` rationale (§1 D4)
//! [`FieldCodec::classify_transition`] is an associated function (not
//! a method on `&self`) because the question it answers — "if a field
//! moves from codec `Self` to codec `Other`, what migration safety
//! class applies?" — is a property of the *types*, not of any
//! particular value. Returning [`OnlineSafetyClassification::OnlineSafe`]
//! for the identity transition (a codec to itself) is the conventional
//! no-op signal; returning [`OnlineSafetyClassification::OfflineOnly`]
//! refuses the change; returning
//! [`OnlineSafetyClassification::ExpandContract`] hands the migration
//! over to 's live-plan layer per the boundary contract
//! frozen in [`crate::migrate::OnlineSafetyClassification`].
//! # AES-256-GCM codec (feature `aes-codec`)
//! When the `aes-codec` feature is enabled, the [`aes`] submodule provides
//! [`aes::Aes256GcmV1`], an AEAD codec that encrypts `String` fields into
//! `Vec<u8>` ciphertext using AES-256-GCM. The codec ID
//! `"aes256_gcm_v1"` is registered in the runtime [`REGISTRY`] and the
//! macro-side [`crate::__private::protected::KNOWN_CODEC_IDS`] slice.

use crate::migrate::OnlineSafetyClassification;

// ── Error types (feature-gated with aes-codec) ─────────────────────────────
// `CodecError` references `aes_gcm::Error`, which only exists when the
// optional `aes-gcm` dependency is compiled. Both error types are gated
// so they compile cleanly without the feature and are available when codecs
// are wired in.

#[cfg(feature = "aes-codec")]
mod error_types {
    /// Errors produced by field codec operations.
    /// Error messages are hygiene-safe: they carry structural information
    /// (lengths, ring indices, codec IDs) but never plaintext, key material,
    /// nonce values, or ciphertext bytes.
    #[derive(Debug)]
    pub enum CodecError {
        /// Missing or malformed key material at startup for a specific ring
        /// index. Carries the offending index so the message — and the
        /// [`CodecStartupError`] it is rendered into — names the exact
        /// `DJOGI_FIELD_CODEC_KEY_{index}` variable the operator must fix.
        /// `kind` distinguishes a ring gap (a lower index is absent while a
        /// higher one is present) from a malformed entry (non-hex / wrong
        /// length).
        MissingKey { index: u8, kind: MissingKeyKind },
        /// No `DJOGI_FIELD_CODEC_KEY_*` variable is set at all, so the ring
        /// has zero entries. (If `_0` is absent but a higher index *is* set,
        /// that surfaces as `MissingKey { index: 0, kind: Gap }` instead,
        /// naming `_0` explicitly.) Display names `DJOGI_FIELD_CODEC_KEY_0`
        /// as the always-required base entry.
        RingEmpty,
        /// Stored blob is shorter than the 30-byte minimum
        /// (1 version + 1 key_index + 12 nonce + 16 tag).
        CiphertextTooShort,
        /// Ciphertext version byte is not recognized by this codec version.
        UnknownVersion(u8),
        /// Key index recorded in a ciphertext is out of range for the current
        /// ring. Carries both the offending `index` byte and the `ring_len`
        /// captured at decode time so the message distinguishes a key that was
        /// never present from one that has been retired.
        UnknownKeyIndex { index: u8, ring_len: u8 },
        /// AES-GCM authentication or decryption failure (tag mismatch, wrong
        /// key, tampered data).
        AeadError(aes_gcm::Error),
        /// OS CSPRNG failed while generating the per-encryption nonce.
        /// Effectively unreachable in practice (a failing `getrandom` means the
        /// host entropy source is unavailable), but surfaced as a typed error
        /// rather than a panic. Holds [`getrandom::Error`] so the bare `?`
        /// operator works in `generate_nonce` via the `From` impl below.
        RngFailure(getrandom::Error),
        /// Decrypted bytes are not valid UTF-8.
        Utf8Error(std::string::FromUtf8Error),
    }

    /// Why a specific ring index failed to load. Carried by
    /// [`CodecError::MissingKey`] so startup errors name the exact variable
    /// and failure reason.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum MissingKeyKind {
        /// A lower index is absent while a higher index is present (ring gap).
        Gap,
        /// The variable is set but is not 64 lowercase hex characters, or does
        /// not decode to exactly 32 bytes.
        Malformed,
    }

    impl std::fmt::Display for CodecError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                CodecError::MissingKey { index, kind } => match kind {
                    MissingKeyKind::Gap => write!(
                        f,
                        "ring gap: DJOGI_FIELD_CODEC_KEY_{index} is not set but a higher index is present"
                    ),
                    MissingKeyKind::Malformed => write!(
                        f,
                        "malformed key: DJOGI_FIELD_CODEC_KEY_{index} must be 64 lowercase hex characters"
                    ),
                },
                CodecError::RingEmpty => f.write_str(
                    "no field codec key configured: set DJOGI_FIELD_CODEC_KEY_0 (64 lowercase hex characters)",
                ),
                CodecError::CiphertextTooShort => {
                    f.write_str("ciphertext too short: minimum valid ciphertext is 30 bytes")
                }
                CodecError::UnknownVersion(v) => write!(f, "unknown ciphertext version byte: {v}"),
                CodecError::UnknownKeyIndex { index, ring_len } => {
                    write!(f, "key index {index} not in ring of length {ring_len}")
                }
                CodecError::AeadError(_) => f.write_str("generic AEAD error"),
                CodecError::RngFailure(_) => {
                    f.write_str("OS CSPRNG failed while generating nonce")
                }
                CodecError::Utf8Error(e) => write!(f, "decoded bytes are not valid UTF-8: {e}"),
            }
        }
    }

    impl std::error::Error for CodecError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                // aes_gcm::aead::Error is an opaque single-byte type that does
                // not implement std::error::Error, so we cannot chain it as a
                // source.
                CodecError::AeadError(_) => None,
                // getrandom::Error implements std::error::Error, so chain it.
                CodecError::RngFailure(e) => Some(e),
                CodecError::Utf8Error(e) => Some(e),
                CodecError::MissingKey { .. }
                | CodecError::RingEmpty
                | CodecError::CiphertextTooShort
                | CodecError::UnknownVersion(_)
                | CodecError::UnknownKeyIndex { .. } => None,
            }
        }
    }

    // `From` impls so the bare `?` operator threads the source error through
    // `encode` / `decode` / `generate_nonce` without a manual `.map_err`.
    // `MissingKey` deliberately gets no `From` impl — it is always constructed
    // explicitly inside `load_ring` so the offending index is captured.
    impl From<aes_gcm::Error> for CodecError {
        fn from(e: aes_gcm::Error) -> Self {
            CodecError::AeadError(e)
        }
    }
    impl From<getrandom::Error> for CodecError {
        fn from(e: getrandom::Error) -> Self {
            CodecError::RngFailure(e)
        }
    }
    impl From<std::string::FromUtf8Error> for CodecError {
        fn from(e: std::string::FromUtf8Error) -> Self {
            CodecError::Utf8Error(e)
        }
    }

    /// Individual codec startup validation error.
    /// Aggregated into [`crate::DjogiError::FieldCodecStartup`] so the operator
    /// can see every misconfigured codec in one pass.
    #[derive(Debug)]
    pub struct CodecStartupError {
        /// The codec ID (e.g., `"aes256_gcm_v1"`).
        pub codec_id: &'static str,
        /// The required environment variable name.
        pub env_var: &'static str,
        /// Human-readable description of the failure.
        pub error: String,
    }

    impl std::fmt::Display for CodecStartupError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{} requires {}: {}",
                self.codec_id, self.env_var, self.error
            )
        }
    }

    impl std::error::Error for CodecStartupError {}
}

#[cfg(feature = "aes-codec")]
pub use error_types::{CodecError, CodecStartupError, MissingKeyKind};

// ── AES-256-GCM submodule (feature-gated) ──────────────────────────────────

#[cfg(feature = "aes-codec")]
pub(crate) mod aes;

/// Trait implemented by every field-level codec.
/// Implementors are zero-sized marker types (a codec is *code*, not a
/// runtime instance) and supply:
/// - [`Self::ID`] — the compile-time string constant the descriptor
///   stores in [`crate::descriptor::ProtectedFieldMetadata::codec`].
///   Identifiers are short ASCII labels following the SQL-identifier
///   convention used elsewhere in the framework: an ASCII letter or
///   underscore followed by ASCII alphanumerics or underscores, up to
///   63 bytes. Validation lives in the macro layer. The only requirement
///   here is that the ID be a `&'static str`.
/// - [`Self::Decoded`] — the in-memory Rust type the application code
///   sees (e.g. `String` for a plaintext column representation).
/// - [`Self::Encoded`] — the at-rest shape stored in Postgres (e.g.
///   `Vec<u8>` for an AEAD-protected column).
/// - [`Self::Error`] — the codec's error type, returned from both
///   [`encode`](Self::encode) and [`decode`](Self::decode).
///   `encode` and `decode` round-trip a single value across the
///   in-memory ↔ at-rest boundary. `classify_transition` answers
///   migration questions: see the module-level docs for the rationale.
pub trait FieldCodec: Send + Sync + 'static {
    /// Compile-time identifier referenced by
    /// `#[field(protected(codec = "<id>"))]`. Must be unique across
    /// every registered codec; the macro rejects duplicates and
    /// rejects identifiers that are not present in [`REGISTRY`].
    const ID: &'static str;

    /// In-memory representation seen by application code.
    type Decoded;

    /// At-rest representation stored in Postgres.
    type Encoded;

    /// Error returned from both [`encode`](Self::encode) and
    /// [`decode`](Self::decode). Required to be `Send + Sync + 'static`
    /// so it can be wrapped into [`crate::DjogiError`] without any
    /// lifetime gymnastics.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Convert a decoded value into its at-rest form.
    /// The `model` and `field` parameters carry the owning model name and field
    /// identifier so AEAD codecs can bind them as AAD (additional authenticated
    /// data), ensuring ciphertext cannot be replayed across different fields or
    /// models even with the same key.
    fn encode(
        model: &'static str,
        field: &'static str,
        value: &Self::Decoded,
    ) -> Result<Self::Encoded, Self::Error>;

    /// Convert an at-rest value back into its decoded form.
    /// The `model` and `field` parameters supply the AAD binding for AEAD codecs;
    /// non-AEAD implementations may ignore them.
    fn decode(
        model: &'static str,
        field: &'static str,
        stored: &Self::Encoded,
    ) -> Result<Self::Decoded, Self::Error>;

    /// Classify the migration from `Self` to `Other`.
    /// - [`OnlineSafetyClassification::OnlineSafe`] is the convention
    ///   for the identity transition (`Self == Other`).
    /// - [`OnlineSafetyClassification::ExpandContract`] hands the
    ///   migration over to 's live-plan layer.
    /// - [`OnlineSafetyClassification::OfflineOnly`] refuses the
    ///   migration outright; the operator must acknowledge downtime
    ///   or rewrite the change by hand.
    /// - [`OnlineSafetyClassification::FastLockDestructiveGuarded`] is
    ///   gated by `--allow-destructive` in the runner.
    ///   Reads as an associated function because the answer is a
    ///   property of the two codec types, not of any value.
    fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification;
}

/// Startup requirement for a field codec: codec ID, required environment
/// variable, and validation function pointer. Submitted via
/// `inventory::submit!` by macro-generated code; collected and iterated
/// at startup by the djogi crate through
/// [`validate_codec_startup_inventory`].
/// Follows the existing [`crate::presentation::inventory::PresentationCodecUsage`]
/// pattern for linked-inventory-based startup validation.
#[cfg(feature = "aes-codec")]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct FieldCodecStartupRequirement {
    /// The codec ID (e.g., `"aes256_gcm_v1"`).
    pub codec_id: &'static str,
    /// The environment variable name required for this codec (the always-
    /// required base entry; the validator scans the full indexed family).
    pub env_var: &'static str,
    /// Validation function that populates the `OnceLock<Vec<[u8; 32]>>` key-ring
    /// cache on success and returns `Ok(())`. MUST be `load_ring` (or
    /// equivalent codec init), not a separate validate function — startup
    /// success then implies the ring is validated AND cached, which closes the
    /// decode-time side-channel.
    pub validate: fn() -> Result<(), CodecError>,
}

#[cfg(feature = "aes-codec")]
impl FieldCodecStartupRequirement {
    /// Construct a `FieldCodecStartupRequirement` from macro-emitted code.
    /// Required because the struct is `#[non_exhaustive]`, which blocks
    /// struct-literal construction from external crates (including adopter
    /// crate macro expansions). Matches the [`crate::presentation::inventory::PresentationCodecUsage::const_new`] pattern.
    #[doc(hidden)]
    pub const fn const_new(
        codec_id: &'static str,
        env_var: &'static str,
        validate: fn() -> Result<(), CodecError>,
    ) -> Self {
        Self {
            codec_id,
            env_var,
            validate,
        }
    }
}

#[cfg(feature = "aes-codec")]
inventory::collect!(FieldCodecStartupRequirement);

/// Iterate all registered field codec startup requirements and call each
/// validator. On success returns `Ok(())`; on failure collects all errors
/// into a `Vec<CodecStartupError>` so the operator can fix every misconfigured
/// codec in one pass — matching the presentation startup pattern from
/// [`crate::presentation::validate_startup_inventory`].
///
/// The `validate` pointer IS the codec's `load_ring` function (not a separate
/// validator), so startup success implies the `OnceLock<Vec<[u8; 32]>>` cache
/// is populated. After successful validation, every subsequent `load_ring()`
/// returns `Ok`, making decode-time `RingEmpty` / `MissingKey` impossible — the
/// side-channel prevention property holds when startup validation runs before
/// CRUD calls.
#[cfg(feature = "aes-codec")]
pub fn validate_codec_startup_inventory() -> Result<(), Vec<CodecStartupError>> {
    let mut errors: Vec<CodecStartupError> = Vec::new();
    for req in ::inventory::iter::<FieldCodecStartupRequirement> {
        if let Err(e) = (req.validate)() {
            errors.push(CodecStartupError {
                codec_id: req.codec_id,
                env_var: req.env_var,
                error: e.to_string(),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Registry of every codec identifier known to the framework at
/// compile time.
/// Built with [`phf::phf_set!`] so [`is_registered`] resolves to a
/// constant-time perfect-hash lookup. The set is `pub(crate)` because adopters
/// query it through [`is_registered`] rather than reaching for the
/// underlying container; that indirection lets future phases swap the
/// representation without breaking downstream code.
/// # Synchronization contract with `djogi-macros`
/// Adding a codec ID requires updating BOTH this `phf::Set` and the
/// sibling `KNOWN_CODEC_IDS` sorted const slice in
/// `djogi-macros::model::protected`. The macro validates
/// `#[field(protected(codec = "<id>"))]` at proc-macro expansion time
/// ; the runtime registry serves runtime queries.
/// The duplicate is intentional: proc macros run before any runtime
/// dependency is available, so the macro crate cannot read this
/// `phf::Set` directly. A `const fn is_registered` would push the
/// error from "macro expansion" to "downstream compile" — the source
/// span would point at the macro emission site rather than the user's
/// `codec = "X"` literal, and the error message would degrade to a
/// generic "assertion failed" instead of listing valid IDs. Future
/// framework-internal codec additions are rare; the duplicate is the
/// explicit cost of compile-time validation with span-precise errors.
#[cfg(feature = "aes-codec")]
pub(crate) static REGISTRY: phf::Set<&'static str> = phf::phf_set! {
    "aes256_gcm_v1",
};

#[cfg(not(feature = "aes-codec"))]
pub(crate) static REGISTRY: phf::Set<&'static str> = phf::phf_set! {};

/// Returns `true` iff `id` is the compile-time identifier of a codec
/// shipped with this build of Djogi.
/// The macro consumes this during expansion of
/// `#[field(protected(codec = "<id>"))]` — an unknown identifier is a
/// compile error, never a runtime failure. Adopter code rarely needs
/// to reach the registry directly because the macro layer already
/// guards every declaration site.
/// `#[doc(hidden)]` — primarily a macro-layer validator at expansion
/// time; the macro crate keeps its own mirror of the registry, so this
/// runtime fn has no current adopter consumer.
#[doc(hidden)]
pub fn is_registered(id: &str) -> bool {
    REGISTRY.contains(id)
}

#[cfg(test)]
mod tests {
    use super::{FieldCodec, is_registered};
    use crate::migrate::OnlineSafetyClassification;
    use std::fmt;

    /// Test-only error type. Lives in the test module so it cannot
    /// leak into the public surface of the framework.
    #[derive(Debug)]
    struct Utf8RoundtripError;

    impl fmt::Display for Utf8RoundtripError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("utf-8 round-trip failed")
        }
    }

    impl std::error::Error for Utf8RoundtripError {}

    /// Trivial test codec: `String` ↔ `Vec<u8>` via UTF-8. The codec
    /// is *not* added to the production [`super::REGISTRY`] — it
    /// exists solely to exercise the trait surface from inside this
    /// test module.
    struct Utf8Roundtrip;

    impl FieldCodec for Utf8Roundtrip {
        const ID: &'static str = "_djogi_test_utf8_roundtrip";
        type Decoded = String;
        type Encoded = Vec<u8>;
        type Error = Utf8RoundtripError;

        fn encode(
            _model: &'static str,
            _field: &'static str,
            value: &Self::Decoded,
        ) -> Result<Self::Encoded, Self::Error> {
            Ok(value.as_bytes().to_vec())
        }

        fn decode(
            _model: &'static str,
            _field: &'static str,
            stored: &Self::Encoded,
        ) -> Result<Self::Decoded, Self::Error> {
            std::str::from_utf8(stored)
                .map(|s| s.to_owned())
                .map_err(|_| Utf8RoundtripError)
        }

        fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification {
            // Identity-transition convention: codec → same codec is a
            // no-op the runner can apply directly. Distinct codecs are
            // refused; a real codec pair would return `ExpandContract`
            // when it can plan a live migration, but the test codec
            // makes the conservative choice so the surface is
            // observable from a test.
            if Self::ID == Other::ID {
                OnlineSafetyClassification::OnlineSafe
            } else {
                OnlineSafetyClassification::OfflineOnly
            }
        }
    }

    /// Distinct test codec used to exercise the cross-codec branch of
    /// `classify_transition`.
    struct OtherTestCodec;

    impl FieldCodec for OtherTestCodec {
        const ID: &'static str = "_djogi_test_other";
        type Decoded = String;
        type Encoded = Vec<u8>;
        type Error = Utf8RoundtripError;

        fn encode(
            _model: &'static str,
            _field: &'static str,
            value: &Self::Decoded,
        ) -> Result<Self::Encoded, Self::Error> {
            Ok(value.as_bytes().to_vec())
        }

        fn decode(
            _model: &'static str,
            _field: &'static str,
            stored: &Self::Encoded,
        ) -> Result<Self::Decoded, Self::Error> {
            std::str::from_utf8(stored)
                .map(|s| s.to_owned())
                .map_err(|_| Utf8RoundtripError)
        }

        fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification {
            if Self::ID == Other::ID {
                OnlineSafetyClassification::OnlineSafe
            } else {
                OnlineSafetyClassification::ExpandContract
            }
        }
    }

    #[test]
    fn encode_decode_round_trips() {
        let original = String::from("djogi field codec round-trip");
        let encoded = Utf8Roundtrip::encode("TestModel", "name", &original).expect("encode");
        let decoded = Utf8Roundtrip::decode("TestModel", "name", &encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[cfg(not(feature = "aes-codec"))]
    #[test]
    fn registry_empty_without_aes_codec_feature() {
        // Without the aes-codec feature, the registry should be empty.
        assert!(!is_registered("aes256_gcm_v1"));
    }

    #[cfg(feature = "aes-codec")]
    #[test]
    fn registry_contains_aes_codec_when_feature_enabled() {
        // With the aes-codec feature, aes256_gcm_v1 should be registered.
        assert!(is_registered("aes256_gcm_v1"));
    }

    #[test]
    fn registry_does_not_contain_test_codec_id() {
        // The test codec impls live only in this module; they must
        // never be visible through the framework's compile-time
        // registry.
        assert!(!is_registered(Utf8Roundtrip::ID));
        assert!(!is_registered(OtherTestCodec::ID));
    }

    #[test]
    fn classify_transition_to_self_is_online_safe() {
        // Same codec on both sides — by convention, no-op transition.
        let classification = Utf8Roundtrip::classify_transition::<Utf8Roundtrip>();
        assert_eq!(classification, OnlineSafetyClassification::OnlineSafe);
    }

    #[test]
    fn classify_transition_across_codecs_is_callable() {
        // Cross-codec branch — the precise classification depends on
        // the codec pair, but the surface must dispatch and return
        // *some* variant. The test codec returns `OfflineOnly` here
        // and `OtherTestCodec` returns `ExpandContract`; both confirm
        // the trait method is reachable through normal generics.
        let to_other = Utf8Roundtrip::classify_transition::<OtherTestCodec>();
        assert_eq!(to_other, OnlineSafetyClassification::OfflineOnly);

        let from_other = OtherTestCodec::classify_transition::<Utf8Roundtrip>();
        assert_eq!(from_other, OnlineSafetyClassification::ExpandContract);
    }
}
