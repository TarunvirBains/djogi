//! Macro-side parsing and validation for `#[field(protected(...))]` and
//! `#[field(default_volatility = "...")]`.
//!
//! Phase 7.5 T3 owns:
//!
//! 1. **Parsing** the `protected(...)` nested attribute into a
//!    [`ProtectedSpec`] that the descriptor emitter consumes when
//!    building the `Option<ProtectedFieldMetadata>` literal.
//! 2. **Validating** the four rules from the v3 plan §6 with
//!    span-precise errors:
//!    - `sensitivity = "none"` cannot be combined with any other
//!      protected field.
//!    - `sensitivity > none` requires a non-empty `rationale`.
//!    - `codec = "X"` must reference a registered codec ID.
//!    - `redaction = "hash_id"` is only valid on a HeerId / RanjId /
//!      custom-PK-compatible field type.
//! 3. **Parsing and validating** the optional
//!    `#[field(default_volatility = "...")]` override into
//!    [`DefaultVolatilityLit`].
//!
//! # Codec ID validation strategy
//!
//! Proc macros run before any runtime dependency is available, so the
//! macro crate cannot read `djogi::field_codec::REGISTRY` directly.
//! Instead, [`KNOWN_CODEC_IDS`] mirrors the runtime `phf::Set` as a
//! sorted const slice. Validation uses `binary_search` for O(log n)
//! lookup — no regex, no runtime FFI, span-precise diagnostics. The
//! synchronization contract (update both lists when adding a codec)
//! is documented on the runtime `REGISTRY` static.
//!
//! V1 ships an empty registry, so any `codec = "..."` literal triggers
//! the unknown-codec error. The error message lists the empty set
//! verbatim ("(none)") so adopters reading the diagnostic understand
//! that codec support has not yet shipped, rather than thinking they
//! mistyped a real codec name.

use proc_macro2::Span;
use quote::quote;
use syn::{
    Expr, ExprLit, Lit, Meta, MetaNameValue, Token, punctuated::Punctuated, spanned::Spanned,
};

/// Sorted const slice of every codec ID that the macro recognises at
/// expansion time.
///
/// Kept in sync with `djogi::field_codec::REGISTRY` per the contract
/// documented on that static. V1 ships empty — every real codec lands
/// in later Phase 7.5 tasks. Sorted so [`is_known_codec`] resolves via
/// `slice::binary_search`; matches the project's no-regex,
/// sorted-const-slice convention.
pub const KNOWN_CODEC_IDS: &[&str] = &[];

/// `true` when `id` appears in [`KNOWN_CODEC_IDS`]. Resolved by
/// `binary_search` on the sorted slice — O(log n) with zero allocation.
pub fn is_known_codec(id: &str) -> bool {
    KNOWN_CODEC_IDS.binary_search(&id).is_ok()
}

/// Parsed `sensitivity = "..."` literal.
///
/// Mirrors `djogi::descriptor::Sensitivity` one-for-one. Stored as a
/// macro-side enum (rather than as the `String` literal) so the
/// emitter renders the exact `Sensitivity::Variant` ident without
/// re-parsing. T3 covers parsing + validation; the emitter calls
/// [`Self::ident_tokens`] when populating the descriptor literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitivityLit {
    None,
    Internal,
    Pii,
    Sensitive,
    Secret,
}

impl SensitivityLit {
    fn parse(value: &str, span: Span) -> syn::Result<Self> {
        match value {
            "none" => Ok(SensitivityLit::None),
            "internal" => Ok(SensitivityLit::Internal),
            "pii" => Ok(SensitivityLit::Pii),
            "sensitive" => Ok(SensitivityLit::Sensitive),
            "secret" => Ok(SensitivityLit::Secret),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown sensitivity `{other}`; expected one of: \
                     none, internal, pii, sensitive, secret",
                ),
            )),
        }
    }

    fn ident_tokens(self) -> proc_macro2::TokenStream {
        match self {
            SensitivityLit::None => quote! { None },
            SensitivityLit::Internal => quote! { Internal },
            SensitivityLit::Pii => quote! { Pii },
            SensitivityLit::Sensitive => quote! { Sensitive },
            SensitivityLit::Secret => quote! { Secret },
        }
    }
}

