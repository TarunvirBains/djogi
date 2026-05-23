//! Per-scope presentation codecs for protected fields.
//!
//! This module provides the runtime trait surface for the
//! `#[field(protected(per_scope = { ... }))]` presentation-codec feature
//! (GH #227). A presentation codec transforms a model's stored field value
//! at visage-projection time — after at-rest decode and before the visage
//! is serialized or returned to the caller.
//!
//! # Core concept
//!
//! For every protected scalar field with a `per_scope` block, the `#[model]`
//! macro generates per-scope `From<&Model>` / `TryFrom<&Model>` impls that
//! call the selected codec's `present` / `try_present` method. The result
//! becomes the visage struct's field value for that scope. The storage-truth
//! model field is unaffected.
//!
//! # Traits
//!
//! | Trait | When to implement |
//! |-------|------------------|
//! | [`PresentationCodecInfo<Input>`] | Always — declares `Output`, `REVERSIBILITY`, `QUERYABILITY`, and startup validation |
//! | [`PresentationCodec<Input>`] | Infallible presentation (use `presentation_codec = ...`) |
//! | [`TryPresentationCodec<Input>`] | Fallible presentation (use `try_presentation_codec = ...`) |
//! | [`ReversiblePresentationCodec<Input>`] | Infallible + reversible |
//! | [`ReversibleTryPresentationCodec<Input>`] | Fallible + reversible |
//! | [`PresentationQueryCodec<Input>`] | Predicates against presented values |
//! | [`PresentationOrderCodec<Input>`] | Ordering against presented values |
//!
//! # Security defaults
//!
//! - [`Reversibility`] defaults to [`Reversibility::OneWay`].
//! - [`Queryability`] defaults to [`Queryability::Disabled`].
//! - Query and order accessors on generated visage fields are only available
//!   when the codec implements [`PresentationQueryCodec`] /
//!   [`PresentationOrderCodec`].
//! - Source-model `Model::objects()` accessors remain privileged; the
//!   presentation codec does not restrict storage-truth queries.
//!
//! # Startup validation
//!
//! Call [`validate_startup_inventory`] before serving traffic. Keyed codecs
//! (e.g. [`builtins::HmacSha256HexString`]) validate their environment-
//! variable key during this pass. `DjogiPool::connect` / `DjogiPoolBuilder::build`
//! call this automatically (Stage 3). Apps that construct visages without
//! a pool must call it explicitly.
//!
//! # Example — adopter-defined codec
//!
//! ```ignore
//! use djogi::presentation::{PresentationCodecInfo, PresentationCodec, Reversibility, Queryability};
//!
//! /// A codec that formats a phone number as `+X-XXX-XXX-XXXX` while
//! /// masking the last four digits.
//! pub struct PhoneLastFourMask;
//!
//! #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
//! pub struct MaskedPhone(pub String);
//!
//! impl PresentationCodecInfo<String> for PhoneLastFourMask {
//!     type Output = MaskedPhone;
//!     const REVERSIBILITY: Reversibility = Reversibility::OneWay;
//!     const QUERYABILITY: Queryability = Queryability::Disabled;
//! }
//!
//! impl PresentationCodec<String> for PhoneLastFourMask {
//!     fn present(value: &String) -> MaskedPhone {
//!         // ... masking logic ...
//!         MaskedPhone(value.clone())
//!     }
//! }
//! ```

pub mod builtins;
pub mod inventory;
pub mod query;

/// Whether a presentation codec is reversible.
///
/// A codec is [`Reversible`](Self::Reversible) when the transform can be
/// undone — i.e. it implements
/// [`ReversiblePresentationCodec`] or [`ReversibleTryPresentationCodec`].
/// [`OneWay`](Self::OneWay) transforms cannot be reversed (e.g. masking,
/// hashing).
///
/// # Security note
///
/// Reversibility does **not** imply queryability. A reversible token may
/// still be non-queryable. See [`Queryability`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Reversibility {
    /// The presentation transform is one-way (e.g. masking, hashing). The
    /// original value cannot be recovered from the presented output.
    ///
    /// This is the default for [`PresentationCodecInfo`].
    #[default]
    OneWay,
    /// The presentation transform is reversible — the original value can
    /// be recovered from the presented output via
    /// [`ReversiblePresentationCodec::try_reverse`] /
    /// [`ReversibleTryPresentationCodec::try_reverse`].
    Reversible,
}

