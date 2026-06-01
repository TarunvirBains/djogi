//! `#[derive(DjogiEnum)]` proc macro — typed Postgres enum support.
//! Emits four things per enum:
//! 1. `impl postgres_types::ToSql for MyEnum` — encodes the Rust variant as its mapped
//!    Postgres wire string. Uses `to_sql_checked!` for the forwarded type-check path.
//! 2. `impl<'a> postgres_types::FromSql<'a> for MyEnum` — decodes the wire bytes as a
//!    string, matches against known variants, returns `Err(EnumDecodeError { ... })` for
//!    unknown labels.
//! 3. `inventory::submit!(::djogi::descriptor::EnumDescriptor { ... })` — registers the
//!    enum's metadata for the migration differ.
//! 4. `impl MyEnum { pub fn variants -> &'static [&'static str] }` — convenience fn.
//! # Attribute grammar
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
//! - `name` (required) — the Postgres type name.
//! - `rename_all` (optional, default `"snake_case"`) — case conversion applied to all
//!   variants. Supported values: `snake_case`, `SCREAMING_SNAKE_CASE`, `lowercase`,
//!   `UPPERCASE`, `PascalCase`, `camelCase`, `kebab-case`.
//!   Per-variant override: `#[djogi_enum_variant(name = "...")]` takes precedence over
//!   `rename_all`.
//! # Compile-time validation
//! - Empty enum → error: "requires at least one variant".
//! - Non-unit variant (tuple/struct) → error: "variants must be unit-only".
//! - Two variants map to the same Postgres string → error at the second variant.
//! - Missing `#[djogi_enum(name = "...")]` → error at the enum.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Expr, ExprLit, Fields, Lit, Meta, MetaNameValue};

