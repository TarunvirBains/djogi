//! Macro-side parsing and validation for `#[field(protected(...))]` and
//! `#[field(default_volatility = "...")]`.
//! Owns:
//! 1. **Parsing** the `protected(...)` nested attribute into a
//!    [`ProtectedSpec`] that the descriptor emitter consumes when
//!    building the `Option<ProtectedFieldMetadata>` literal.
//! 2. **Validating** the four rules from the v3 plan §6 with
//!    span-precise errors:
//! - `sensitivity = "none"` cannot be combined with any other
//!   protected field.
//! - `sensitivity > none` requires a non-empty `rationale`.
//! - `codec = "X"` must reference a registered codec ID.
//! - `redaction = "hash_id"` is only valid on a HeerId / RanjId /
//!   custom-PK-compatible field type.
//! 3. **Parsing and validating** the optional
//!    `#[field(default_volatility = "...")]` override into
//!    [`DefaultVolatilityLit`].
//! # Codec ID validation strategy
//! Proc macros run before any runtime dependency is available, so the
//! macro crate cannot read `djogi::field_codec::REGISTRY` directly.
//! Instead, [`KNOWN_CODEC_IDS`] mirrors the runtime `phf::Set` as a
//! sorted const slice. Validation uses `binary_search` for O(log n)
//! lookup — no regex, no runtime FFI, span-precise diagnostics. The
//! synchronization contract (update both lists when adding a codec)
//! is documented on the runtime `REGISTRY` static.

use proc_macro2::Span;
use quote::quote;
use syn::{
    Expr, ExprLit, Lit, Meta, MetaNameValue, Stmt, Token, punctuated::Punctuated, spanned::Spanned,
};

/// Sorted const slice of every codec ID that the macro recognises at
/// expansion time.
/// Kept in sync with `djogi::field_codec::REGISTRY` per the contract
/// documented on that static. Sorted so [`is_known_codec`] resolves via
/// `slice::binary_search`; matches the project's no-regex,
/// sorted-const-slice convention.
pub const KNOWN_CODEC_IDS: &[&str] = &["aes256_gcm_v1"];

/// `true` when `id` appears in [`KNOWN_CODEC_IDS`]. Resolved by
/// `binary_search` on the sorted slice — O(log n) with zero allocation.
pub fn is_known_codec(id: &str) -> bool {
    KNOWN_CODEC_IDS.binary_search(&id).is_ok()
}

/// Parsed `sensitivity = "..."` literal.
/// Mirrors `djogi::descriptor::Sensitivity` one-for-one. Stored as a
/// macro-side enum (rather than as the `String` literal) so the
/// emitter renders the exact `Sensitivity::Variant` ident without
/// re-parsing. The emitter calls
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

/// Per-scope presentation-codec declaration parsed from
/// `protected(per_scope = { scope = { presentation_codec = Path } })`
/// GH #227.
/// One entry exists per scope key declared inside a `per_scope = { ... }`
/// block. `fallible = false` selects the infallible
/// `PresentationCodec<Input>` dispatch path; `fallible = true` selects the
/// fallible `TryPresentationCodec<Input>` path (which surfaces
/// `VisageError::PresentationCodec` from the visage's `TryFrom<&Model>`
/// impl). The two are mutually exclusive within a single scope block
/// declaring both `presentation_codec` and `try_presentation_codec` on
/// the same scope is rejected at parse time.
/// Presentation codecs are runtime-only metadata: they do NOT flow into
/// `ProtectedFieldMetadata` or any other migration-differ surface, so
/// changing a codec is never a schema event. The macro lowers per-scope
/// codecs into visage codegen + `inventory::submit!` records consumed by
/// startup validation.
#[derive(Debug, Clone)]
pub struct PerScopeCodecEntry {
    /// Scope key (e.g. `"public"`, `"self_view"`, or a custom scope
    /// declared via `#[model(visage_scopes(...))]`).
    pub scope: String,
    /// Span of the scope ident literal in the user's source, used to
    /// anchor downstream diagnostics (e.g. "scope `support` is not in
    /// `visage_scopes(...)`") at the offending key rather than the
    /// whole `per_scope = { ... }` block.
    pub scope_span: Span,
    /// Rust type path of the codec — typically
    /// `djogi::presentation::builtins::MaskString` or an adopter type.
    /// Routed verbatim into the emitted projection code, so the path
    /// must resolve at the use site.
    pub codec_type: syn::Path,
    /// `true` when the entry was declared as `try_presentation_codec`
    /// (selects `TryPresentationCodec<Input>` dispatch); `false` for
    /// `presentation_codec` (selects `PresentationCodec<Input>`).
    pub fallible: bool,
}