/// Whether a presentation codec exposes predicate or ordering access.
///
/// The generated visage query/order accessors for a protected field are
/// governed by this enum, which a codec sets via
/// [`PresentationCodecInfo::QUERYABILITY`]. The default is
/// [`Disabled`](Self::Disabled), which suppresses all query/order access
/// on the visage's presentation-gated field.
///
/// # Security note
///
/// Enabling predicate or order access grants adopter callers the ability to
/// probe field values through the generated accessor. Ensure the codec
/// provides appropriate transform-level protection before enabling
/// queryability. Source-model `Model::objects()` always retains storage-level
/// query access regardless of this setting.
///
/// # Relation to reversibility
///
/// Queryability and reversibility are independent axes. A reversible codec
/// may have `Disabled` queryability. A one-way codec (such as a HMAC blind
/// index) may have `PredicateOnly` queryability.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Queryability {
    /// No predicate or order access on the presentation-gated field. This
    /// is the secure default.
    #[default]
    Disabled,
    /// Only equality predicates are available (requires
    /// [`PresentationQueryCodec`]).
    PredicateOnly,
    /// Only ordering is available (requires [`PresentationOrderCodec`]).
    OrderOnly,
    /// Both predicates and ordering are available (requires both
    /// [`PresentationQueryCodec`] and [`PresentationOrderCodec`]).
    PredicateAndOrder,
}