use crate::case::RenameAll;

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
    let enum_name_snake =
        enum_name_str
            .chars()
            .enumerate()
            .fold(String::new(), |mut out, (idx, ch)| {
                if ch.is_ascii_uppercase() {
                    if idx > 0 {
                        out.push('_');
                    }
                    out.extend(ch.to_lowercase());
                } else {
                    out.push(ch);
                }
                out
            });
    let bind_value_fn = format_ident!("__djogi_enum_bind_value_{}", enum_name_snake);
    let bind_list_fn = format_ident!("__djogi_enum_bind_list_{}", enum_name_snake);
    let bind_option_value_fn = format_ident!("__djogi_enum_bind_option_value_{}", enum_name_snake);
    let bind_option_list_fn = format_ident!("__djogi_enum_bind_option_list_{}", enum_name_snake);
    let matches_field_type_fn =
        format_ident!("__djogi_enum_matches_field_type_{}", enum_name_snake);

    // ── Emit IntoFilterValue match arms (, Step 8) ─────────
    // A `DjogiEnum` round-trips as a Postgres enum column (backed by a
    // wire string), so for filter-closure use it converts into
    // `FilterValue::String(<wire>)`. This lets users write
    // `f.status.eq(VehicleStatus::Active)` in a filter closure and have
    // the clause encode the enum variant as its wire label.
    // The match mirrors the `ToSql` arms — same `(variant, wire)` pairs,
    // re-emitted as an owned `String`. Keeping a dedicated match (rather
    // than delegating to `variants[self as usize]`) avoids taking a
    // dependency on discriminant ordering and keeps the encoding
    // branch-free at the call site.
    let into_filter_value_arms = variant_pairs.iter().map(|(ident, wire)| {
        quote! {
            #enum_name::#ident => #wire,
        }
    });

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
            /// The order matches the enum declaration order and the
            /// [`::djogi::descriptor::EnumDescriptor::variants`] slice.
            pub fn variants() -> &'static [&'static str] {
                #variants_array
            }
        }

        impl ::djogi::descriptor::DjogiSqlType for #enum_name {
            const SQL_TYPE: &'static str = #postgres_type_str;
        }

        impl ::djogi::query::DjogiPortableEq for #enum_name {}

        #[doc(hidden)]
        fn #matches_field_type_fn(type_id: ::std::any::TypeId) -> bool {
            type_id == ::std::any::TypeId::of::<#enum_name>()
                || type_id == ::std::any::TypeId::of::<::std::option::Option<#enum_name>>()
                || type_id == ::std::any::TypeId::of::<::djogi::Tracked<#enum_name>>()
                || type_id
                    == ::std::any::TypeId::of::<::djogi::Tracked<::std::option::Option<#enum_name>>>()
                || type_id
                    == ::std::any::TypeId::of::<::std::option::Option<::djogi::Tracked<#enum_name>>>()
        }

        #[doc(hidden)]
        fn #bind_value_fn(
            value: &(dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync),
        ) -> ::std::option::Option<::djogi::descriptor::BoxedSqlBind> {
            if let ::std::option::Option::Some(value) = value.downcast_ref::<#enum_name>() {
                return ::std::option::Option::Some(
                    ::std::boxed::Box::new(<#enum_name as ::std::clone::Clone>::clone(value))
                        as ::djogi::descriptor::BoxedSqlBind,
                );
            }
            if let ::std::option::Option::Some(value) =
                value.downcast_ref::<::djogi::Tracked<#enum_name>>()
            {
                let value: &#enum_name = ::std::ops::Deref::deref(value);
                return ::std::option::Option::Some(
                    ::std::boxed::Box::new(<#enum_name as ::std::clone::Clone>::clone(value))
                        as ::djogi::descriptor::BoxedSqlBind,
                );
            }
            ::std::option::Option::None
        }

        #[doc(hidden)]
        fn #bind_list_fn(
            value: &(dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync),
        ) -> ::std::option::Option<::std::vec::Vec<::djogi::descriptor::BoxedSqlBind>> {
            if let ::std::option::Option::Some(values) =
                value.downcast_ref::<::std::vec::Vec<#enum_name>>()
            {
                return ::std::option::Option::Some(
                    values
                        .iter()
                        .map(|value| {
                            ::std::boxed::Box::new(
                                <#enum_name as ::std::clone::Clone>::clone(value),
                            )
                                as ::djogi::descriptor::BoxedSqlBind
                        })
                        .collect(),
                );
            }
            if let ::std::option::Option::Some(values) =
                value.downcast_ref::<::std::vec::Vec<::djogi::Tracked<#enum_name>>>()
            {
                return ::std::option::Option::Some(
                    values
                        .iter()
                        .map(|value| {
                            let value: &#enum_name = ::std::ops::Deref::deref(value);
                            ::std::boxed::Box::new(
                                <#enum_name as ::std::clone::Clone>::clone(value),
                            )
                                as ::djogi::descriptor::BoxedSqlBind
                        })
                        .collect(),
                );
            }
            ::std::option::Option::None
        }

        #[doc(hidden)]
        fn #bind_option_value_fn(
            value: &(dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync),
        ) -> ::std::option::Option<::std::option::Option<::djogi::descriptor::BoxedSqlBind>> {
            if let ::std::option::Option::Some(value) =
                value.downcast_ref::<::std::option::Option<#enum_name>>()
            {
                return ::std::option::Option::Some(value.as_ref().map(|value| {
                    ::std::boxed::Box::new(<#enum_name as ::std::clone::Clone>::clone(value))
                        as ::djogi::descriptor::BoxedSqlBind
                }));
            }
            if let ::std::option::Option::Some(value) =
                value.downcast_ref::<::djogi::Tracked<::std::option::Option<#enum_name>>>()
            {
                let value: &::std::option::Option<#enum_name> = ::std::ops::Deref::deref(value);
                return ::std::option::Option::Some(value.as_ref().map(|value| {
                    ::std::boxed::Box::new(<#enum_name as ::std::clone::Clone>::clone(value))
                        as ::djogi::descriptor::BoxedSqlBind
                }));
            }
            if let ::std::option::Option::Some(value) =
                value.downcast_ref::<::std::option::Option<::djogi::Tracked<#enum_name>>>()
            {
                return ::std::option::Option::Some(value.as_ref().map(|value| {
                    let value: &#enum_name = ::std::ops::Deref::deref(value);
                    ::std::boxed::Box::new(<#enum_name as ::std::clone::Clone>::clone(value))
                        as ::djogi::descriptor::BoxedSqlBind
                }));
            }
            ::std::option::Option::None
        }

        #[doc(hidden)]
        fn #bind_option_list_fn(
            value: &(dyn ::std::any::Any + ::std::marker::Send + ::std::marker::Sync),
        ) -> ::std::option::Option<
            ::std::vec::Vec<::std::option::Option<::djogi::descriptor::BoxedSqlBind>>,
        > {
            if let ::std::option::Option::Some(values) =
                value.downcast_ref::<::std::vec::Vec<::std::option::Option<#enum_name>>>()
            {
                return ::std::option::Option::Some(
                    values
                        .iter()
                        .map(|value| {
                            value.as_ref().map(|value| {
                                ::std::boxed::Box::new(
                                    <#enum_name as ::std::clone::Clone>::clone(value),
                                )
                                    as ::djogi::descriptor::BoxedSqlBind
                            })
                        })
                        .collect(),
                );
            }
            if let ::std::option::Option::Some(values) = value
                .downcast_ref::<::std::vec::Vec<::djogi::Tracked<::std::option::Option<#enum_name>>>>()
            {
                return ::std::option::Option::Some(
                    values
                        .iter()
                        .map(|value| {
                            let value: &::std::option::Option<#enum_name> =
                                ::std::ops::Deref::deref(value);
                            value.as_ref().map(|value| {
                                ::std::boxed::Box::new(
                                    <#enum_name as ::std::clone::Clone>::clone(value),
                                )
                                    as ::djogi::descriptor::BoxedSqlBind
                            })
                        })
                        .collect(),
                );
            }
            if let ::std::option::Option::Some(values) = value
                .downcast_ref::<::std::vec::Vec<::std::option::Option<::djogi::Tracked<#enum_name>>>>()
            {
                return ::std::option::Option::Some(
                    values
                        .iter()
                        .map(|value| {
                            value.as_ref().map(|value| {
                                let value: &#enum_name = ::std::ops::Deref::deref(value);
                                ::std::boxed::Box::new(
                                    <#enum_name as ::std::clone::Clone>::clone(value),
                                )
                                    as ::djogi::descriptor::BoxedSqlBind
                            })
                        })
                        .collect(),
                );
            }
            ::std::option::Option::None
        }

        ::djogi::__private::inventory::submit! {
            ::djogi::descriptor::EnumDescriptor {
                type_name: #type_name_str,
                postgres_type: #postgres_type_str,
                variants: #variants_array,
            }
        }

        ::djogi::__private::inventory::submit! {
            ::djogi::descriptor::EnumPredicateCodec {
                type_name: #type_name_str,
                postgres_type: #postgres_type_str,
                matches_field_type: #matches_field_type_fn,
                bind_value: #bind_value_fn,
                bind_list: #bind_list_fn,
                bind_option_value: #bind_option_value_fn,
                bind_option_list: #bind_option_list_fn,
            }
        }

        // Filter-closure support. Encoding the enum
        // variant as its Postgres wire string (`FilterValue::String`)
        // matches how the `ToSql` impl sends it over the wire, so
        // `.eq(MyEnum::Variant)` / `.neq(...)` / `.in_list([...])` in a
        // filter closure produce the same bind shape the SELECT emitter
        // itself uses for the column. No ordinal coupling — the match
        // arms enumerate the same `(variant, wire)` pairs as the
        // `ToSql` branch above.
        impl ::djogi::IntoFilterValue for #enum_name {
            fn into_filter_value(self) -> ::djogi::query::internal::FilterValue {
                let wire: &'static str = match self {
                    #(#into_filter_value_arms)*
                };
                ::djogi::query::internal::FilterValue::String(::std::string::String::from(wire))
            }
        }
    };

    Ok(expanded)
}