/// Parsed `redaction = "..."` literal. Mirrors
/// `djogi::descriptor::RedactionPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionLit {
    None,
    HashId,
    Mask,
    Drop,
}

impl RedactionLit {
    fn parse(value: &str, span: Span) -> syn::Result<Self> {
        match value {
            "none" => Ok(RedactionLit::None),
            "hash_id" => Ok(RedactionLit::HashId),
            "mask" => Ok(RedactionLit::Mask),
            "drop" => Ok(RedactionLit::Drop),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown redaction `{other}`; expected one of: \
                     none, hash_id, mask, drop",
                ),
            )),
        }
    }

    fn ident_tokens(self) -> proc_macro2::TokenStream {
        match self {
            RedactionLit::None => quote! { None },
            RedactionLit::HashId => quote! { HashId },
            RedactionLit::Mask => quote! { Mask },
            RedactionLit::Drop => quote! { Drop },
        }
    }
}

/// Parsed `retention = "..."` literal. Mirrors
/// `djogi::descriptor::RetentionLabel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionLit {
    Transient,
    Standard,
    Extended,
    Archival,
}

impl RetentionLit {
    fn parse(value: &str, span: Span) -> syn::Result<Self> {
        match value {
            "transient" => Ok(RetentionLit::Transient),
            "standard" => Ok(RetentionLit::Standard),
            "extended" => Ok(RetentionLit::Extended),
            "archival" => Ok(RetentionLit::Archival),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown retention `{other}`; expected one of: \
                     transient, standard, extended, archival",
                ),
            )),
        }
    }

    fn ident_tokens(self) -> proc_macro2::TokenStream {
        match self {
            RetentionLit::Transient => quote! { Transient },
            RetentionLit::Standard => quote! { Standard },
            RetentionLit::Extended => quote! { Extended },
            RetentionLit::Archival => quote! { Archival },
        }
    }
}

/// Parsed `#[field(default_volatility = "...")]` literal. Mirrors
/// `djogi::descriptor::DefaultVolatility`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultVolatilityLit {
    Immutable,
    Stable,
    Volatile,
}

impl DefaultVolatilityLit {
    /// Parse the string literal. The error message lists every valid
    /// choice so adopters can fix the typo without consulting the docs.
    pub fn parse(value: &str, span: Span) -> syn::Result<Self> {
        match value {
            "immutable" => Ok(DefaultVolatilityLit::Immutable),
            "stable" => Ok(DefaultVolatilityLit::Stable),
            "volatile" => Ok(DefaultVolatilityLit::Volatile),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown `default_volatility = \"{other}\"`; \
                     expected one of: \"immutable\", \"stable\", \"volatile\"",
                ),
            )),
        }
    }

    /// Emit the matching `::djogi::DefaultVolatility::Variant` token
    /// path. Called by the descriptor emitter when populating the
    /// `default_volatility_override` field.
    pub fn to_tokens(self) -> proc_macro2::TokenStream {
        match self {
            DefaultVolatilityLit::Immutable => {
                quote! { ::djogi::DefaultVolatility::Immutable }
            }
            DefaultVolatilityLit::Stable => {
                quote! { ::djogi::DefaultVolatility::Stable }
            }
            DefaultVolatilityLit::Volatile => {
                quote! { ::djogi::DefaultVolatility::Volatile }
            }
        }
    }
}