/// Parsed `#[field(protected(...))]` annotation.
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
    /// Per-scope presentation codec entries parsed from
    /// `per_scope = { scope = { (try_)?presentation_codec = Path } }`
    /// GH #227. Empty when the user did not write a `per_scope`
    /// block. Source order is preserved so downstream emission is
    /// deterministic.
    pub per_scope: Vec<PerScopeCodecEntry>,
    /// `Some(span)` when the user wrote a `per_scope = { ... }` block
    /// the span anchors rule (a) "sensitivity = none cannot be
    /// combined with any other knob" so the diagnostic points at the
    /// `per_scope` key rather than the unrelated keys (rationale /
    /// redaction / codec / retention).
    pub per_scope_span: Option<Span>,
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
    /// The [`Self::per_scope`] field is intentionally NOT lowered into
    /// the descriptor — presentation codecs are runtime-only metadata
    /// (consumed by visage codegen + `inventory` startup validation)
    /// and do not influence SQL DDL. Including them in the migration
    /// differ's descriptor surface would erroneously trigger schema-
    /// drift warnings when an adopter swaps codecs; the visage emitter
    /// reads `per_scope` directly off the `ProtectedSpec`.
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
/// Multiple `protected(...)` annotations on the same field are
/// rejected — the error span lands on the second occurrence. Returns
/// `Ok(None)` when no `protected(...)` is present (the common case).
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
    let mut per_scope: Vec<PerScopeCodecEntry> = Vec::new();
    let mut per_scope_span: Option<Span> = None;

    for meta in &entries {
        // GH #227 — `per_scope = { scope = { codec_key = Path } }`
        // arrives as `Meta::NameValue { value: Expr::Block { ... } }`, which
        // does NOT match the string-literal let-else below. Handle it first
        // so the generic "every entry must be `key = \"value\"`" rejection
        // does not swallow this shape with a misleading diagnostic.
        if let Meta::NameValue(MetaNameValue {
            path,
            value: Expr::Block(expr_block),
            ..
        }) = meta
            && path.is_ident("per_scope")
        {
            if per_scope_span.is_some() {
                return Err(syn::Error::new(
                    path.span(),
                    "`per_scope` declared twice in the same `protected(...)`",
                ));
            }
            per_scope_span = Some(path.span());
            per_scope = parse_per_scope_block(&expr_block.block)?;
            continue;
        }

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
                 `redaction`, `codec`, `retention`, `per_scope`",
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
            "per_scope" => {
                // `per_scope` requires the `{ ... }` block-expression
                // form (handled above). Reach this arm only when the
                // user wrote `per_scope = "<string>"` or similar
                // surface a dedicated diagnostic instead of the
                // generic unknown-key error so the migration message
                // is actionable.
                return Err(syn::Error::new(
                    path.span(),
                    "`per_scope` requires a `{ scope = { presentation_codec = Path } }` \
                     block expression, not a string literal. See GH #227 — the \
                     per-scope codec grammar uses a nested block to declare scope \
                     entries.",
                ));
            }
            other => {
                return Err(syn::Error::new(
                    path.span(),
                    format!(
                        "unknown `protected` key `{other}`; expected one of: \
                         sensitivity, rationale, redaction, codec, retention, per_scope",
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
        per_scope,
        per_scope_span,
        list_span,
    })
}

/// Parse the `{ scope = { codec_key = Path } }` block that follows
/// `per_scope = ` inside a `protected(...)` annotation — GH #227.
/// The block arrives as `syn::Block` whose statements are each an
/// assignment expression: `scope_ident = { codec_key = codec_path }`.
/// Each statement is parsed into one [`PerScopeCodecEntry`]; duplicate
/// scope keys are rejected here so the diagnostic anchors at the second
/// occurrence rather than waiting for the visage emitter to see a
/// duplicate.
/// Grammar (one statement):
/// ```text
/// scope_ident = {
///     (presentation_codec | try_presentation_codec) = SomeCodec::Path,
/// }
/// ```
/// Both `presentation_codec` (infallible) and `try_presentation_codec`
/// (fallible) are accepted; declaring both inside the same scope block
/// is rejected so the emitter does not have to disambiguate. The codec
/// path is captured verbatim and routed through the visage emitter; the
/// emitter validates that the path resolves to a type implementing
/// `PresentationCodec<FieldTy>` / `TryPresentationCodec<FieldTy>` via
/// trait bounds in the generated init expression.
fn parse_per_scope_block(block: &syn::Block) -> syn::Result<Vec<PerScopeCodecEntry>> {
    let mut entries: Vec<PerScopeCodecEntry> = Vec::new();
    for stmt in &block.stmts {
        // Each statement must be an expression statement carrying an
        // assignment expression (no `;` needed inside the user's block
        // syn lowers both shapes to `Stmt::Expr(..., None)`).
        let outer_expr = match stmt {
            Stmt::Expr(expr, _) => expr,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "every `per_scope = { ... }` statement must be a \
                     `scope_ident = { codec_key = Path }` assignment expression",
                ));
            }
        };

        let Expr::Assign(outer_assign) = outer_expr else {
            return Err(syn::Error::new(
                outer_expr.span(),
                "every `per_scope = { ... }` entry must be a \
                 `scope_ident = { codec_key = Path }` assignment expression",
            ));
        };

        // Left-hand side: `scope_ident`. Must be a bare ident path so
        // downstream emission can stash the scope key verbatim.
        let Expr::Path(scope_path_expr) = outer_assign.left.as_ref() else {
            return Err(syn::Error::new(
                outer_assign.left.span(),
                "`per_scope` entries must start with a bare scope ident — \
                 write `support = { ... }`, not a path / literal / etc.",
            ));
        };
        let Some(scope_ident) = scope_path_expr.path.get_ident() else {
            return Err(syn::Error::new(
                scope_path_expr.path.span(),
                "`per_scope` scope name must be a single-segment ident",
            ));
        };
        let scope_name = scope_ident.to_string();
        let scope_span = scope_ident.span();

        if entries.iter().any(|e| e.scope == scope_name) {
            return Err(syn::Error::new(
                scope_span,
                format!(
                    "scope `{scope_name}` declared twice inside the same \
                     `per_scope = {{ ... }}` block",
                ),
            ));
        }

        // Right-hand side: an inner block `{ codec_key = Path }`.
        let Expr::Block(inner_block_expr) = outer_assign.right.as_ref() else {
            return Err(syn::Error::new(
                outer_assign.right.span(),
                "`per_scope` entry value must be a `{ codec_key = Path }` \
                 block expression",
            ));
        };

        let entry = parse_per_scope_inner_block(&inner_block_expr.block, scope_name, scope_span)?;
        entries.push(entry);
    }

    Ok(entries)
}

