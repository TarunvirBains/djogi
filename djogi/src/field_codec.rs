//! Field-level codecs for at-rest transformations of protected fields.
//!
//! Phase 7.5 T4 — trait + compile-time registry surface.
//!
//! # Boundary contract (§1 D3 + §1 D4)
//!
//! Codec identifiers are `&'static str` consts declared on the trait
//! impl, and the registry is a [`phf::Set`] populated at framework
//! compile time. Runtime registration is *not* exposed: a codec that
//! could be added at adopter-runtime would also be unavailable when the
//! descriptor differ runs at macro-expansion time, and a missing codec
//! at differ-time means a protected field could ship referencing an
//! identifier that simply does not exist. That outcome is silent
//! at-rest data corruption, so the framework treats codecs as code,
//! not data — the only way to add a codec is to ship a new release of
//! Djogi with that codec wired into the static registry below. T3's
//! macro (`#[field(protected(codec = "<id>"))]`) calls
//! [`is_registered`] during expansion; an unknown identifier is a
//! compile error at the call site, never a runtime surprise.
//!
//! # The trait — `classify_transition` rationale (§1 D4)
//!
//! [`FieldCodec::classify_transition`] is an associated function (not
//! a method on `&self`) because the question it answers — "if a field
//! moves from codec `Self` to codec `Other`, what migration safety
//! class applies?" — is a property of the *types*, not of any
//! particular value. Returning [`OnlineSafetyClassification::OnlineSafe`]
//! for the identity transition (a codec to itself) is the conventional
//! no-op signal; returning [`OnlineSafetyClassification::OfflineOnly`]
//! refuses the change; returning
//! [`OnlineSafetyClassification::ExpandContract`] hands the migration
//! over to Phase 7.5's live-plan layer per the boundary contract
//! frozen in [`crate::migrate::OnlineSafetyClassification`].
//!
//! # V1 registry contents
//!
//! V1 ships an empty registry. Real codecs (the AEAD / blind-index
//! pair the spec mentions, etc.) land in later Phase 7.5 tasks. T4's
//! job is the trait shape and the registry surface — nothing more.

use crate::migrate::OnlineSafetyClassification;

/// Trait implemented by every field-level codec.
///
/// Implementors are zero-sized marker types (a codec is *code*, not a
/// runtime instance) and supply:
///
/// - [`Self::ID`] — the compile-time string constant the descriptor
///   stores in [`crate::descriptor::ProtectedFieldMetadata::codec`].
///   Identifiers are short ASCII labels following the SQL-identifier
///   convention used elsewhere in the framework: an ASCII letter or
///   underscore followed by ASCII alphanumerics or underscores, up to
///   63 bytes. Validation lives in T3's macro layer; T4 only requires
///   that the ID be a `&'static str`.
/// - [`Self::Decoded`] — the in-memory Rust type the application code
///   sees (e.g. `String` for a plaintext column representation).
/// - [`Self::Encoded`] — the at-rest shape stored in Postgres (e.g.
///   `Vec<u8>` for an AEAD-protected column).
/// - [`Self::Error`] — the codec's error type, returned from both
///   [`encode`](Self::encode) and [`decode`](Self::decode).
///
/// `encode` and `decode` round-trip a single value across the
/// in-memory ↔ at-rest boundary. `classify_transition` answers
/// migration questions: see the module-level docs for the rationale.
pub trait FieldCodec: Send + Sync + 'static {
    /// Compile-time identifier referenced by
    /// `#[field(protected(codec = "<id>"))]`. Must be unique across
    /// every registered codec; T3's macro rejects duplicates and
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
    fn encode(value: &Self::Decoded) -> Result<Self::Encoded, Self::Error>;

    /// Convert an at-rest value back into its decoded form.
    fn decode(stored: &Self::Encoded) -> Result<Self::Decoded, Self::Error>;

    /// Classify the migration from `Self` to `Other`.
    ///
    /// - [`OnlineSafetyClassification::OnlineSafe`] is the convention
    ///   for the identity transition (`Self == Other`).
    /// - [`OnlineSafetyClassification::ExpandContract`] hands the
    ///   migration over to Phase 7.5's live-plan layer.
    /// - [`OnlineSafetyClassification::OfflineOnly`] refuses the
    ///   migration outright; the operator must acknowledge downtime
    ///   or rewrite the change by hand.
    /// - [`OnlineSafetyClassification::FastLockDestructiveGuarded`] is
    ///   gated by `--allow-destructive` in the Phase 7 runner.
    ///
    /// Reads as an associated function because the answer is a
    /// property of the two codec types, not of any value.
    fn classify_transition<Other: FieldCodec>() -> OnlineSafetyClassification;
}

/// Registry of every codec identifier known to the framework at
/// compile time.
///
/// Built with [`phf::phf_set!`] so [`is_registered`] resolves to a
/// constant-time perfect-hash lookup. V1 is empty — real codecs land
/// in later Phase 7.5 tasks. The set is `pub(crate)` because adopters
/// query it through [`is_registered`] rather than reaching for the
/// underlying container; that indirection lets future phases swap the
/// representation without breaking downstream code.
///
/// # Synchronization contract with `djogi-macros`
///
/// Adding a codec ID requires updating BOTH this `phf::Set` and the
/// sibling `KNOWN_CODEC_IDS` sorted const slice in
/// `djogi-macros::model::protected`. The macro validates
/// `#[field(protected(codec = "<id>"))]` at proc-macro expansion time
/// (Phase 7.5 T3); the runtime registry serves runtime queries.
///
/// The duplicate is intentional: proc macros run before any runtime
/// dependency is available, so the macro crate cannot read this
/// `phf::Set` directly. A `const fn is_registered` would push the
/// error from "macro expansion" to "downstream compile" — the source
/// span would point at the macro emission site rather than the user's
/// `codec = "X"` literal, and the error message would degrade to a
/// generic "assertion failed" instead of listing valid IDs. Future
/// framework-internal codec additions are rare; the duplicate is the
/// explicit cost of compile-time validation with span-precise errors.
pub(crate) static REGISTRY: phf::Set<&'static str> = phf::phf_set! {};

/// Returns `true` iff `id` is the compile-time identifier of a codec
/// shipped with this build of Djogi.
///
/// T3's macro consumes this during expansion of
/// `#[field(protected(codec = "<id>"))]` — an unknown identifier is a
/// compile error, never a runtime failure. Adopter code may also call
/// it (e.g. when round-tripping a snapshot through code that needs to
/// reject obsolete identifiers), but reaching for the registry is
/// rare in practice because the macro layer already guards every
/// declaration site.
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

        fn encode(value: &Self::Decoded) -> Result<Self::Encoded, Self::Error> {
            Ok(value.as_bytes().to_vec())
        }

        fn decode(stored: &Self::Encoded) -> Result<Self::Decoded, Self::Error> {
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

        fn encode(value: &Self::Decoded) -> Result<Self::Encoded, Self::Error> {
            Ok(value.as_bytes().to_vec())
        }

        fn decode(stored: &Self::Encoded) -> Result<Self::Decoded, Self::Error> {
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
        let encoded = Utf8Roundtrip::encode(&original).expect("encode");
        let decoded = Utf8Roundtrip::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn registry_does_not_contain_unshipped_codec_id() {
        // Real codecs (e.g. the AEAD pair the spec gestures at) land
        // in later Phase 7.5 tasks; V1 ships with no codecs.
        assert!(!is_registered("aes256_gcm_v1"));
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