/// Parsed `#[field(protected(...))]` annotation.
///
/// `sensitivity` is mandatory (the protected attribute is meaningless
/// without it); every other knob is optional and falls back to the
/// neutral default the descriptor's `Default` impl supplies. Spans for
/// the per-key literals are stored on each parsed field so validation
/// errors can underline the offending value rather than the whole
/// attribute.
#[derive(Debug, Clone)]
pub struct ProtectedSpec {
    pub sensitivity: SensitivityLit,
    pub sensitivity_span: Span,
    pub rationale: Option<String>,
    pub rationale_span: Option<Span>,
    pub redaction: RedactionLit,
    pub redaction_span: Option<Span>,
    pub codec: Option<String>,
    pub codec_span: Option<Span>,
    pub retention: RetentionLit,
    /// `Some(span)` when `retention = "..."` was written explicitly,
    /// even when its value happens to equal the neutral default. Rule
    /// (a) discriminates "the user wrote this key" from "the value is
    /// the default" via this span — explicit `retention = "standard"`
    /// alongside `sensitivity = "none"` is still a contradiction, even
    /// though the resulting value matches the default.
    pub retention_span: Option<Span>,
    /// Span of the entire `protected(...)` list — used as the fallback
    /// span when an error references the attribute as a whole rather
    /// than a single key.
    pub list_span: Span,
}

