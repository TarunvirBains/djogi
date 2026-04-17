//! Attribute parsing for `#[model(table = "...", pk = "...")]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
//!
//! Why raw `syn` instead of `darling`? The attribute surface is small and
//! the error messages from `syn::Error::new_spanned` give us precise source
//! spans at zero extra dependency cost. `darling` is kept as a workspace dep
//! for later tasks that need richer derive-input traversal.

use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Field, Lit, Meta, MetaNameValue, Token};

/// Options extracted from `#[model(table = "...", pk = "...")]`.
// Fields are read by Tasks 4–9 (inject, crud, descriptor, stubs). The
// dead-code lint fires now because those callers are stubs — suppress it.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ModelAttrs {
    /// SQL table name, e.g. `"posts"`.
    pub table: String,
    /// Primary key strategy.
    pub pk: PkStrategy,
}

/// Parsed `pk = "..."` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkStrategy {
    HeerId,
    RanjId,
    Serial,
    None,
}

impl ModelAttrs {
    /// Parse `#[model(table = "posts", pk = "heerid")]` from the attribute token stream.
    ///
    /// Duplicate keys are rejected with a span-carrying error pointing at the
    /// second occurrence — last-write-wins silently is a footgun in proc-macro
    /// UX (users can't see which key won without expanding the macro).
    pub fn parse(attr_tokens: proc_macro2::TokenStream) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr_tokens)?;

        let mut table: Option<String> = Option::None;
        let mut pk: Option<PkStrategy> = Option::None;

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
                    if path.is_ident("table") {
                        if table.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `table` key in #[model(...)]",
                            ));
                        }
                        table = Some(s.value());
                    } else if path.is_ident("pk") {
                        if pk.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `pk` key in #[model(...)]",
                            ));
                        }
                        pk = Some(
                            PkStrategy::from_str(&s.value())
                                .map_err(|msg| syn::Error::new_spanned(s, msg))?,
                        );
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[model] attribute `{}`; expected `table` or `pk`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` attribute",
                    ));
                }
            }
        }

        let table = table.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[model] requires `table = \"...\"`",
            )
        })?;
        let pk = pk.unwrap_or(PkStrategy::HeerId);

        Ok(ModelAttrs { table, pk })
    }
}

impl PkStrategy {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "heerid" => Ok(PkStrategy::HeerId),
            "ranjid" => Ok(PkStrategy::RanjId),
            "serial" => Ok(PkStrategy::Serial),
            "none" => Ok(PkStrategy::None),
            other => Err(format!(
                "unknown pk strategy `{other}`; expected one of: heerid, ranjid, serial, none"
            )),
        }
    }
}

/// Options extracted from a single `#[field(...)]` annotation on a struct field.
#[derive(Debug, Default)]
pub struct FieldAttrs {
    pub unique: bool,
    pub index: bool,
    pub max_length: Option<u32>,
    pub renamed_from: Option<String>,
    /// Only valid on `ForeignKey<T>` fields. Values: cascade, restrict, set_null, set_default, protect, do_nothing.
    pub on_delete: Option<String>,
}