#[cfg(test)]
mod case_conversion_tests {
    use crate::case::pascal_to_snake;

    #[test]
    fn single_word() {
        assert_eq!(pascal_to_snake("Active"), "active");
    }

    #[test]
    fn standard_camel_boundary() {
        assert_eq!(pascal_to_snake("InMaintenance"), "in_maintenance");
        assert_eq!(pascal_to_snake("MyVariantName"), "my_variant_name");
    }

    #[test]
    fn leading_acronym_before_word() {
        // The trailing cap of the acronym (`L` in XML → the following `P`
        // starts a new word) must get an underscore.
        assert_eq!(pascal_to_snake("XMLParser"), "xml_parser");
        assert_eq!(pascal_to_snake("HTTPSProxy"), "https_proxy");
    }

    #[test]
    fn all_caps_identifier() {
        // No lowercase letter appears anywhere, so no underscores are inserted.
        assert_eq!(pascal_to_snake("ABC"), "abc");
        assert_eq!(pascal_to_snake("AB"), "ab");
        assert_eq!(pascal_to_snake("A"), "a");
    }

    #[test]
    fn trailing_acronym() {
        // `ParserXML` — boundary is at X (prev=r lowercase → underscore).
        // Subsequent M and L are part of the trailing all-caps run with no
        // following lowercase, so no further underscores get inserted.
        assert_eq!(pascal_to_snake("ParserXML"), "parser_xml");
    }

    #[test]
    fn lowercase_start_word() {
        // Already lowercase — no change to the first letter, standard boundaries apply.
        assert_eq!(pascal_to_snake("myField"), "my_field");
    }

    #[test]
    fn empty() {
        assert_eq!(pascal_to_snake(""), "");
    }

    #[test]
    fn mixed_acronym_and_word() {
        // `IOError` — I at i=0 (no underscore), O at i=1 (prev=I upper,
        // next=E upper → no underscore), E at i=2 (prev=O upper, next=r
        // lower → INSERT underscore).
        assert_eq!(pascal_to_snake("IOError"), "io_error");
    }
}
