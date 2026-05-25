//! Inventory structs for the presentation-codec system.
//!
//! This module defines the presentation-codec record types used for
//! runtime startup validation and audit/debug visibility.
//!
//! ## Record types
//!
//! - [`ProtectedPresentationScopeMetadata`] — per-scope metadata inside a
//!   field's presentation declaration. One entry per `(field, scope)` pair
//!   declared in `protected(per_scope = { ... })`.
//! - [`ProtectedPresentationFieldMetadata`] — per-field aggregate carrying
//!   a static slice of scope entries. This is a Stage-facing shape for richer
//!   introspection tooling; startup validation currently consumes
//!   `PresentationCodecUsage` entries directly.
//! - [`PresentationCodecUsage`] — one submission per `(model, field, scope,
//!   codec)` usage. Consumed by [`super::validate_startup_inventory`] to call
//!   each codec's [`validate_startup`](super::PresentationCodecInfo::validate_startup)
//!   before the framework accepts traffic.
//!
//! ## Design note
//!
//! All structs are `Copy` (enforced by the `Copy` derive) so macro-emitted
//! `inventory::submit!` blocks and static slice initializers can embed values
//! without ownership complexity. `#[non_exhaustive]` on the structs allows
//! future fields to be added without breaking downstream code that constructs
//! values via the `const_new` constructors.

use crate::presentation::{PresentationStartupError, Queryability, Reversibility};

/// Per-scope presentation metadata for a single protected field.
///
/// Emitted by `#[model]` for every `(field, scope)` entry inside a
/// `protected(per_scope = { ... })` block. Aggregated into a static slice
/// inside [`ProtectedPresentationFieldMetadata`].
///
/// # Audit and debug use only
///
/// This struct is **not** consulted at runtime to decide presentation
/// behavior — that is driven by the trait-dispatch paths in generated
/// `From<&Model>` / `TryFrom<&Model>` impls. The inventory record
/// supports tooling (audit logs, `djogi docs`, future CLI introspection)
/// that needs to enumerate which codec is active for which field/scope.
///
/// # Invariants
///
/// - `scope` matches one of the model's declared scope keys.
/// - `codec_path` is a Rust type path to the codec implementing
///   [`PresentationCodecInfo`](super::PresentationCodecInfo).
/// - `reversible` and `queryability` reflect the codec's trait constants.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtectedPresentationScopeMetadata {
    /// Scope key (e.g. `"public"`, `"self_view"`, `"admin"`, `"export"`,
    /// or a custom scope declared at the model level).
    pub scope: &'static str,
    /// Rust type-path string of the codec (e.g.
    /// `"djogi::presentation::builtins::MaskString"`). Used for audit and
    /// debug — not evaluated at runtime.
    pub codec_path: &'static str,
    /// Whether the codec uses `TryPresentationCodec` (fallible) or
    /// `PresentationCodec` (infallible) for this scope.
    pub fallible: bool,
    /// Reversibility declared on the codec's
    /// [`PresentationCodecInfo`](super::PresentationCodecInfo) impl.
    pub reversible: Reversibility,
    /// Queryability declared on the codec's
    /// [`PresentationCodecInfo`](super::PresentationCodecInfo) impl.
    pub queryability: Queryability,
}

impl ProtectedPresentationScopeMetadata {
    /// Construct a `ProtectedPresentationScopeMetadata` from macro-emitted
    /// code.
    ///
    /// All parameters map 1:1 to the public fields. This constructor exists
    /// because the struct is `#[non_exhaustive]` — struct-expression
    /// construction outside this crate is blocked by the attribute.
    ///
    /// # Note
    ///
    /// This is a `#[doc(hidden)]` constructor intended exclusively for
    /// `#[model]`-emitted code. Downstream adoption of this constructor
    /// from outside generated macro output is outside the public contract.
    #[doc(hidden)]
    pub const fn const_new(
        scope: &'static str,
        codec_path: &'static str,
        fallible: bool,
        reversible: Reversibility,
        queryability: Queryability,
    ) -> Self {
        Self {
            scope,
            codec_path,
            fallible,
            reversible,
            queryability,
        }
    }
}