impl FieldAttrs {
    /// Parse `#[field(...)]` attributes from a struct field.
    /// Returns `Default::default()` if no `#[field]` annotation is present.
    ///
    /// Duplicate keys (including repeats across multiple `#[field(...)]`
    /// attributes on the same field) are rejected with a span-carrying error
    /// at the second occurrence — last-write-wins silently is a footgun.
    pub fn parse(field: &Field) -> syn::Result<Self> {
        let mut attrs = FieldAttrs::default();
        let mut seen_unique = false;
        let mut seen_index = false;

        for attr in &field.attrs {
            if !attr.path().is_ident("field") {
                continue;
            }
            let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            for meta in &metas {
                match meta {
                    Meta::Path(path) if path.is_ident("unique") => {
                        if seen_unique {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `unique` flag in #[field(...)]",
                            ));
                        }
                        seen_unique = true;
                        attrs.unique = true;
                    }
                    Meta::Path(path) if path.is_ident("index") => {
                        if seen_index {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `index` flag in #[field(...)]",
                            ));
                        }
                        seen_index = true;
                        attrs.index = true;
                    }
                    Meta::NameValue(MetaNameValue {
                        path,
                        value:
                            Expr::Lit(ExprLit {
                                lit: Lit::Int(n), ..
                            }),
                        ..
                    }) if path.is_ident("max_length") => {
                        if attrs.max_length.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `max_length` key in #[field(...)]",
                            ));
                        }
                        attrs.max_length = Some(n.base10_parse()?);
                    }
                    Meta::NameValue(MetaNameValue {
                        path,
                        value:
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(s), ..
                            }),
                        ..
                    }) if path.is_ident("renamed_from") => {
                        if attrs.renamed_from.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `renamed_from` key in #[field(...)]",
                            ));
                        }
                        attrs.renamed_from = Some(s.value());
                    }
                    Meta::NameValue(MetaNameValue {
                        path,
                        value:
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(s), ..
                            }),
                        ..
                    }) if path.is_ident("on_delete") => {
                        if attrs.on_delete.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `on_delete` key in #[field(...)]",
                            ));
                        }
                        let val = s.value();
                        let valid = [
                            "cascade",
                            "restrict",
                            "set_null",
                            "set_default",
                            "protect",
                            "do_nothing",
                        ];
                        if !valid.contains(&val.as_str()) {
                            return Err(syn::Error::new_spanned(
                                s,
                                format!(
                                    "unknown on_delete value `{val}`; expected one of: {}",
                                    valid.join(", ")
                                ),
                            ));
                        }
                        attrs.on_delete = Some(val);
                    }
                    other => {
                        return Err(syn::Error::new_spanned(other, "unknown #[field] attribute"));
                    }
                }
            }
        }

        Ok(attrs)
    }
}

/// The SQL column type for a Rust type string.
///
/// Returns `None` for `Option<T>` (handled by the caller via `unwrap_option`) and
/// for unrecognized types (the caller should emit a compile error).
///
/// Called by `descriptor::expand` and `inject::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn rust_type_to_sql(ty: &syn::Type) -> Option<&'static str> {
    let s = quote::quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
        "String" => Some("TEXT"),
        "i16" => Some("SMALLINT"),
        "i32" => Some("INTEGER"),
        "i64" => Some("BIGINT"),
        "f32" => Some("REAL"),
        "f64" => Some("DOUBLE PRECISION"),
        "bool" => Some("BOOLEAN"),
        "DateTime" | "time::OffsetDateTime" | "OffsetDateTime" => Some("TIMESTAMPTZ"),
        "Date" | "time::Date" => Some("DATE"),
        "Decimal" | "rust_decimal::Decimal" => Some("NUMERIC"),
        "Uuid" | "uuid::Uuid" => Some("UUID"),
        "serde_json::Value" | "Value" => Some("JSONB"),
        "Vec<String>" => Some("TEXT[]"),
        "Vec<i32>" => Some("INTEGER[]"),
        "Vec<i64>" => Some("BIGINT[]"),
        "Vec<bool>" => Some("BOOLEAN[]"),
        // Option<T> is handled at call site — strip and recurse via unwrap_option
        _ if s.starts_with("Option<") => None,
        _ => None,
    }
}

/// Strip `Option<T>` → returns the inner type and `nullable = true`.
///
/// Only recognizes the prelude's `Option` — specifically the path forms
/// `Option<T>`, `std::option::Option<T>`, and `core::option::Option<T>`. A user
/// type that happens to be named `Option` in their own module (e.g.
/// `my_crate::Option<T>`) is left unchanged, because treating it as nullable
/// silently would produce wrong migrations. This matches how users actually
/// read the type: `Option<T>` in the prelude means "SQL NULL allowed"; anything
/// else is a user type that must map via `rust_type_to_sql`.
///
/// Non-`Option` types are returned unchanged with `nullable = false`.
///
/// Called by `inject::expand` and `descriptor::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn unwrap_option(ty: &syn::Type) -> (syn::Type, bool) {
    if let syn::Type::Path(syn::TypePath { path, .. }) = ty
        && is_prelude_option_path(path)
        && let Some(last) = path.segments.last()
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (inner.clone(), true);
    }
    (ty.clone(), false)
}

/// True if `path` is one of the three canonical prelude `Option` forms:
/// bare `Option`, `std::option::Option`, or `core::option::Option`.
fn is_prelude_option_path(path: &syn::Path) -> bool {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    match segs.as_slice() {
        [sole] => sole == "Option",
        [root, module, ty] => {
            (root == "std" || root == "core") && module == "option" && ty == "Option"
        }
        _ => false,
    }
}
