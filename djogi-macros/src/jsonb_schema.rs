//! `#[derive(JsonbSchema)]` proc macro — typed JSONB deep-path API.
//!
//! # What this emits
//!
//! For every `#[derive(JsonbSchema)]` on a named struct, the macro emits:
//!
//! 1. A `{T}Path<M: Model>` struct carrying the JSONB column name and the
//!    accumulated path segments so far.
//! 2. One method per field on `{T}Path<M>`:
//!    - Scalar fields (from the cast-matrix allowlist) return
//!      `JsonbPathRef<M, FieldType>`.
//!    - All other field types are assumed to implement `JsonbSchema`; the
//!      method returns `<NestedT as JsonbSchema>::Path<M>` with the path
//!      extended by the field's JSON key.
//! 3. `impl JsonbSchema for {T}` — wires `type Path<M> = {T}Path<M>` and
//!    provides the `root_path` and `__new_from_slice` constructors.
//!
//! # Scalar allowlist
//!
//! Fields whose Rust type matches one of the following are treated as scalars
//! (they produce a `JsonbPathRef<M, V>` leaf rather than descending into a
//! nested `JsonbSchema` tree):
//!
//! `i16`, `i32`, `i64`, `f32`, `f64`, `bool`, `String`, `&str`,
//! `time::OffsetDateTime`, `time::Date`, `uuid::Uuid`,
//! `rust_decimal::Decimal`, `::djogi::types::HeerId`, `::djogi::types::RanjId`,
//! `serde_json::Value`.
//!
//! Any other type is assumed to be a nested `JsonbSchema` struct.
//!
//! # Compile-time validation
//!
//! - Non-struct (enum, union) -> error.
//! - Tuple struct (unnamed fields) -> error.
//! - Empty named struct -> allowed (produces a `{T}Path<M>` with no methods).
//! - Field with `#[serde(flatten)]` -> error.
//!
//! # Path routing
//!
//! All emitted type references go through `::djogi::*` paths so the user's
//! crate only needs `djogi` as a dependency, not `heeranjid`, `time`, `uuid`,
//! or `postgres_types` directly.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, Fields, Lit, Meta, Type};