/// Parse the inner `{ presentation_codec = Path }` block for a single
/// scope entry inside `per_scope = { ... }`.
/// Accepts exactly one of `presentation_codec` or `try_presentation_codec`.
/// Declaring both keys within the same inner block is rejected; future
/// extensions (e.g. a queryability-override key) slot in alongside these
/// two without reshaping the per-scope grammar.
fn parse_per_scope_inner_block(
    inner_block: &syn::Block,
    scope_name: String,
    scope_span: Span,
) -> syn::Result<PerScopeCodecEntry> {
    let mut codec_decl: Option<(syn::Path, bool, Span)> = None;
    for stmt in &inner_block.stmts {
        let inner_expr = match stmt {
            Stmt::Expr(expr, _) => expr,
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "every per-scope codec entry must be a \
                     `(try_)?presentation_codec = Path` assignment expression",
                ));
            }
        };
        let Expr::Assign(inner_assign) = inner_expr else {
            return Err(syn::Error::new(
                inner_expr.span(),
                "every per-scope codec entry must be a \
                 `(try_)?presentation_codec = Path` assignment expression",
            ));
        };
        let Expr::Path(key_path_expr) = inner_assign.left.as_ref() else {
            return Err(syn::Error::new(
                inner_assign.left.span(),
                "per-scope codec key must be `presentation_codec` or \
                 `try_presentation_codec`",
            ));
        };
        let Some(key_ident) = key_path_expr.path.get_ident() else {
            return Err(syn::Error::new(
                key_path_expr.path.span(),
                "per-scope codec key must be a single-segment ident — \
                 `presentation_codec` or `try_presentation_codec`",
            ));
        };
        let key_name = key_ident.to_string();
        let fallible = match key_name.as_str() {
            "presentation_codec" => false,
            "try_presentation_codec" => true,
            other => {
                return Err(syn::Error::new(
                    key_ident.span(),
                    format!(
                        "unknown per-scope codec key `{other}`; expected \
                         `presentation_codec` (infallible) or \
                         `try_presentation_codec` (fallible)",
                    ),
                ));
            }
        };

        let Expr::Path(codec_path_expr) = inner_assign.right.as_ref() else {
            return Err(syn::Error::new(
                inner_assign.right.span(),
                "per-scope codec value must be a Rust type path — \
                 e.g. `djogi::presentation::builtins::MaskString`",
            ));
        };
        let codec_path = codec_path_expr.path.clone();

        if let Some((_, prev_fallible, prev_span)) = codec_decl {
            let prev_key = if prev_fallible {
                "try_presentation_codec"
            } else {
                "presentation_codec"
            };
            let _ = prev_span;
            return Err(syn::Error::new(
                key_ident.span(),
                format!(
                    "scope `{scope_name}` already declared `{prev_key}`; \
                     each scope block accepts exactly one codec key",
                ),
            ));
        }
        codec_decl = Some((codec_path, fallible, key_ident.span()));
    }

    let Some((codec_type, fallible, _)) = codec_decl else {
        return Err(syn::Error::new(
            scope_span,
            format!(
                "scope `{scope_name}` declares no codec; write \
                 `{scope_name} = {{ presentation_codec = SomeCodec }}` \
                 (infallible) or \
                 `{scope_name} = {{ try_presentation_codec = SomeCodec }}` \
                 (fallible)",
            ),
        ));
    };

    Ok(PerScopeCodecEntry {
        scope: scope_name,
        scope_span,
        codec_type,
        fallible,
    })
}