/// Per-field inventory record for protected presentation behavior.
///
/// The `scopes` slice holds one
/// [`ProtectedPresentationScopeMetadata`] entry per scope entry in the
/// `protected(per_scope = { ... })` block.
///
/// # Inventory semantics
///
/// `ProtectedPresentationFieldMetadata` is a per-field aggregate for future
/// tooling. Stage 4+ does not currently emit this type with
/// `inventory::submit!`, so startup validation iterates
/// `inventory::iter::<PresentationCodecUsage>` instead. The public record still
/// exists for future extensions and richer tooling.
///
/// # Separate from `FieldDescriptor`
///
/// This struct intentionally does NOT attach to `FieldDescriptor` or
/// `ModelDescriptor`. Presentation metadata is runtime-only and does not
/// affect SQL DDL — attaching it to the migration differ's descriptor
/// graph would erroneously trigger schema drift warnings when codecs change.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ProtectedPresentationFieldMetadata {
    /// Rust type name of the owning model (e.g. `"User"`).
    pub model: &'static str,
    /// Field name (Rust ident, same as the database column name unless
    /// `#[field(column = "...")]` overrides it).
    pub field: &'static str,
    /// Scope-level metadata for each entry in the field's `per_scope` block.
    /// Static slice embedded by macro-generated code.
    pub scopes: &'static [ProtectedPresentationScopeMetadata],
}

impl ProtectedPresentationFieldMetadata {
    /// Construct a `ProtectedPresentationFieldMetadata` from generated or
    /// macro-emitted code.
    ///
    /// This constructor exists because the struct is `#[non_exhaustive]`.
    /// See [`ProtectedPresentationScopeMetadata::const_new`] for the
    /// scope-level equivalent.
    #[doc(hidden)]
    pub const fn const_new(
        model: &'static str,
        field: &'static str,
        scopes: &'static [ProtectedPresentationScopeMetadata],
    ) -> Self {
        Self {
            model,
            field,
            scopes,
        }
    }
}

/// Per-usage inventory record consumed by startup validation.
///
/// Emitted by `#[model]` for each concrete `(model, field, scope, codec)`
/// usage in a `protected(per_scope = { ... })` block. Startup validation
/// walks all `PresentationCodecUsage` entries via
/// `inventory::iter::<PresentationCodecUsage>` and calls
/// `validate_startup` on each one.
///
/// ## Startup contract
///
/// `DjogiPool::connect`, `DjogiPool::from_database_config`, and
/// `DjogiPoolBuilder::build` call
/// [`validate_startup_inventory`](super::validate_startup_inventory) before
/// returning a pool. Any codec that needs environment-variable keying (e.g.
/// [`HmacSha256HexString`](super::builtins::HmacSha256HexString)) validates
/// its key here before any request traffic is served.
///
/// ## Type-name functions
///
/// `input_type_name` and `output_type_name` are function pointers (not
/// `&'static str` directly) so `std::any::type_name::<T>()` can be called
/// lazily without materializing the string at construction time. This
/// avoids needing const-eval for type-name strings at macro expansion time.
///
/// ## Security note
///
/// The inventory record is **not** consulted at per-request presentation
/// time. Generated `From<&Model>` / `TryFrom<&Model>` impls call the
/// codec trait methods directly. The inventory record supports startup
/// validation and offline tooling only.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct PresentationCodecUsage {
    /// Rust type name of the owning model.
    pub model: &'static str,
    /// Field name (Rust ident).
    pub field: &'static str,
    /// Scope key for this usage (e.g. `"public"`).
    pub scope: &'static str,
    /// Rust type-path string of the codec.
    pub codec_path: &'static str,
    /// Whether this usage involves a fallible codec (`TryPresentationCodec`).
    pub fallible: bool,
    /// Returns the Rust type name of the codec's input type. Evaluated
    /// lazily via `std::any::type_name::<Input>()`.
    pub input_type_name: fn() -> &'static str,
    /// Returns the Rust type name of the codec's output type. Evaluated
    /// lazily via `std::any::type_name::<Output>()`.
    pub output_type_name: fn() -> &'static str,
    /// Calls `<Codec as PresentationCodecInfo<Input>>::validate_startup()`.
    /// Returns `Ok(())` when the codec is ready, or a
    /// [`PresentationStartupError`] variant describing the failure.
    pub validate_startup: fn() -> Result<(), PresentationStartupError>,
}