/// Startup error variants for presentation codec validation.
///
/// Returned by [`validate_startup_inventory`] when one or more
/// codec registrations fail their startup check. Every keyed or
/// environment-variable-dependent codec should implement
/// [`PresentationCodecInfo::validate_startup`] to surface its failure
/// as one of these variants.
///
/// # Display and Error
///
/// `PresentationStartupError` implements both [`std::fmt::Display`] and
/// [`std::error::Error`]. Display messages never log key material —
/// only environment-variable names and structural information.
#[non_exhaustive]
#[derive(Debug)]
pub enum PresentationStartupError {
    /// A required environment variable was absent from the process
    /// environment at startup validation time.
    MissingEnvVar {
        /// Environment-variable name (e.g. `"DJOGI_PRESENTATION_HMAC_KEY"`).
        name: &'static str,
    },
    /// A required environment variable was present but contained non-UTF-8
    /// bytes that could not be decoded.
    NonUnicodeEnvVar {
        /// Environment-variable name.
        name: &'static str,
    },
    /// A hex-encoded key environment variable had the wrong number of
    /// hex characters. Expected exactly 64 lowercase hex characters
    /// (= 32 bytes) for HMAC.
    InvalidHexLength {
        /// Environment-variable name.
        name: &'static str,
        /// Actual number of characters observed.
        actual: usize,
    },
    /// A hex-encoded key environment variable contained a byte that is
    /// not a valid ASCII hex character (`0`–`9`, `a`–`f`, or `A`–`F`).
    InvalidHexByte {
        /// Environment-variable name.
        name: &'static str,
        /// Zero-based byte index of the invalid character.
        idx: usize,
    },
    /// A hex-encoded key environment variable contained an uppercase hex
    /// character (`A`–`F`). The canonical presentation-HMAC format requires
    /// exclusively lowercase hex characters (`a`–`f`) so that a case-change
    /// in the env-var is treated as a rejected key rather than a silent
    /// same-key re-entry.
    ///
    /// Unlike snapshot signing's mixed-case parser, presentation HMAC v1
    /// enforces lowercase-only: accepting `AABB...` and `aabb...` as
    /// equivalent would allow a case-only env-var change to look like a
    /// key rotation while actually keeping the same key bytes, creating a
    /// false sense of rotation.
    NonLowercaseHexByte {
        /// Environment-variable name.
        name: &'static str,
        /// Zero-based byte index of the uppercase character.
        idx: usize,
    },
    /// A codec-level startup error not covered by the structural variants
    /// above. The `source` carries the codec's own error type.
    Codec {
        /// Rust type-path string of the codec that failed.
        codec_path: &'static str,
        /// The underlying error from the codec's startup validation.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A per-`(model, field, scope, codec)` usage wrapping a lower-level
    /// startup error. [`validate_startup_inventory`] wraps each
    /// per-usage failure in this variant so callers can identify which
    /// specific field/scope combination failed.
    Usage {
        /// Model type name.
        model: &'static str,
        /// Field name.
        field: &'static str,
        /// Scope key.
        scope: &'static str,
        /// Codec type-path string.
        codec_path: &'static str,
        /// The underlying startup error from the codec validation.
        source: Box<PresentationStartupError>,
    },
}

impl std::fmt::Display for PresentationStartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEnvVar { name } => {
                write!(f, "presentation codec startup: missing env var `{name}`")
            }
            Self::NonUnicodeEnvVar { name } => {
                write!(
                    f,
                    "presentation codec startup: env var `{name}` contains non-UTF-8 bytes"
                )
            }
            Self::InvalidHexLength { name, actual } => {
                write!(
                    f,
                    "presentation codec startup: env var `{name}` must be exactly 64 lowercase \
                     hex characters (32 bytes); got {actual} characters"
                )
            }
            Self::InvalidHexByte { name, idx } => {
                write!(
                    f,
                    "presentation codec startup: env var `{name}` contains an invalid hex \
                     character at index {idx} (expected `0`–`9` or `a`–`f`)"
                )
            }
            Self::NonLowercaseHexByte { name, idx } => {
                write!(
                    f,
                    "presentation codec startup: env var `{name}` contains an uppercase hex \
                     character at index {idx}; presentation HMAC requires lowercase-only \
                     hex characters (`a`–`f`, not `A`–`F`)"
                )
            }
            Self::Codec { codec_path, source } => {
                write!(
                    f,
                    "presentation codec startup: codec `{codec_path}` failed validation: {source}"
                )
            }
            Self::Usage {
                model,
                field,
                scope,
                codec_path,
                source,
            } => {
                write!(
                    f,
                    "presentation codec startup: {model}.{field} scope `{scope}` \
                     codec `{codec_path}` failed: {source}"
                )
            }
        }
    }
}

impl std::error::Error for PresentationStartupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec { source, .. } => Some(source.as_ref()),
            Self::Usage { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Error returned by built-in keyed presentation codecs when called
/// without a validated key.
///
/// Keyed built-ins (e.g. [`builtins::HmacSha256HexString`]) use a
/// [`OnceLock`](std::sync::OnceLock)-backed cache for their key. When
/// the per-value `try_present` path is called and the cache is empty
/// (startup validation was bypassed), they return this error rather than
/// panicking or reading the environment.
///
/// This error is mapped to
/// [`VisageError::PresentationCodec`](crate::visage::VisageError::PresentationCodec)
/// by generated `TryFrom<&Model>` impls so the failure surfaces as
/// a typed `DjogiError::Visage(...)` rather than as a panic.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltInPresentationError {
    /// The per-value codec path was called before
    /// [`validate_startup_inventory`] successfully loaded and cached the
    /// key. The `env` field names the environment variable that provides
    /// the key so callers can report the configuration gap.
    KeyNotValidated {
        /// Environment-variable name expected to hold the key.
        env: &'static str,
    },
}

impl std::fmt::Display for BuiltInPresentationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyNotValidated { env } => {
                write!(
                    f,
                    "built-in presentation codec key not validated: \
                     call `djogi::presentation::validate_startup_inventory()` \
                     before projecting visages, or set `{env}` and retry startup"
                )
            }
        }
    }
}

impl std::error::Error for BuiltInPresentationError {}