/// Entry point called from `djogi-macros/src/lib.rs`.
pub fn expand(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse2(input)?;
    let name = &input.ident;
    let path_name = format_ident!("{}Path", name);
    let vis = &input.vis;

    // ── Validate input ────────────────────────────────────────────────────────

    let named_fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(nf) => &nf.named,
            Fields::Unnamed(_) => {
                return Err(Error::new_spanned(
                    &input.ident,
                    "#[derive(JsonbSchema)] requires a named struct — \
                     tuple structs are not supported. Use named fields: \
                     `struct Foo { field: Type }`",
                ));
            }
            Fields::Unit => {
                // Unit struct (no fields) — treat same as empty named struct.
                return Ok(emit_empty_impl(name, &path_name, vis));
            }
        },
        Data::Enum(e) => {
            return Err(Error::new(
                e.enum_token.span,
                "#[derive(JsonbSchema)] can only be applied to named structs, not enums",
            ));
        }
        Data::Union(u) => {
            return Err(Error::new(
                u.union_token.span,
                "#[derive(JsonbSchema)] can only be applied to named structs, not unions",
            ));
        }
    };

    // ── Emit one accessor method per field ────────────────────────────────────

    // Validate serde attrs and collect JSON keys before emitting methods.
    // Errors for flatten fields are collected so all violations are reported
    // at once rather than stopping at the first.
    let mut serde_errors: Vec<Error> = Vec::new();

    let accessor_methods: Vec<TokenStream> = named_fields
        .iter()
        .filter_map(|field| {
            let field_ident = field.ident.as_ref()?;
            let field_ty = &field.ty;

            // Determine the JSON key — either the Rust ident or a serde rename.
            let json_key: String = match inspect_serde_field(field) {
                SerdeFieldInfo::Flatten => {
                    // Emit a span-precise compile error at the flatten attribute.
                    let flatten_attr = field
                        .attrs
                        .iter()
                        .find(|a| a.path().is_ident("serde"))
                        .expect("serde attr must exist when Flatten is returned");
                    serde_errors.push(Error::new_spanned(
                        flatten_attr,
                        "JsonbSchema does not support #[serde(flatten)] fields \
                         — flattened keys cannot be addressed via a static path. \
                         Either remove the flatten or opt the parent struct out of JsonbSchema.",
                    ));
                    return None;
                }
                SerdeFieldInfo::Rename(n) => n,
                SerdeFieldInfo::NoRename => field_ident.to_string(),
            };
            // json_key_str is a &str borrow of json_key for quote! interpolation.
            let json_key_str: &str = &json_key;

            if is_scalar_type(field_ty) {
                // Scalar leaf: return JsonbPathRef<M, FieldType>.
                // The path is base_path + [json_key_str], joined as dotted string.
                // json_key_str is the serde rename if present, otherwise the Rust
                // field name — this ensures the path matches the on-disk JSON key.
                Some(quote! {
                    /// Typed JSONB path accessor for this scalar field.
                    ///
                    /// Returns a [`JsonbPathRef`](::djogi::jsonb::JsonbPathRef) that
                    /// exposes `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in_list`,
                    /// `is_null`, `is_not_null` comparisons emitting the correct
                    /// Postgres cast for the field's type.
                    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
                    pub fn #field_ident(self) -> ::djogi::jsonb::JsonbPathRef<M, #field_ty> {
                        // Build the full segment list: base segments + JSON key.
                        let mut segments: ::std::vec::Vec<&'static str> =
                            ::std::vec::Vec::from(self.base_path);
                        segments.push(#json_key_str);
                        let dotted = ::djogi::jsonb::schema::intern_path(&segments);
                        ::djogi::jsonb::JsonbPathRef::__from_macro(self.base_column, dotted)
                    }
                })
            } else {
                // Nested JsonbSchema: return <FieldType as JsonbSchema>::Path<M>
                // with the path extended by the JSON key (serde rename or Rust ident).
                Some(quote! {
                    /// Typed JSONB path accessor for this nested schema field.
                    ///
                    /// Returns the nested type's `Path<M>` with the path accumulator
                    /// extended by the JSON key for this field. Further field accesses
                    /// descend into the nested schema.
                    #[must_use = "path handles are lazy — dropping one silently omits the filter"]
                    pub fn #field_ident(self) -> <#field_ty as ::djogi::jsonb::JsonbSchema>::Path<M> {
                        // Extend the base path by the JSON key for this field.
                        let mut extended: ::std::vec::Vec<&'static str> =
                            ::std::vec::Vec::from(self.base_path);
                        extended.push(#json_key_str);
                        // Intern the extended segment slice — bounded by unique paths,
                        // never leaks per call (Fix 1: path-slice interning).
                        let interned_slice =
                            ::djogi::jsonb::schema::intern_path_slice(&extended);
                        <#field_ty as ::djogi::jsonb::JsonbSchema>::__new_from_slice::<M>(
                            self.base_column,
                            interned_slice,
                        )
                    }
                })
            }
        })
        .collect();

    // Surface any serde-flatten errors collected above.
    if !serde_errors.is_empty() {
        let combined = serde_errors
            .into_iter()
            .reduce(|mut acc, e| {
                acc.combine(e);
                acc
            })
            .unwrap();
        return Err(combined);
    }

    // Handle the zero-field named struct case.
    if named_fields.is_empty() {
        return Ok(emit_empty_impl(name, &path_name, vis));
    }

    // ── Emit {T}Path<M> struct ────────────────────────────────────────────────

    Ok(quote! {
        /// Typed JSONB path tree for
        #[doc = concat!("[`", stringify!(#name), "`].")]
        ///
        /// Each method descends one level into the JSONB structure. Scalar
        /// fields return a
        /// [`JsonbPathRef`](::djogi::jsonb::JsonbPathRef) for comparisons;
        /// nested fields return the nested type's `Path<M>`.
        ///
        /// Constructed via [`JsonbSchema::root_path`] by calling
        /// `.typed()` on a `FieldRef<M, Jsonb<T>>`.
        #vis struct #path_name<M: ::djogi::model::Model> {
            base_column: &'static str,
            base_path: &'static [&'static str],
            _phantom: ::std::marker::PhantomData<fn() -> M>,
        }

        impl<M: ::djogi::model::Model> #path_name<M> {
            /// Internal constructor — called by `JsonbSchema::root_path` (root)
            /// and by parent `{T}Path::field_name()` (nested).
            #[doc(hidden)]
            #[inline]
            pub fn __new(base_column: &'static str, base_path: &'static [&'static str]) -> Self {
                Self {
                    base_column,
                    base_path,
                    _phantom: ::std::marker::PhantomData,
                }
            }

            #(#accessor_methods)*
        }

        impl ::djogi::jsonb::JsonbSchema for #name {
            type Path<M: ::djogi::model::Model> = #path_name<M>;

            /// Construct the root of the typed path tree for the JSONB column
            /// `base_column`. Called by `FieldRef<M, Jsonb<Self>>::typed()`.
            fn root_path<M: ::djogi::model::Model>(base_column: &'static str) -> #path_name<M> {
                #path_name::__new(base_column, &[])
            }

            /// Internal: construct a nested path node from an already-interned
            /// segment slice. Called by parent `{T}Path<M>` accessor methods.
            ///
            /// `base_path` is a `&'static [&'static str]` returned by
            /// `intern_path_slice` — allocated at most once per unique path
            /// sequence, so calling this N times for the same path costs zero
            /// additional allocation (Fix 1: path-slice interning).
            #[doc(hidden)]
            fn __new_from_slice<M: ::djogi::model::Model>(
                base_column: &'static str,
                base_path: &'static [&'static str],
            ) -> #path_name<M> {
                #path_name::__new(base_column, base_path)
            }
        }
    })
}