impl PresentationCodecUsage {
    /// Construct a `PresentationCodecUsage` from macro-emitted code.
    ///
    /// Each parameter maps 1:1 to the public fields. This constructor
    /// exists because the struct is `#[non_exhaustive]`.
    ///
    /// The 8-parameter signature is intentional: the struct carries 8 fields,
    /// all of which are required by macro-emitted `inventory::submit!` blocks.
    /// A builder pattern would add complexity for a `#[doc(hidden)]`
    /// macro-only constructor that is never called from user code.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub const fn const_new(
        model: &'static str,
        field: &'static str,
        scope: &'static str,
        codec_path: &'static str,
        fallible: bool,
        input_type_name: fn() -> &'static str,
        output_type_name: fn() -> &'static str,
        validate_startup: fn() -> Result<(), PresentationStartupError>,
    ) -> Self {
        Self {
            model,
            field,
            scope,
            codec_path,
            fallible,
            input_type_name,
            output_type_name,
            validate_startup,
        }
    }
}

// Register the collection points for both inventory types so
// `inventory::iter::<T>` works across the linked binary.
inventory::collect!(ProtectedPresentationFieldMetadata);
inventory::collect!(PresentationCodecUsage);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_metadata_const_new_round_trips_fields() {
        let meta = ProtectedPresentationScopeMetadata::const_new(
            "public",
            "djogi::presentation::builtins::MaskString",
            false,
            Reversibility::OneWay,
            Queryability::Disabled,
        );
        assert_eq!(meta.scope, "public");
        assert_eq!(meta.codec_path, "djogi::presentation::builtins::MaskString");
        assert!(!meta.fallible);
        assert_eq!(meta.reversible, Reversibility::OneWay);
        assert_eq!(meta.queryability, Queryability::Disabled);
    }

    #[test]
    fn field_metadata_const_new_round_trips_fields() {
        static SCOPES: &[ProtectedPresentationScopeMetadata] =
            &[ProtectedPresentationScopeMetadata {
                scope: "admin",
                codec_path: "djogi::presentation::builtins::Identity",
                fallible: false,
                reversible: Reversibility::Reversible,
                queryability: Queryability::PredicateAndOrder,
            }];
        let meta = ProtectedPresentationFieldMetadata::const_new("User", "email", SCOPES);
        assert_eq!(meta.model, "User");
        assert_eq!(meta.field, "email");
        assert_eq!(meta.scopes.len(), 1);
        assert_eq!(meta.scopes[0].scope, "admin");
    }

    #[test]
    fn codec_usage_const_new_round_trips_fields() {
        fn input_name() -> &'static str {
            "String"
        }
        fn output_name() -> &'static str {
            "String"
        }
        fn validate() -> Result<(), PresentationStartupError> {
            Ok(())
        }

        let usage = PresentationCodecUsage::const_new(
            "User",
            "email",
            "public",
            "djogi::presentation::builtins::MaskString",
            false,
            input_name,
            output_name,
            validate,
        );
        assert_eq!(usage.model, "User");
        assert_eq!(usage.field, "email");
        assert_eq!(usage.scope, "public");
        assert_eq!(
            usage.codec_path,
            "djogi::presentation::builtins::MaskString"
        );
        assert!(!usage.fallible);
        assert_eq!((usage.input_type_name)(), "String");
        assert_eq!((usage.output_type_name)(), "String");
        assert!((usage.validate_startup)().is_ok());
    }
}
