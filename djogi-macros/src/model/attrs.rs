//! Attribute parsing for `#[model(table = "...", pk = "...")]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
//!
//! `ModelAttrs` keeps a hand-rolled parser: the surface is three keys, the
//! error messages from `syn::Error::new_spanned` already carry precise
//! source spans, and there is no incentive to grow it.
//!
//! `FieldAttrs` parses via `darling::FromField`. Per-field attrs grow over
//! time (later phases add `db_column`, `choices`, `validators`, etc.), and
//! darling's declarative derive gives us span-aware errors for unknown
//! keys, type mismatches, and duplicate keys for free — matching the prior
//! hand-rolled behaviour without each new key duplicating the same
//! `Meta::NameValue` match arm.

use darling::FromField;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token};

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
    /// When `true`, the macro skips generating the `Default` impl.
    ///
    /// Use `#[model(table = "...", no_default)]` for models that contain
    /// field types that do not implement `Default` (e.g. `time::Date`).
    /// Without this flag the generated `Default` impl would fail to compile.
    /// Users must then initialise all fields explicitly instead of relying
    /// on struct-update syntax (`..Model::default()`).
    pub no_default: bool,
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
        let mut no_default = false;
        let mut seen_no_default = false;

        for meta in &metas {
            match meta {
                // Flag-only attribute: `no_default`
                Meta::Path(path) if path.is_ident("no_default") => {
                    if seen_no_default {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `no_default` flag in #[model(...)]",
                        ));
                    }
                    seen_no_default = true;
                    no_default = true;
                }
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
                                "unknown #[model] attribute `{}`; expected `table`, `pk`, or `no_default`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` attribute or bare flag (`no_default`)",
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

        Ok(ModelAttrs {
            table,
            pk,
            no_default,
        })
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
///
/// Parsed via `darling::FromField`. Unknown keys, type mismatches, and
/// duplicate keys are reported by darling with source spans. `ident` and
/// `ty` are darling "magic" fields — the derive auto-populates them from
/// `syn::Field::{ident, ty}` at call time, independent of the attribute
/// list — so [`FieldAttrs::parse`] callers can read them alongside the
/// parsed attrs without threading the `syn::Field` separately.
// Not every field is read on every call site (e.g. `ident`/`ty` are pending
// use by later Phase 2 / Phase 3 codegen). Suppress dead_code at struct
// granularity so new fields don't spuriously re-trip the lint.
#[allow(dead_code)]
#[derive(Debug, FromField)]
#[darling(attributes(field))]
pub struct FieldAttrs {
    /// The struct field's identifier.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ident` by magic field name. Always `Some(_)` for
    /// named-field structs; tuple/unit structs are rejected earlier in
    /// `inject::expand`.
    pub ident: Option<syn::Ident>,
    /// The struct field's Rust type.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ty` by magic field name. The type must be `syn::Type`
    /// (not `Option<syn::Type>`) because the derive emits
    /// `ty: field.ty.clone()` verbatim.
    pub ty: syn::Type,

    /// `#[field(unique)]` — emits a `UNIQUE` constraint in migrations.
    #[darling(default)]
    pub unique: bool,
    /// `#[field(index)]` — emits a `CREATE INDEX` in migrations.
    #[darling(default)]
    pub index: bool,
    /// `#[field(max_length = N)]` — caps `TEXT` columns at `VARCHAR(N)`.
    #[darling(default)]
    pub max_length: Option<u32>,
    /// `#[field(renamed_from = "old_name")]` — column rename hint for migrations.
    #[darling(default)]
    pub renamed_from: Option<String>,
    /// `#[field(on_delete = "...")]` — only valid on `ForeignKey<T>` fields.
    /// Accepted values: cascade, restrict, set_null, set_default, protect,
    /// do_nothing. Darling validates the literal is a string;
    /// [`FieldAttrs::parse`] post-validates the value is in the accepted
    /// set (darling's derive alone cannot constrain a `String` domain).
    #[darling(default)]
    pub on_delete: Option<String>,
}

impl FieldAttrs {
    /// Parse `#[field(...)]` from a struct field.
    ///
    /// Returns an all-default instance if no `#[field]` attr is present
    /// (darling's `#[darling(default)]` container attr handles the no-attr
    /// case). Darling emits span-aware errors for:
    /// - Unknown attribute keys (e.g. `#[field(nonexistent)]`).
    /// - Type mismatches (e.g. `max_length = "x"` where an integer is required).
    /// - Duplicate keys across multiple `#[field(...)]` attrs.
    ///
    /// `on_delete` is a string with a constrained value set that darling's
    /// type-level parsing cannot enforce; we post-validate it below and
    /// point the error span at the whole field (the literal's span is lost
    /// by the time darling hands us a `String`).
    pub fn parse(field: &syn::Field) -> syn::Result<Self> {
        // `darling::Error` carries source spans from the originating
        // attribute tokens; `From<darling::Error> for syn::Error` preserves
        // them, so rely on the built-in conversion rather than collapsing
        // everything onto the whole field with `new_spanned`.
        let attrs = <Self as darling::FromField>::from_field(field).map_err(syn::Error::from)?;

        if let Some(on_delete) = &attrs.on_delete {
            let valid = [
                "cascade",
                "restrict",
                "set_null",
                "set_default",
                "protect",
                "do_nothing",
            ];
            if !valid.contains(&on_delete.as_str()) {
                return Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "unknown on_delete value `{on_delete}`; expected one of: {}",
                        valid.join(", ")
                    ),
                ));
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