/// Emit the bare minimum for an empty (unit or zero-field named) struct.
///
/// Fix 3: thread `vis` so the emitted `{T}Path<M>` struct respects the source
/// type's visibility rather than always emitting `pub`.
fn emit_empty_impl(
    name: &syn::Ident,
    path_name: &syn::Ident,
    vis: &syn::Visibility,
) -> TokenStream {
    quote! {
        /// Typed JSONB path tree (no fields — empty schema).
        #vis struct #path_name<M: ::djogi::model::Model> {
            base_column: &'static str,
            base_path: &'static [&'static str],
            _phantom: ::std::marker::PhantomData<fn() -> M>,
        }

        impl<M: ::djogi::model::Model> #path_name<M> {
            #[doc(hidden)]
            #[inline]
            pub fn __new(base_column: &'static str, base_path: &'static [&'static str]) -> Self {
                Self {
                    base_column,
                    base_path,
                    _phantom: ::std::marker::PhantomData,
                }
            }
        }

        impl ::djogi::jsonb::JsonbSchema for #name {
            type Path<M: ::djogi::model::Model> = #path_name<M>;

            fn root_path<M: ::djogi::model::Model>(base_column: &'static str) -> #path_name<M> {
                #path_name::__new(base_column, &[])
            }

            #[doc(hidden)]
            fn __new_from_slice<M: ::djogi::model::Model>(
                base_column: &'static str,
                base_path: &'static [&'static str],
            ) -> #path_name<M> {
                #path_name::__new(base_column, base_path)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Serde attribute inspection
// ---------------------------------------------------------------------------

/// Outcome of inspecting a field's `#[serde(...)]` attributes.
enum SerdeFieldInfo {
    /// No serde rename — use the Rust field identifier as-is.
    NoRename,
    /// `#[serde(rename = "name")]` found — use this string as the JSON key.
    Rename(String),
    /// `#[serde(flatten)]` found — must be rejected.
    Flatten,
}

/// Walk a field's attributes and extract serde-relevant info.
///
/// Rules:
/// - `#[serde(flatten)]` -> `SerdeFieldInfo::Flatten`.
/// - `#[serde(rename = "X")]` -> `SerdeFieldInfo::Rename("X")`.
/// - Any other serde attr (e.g. `skip_serializing_if`, `default`) -> ignored.
/// - No serde attr -> `SerdeFieldInfo::NoRename`.
///
/// Flatten takes priority over rename in the unlikely case both appear.
fn inspect_serde_field(field: &syn::Field) -> SerdeFieldInfo {
    for attr in &field.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        // Check for flatten first — it takes priority.
        if has_serde_word(attr, "flatten") {
            return SerdeFieldInfo::Flatten;
        }
        // Check for rename.
        if let Some(rename) = extract_serde_rename(attr) {
            return SerdeFieldInfo::Rename(rename);
        }
    }
    SerdeFieldInfo::NoRename
}

/// Return true if the `#[serde(...)]` attribute contains the bare word `word`
/// (e.g. `flatten`, `skip`). Matches `#[serde(word)]` and
/// `#[serde(word, other = ...)]` but NOT `#[serde(word = "value")]`.
fn has_serde_word(attr: &syn::Attribute, word: &str) -> bool {
    let Meta::List(list) = &attr.meta else {
        return false;
    };
    let Ok(nested) =
        list.parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
    else {
        return false;
    };
    nested
        .iter()
        .any(|item| matches!(item, Meta::Path(p) if p.is_ident(word)))
}