/// Core metadata trait for a presentation codec.
///
/// Every codec type must implement `PresentationCodecInfo<Input>` to
/// declare its output type and metadata. The codec then additionally
/// implements either [`PresentationCodec<Input>`] (infallible) or
/// [`TryPresentationCodec<Input>`] (fallible).
///
/// # Output type bounds
///
/// The `Output` associated type must implement `Clone + Debug + Serialize +
/// DeserializeOwned + 'static`. These are the bounds required by generated
/// visage structs, which unconditionally derive `Clone`, `Debug`,
/// `serde::Serialize`, and `serde::Deserialize`. Codec types whose output
/// does not satisfy these bounds will fail at the macro's trait-check step.
///
/// # Dual implementation
///
/// A single codec type may implement both [`PresentationCodec<Input>`] and
/// [`TryPresentationCodec<Input>`] for the same `Input`. When it does,
/// both implementations must share `PresentationCodecInfo<Input>::Output`,
/// `REVERSIBILITY`, and `QUERYABILITY` — those are declared on this trait,
/// not on the subtraits. Attempted divergent `Output` types will fail at
/// the Rust type level because the associated type lives on this one trait.
pub trait PresentationCodecInfo<Input>: Send + Sync + 'static {
    /// The type produced by this codec's presentation transform.
    ///
    /// Must implement `Clone + Debug + Serialize + DeserializeOwned + 'static`
    /// to satisfy the bounds generated visage structs place on their fields.
    type Output: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + 'static;

    /// Reversibility of this codec's transform.
    ///
    /// Defaults to [`Reversibility::OneWay`]. Override to
    /// [`Reversibility::Reversible`] and implement
    /// [`ReversiblePresentationCodec`] / [`ReversibleTryPresentationCodec`]
    /// to declare and expose the reverse operation.
    const REVERSIBILITY: Reversibility = Reversibility::OneWay;

    /// Queryability of this codec's transform.
    ///
    /// Defaults to [`Queryability::Disabled`]. Override to enable generated
    /// visage field accessors for predicates / ordering. Enabling
    /// queryability grants callers the ability to probe field values through
    /// the generated accessor; choose the minimum necessary queryability.
    const QUERYABILITY: Queryability = Queryability::Disabled;

    /// Validate that the codec is properly configured for request-time use.
    ///
    /// Called during [`validate_startup_inventory`] (which is invoked by
    /// `DjogiPool::connect` / `DjogiPoolBuilder::build`). Return `Ok(())`
    /// if the codec is ready, or a [`PresentationStartupError`] if not.
    ///
    /// The default implementation returns `Ok(())` — override for codecs
    /// that require environment-variable keys or other external resources.
    ///
    /// # Retry semantics
    ///
    /// This function must be retryable: a failed validation attempt must not
    /// poison a later attempt that occurs after the environment is fixed.
    fn validate_startup() -> Result<(), PresentationStartupError> {
        Ok(())
    }
}

/// Infallible presentation codec.
///
/// Implement this trait when the presentation transform is total (cannot
/// fail). The `#[field(protected(per_scope = { scope = { presentation_codec
/// = YourCodec } }))]` attribute key selects this path.
///
/// For transforms that can fail (e.g. because a key has not been loaded),
/// implement [`TryPresentationCodec`] instead.
pub trait PresentationCodec<Input>: PresentationCodecInfo<Input> {
    /// Transform `value` into the codec's output type.
    ///
    /// Must be total — no panics, no environment reads, no I/O. Keyed
    /// transforms must pre-cache their key via [`PresentationCodecInfo::validate_startup`]
    /// and use [`TryPresentationCodec`] if the key may be absent.
    fn present(value: &Input) -> <Self as PresentationCodecInfo<Input>>::Output;
}

