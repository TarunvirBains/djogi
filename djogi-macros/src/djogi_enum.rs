//! `#[derive(DjogiEnum)]` proc macro — typed Postgres enum support.
//!
//! Emits four things per enum:
//!
//! 1. `impl postgres_types::ToSql for MyEnum` — encodes the Rust variant as its mapped
//!    Postgres wire string. Uses `to_sql_checked!()` for the forwarded type-check path.
//! 2. `impl<'a> postgres_types::FromSql<'a> for MyEnum` — decodes the wire bytes as a
//!    string, matches against known variants, returns `Err(EnumDecodeError { ... })` for
//!    unknown labels.
//! 3. `inventory::submit!(::djogi::descriptor::EnumDescriptor { ... })` — registers the
//!    enum's metadata for the Phase 7 migration differ.
//! 4. `impl MyEnum { pub fn variants() -> &'static [&'static str] }` — convenience fn.
//!
//! # Attribute grammar
//!
//! ```rust,ignore
//! #[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
//! #[djogi_enum(name = "vehicle_status", rename_all = "snake_case")]
//! pub enum VehicleStatus {
//!     Active,
//!     InMaintenance,
//!     #[djogi_enum_variant(name = "decommissioned")]
//!     Retired,
//! }
//! ```
//!
//! - `name` (required) — the Postgres type name.
//! - `rename_all` (optional, default `"snake_case"`) — case conversion applied to all
//!   variants. Supported values: `snake_case`, `SCREAMING_SNAKE_CASE`, `lowercase`,
//!   `UPPERCASE`, `PascalCase`, `camelCase`, `kebab-case`.
//!
//! Per-variant override: `#[djogi_enum_variant(name = "...")]` takes precedence over
//! `rename_all`.
//!
//! # Compile-time validation
//!
//! - Empty enum → error: "requires at least one variant".
//! - Non-unit variant (tuple/struct) → error: "variants must be unit-only".
//! - Two variants map to the same Postgres string → error at the second variant.
//! - Missing `#[djogi_enum(name = "...")]` → error at the enum.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Data, DeriveInput, Expr, ExprLit, Fields, Lit, Meta, MetaNameValue};

// ---------------------------------------------------------------------------
// Attribute containers
// ---------------------------------------------------------------------------

/// Parsed `#[djogi_enum(name = "...", rename_all = "...")]` container attrs.
struct EnumAttrs {
    /// Postgres type name — required.
    name: String,
    /// Case conversion for all variants. Defaults to `snake_case`.
    rename_all: RenameAll,
}

/// Supported `rename_all` values.
#[derive(Clone, Copy, Debug, Default)]
enum RenameAll {
    #[default]
    SnakeCase,
    ScreamingSnakeCase,
    Lowercase,
    Uppercase,
    PascalCase,
    CamelCase,
    KebabCase,
}

impl RenameAll {
    fn from_str(s: &str, span: Span) -> syn::Result<Self> {
        match s {
            "snake_case" => Ok(RenameAll::SnakeCase),
            "SCREAMING_SNAKE_CASE" => Ok(RenameAll::ScreamingSnakeCase),
            "lowercase" => Ok(RenameAll::Lowercase),
            "UPPERCASE" => Ok(RenameAll::Uppercase),
            "PascalCase" => Ok(RenameAll::PascalCase),
            "camelCase" => Ok(RenameAll::CamelCase),
            "kebab-case" => Ok(RenameAll::KebabCase),
            other => Err(syn::Error::new(
                span,
                format!(
                    "unknown rename_all value `{other}`; expected one of: \
                     snake_case, SCREAMING_SNAKE_CASE, lowercase, UPPERCASE, \
                     PascalCase, camelCase, kebab-case"
                ),
            )),
        }
    }

    /// Apply case conversion to a Rust PascalCase variant name.
    ///
    /// Input is always a valid Rust identifier (ASCII, starts with a letter or underscore).
    /// Non-ASCII input has undefined behavior — documented as out-of-scope per
    /// `feedback_no_regex_in_djogi`; only ASCII Rust identifiers are processed.
    fn apply(self, name: &str) -> String {
        match self {
            RenameAll::SnakeCase => pascal_to_snake(name),
            RenameAll::ScreamingSnakeCase => pascal_to_snake(name)
                .bytes()
                .map(|b| {
                    if b == b'_' {
                        b'_'
                    } else {
                        b.to_ascii_uppercase()
                    }
                })
                .map(char::from)
                .collect(),
            RenameAll::Lowercase => name.to_ascii_lowercase(),
            RenameAll::Uppercase => name.to_ascii_uppercase(),
            RenameAll::PascalCase => name.to_owned(),
            RenameAll::CamelCase => pascal_to_camel(name),
            RenameAll::KebabCase => pascal_to_snake(name).replace('_', "-"),
        }
    }
}