/// Run the four cross-key validation rules from §6 of the
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
            .or(spec.retention_span)
            .or(spec.per_scope_span);
        if let Some(span) = first_extra_span {
            return Err(syn::Error::new(
                span,
                "`sensitivity = \"none\"` cannot be combined with other \
                 protected-field metadata (rationale / redaction / codec / \
                 retention / per_scope). Either drop the `protected(...)` \
                 attribute entirely or set `sensitivity` higher.",
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

    // Rule (c): codec ID must be in the compile-time registry.
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
    // custom-PK-compatible field type. The check is conservative
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
                 stored type is `HeerId`, `RanjId`, or one of their family \
                 aliases (`HeerIdDesc` / `HeerIdRecencyBiased` / \
                 `RanjIdDesc` / `RanjIdRecencyBiased`). Field \
                 `{field_name}` has type `{rust_type}` which is not a \
                 HeerId/RanjId-compatible type. Custom-PK newtypes \
                 declared via `djogi::primary_key!` are not yet \
                 accepted by this rule — the macro cannot prove a \
                 user-named ident implements `PrimaryKey` at parse \
                 time, and a wrong accept ships an unsafe redaction \
                 policy at runtime.",
            ),
        ));
    }

    Ok(())
}

/// `true` when `ty` is one of the HeerId-compatible PK / FK ident
/// shapes the framework recognises.
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
        // so the macro must reject this.
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

    // ─────────────────────────────────────────────────────────────────
    // GH #227 — `per_scope = { ... }` presentation-codec block
    // parser tests.
    // The visage codegen pass consumes [`ProtectedSpec::per_scope`]
    // directly off the field's parsed spec; these tests cover the
    // attribute-parse surface only (no expanded codegen).
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn per_scope_single_infallible_codec_parses() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — codec parser smoke test",
                per_scope = {
                    public = {
                        presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert_eq!(spec.per_scope.len(), 1, "exactly one scope entry");
        let entry = &spec.per_scope[0];
        assert_eq!(entry.scope, "public");
        assert!(!entry.fallible, "presentation_codec is infallible");
        // Codec path renders into the same token-string the visage
        // emitter consumes — assert on the segment join rather than
        // the raw `quote!` whitespace.
        let codec_str = entry
            .codec_type
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        assert_eq!(codec_str, "djogi::presentation::builtins::MaskString");
        // The per_scope_span is populated, so rule (a) anchors at this
        // key when combined with `sensitivity = "none"`.
        assert!(spec.per_scope_span.is_some());
    }

    #[test]
    fn per_scope_try_presentation_codec_marks_fallible() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — fallible codec smoke test",
                per_scope = {
                    public = {
                        try_presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub phone: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert_eq!(spec.per_scope.len(), 1);
        assert_eq!(spec.per_scope[0].scope, "public");
        assert!(
            spec.per_scope[0].fallible,
            "try_presentation_codec selects the fallible dispatch"
        );
    }

    #[test]
    fn per_scope_rejects_unknown_codec_key() {
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — unknown codec key",
                per_scope = {
                    public = {
                        encrypted = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let err = parse_from_field(&f).expect_err("unknown codec key");
        let msg = err.to_string();
        assert!(msg.contains("encrypted"), "got: {msg}");
        assert!(msg.contains("presentation_codec"), "got: {msg}");
        assert!(msg.contains("try_presentation_codec"), "got: {msg}");
    }

    #[test]
    fn per_scope_rejects_duplicate_scope() {
        // Two entries for the same `public` scope inside one
        // per_scope block — the second must surface as a parse-time
        // error so the diagnostic anchors at the duplicate ident.
        // The `per_scope = { ... }` body parses as a Rust block, so
        // statements separate via `;` (not `,`); the trailing entry
        // omits the separator.
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — duplicate scope",
                per_scope = {
                    public = {
                        presentation_codec = djogi::presentation::builtins::MaskString
                    };
                    public = {
                        try_presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let err = parse_from_field(&f).expect_err("duplicate scope");
        let msg = err.to_string();
        assert!(msg.contains("public"), "got: {msg}");
        assert!(msg.contains("declared twice"), "got: {msg}");
    }

    #[test]
    fn per_scope_rejects_both_codec_keys_in_same_scope_block() {
        // Same `;`-separator convention applies inside the inner
        // codec block — the user writes the second key after a
        // semicolon to express "and also try_presentation_codec".
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — both codec keys",
                per_scope = {
                    public = {
                        presentation_codec = djogi::presentation::builtins::MaskString;
                        try_presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let err = parse_from_field(&f).expect_err("both codec keys");
        let msg = err.to_string();
        assert!(msg.contains("public"), "got: {msg}");
    }

    #[test]
    fn rule_a_rejects_per_scope_alongside_sensitivity_none() {
        // The per_scope key carries its own span; rule (a) must pick
        // it up alongside the other "extra knob" spans so the
        // diagnostic anchors at the offending block.
        let f = field(quote! {
            #[field(protected(
                sensitivity = "none",
                per_scope = {
                    public = {
                        presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        assert!(spec.per_scope_span.is_some());
        let err = validate(&spec, &f).expect_err("rule (a) per_scope");
        let msg = err.to_string();
        assert!(msg.contains("cannot be combined"), "got: {msg}");
        assert!(msg.contains("per_scope"), "got: {msg}");
    }

    #[test]
    fn per_scope_to_tokens_does_not_emit_into_descriptor() {
        // Presentation codecs are runtime-only metadata — they must
        // not flow into `ProtectedFieldMetadata`. Assert the emitted
        // token stream does NOT mention `per_scope` / codec paths so
        // the migration differ stays isolated from runtime codec
        // changes.
        let f = field(quote! {
            #[field(protected(
                sensitivity = "pii",
                rationale = "GH #227 — descriptor isolation",
                per_scope = {
                    public = {
                        presentation_codec = djogi::presentation::builtins::MaskString
                    }
                }
            ))]
            pub email: String,
        });
        let spec = parse_from_field(&f).expect("parse").expect("present");
        let tokens = spec.to_tokens().to_string();
        assert!(
            !tokens.contains("per_scope"),
            "per_scope must not leak into ProtectedFieldMetadata; got: {tokens}"
        );
        assert!(
            !tokens.contains("MaskString"),
            "codec path must not leak into ProtectedFieldMetadata; got: {tokens}"
        );
    }
}