impl ProtectedSpec {
    /// Emit `Some(::djogi::ProtectedFieldMetadata { ... })` token stream
    /// for the descriptor literal. The codec / rationale fields lower
    /// to their literal forms; absent values use the matching `None` /
    /// neutral defaults declared on the descriptor.
    pub fn to_tokens(&self) -> proc_macro2::TokenStream {
        let sensitivity = self.sensitivity.ident_tokens();
        let redaction = self.redaction.ident_tokens();
        let retention = self.retention.ident_tokens();
        let rationale = match self.rationale.as_deref() {
            Some(s) => quote! { #s },
            None => quote! { "" },
        };
        let codec = match self.codec.as_deref() {
            Some(s) => quote! { ::std::option::Option::Some(#s) },
            None => quote! { ::std::option::Option::None },
        };
        quote! {
            ::std::option::Option::Some(::djogi::ProtectedFieldMetadata {
                sensitivity: ::djogi::Sensitivity::#sensitivity,
                rationale: #rationale,
                redaction: ::djogi::RedactionPolicy::#redaction,
                codec: #codec,
                retention: ::djogi::RetentionLabel::#retention,
            })
        }
    }
}

/// Walk the raw `#[field(...)]` attrs on `field` and parse every
/// `protected(...)` nested list into a [`ProtectedSpec`].
///
/// Multiple `protected(...)` annotations on the same field are
/// rejected — the error span lands on the second occurrence. Returns
/// `Ok(None)` when no `protected(...)` is present (the common case).
///
/// The parser does not validate cross-key rules; that runs in
/// [`validate`] after the spec is fully assembled. Splitting parse vs
/// validate keeps span recovery local: parse owns the per-literal
/// spans, validate owns the cross-key reasoning that knows which span
/// is most useful for each error.
pub fn parse_from_field(field: &syn::Field) -> syn::Result<Option<ProtectedSpec>> {
    let mut found: Option<ProtectedSpec> = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        let Ok(inner) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
        else {
            continue;
        };
        for nested in &inner {
            // `protected` is only valid as a list — i.e. `protected(...)`.
            // The bare-path (`#[field(protected)]`) and name-value
            // (`#[field(protected = "x")]`) shapes were silently dropped
            // by an earlier pass, so adopters writing those malformed
            // forms got no diagnostic and their intent vanished. Reject
            // them up front; the actionable error tells them which
            // shape the macro accepts.
            match nested {
                Meta::Path(path) if path.is_ident("protected") => {
                    return Err(syn::Error::new(
                        path.span(),
                        "`protected` must be invoked as `protected(sensitivity = \"...\", ...)`. \
                         Bare `protected` and name-value `protected = \"...\"` forms are not \
                         valid syntax for protected-field metadata.",
                    ));
                }
                Meta::NameValue(nv) if nv.path.is_ident("protected") => {
                    return Err(syn::Error::new(
                        nv.value.span(),
                        "`protected` must be invoked as `protected(sensitivity = \"...\", ...)`. \
                         Bare `protected` and name-value `protected = \"...\"` forms are not \
                         valid syntax for protected-field metadata.",
                    ));
                }
                _ => {}
            }
            let Meta::List(list) = nested else { continue };
            if !list.path.is_ident("protected") {
                continue;
            }
            if found.is_some() {
                return Err(syn::Error::new(
                    list.span(),
                    "duplicate `protected(...)` annotation on the same field; \
                     a prior `protected(...)` was already declared",
                ));
            }
            let spec = parse_protected_list(list)?;
            found = Some(spec);
        }
    }
    Ok(found)
}

fn parse_protected_list(list: &syn::MetaList) -> syn::Result<ProtectedSpec> {
    let entries: Punctuated<Meta, Token![,]> =
        list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

    let list_span = list.span();
    let mut sensitivity: Option<(SensitivityLit, Span)> = None;
    let mut rationale: Option<(String, Span)> = None;
    let mut redaction: Option<(RedactionLit, Span)> = None;
    let mut codec: Option<(String, Span)> = None;
    let mut retention: Option<(RetentionLit, Span)> = None;

    for meta in &entries {
        let Meta::NameValue(MetaNameValue {
            path,
            value:
                Expr::Lit(ExprLit {
                    lit: Lit::Str(lit_str),
                    ..
                }),
            ..
        }) = meta
        else {
            return Err(syn::Error::new(
                meta.span(),
                "every `protected(...)` entry must be `key = \"value\"` with a \
                 string literal; supported keys are `sensitivity`, `rationale`, \
                 `redaction`, `codec`, `retention`",
            ));
        };
        let Some(key) = path.get_ident().map(|i| i.to_string()) else {
            return Err(syn::Error::new(
                path.span(),
                "expected a bare identifier key in `protected(...)`",
            ));
        };
        let value = lit_str.value();
        let lit_span = lit_str.span();
        match key.as_str() {
            "sensitivity" => {
                if sensitivity.is_some() {
                    return Err(syn::Error::new(
                        path.span(),
                        "`sensitivity` declared twice in the same `protected(...)`",
                    ));
                }
                sensitivity = Some((SensitivityLit::parse(&value, lit_span)?, lit_span));
            }
            "rationale" => {
                if rationale.is_some() {
                    return Err(syn::Error::new(
                        path.span(),
                        "`rationale` declared twice in the same `protected(...)`",
                    ));
                }
                rationale = Some((value, lit_span));
            }
            "redaction" => {
                if redaction.is_some() {
                    return Err(syn::Error::new(
                        path.span(),
                        "`redaction` declared twice in the same `protected(...)`",
                    ));
                }
                redaction = Some((RedactionLit::parse(&value, lit_span)?, lit_span));
            }
            "codec" => {
                if codec.is_some() {
                    return Err(syn::Error::new(
                        path.span(),
                        "`codec` declared twice in the same `protected(...)`",
                    ));
                }
                codec = Some((value, lit_span));
            }
            "retention" => {
                if retention.is_some() {
                    return Err(syn::Error::new(
                        path.span(),
                        "`retention` declared twice in the same `protected(...)`",
                    ));
                }
                retention = Some((RetentionLit::parse(&value, lit_span)?, lit_span));
            }
            other => {
                return Err(syn::Error::new(
                    path.span(),
                    format!(
                        "unknown `protected` key `{other}`; expected one of: \
                         sensitivity, rationale, redaction, codec, retention",
                    ),
                ));
            }
        }
    }

    let Some((sensitivity_lit, sensitivity_span)) = sensitivity else {
        return Err(syn::Error::new(
            list_span,
            "`protected(...)` requires `sensitivity = \"...\"`; \
             expected one of: none, internal, pii, sensitive, secret",
        ));
    };

    Ok(ProtectedSpec {
        sensitivity: sensitivity_lit,
        sensitivity_span,
        rationale: rationale.as_ref().map(|(s, _)| s.clone()),
        rationale_span: rationale.as_ref().map(|(_, sp)| *sp),
        redaction: redaction.map(|(r, _)| r).unwrap_or(RedactionLit::None),
        redaction_span: redaction.as_ref().map(|(_, sp)| *sp),
        codec: codec.as_ref().map(|(s, _)| s.clone()),
        codec_span: codec.as_ref().map(|(_, sp)| *sp),
        retention: retention
            .as_ref()
            .map(|(r, _)| *r)
            .unwrap_or(RetentionLit::Standard),
        retention_span: retention.as_ref().map(|(_, sp)| *sp),
        list_span,
    })
}

/// Run the four cross-key validation rules from §6 of the Phase 7.5
/// v3 plan. Each rule emits a `syn::Error` carrying the most useful
/// span for the violation (e.g. the bad codec literal, not the whole
/// attribute).
pub fn validate(spec: &ProtectedSpec, field: &syn::Field) -> syn::Result<()> {
    // Rule (a): `sensitivity = "none"` is the explicit "ordinary
    // field" assertion and cannot be combined with any other knob.
    // Discrimination is by per-key *presence* (span-tracked), not
    // value comparison: an explicit `redaction = "none"` is still a
    // user-written extra knob even though the resulting value is the
    // neutral default. Anchoring the caret at the first non-sensitivity
    // span the user wrote gives them a "drop this key or raise
    // sensitivity" pointer instead of a generic complaint about the
    // sensitivity literal itself.
    if spec.sensitivity == SensitivityLit::None {
        let first_extra_span = spec
            .rationale_span
            .or(spec.redaction_span)
            .or(spec.codec_span)
            .or(spec.retention_span);
        if let Some(span) = first_extra_span {
            return Err(syn::Error::new(
                span,
                "`sensitivity = \"none\"` cannot be combined with other \
                 protected-field metadata (rationale / redaction / codec / \
                 retention). Either drop the `protected(...)` attribute \
                 entirely or set `sensitivity` higher.",
            ));
        }
    }

    // Rule (b): elevated sensitivity requires a non-empty rationale.
    // The rationale is the audit trail's primary signal — empty or
    // missing rationale defeats the entire point of the annotation.
    if spec.sensitivity != SensitivityLit::None {
        let rationale_ok = spec
            .rationale
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !rationale_ok {
            // Prefer the rationale literal's span when present (e.g.
            // empty string); fall back to the sensitivity literal so
            // the missing-rationale error still has a useful caret.
            let span = spec.rationale_span.unwrap_or(spec.sensitivity_span);
            return Err(syn::Error::new(
                span,
                "`protected(sensitivity = ...)` requires a non-empty \
                 `rationale = \"...\"` when sensitivity is above `none`. \
                 Per §6 the rationale documents the legitimate basis \
                 (e.g., GDPR article reference).",
            ));
        }
    }

    // Rule (c): codec ID must be in the compile-time registry. The
    // registry is currently empty; the diagnostic states "(none)" so
    // adopters know codec support has not yet shipped.
    if let Some(id) = spec.codec.as_deref()
        && !is_known_codec(id)
    {
        let valid = if KNOWN_CODEC_IDS.is_empty() {
            "(none). The registry will be populated in future \
             phases — codecs ship with the framework, not adopter \
             code."
                .to_string()
        } else {
            KNOWN_CODEC_IDS.join(", ")
        };
        let span = spec.codec_span.unwrap_or(spec.list_span);
        return Err(syn::Error::new(
            span,
            format!(
                "unregistered codec ID `{id}`. Valid codec IDs in \
                 this build of Djogi: {valid}",
            ),
        ));
    }

    // Rule (d): `redaction = "hash_id"` requires a HeerId / RanjId /
    // custom-PK-compatible field type. The check is conservative —
    // any unrecognised type rejects, even when the underlying SQL
    // shape could in principle support hashing, because a wrong
    // accept here ships an unsafe redaction policy at runtime.
    if spec.redaction == RedactionLit::HashId && !is_heerid_compatible_type(&field.ty) {
        let field_name = field
            .ident
            .as_ref()
            .map(|i| i.to_string())
            .unwrap_or_else(|| "<anonymous>".to_string());
        // Render only the type tokens — `quote!(#field)` would
        // include the field's attributes / visibility, which clutters
        // the diagnostic. The type alone is the load-bearing detail
        // for adopters figuring out why the rule rejected.
        let ty = &field.ty;
        let rust_type = quote::quote!(#ty).to_string();
        let span = spec.redaction_span.unwrap_or(spec.list_span);
        return Err(syn::Error::new(
            span,
            format!(
                "`redaction = \"hash_id\"` is only valid on fields whose \
                 stored type is `HeerId`, `RanjId`, or a custom-PK type. \
                 Field `{field_name}` has type `{rust_type}` which is not \
                 a HeerId-compatible type.",
            ),
        ));
    }

    Ok(())
}

/// `true` when `ty` is one of the HeerId-compatible PK / FK ident
/// shapes the framework recognises.
///
/// Recognises the bare ident, every `djogi::*` / `djogi::types::*`
/// fully-qualified path, and one layer of `Option<...>`. Custom-PK
/// types declared via `djogi::primary_key!` carry an arbitrary user
/// ident (e.g. `MyAccountId`); the macro cannot prove a custom ident
/// implements `PrimaryKey` at attribute-parse time, so this checker
/// recognises the heeranjid-derived names only and leaves any further
/// "looks like a custom PK" inference to a later phase that has access
/// to the full descriptor pass. A conservative recognise-set keeps the
/// rejection actionable — a wrong accept ships an unsafe redaction
/// policy at runtime.
fn is_heerid_compatible_type(ty: &syn::Type) -> bool {
    let inner = match ty {
        syn::Type::Path(syn::TypePath { qself: None, path }) => {
            // Strip a single `Option<T>` wrapper so nullable PK columns
            // pass the check too. `Option<HeerId>` in user code is rare
            // (the framework's injected `id` is non-nullable), but FK
            // columns spelled `Option<ForeignKey<…>>` reach this check
            // through different attribute paths and could legitimately
            // carry a HeerId-shaped stored type.
            if let Some(seg) = path.segments.last()
                && seg.ident == "Option"
                && let syn::PathArguments::AngleBracketed(args) = &seg.arguments
                && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
            {
                inner.clone()
            } else {
                ty.clone()
            }
        }
        _ => return false,
    };
    let syn::Type::Path(syn::TypePath { qself: None, path }) = &inner else {
        return false;
    };
    let Some(last) = path.segments.last() else {
        return false;
    };
    matches!(
        last.ident.to_string().as_str(),
        "HeerId"
            | "HeerIdDesc"
            | "HeerIdRecencyBiased"
            | "RanjId"
            | "RanjIdDesc"
            | "RanjIdRecencyBiased"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn field(input: proc_macro2::TokenStream) -> syn::Field {
        let parsed: syn::ItemStruct = syn::parse2(quote! {
            struct Wrapper {
                #input
            }
        })
        .expect("failed to parse wrapper struct");
        match parsed.fields {
            syn::Fields::Named(named) => named.named.into_iter().next().expect("one field"),
            _ => panic!("expected named field"),
        }
    }

    #[test]
    fn known_codec_ids_are_sorted() {
        // Binary search relies on sorted input; assert the invariant
        // so a future addition cannot silently land out of order and
        // break lookups.
        let mut sorted = KNOWN_CODEC_IDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, KNOWN_CODEC_IDS.to_vec());
    }

    #[test]
    fn empty_registry_rejects_every_codec_id() {
        assert!(!is_known_codec("aes256_gcm_v1"));
        assert!(!is_known_codec(""));
    }

    #[test]
    fn parse_minimal_protected_attr() {
        let f = field(quote! {
            #[field(protected(sensitivity = "none"))]
            pub note: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert_eq!(spec.sensitivity, SensitivityLit::None);
        assert!(spec.rationale.is_none());
        assert_eq!(spec.redaction, RedactionLit::None);
        assert!(spec.codec.is_none());
        assert_eq!(spec.retention, RetentionLit::Standard);
    }

    #[test]
    fn parse_full_protected_attr_round_trips_via_to_tokens() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GDPR Art. 6(1)(b)",
                redaction = "mask",
                retention = "extended"
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        validate(&spec, &f).expect("valid");
        let tokens = spec.to_tokens().to_string();
        assert!(tokens.contains("Sensitivity :: Pii"));
        assert!(tokens.contains("RedactionPolicy :: Mask"));
        assert!(tokens.contains("RetentionLabel :: Extended"));
        // The rationale is rendered as a string literal; quote!'s
        // formatting collapses internal whitespace via display so the
        // exact byte sequence depends on the formatter. Asserting the
        // distinguishing substring keeps the test stable across
        // proc_macro2 spacing changes.
        assert!(tokens.contains("GDPR"), "got: {tokens}");
    }

    #[test]
    fn no_attribute_returns_none() {
        let f = field(quote! {
            pub name: String,
        });
        assert!(parse_from_field(&f).expect("parse").is_none());
    }

    #[test]
    fn rule_a_sensitivity_none_with_other_knob_rejects() {
        let f = field(quote! {
            #[field(protected(sensitivity = "none", redaction = "mask"))]
            pub note: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let err = validate(&spec, &f).expect_err("rule (a)");
        let msg = err.to_string();
        assert!(msg.contains("sensitivity = \"none\""), "got: {msg}");
        assert!(msg.contains("cannot be combined"), "got: {msg}");
    }

    #[test]
    fn rule_a_rejects_explicit_neutral_redaction_alongside_sensitivity_none() {
        // The user wrote `redaction = "none"` explicitly. Even though
        // the resulting `RedactionLit` value equals the neutral default,
        // rule (a) treats the *presence* of the key as a contradiction
        // — so the macro must reject this.
        let f = field(quote! {
            #[field(protected(sensitivity = "none", redaction = "none"))]
            pub note: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert!(
            spec.redaction_span.is_some(),
            "redaction_span must be populated when `redaction = ...` was written",
        );
        let err = validate(&spec, &f).expect_err("rule (a) presence check");
        let msg = err.to_string();
        assert!(msg.contains("cannot be combined"), "got: {msg}");
    }

    #[test]
    fn rule_a_rejects_explicit_neutral_retention_alongside_sensitivity_none() {
        // Same shape as the redaction case but for `retention =
        // "standard"` — the previous value-comparison gate accepted
        // this silently. Span-presence rejects it correctly.
        let f = field(quote! {
            #[field(protected(sensitivity = "none", retention = "standard"))]
            pub note: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert!(
            spec.retention_span.is_some(),
            "retention_span must be populated when `retention = ...` was written",
        );
        let err = validate(&spec, &f).expect_err("rule (a) retention presence");
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn parse_from_field_rejects_bare_protected_path() {
        let f = field(quote! {
            #[field(protected)]
            pub note: String,
        });
        let err = parse_from_field(&f).expect_err("bare path form");
        let msg = err.to_string();
        assert!(msg.contains("must be invoked as `protected("), "got: {msg}");
    }

    #[test]
    fn parse_from_field_rejects_name_value_protected() {
        let f = field(quote! {
            #[field(protected = "pii")]
            pub note: String,
        });
        let err = parse_from_field(&f).expect_err("name-value form");
        let msg = err.to_string();
        assert!(msg.contains("must be invoked as `protected("), "got: {msg}");
    }

    #[test]
    fn rule_b_pii_without_rationale_rejects() {
        let f = field(quote! {
            #[field(protected(sensitivity = "pii"))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let err = validate(&spec, &f).expect_err("rule (b)");
        assert!(err.to_string().contains("rationale"));
    }

    #[test]
    fn rule_b_pii_with_empty_rationale_rejects() {
        let f = field(quote! {
            #[field(protected(sensitivity = "pii", rationale = "   "))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let err = validate(&spec, &f).expect_err("rule (b) empty");
        assert!(err.to_string().contains("rationale"));
    }

    #[test]
    fn rule_c_unknown_codec_rejects() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GDPR",
                codec = "aes256_gcm_v1"
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let err = validate(&spec, &f).expect_err("rule (c)");
        let msg = err.to_string();
        assert!(msg.contains("aes256_gcm_v1"), "got: {msg}");
        assert!(msg.contains("(none)"), "got: {msg}");
    }

    #[test]
    fn rule_d_hash_id_on_string_rejects() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GDPR",
                redaction = "hash_id"
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let err = validate(&spec, &f).expect_err("rule (d)");
        assert!(err.to_string().contains("hash_id"));
    }

    #[test]
    fn rule_d_hash_id_on_heerid_passes() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GDPR",
                redaction = "hash_id"
            ))]
            pub owner: HeerId,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        validate(&spec, &f).expect("HeerId is hash_id-compatible");
    }

    #[test]
    fn rule_d_hash_id_on_optional_ranjid_passes() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GDPR",
                redaction = "hash_id"
            ))]
            pub owner: Option<RanjId>,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        validate(&spec, &f).expect("Option<RanjId> is hash_id-compatible");
    }

    #[test]
    fn missing_sensitivity_rejects() {
        let f = field(quote! {
            #[field(protected(rationale = "no sensitivity given"))]
            pub email: String,
        });
        let err = parse_from_field(&f).expect_err("requires sensitivity");
        assert!(err.to_string().contains("sensitivity"));
    }

    #[test]
    fn unknown_protected_key_rejects() {
        let f = field(quote! {
            #[field(protected(sensitivity = "none", flavour = "vanilla"))]
            pub note: String,
        });
        let err = parse_from_field(&f).expect_err("unknown key");
        assert!(err.to_string().contains("flavour"));
    }

    #[test]
    fn duplicate_protected_attr_rejects() {
        let f: syn::Field = parse_quote! {
            #[field(protected(sensitivity = "none"))]
            #[field(protected(sensitivity = "pii", rationale = "x"))]
            pub note: String
        };
        let err = parse_from_field(&f).expect_err("duplicate");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn default_volatility_lit_parses_known_variants() {
        let span = Span::call_site();
        assert_eq!(
            DefaultVolatilityLit::parse("immutable", span).unwrap(),
            DefaultVolatilityLit::Immutable,
        );
        assert_eq!(
            DefaultVolatilityLit::parse("stable", span).unwrap(),
            DefaultVolatilityLit::Stable,
        );
        assert_eq!(
            DefaultVolatilityLit::parse("volatile", span).unwrap(),
            DefaultVolatilityLit::Volatile,
        );
    }

    #[test]
    fn default_volatility_lit_rejects_unknown_variant() {
        let err =
            DefaultVolatilityLit::parse("wibble", Span::call_site()).expect_err("unknown variant");
        let msg = err.to_string();
        assert!(msg.contains("wibble"));
        assert!(msg.contains("immutable"));
        assert!(msg.contains("stable"));
        assert!(msg.contains("volatile"));
    }

    #[test]
    fn default_volatility_lit_token_emission() {
        let imm = DefaultVolatilityLit::Immutable.to_tokens().to_string();
        let stb = DefaultVolatilityLit::Stable.to_tokens().to_string();
        let vol = DefaultVolatilityLit::Volatile.to_tokens().to_string();
        assert!(imm.contains("DefaultVolatility :: Immutable"));
        assert!(stb.contains("DefaultVolatility :: Stable"));
        assert!(vol.contains("DefaultVolatility :: Volatile"));
    }
}