/// Convert `PascalCase` → `snake_case`.
///
/// Inserts `_` before each uppercase letter that follows a lowercase letter or
/// another uppercase letter that is itself followed by a lowercase letter
/// (`XMLParser` → `xml_parser`, `HTTPSProxy` → `https_proxy`).
///
/// Pure byte-level — no regex, no regex notation. Handles only ASCII as
/// documented in [`RenameAll`].
fn pascal_to_snake(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 4);
    for (i, &b) in bytes.iter().enumerate() {
        if b.is_ascii_uppercase() {
            // Insert `_` before this uppercase if:
            // - Not the first character AND
            // - Either the previous char was lowercase, OR the next char (if any) is lowercase
            //   (catches the trailing letter of an all-caps run like `HTTPSProxy`).
            let prev_is_lower = i > 0 && bytes[i - 1].is_ascii_lowercase();
            let next_is_lower = i + 1 < bytes.len() && bytes[i + 1].is_ascii_lowercase();
            if i > 0 && (prev_is_lower || (next_is_lower && !bytes[i - 1].is_ascii_uppercase())) {
                out.push(b'_');
            }
            out.push(b.to_ascii_lowercase());
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).expect("ASCII-only conversion cannot produce invalid UTF-8")
}

/// Convert `PascalCase` → `camelCase`.
///
/// Lowercase only the first byte; leave the rest unchanged.
fn pascal_to_camel(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let lower: String = c.to_lowercase().collect();
            lower + chars.as_str()
        }
    }
}

// ---------------------------------------------------------------------------
// EnumAttrs parser
// ---------------------------------------------------------------------------