/// Fallible presentation codec.
///
/// Implement this trait when the presentation transform can fail — for
/// example, because a key has not been loaded (startup validation was
/// bypassed) or because the input is malformed. The `#[field(protected(per_scope
/// = { scope = { try_presentation_codec = YourCodec } }))]` attribute key
/// selects this path.
///
/// Generated `TryFrom<&Model>` impls map `Self::Error` to
/// [`VisageError::PresentationCodec`](crate::visage::VisageError::PresentationCodec).
pub trait TryPresentationCodec<Input>: PresentationCodecInfo<Input> {
    /// The error type returned when `try_present` fails.
    ///
    /// Must implement `std::error::Error + Send + Sync + 'static` so
    /// it can be boxed into `VisageError::PresentationCodec::source`.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Attempt to transform `value` into the codec's output type.
    ///
    /// Errors are mapped to
    /// [`VisageError::PresentationCodec`](crate::visage::VisageError::PresentationCodec)
    /// by generated projection code. Must not panic — panic-free is the
    /// contract for request-path transforms.
    fn try_present(
        value: &Input,
    ) -> Result<<Self as PresentationCodecInfo<Input>>::Output, Self::Error>;
}

/// Infallible presentation codec that additionally supports reversing the
/// transform.
///
/// Implementing this trait (on top of [`PresentationCodec<Input>`]) does
/// not automatically enable queryability — that requires additionally
/// implementing [`PresentationQueryCodec`] / [`PresentationOrderCodec`].
pub trait ReversiblePresentationCodec<Input>: PresentationCodec<Input> {
    /// Error type for the reverse operation.
    ///
    /// A reverse operation is often fallible even if the forward transform
    /// is not, because the output space may not be injective into the input
    /// space.
    type ReverseError: std::error::Error + Send + Sync + 'static;

    /// Attempt to reverse `value` back to the input type.
    ///
    /// # Correctness invariant
    ///
    /// For any `input`, `try_reverse(&Self::present(input))` must return
    /// `Ok(original)` where `original == *input`. Violating this invariant
    /// produces incorrect data round-trips.
    fn try_reverse(
        value: &<Self as PresentationCodecInfo<Input>>::Output,
    ) -> Result<Input, Self::ReverseError>;
}

/// Fallible presentation codec that additionally supports reversing the
/// transform.
///
/// Implementing this trait (on top of [`TryPresentationCodec<Input>`]) does
/// not automatically enable queryability.
pub trait ReversibleTryPresentationCodec<Input>: TryPresentationCodec<Input> {
    /// Error type for the reverse operation.
    type ReverseError: std::error::Error + Send + Sync + 'static;

    /// Attempt to reverse `value` back to the input type.
    fn try_reverse(
        value: &<Self as PresentationCodecInfo<Input>>::Output,
    ) -> Result<Input, Self::ReverseError>;
}

