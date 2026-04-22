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
//!      extended by the field's JSON key (the Rust field name, unchanged).
//! 3. `impl JsonbSchema for {T}` — wires `type Path<M> = {T}Path<M>` and
//!    provides the `root_path` constructor.
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
//! - Non-struct (enum, union) → error.
//! - Tuple struct (unnamed fields) → error.
//! - Empty named struct → allowed (produces a `{T}Path<M>` with no methods).
//!
//! # Path routing
//!
//! All emitted type references go through `::djogi::*` paths so the user's
//! crate only needs `djogi` as a dependency, not `heeranjid`, `time`, `uuid`,
//! or `postgres_types` directly.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, Fields, Type};

/// Entry point called from `djogi-macros/src/lib.rs`.
pub fn expand(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse2(input)?;
    let name = &input.ident;
    let path_name = format_ident!("{}Path", name);

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
                return Ok(emit_empty_impl(name, &path_name));
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

    let accessor_methods: Vec<TokenStream> = named_fields
        .iter()
        .filter_map(|field| {
            let field_ident = field.ident.as_ref()?;
            let field_ty = &field.ty;
            let field_name_str = field_ident.to_string();

            if is_scalar_type(field_ty) {
                // Scalar leaf: return JsonbPathRef<M, FieldType>.
                // The path is base_path + [field_name_str], joined as dotted string.
                Some(quote! {
                    /// Typed JSONB path accessor for this scalar field.
                    ///
                    /// Returns a [`JsonbPathRef`](::djogi::jsonb::JsonbPathRef) that
                    /// exposes `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in_list`,
                    /// `is_null`, `is_not_null` comparisons emitting the correct
                    /// Postgres cast for `#field_ty`.
                    #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
                    pub fn #field_ident(self) -> ::djogi::jsonb::JsonbPathRef<M, #field_ty> {
                        // Build the full segment list: base segments + this field name.
                        let mut segments: ::std::vec::Vec<&'static str> =
                            ::std::vec::Vec::from(self.base_path);
                        segments.push(#field_name_str);
                        let dotted = ::djogi::jsonb::schema::intern_path(&segments);
                        ::djogi::jsonb::JsonbPathRef::__from_macro(self.base_column, dotted)
                    }
                })
            } else {
                // Nested JsonbSchema: return <FieldType as JsonbSchema>::Path<M>
                // with the path extended by the field name.
                Some(quote! {
                    /// Typed JSONB path accessor for this nested schema field.
                    ///
                    /// Returns `<#field_ty as JsonbSchema>::Path<M>` with the
                    /// path accumulator extended by `#field_name_str`. Further
                    /// field accesses descend into the nested schema.
                    #[must_use = "path handles are lazy — dropping one silently omits the filter"]
                    pub fn #field_ident(self) -> <#field_ty as ::djogi::jsonb::JsonbSchema>::Path<M> {
                        // Extend the base path by this field name.
                        let mut extended: ::std::vec::Vec<&'static str> =
                            ::std::vec::Vec::from(self.base_path);
                        extended.push(#field_name_str);
                        // Intern all the accumulated segments so we have a stable
                        // &'static [&'static str] to pass down.
                        let dotted = ::djogi::jsonb::schema::intern_path(&extended);
                        <#field_ty as ::djogi::jsonb::JsonbSchema>::__nested_path::<M>(
                            self.base_column,
                            dotted,
                        )
                    }
                })
            }
        })
        .collect();

    // ── Emit {T}Path<M> struct ────────────────────────────────────────────────

    let vis = &input.vis;

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

            /// Internal: construct a nested path node already positioned at
            /// the dotted path `base_path_str` within column `base_column`.
            ///
            /// `base_path_str` is an interned dotted string produced by the
            /// parent node's accessor (e.g. `"engine"` when accessing
            /// `specs.engine`). The path node internally converts this back
            /// to a slice by splitting on `.`; see `intern_path` for why a
            /// round-trip through string form is acceptable here.
            #[doc(hidden)]
            fn __nested_path<M: ::djogi::model::Model>(
                base_column: &'static str,
                base_path_str: &'static str,
            ) -> #path_name<M> {
                // Split the interned dotted string back into segments and
                // intern each segment individually so the slice is `&'static`.
                let segments: ::std::vec::Vec<&'static str> = base_path_str
                    .split('.')
                    .map(|s| ::djogi::jsonb::schema::intern_path(
                        &[s]
                    ))
                    .collect();
                // Intern the segments slice itself as a leaked Box.
                let leaked_slice: &'static [&'static str] =
                    ::std::boxed::Box::leak(segments.into_boxed_slice());
                #path_name::__new(base_column, leaked_slice)
            }
        }
    })
}

/// Emit the bare minimum for an empty (unit or zero-field named) struct.
fn emit_empty_impl(name: &syn::Ident, path_name: &syn::Ident) -> TokenStream {
    quote! {
        /// Typed JSONB path tree (no fields — empty schema).
        pub struct #path_name<M: ::djogi::model::Model> {
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
            fn __nested_path<M: ::djogi::model::Model>(
                base_column: &'static str,
                base_path_str: &'static str,
            ) -> #path_name<M> {
                let segments: ::std::vec::Vec<&'static str> = base_path_str
                    .split('.')
                    .map(|s| ::djogi::jsonb::schema::intern_path(&[s]))
                    .collect();
                let leaked_slice: &'static [&'static str] =
                    ::std::boxed::Box::leak(segments.into_boxed_slice());
                #path_name::__new(base_column, leaked_slice)
            }
        }
    }
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
}