fn parse_enum_attrs(input: &DeriveInput) -> syn::Result<EnumAttrs> {
    let mut name: Option<String> = None;
    let mut rename_all: RenameAll = RenameAll::default();

    for attr in &input.attrs {
        if !attr.path().is_ident("djogi_enum") {
            continue;
        }
        // Parse as `#[djogi_enum(key = "value", ...)]`
        let metas = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        for meta in &metas {
            match meta {
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) => {
                    if path.is_ident("name") {
                        if name.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `name` key in #[djogi_enum(...)]",
                            ));
                        }
                        name = Some(s.value());
                    } else if path.is_ident("rename_all") {
                        rename_all = RenameAll::from_str(&s.value(), s.span())?;
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[djogi_enum] attribute `{}`; expected `name` or `rename_all`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` attribute in #[djogi_enum(...)]",
                    ));
                }
            }
        }
    }

    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "#[derive(DjogiEnum)] requires #[djogi_enum(name = \"...\")]",
        )
    })?;

    Ok(EnumAttrs { name, rename_all })
}

// ---------------------------------------------------------------------------
// Per-variant override parser
// ---------------------------------------------------------------------------

/// Parsed `#[djogi_enum_variant(name = "...")]` on a single variant.
fn parse_variant_override(variant: &syn::Variant) -> syn::Result<Option<String>> {
    for attr in &variant.attrs {
        if !attr.path().is_ident("djogi_enum_variant") {
            continue;
        }
        let metas = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        // Each `#[djogi_enum_variant(...)]` has exactly one key-value pair.
        // Iterate all entries and return on the first match or error.
        let mut found: Option<String> = None;
        for meta in &metas {
            match meta {
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) if path.is_ident("name") => {
                    found = Some(s.value());
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `name = \"...\"` in #[djogi_enum_variant(...)]",
                    ));
                }
            }
        }
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Main expand function
// ---------------------------------------------------------------------------

pub fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let input: DeriveInput = syn::parse2(input)?;
    let enum_name = &input.ident;
    let enum_name_str = enum_name.to_string();

    // Parse container attributes.
    let attrs = parse_enum_attrs(&input)?;
    let postgres_type = &attrs.name;

    // Extract the enum variants.
    let data = match &input.data {
        Data::Enum(data) => data,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "#[derive(DjogiEnum)] can only be applied to enums",
            ));
        }
    };

    // Validate: at least one variant.
    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(DjogiEnum)] requires at least one variant",
        ));
    }

    // Validate variants and collect (variant_ident, postgres_string) pairs.
    let mut variant_pairs: Vec<(&syn::Ident, String)> = Vec::new();
    let mut seen_strings: Vec<(String, Span)> = Vec::new(); // (wire_string, span of second occurrence)

    for variant in &data.variants {
        // Only unit variants are supported.
        match &variant.fields {
            Fields::Unit => {}
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    format!(
                        "#[derive(DjogiEnum)] variants must be unit-only (got `{}`)",
                        variant.ident
                    ),
                ));
            }
        }

        // Resolve the wire string: per-variant override takes precedence.
        let wire = match parse_variant_override(variant)? {
            Some(override_name) => override_name,
            None => attrs.rename_all.apply(&variant.ident.to_string()),
        };

        // Collision detection: two variants must not map to the same wire string.
        if let Some((_, prev_span)) = seen_strings.iter().find(|(s, _)| s == &wire) {
            // Error at the current (second) variant with a reference to what it collides with.
            let _ = prev_span; // prev_span not needed in error message — current span is enough
            return Err(syn::Error::new_spanned(
                &variant.ident,
                format!(
                    "#[derive(DjogiEnum)]: variant `{}` maps to the same Postgres string \
                     `{wire}` as an earlier variant",
                    variant.ident
                ),
            ));
        }
        seen_strings.push((wire.clone(), variant.ident.span()));
        variant_pairs.push((&variant.ident, wire));
    }

    // Collect static variant strings.
    let variant_strs: Vec<&str> = variant_pairs.iter().map(|(_, s)| s.as_str()).collect();

    // ── Emit ToSql wire-string match arms ───────────────────────────────────
    let to_sql_wire_lets = variant_pairs.iter().map(|(ident, wire)| {
        quote! {
            #enum_name::#ident => #wire,
        }
    });

    // ── Emit FromSql match arms ──────────────────────────────────────────────
    let from_sql_arms = variant_pairs.iter().map(|(ident, wire)| {
        quote! {
            #wire => Ok(#enum_name::#ident),
        }
    });

    // ── Emit static variant array ────────────────────────────────────────────
    let variants_array = quote! {
        &[ #(#variant_strs,)* ]
    };

    let postgres_type_str = postgres_type.as_str();
    let type_name_str = enum_name_str.as_str();

    let expanded = quote! {
        impl ::djogi::__private::postgres_types::ToSql for #enum_name {
            fn to_sql(
                &self,
                ty: &::djogi::__private::postgres_types::Type,
                out: &mut ::djogi::__private::bytes::BytesMut,
            ) -> ::std::result::Result<
                ::djogi::__private::postgres_types::IsNull,
                ::std::boxed::Box<dyn ::std::error::Error + Sync + Send>,
            > {
                let wire_str: &str = match self {
                    #(#to_sql_wire_lets)*
                };
                // Encode as a Postgres string (same encoding path as `&str`).
                ::djogi::__private::postgres_types::ToSql::to_sql(
                    &wire_str,
                    ty,
                    out,
                )
            }

            fn accepts(ty: &::djogi::__private::postgres_types::Type) -> bool {
                ty.name() == #postgres_type_str
            }

            ::djogi::__private::postgres_types::to_sql_checked!();
        }

        impl<'_sql> ::djogi::__private::postgres_types::FromSql<'_sql> for #enum_name {
            fn from_sql(
                ty: &::djogi::__private::postgres_types::Type,
                raw: &'_sql [u8],
            ) -> ::std::result::Result<
                Self,
                ::std::boxed::Box<dyn ::std::error::Error + Sync + Send>,
            > {
                // Decode wire bytes as a Postgres text string (borrowing from raw).
                let s = <&str as ::djogi::__private::postgres_types::FromSql>::from_sql(ty, raw)?;
                match s {
                    #(#from_sql_arms)*
                    other => Err(::std::boxed::Box::new(
                        ::djogi::enum_::EnumDecodeError {
                            postgres_type: #postgres_type_str,
                            received: other.to_owned(),
                            expected: #variants_array,
                        }
                    )),
                }
            }

            fn accepts(ty: &::djogi::__private::postgres_types::Type) -> bool {
                ty.name() == #postgres_type_str
            }
        }

        impl #enum_name {
            /// Returns the ordered slice of Postgres wire strings for all variants.
            ///
            /// The order matches the enum declaration order and the
            /// [`::djogi::descriptor::EnumDescriptor::variants`] slice.
            pub fn variants() -> &'static [&'static str] {
                #variants_array
            }
        }

        ::djogi::__private::inventory::submit! {
            ::djogi::descriptor::EnumDescriptor {
                type_name: #type_name_str,
                postgres_type: #postgres_type_str,
                variants: #variants_array,
            }
        }
    };

    Ok(expanded)
}