/// Walk the linked-at-call-time [`PresentationCodecUsage`](inventory::PresentationCodecUsage)
/// inventory and invoke each usage's `validate_startup` function.
///
/// Returns `Ok(())` if all usages validate successfully. Returns
/// `Err(errors)` containing **all** failures collected (not short-circuiting)
/// so the caller can report every broken codec configuration in a single
/// startup message.
///
/// # Startup contract
///
/// `DjogiPool::connect`, `DjogiPool::from_database_config`, and
/// `DjogiPoolBuilder::build` call this function before returning a pool.
/// Apps that construct visages without a pool must call it explicitly
/// during startup.
///
/// # Per-value bypass behavior
///
/// If startup validation is skipped, keyed built-in codecs (e.g.
/// [`builtins::HmacSha256HexString`]) return
/// [`BuiltInPresentationError::KeyNotValidated`] from their `try_present`
/// path, which generated code maps to
/// [`VisageError::PresentationCodec`](crate::visage::VisageError::PresentationCodec).
/// No panics occur regardless of startup state.
///
/// # Plugin / deferred-load architectures
///
/// Call this function **after** loading any crates that use `#[model]` with
/// `per_scope` presentation blocks. Inventory cannot see crates that are not
/// yet linked, so calling before the crate is loaded will silently miss its
/// usages. This is a documented manual boundary.
pub fn validate_startup_inventory() -> Result<(), Vec<PresentationStartupError>> {
    let errors: Vec<PresentationStartupError> =
        ::inventory::iter::<inventory::PresentationCodecUsage>
            .into_iter()
            .filter_map(|usage| {
                (usage.validate_startup)()
                    .err()
                    .map(|e| PresentationStartupError::Usage {
                        model: usage.model,
                        field: usage.field,
                        scope: usage.scope,
                        codec_path: usage.codec_path,
                        source: Box::new(e),
                    })
            })
            .collect();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `validate_startup_inventory` with an empty inventory (no `#[model]`
    /// structs with `per_scope` blocks are linked into the test binary)
    /// must return `Ok(())`.
    #[test]
    fn validate_startup_inventory_empty_inventory_returns_ok() {
        // The test binary does not link any model crates with per_scope blocks,
        // so the inventory should be empty and validation must succeed.
        let result = validate_startup_inventory();
        assert!(
            result.is_ok(),
            "expected Ok(()), got Err with {} error(s)",
            result.unwrap_err().len()
        );
    }

    #[test]
    fn reversibility_default_is_one_way() {
        assert_eq!(Reversibility::default(), Reversibility::OneWay);
    }

    #[test]
    fn queryability_default_is_disabled() {
        assert_eq!(Queryability::default(), Queryability::Disabled);
    }

    #[test]
    fn presentation_startup_error_display_missing_env_var() {
        let e = PresentationStartupError::MissingEnvVar { name: "MY_KEY" };
        let msg = e.to_string();
        assert!(
            msg.contains("MY_KEY"),
            "display must include var name: {msg}"
        );
    }

    #[test]
    fn presentation_startup_error_display_invalid_hex_length() {
        let e = PresentationStartupError::InvalidHexLength {
            name: "MY_KEY",
            actual: 32,
        };
        let msg = e.to_string();
        assert!(
            msg.contains("MY_KEY"),
            "display must include var name: {msg}"
        );
        assert!(
            msg.contains("32"),
            "display must include actual length: {msg}"
        );
        assert!(
            msg.contains("64"),
            "display must include expected length: {msg}"
        );
    }

    #[test]
    fn presentation_startup_error_display_non_lowercase_hex() {
        let e = PresentationStartupError::NonLowercaseHexByte {
            name: "MY_KEY",
            idx: 5,
        };
        let msg = e.to_string();
        assert!(
            msg.contains("MY_KEY"),
            "display must include var name: {msg}"
        );
        assert!(
            msg.contains('5'.to_string().as_str()),
            "display must include index: {msg}"
        );
    }

    #[test]
    fn presentation_startup_error_display_usage_wraps_source() {
        let inner = PresentationStartupError::MissingEnvVar { name: "KEY" };
        let e = PresentationStartupError::Usage {
            model: "User",
            field: "email",
            scope: "public",
            codec_path: "crate::MyCodec",
            source: Box::new(inner),
        };
        let msg = e.to_string();
        assert!(msg.contains("User"), "display must include model: {msg}");
        assert!(msg.contains("email"), "display must include field: {msg}");
        assert!(msg.contains("public"), "display must include scope: {msg}");
    }

    #[test]
    fn builtin_presentation_error_display_key_not_validated() {
        let e = BuiltInPresentationError::KeyNotValidated {
            env: "DJOGI_PRESENTATION_HMAC_KEY",
        };
        let msg = e.to_string();
        assert!(
            msg.contains("DJOGI_PRESENTATION_HMAC_KEY"),
            "display must include env var name: {msg}"
        );
        assert!(
            msg.contains("validate_startup_inventory"),
            "display must mention validate_startup_inventory: {msg}"
        );
    }

    #[test]
    fn builtin_presentation_error_is_std_error() {
        let e = BuiltInPresentationError::KeyNotValidated {
            env: "DJOGI_PRESENTATION_HMAC_KEY",
        };
        // Exercise the Error trait — source() must return None for this variant.
        let dyn_err: &dyn std::error::Error = &e;
        assert!(dyn_err.source().is_none());
    }
}