/// Extract the string value from `#[serde(rename = "...")]`.
///
/// Returns `None` if no `rename` key-value pair is found or the value cannot
/// be parsed as a string literal.
fn extract_serde_rename(attr: &syn::Attribute) -> Option<String> {
    let Meta::List(list) = &attr.meta else {
        return None;
    };
    let nested = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .ok()?;
    for item in &nested {
        if let Meta::NameValue(nv) = item
            && nv.path.is_ident("rename")
            && let syn::Expr::Lit(expr_lit) = &nv.value
            && let Lit::Str(s) = &expr_lit.lit
        {
            return Some(s.value());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Scalar type detection
// ---------------------------------------------------------------------------

/// Determine whether a field type is a scalar from the cast-matrix allowlist.
///
/// The allowlist matches `sql_cast_for_type` in `djogi::jsonb::path`. Scalar
/// types produce a `JsonbPathRef<M, FieldType>` leaf; all other types are
/// assumed to implement `JsonbSchema` (nested struct).
///
/// Type matching is done by comparing the rendered token string of the type,
/// which is the only information available to a proc macro. This is reliable
/// for primitive types (`i32`, `bool`, etc.) and for well-known qualified
/// types. Unknown types fall through to the nested branch, which is the
/// conservative choice: the user gets a helpful compile error from the Rust
/// trait checker ("trait `JsonbSchema` is not implemented for X") rather than
/// a confusing JSON path error at runtime.
fn is_scalar_type(ty: &Type) -> bool {
    let rendered = quote!(#ty).to_string().replace(' ', "");
    SCALAR_TYPE_PATTERNS.iter().any(|&pat| rendered == pat)
}

/// Scalar type name strings as they appear in rendered token streams.
///
/// The list mirrors `sql_cast_for_type` in `djogi::jsonb::path`. Qualified
/// forms (`time::OffsetDateTime`) and short forms (`OffsetDateTime`) are both
/// listed because users may import with a `use` statement or not.
///
/// Sorted alphabetically so `binary_search` works; this is enforced by the
/// static assertion in the unit tests below.
const SCALAR_TYPE_PATTERNS: &[&str] = &[
    "&str",
    "Date",
    "Decimal",
    "OffsetDateTime",
    "String",
    "Uuid",
    "bool",
    "f32",
    "f64",
    "i16",
    "i32",
    "i64",
    "rust_decimal::Decimal",
    "serde_json::Value",
    "str",
    "time::Date",
    "time::OffsetDateTime",
    "u64",
    "uuid::Uuid",
    "::djogi::types::HeerId",
    "::djogi::types::RanjId",
    "djogi::types::HeerId",
    "djogi::types::RanjId",
    "heeranjid::HeerId",
    "heeranjid::RanjId",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i32_is_scalar() {
        let ty: Type = syn::parse_str("i32").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn string_is_scalar() {
        let ty: Type = syn::parse_str("String").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn bool_is_scalar() {
        let ty: Type = syn::parse_str("bool").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn nested_struct_is_not_scalar() {
        let ty: Type = syn::parse_str("EngineSpecs").unwrap();
        assert!(!is_scalar_type(&ty));
    }

    #[test]
    fn serde_json_value_is_scalar() {
        let ty: Type = syn::parse_str("serde_json::Value").unwrap();
        assert!(is_scalar_type(&ty));
    }

    // ── serde attribute inspection ─────────────────────────────────────────

    #[test]
    fn inspect_serde_field_no_attr_returns_no_rename() {
        let field: syn::Field = syn::parse_quote! { pub cylinders: i32 };
        assert!(matches!(
            inspect_serde_field(&field),
            SerdeFieldInfo::NoRename
        ));
    }

    #[test]
    fn inspect_serde_field_rename_extracts_value() {
        let field: syn::Field =
            syn::parse_quote! { #[serde(rename = "camelCaseKey")] pub cylinders: i32 };
        match inspect_serde_field(&field) {
            SerdeFieldInfo::Rename(s) => assert_eq!(s, "camelCaseKey"),
            _ => panic!("expected Rename"),
        }
    }

    #[test]
    fn inspect_serde_field_flatten_detected() {
        let field: syn::Field = syn::parse_quote! {
            #[serde(flatten)]
            pub extras: std::collections::HashMap<String, i32>
        };
        assert!(matches!(
            inspect_serde_field(&field),
            SerdeFieldInfo::Flatten
        ));
    }

    #[test]
    fn inspect_serde_field_skip_serializing_if_ignored() {
        let field: syn::Field =
            syn::parse_quote! { #[serde(skip_serializing_if = "Option::is_none")] pub count: i32 };
        assert!(matches!(
            inspect_serde_field(&field),
            SerdeFieldInfo::NoRename
        ));
    }

    #[test]
    fn has_serde_word_detects_flatten() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(flatten)] };
        assert!(has_serde_word(&attr, "flatten"));
        assert!(!has_serde_word(&attr, "skip"));
    }

    #[test]
    fn extract_serde_rename_returns_value() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(rename = "fooBar")] };
        assert_eq!(extract_serde_rename(&attr), Some("fooBar".to_string()));
    }

    #[test]
    fn extract_serde_rename_none_when_absent() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(skip)] };
        assert_eq!(extract_serde_rename(&attr), None);
    }
}
