//! `#[derive(JsonbSchema)]` proc macro — typed JSONB deep-path API.
//! # What this emits
//! For every `#[derive(JsonbSchema)]` on a named struct, the macro emits:
//! 1. A `{T}Path<M: Model>` struct carrying the JSONB column name and the
//! accumulated path segments so far.
//! 2. One method per field on `{T}Path<M>`:
//! - Scalar fields (from the cast-matrix allowlist OR fields annotated
//! `#[jsonb(scalar)]`) return `JsonbPathRef<M, FieldType>`.
//! - All other field types are assumed to implement `JsonbSchema`; the
//! method returns `<NestedT as JsonbSchema>::Path<M>` with the path
//! extended by the field's JSON key.
//! 3. `impl JsonbSchema for {T}` — wires `type Path<M> = {T}Path<M>` and
//! provides the `root_path` and `__new_from_slice` constructors.
//! # Scalar allowlist
//! Fields whose Rust type matches one of the following are treated as scalars
//! (they produce a `JsonbPathRef<M, V>` leaf rather than descending into a
//! nested `JsonbSchema` tree):
//! `i16`, `i32`, `i64`, `f32`, `f64`, `bool`, `String`, `&str`,
//! `time::OffsetDateTime`, `time::Date`, `djogi::DateTime`, `djogi::Date`,
//! `uuid::Uuid`, `rust_decimal::Decimal`, `::djogi::types::HeerId`,
//! `::djogi::types::RanjId`, `serde_json::Value`.
//! Any other type is assumed to be a nested `JsonbSchema` struct.
//! # `#[jsonb(scalar)]` escape hatch
//! Adopter-defined scalar types — for example, a `primary_key!`-emitted
//! custom PK newtype like `MyAppId(i64)` or a project-local `Username`
//! that wraps `String` — sit outside the built-in allowlist. The
//! `#[jsonb(scalar)]` field-level annotation declares "treat this field
//! as a scalar leaf, not a nested schema":
//! ```ignore
//! #[derive(JsonbSchema, Serialize, Deserialize)]
//! pub struct Spec {
//! #[jsonb(scalar)]
//! pub owner: MyAppId, // emits JsonbPathRef<M, MyAppId>
//! pub engine: EngineSpec, // descends as a nested JsonbSchema
//! }
//! ```
//! The annotation accepts no parameters. Critically, it accepts no raw
//! SQL — Postgres cast selection still flows through `FieldType:
//! IntoFilterValue` (and from there to the typed `JsonbSqlCast` returned
//! by `IntoFilterValue::jsonb_sql_cast`). Adopters cannot inject
//! arbitrary cast text via the macro; the cast comes from the Rust type
//! the macro sees at the field site.
//! # Compile-time validation
//! - Non-struct (enum, union) -> error.
//! - Tuple struct (unnamed fields) -> error.
//! - Empty named struct -> allowed (produces a `{T}Path<M>` with no methods).
//! - Field with `#[serde(flatten)]` -> error.
//! - `#[jsonb(scalar = "...")]` / `#[jsonb(scalar(...))]` -> error
//! (the marker is a bare word; rejecting value forms keeps the door
//! shut on adopter-supplied SQL cast text).
//! # Path routing
//! All emitted type references go through `::djogi::*` paths so the user's
//! crate only needs `djogi` as a dependency, not `heeranjid`, `time`, `uuid`,
//! or `postgres_types` directly.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, Fields, Lit, Meta, Type};

use crate::case::RenameAll;

/// Entry point called from `djogi-macros/src/lib.rs`.
pub fn expand(input: TokenStream) -> Result<TokenStream, Error> {
    let input: DeriveInput = syn::parse2(input)?;
    let name = &input.ident;
    let path_name = format_ident!("{}Path", name);
    let vis = &input.vis;

    // ── Inspect container-level serde attrs ───────────────────────────────────
    // `#[serde(rename_all = "camelCase")]` on the struct sets the default JSON
    // key for every field that lacks a field-level `#[serde(rename = "...")]`.
    let container_rename_all: Option<RenameAll> = inspect_serde_container(&input.attrs);

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

    // collect `#[jsonb(scalar)]` parse errors alongside the
    // existing serde-flatten errors so all violations surface together.
    let mut jsonb_errors: Vec<Error> = Vec::new();

    let accessor_methods: Vec<TokenStream> = named_fields
 .iter()
 .filter_map(|field| {
   let field_ident = field.ident.as_ref()?;
   let field_ty = &field.ty;

   // Determine the JSON key — priority order:
   // 1. Field-level `#[serde(rename = "X")]` → use X.
   // 2. Container-level `#[serde(rename_all = "...")]` →
   // apply case conversion to the snake_case field ident.
   // 3. Default → use the field ident as-is.
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
    SerdeFieldInfo::NoRename => {
     let ident_str = field_ident.to_string();
     if let Some(rule) = container_rename_all {
      // Field idents are snake_case — use apply_to_field.
      rule.apply_to_field(&ident_str)
     } else {
      ident_str
     }
    }
   };
   // json_key_str is a &str borrow of json_key for quote! interpolation.
   let json_key_str: &str = &json_key;

   // check the field-level `#[jsonb(...)]` attribute.
   // The only supported marker is the bare word `scalar`, which
   // opts the field out of the nested-schema branch and emits a
   // `JsonbPathRef<M, FieldType>` leaf. Any other shape
   // (`#[jsonb(scalar = "...")]`, `#[jsonb(unknown)]`, etc.) is
   // rejected with a span-precise diagnostic.
   let explicit_scalar = match inspect_jsonb_field(field) {
    Ok(j) => j.scalar,
    Err(err) => {
     jsonb_errors.push(err);
     return None;
    }
   };

   // Polish (#28): peel `Option<...>` off at the
   // macro layer so `Option<i32>` / `Option<NestedSchema>` work
   // exactly like the bare inner type. Postgres JSONB `->>`
   // returns NULL for missing-key, JSON-null, and
   // non-stringifiable values identically — users wanting the
   // explicit absence check call `.is_null()` / `.is_not_null()`
   // on the resulting `JsonbPathRef` (already in the typed
   // surface). Without this peeling, `#[derive(JsonbSchema)]`
   // on a struct with `Option<T>` fields fails to compile
   // because no blanket `impl<T: JsonbSchema> JsonbSchema for
   // Option<T>` exists and one cannot be added without
   // running into orphan-rule and trait-resolution surprises.
   let effective_ty: Type =
    unwrap_option(field_ty).unwrap_or_else(|| field_ty.clone());

   if explicit_scalar || is_scalar_type(&effective_ty) {
    // Scalar leaf: return JsonbPathRef<M, FieldType>.
    // The path is base_path + [json_key_str], joined as dotted string.
    // json_key_str is the serde rename if present, otherwise the Rust
    // field name — this ensures the path matches the on-disk JSON key.
    Some(quote! {
     /// Typed JSONB path accessor for this scalar field.
     /// Returns a [`JsonbPathRef`](::djogi::jsonb::JsonbPathRef) that
     /// exposes `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `in_list`,
     /// `is_null`, `is_not_null` comparisons emitting the correct
     /// Postgres cast for the field's type. `Option<T>` fields use
     /// the inner type as the cast target — Postgres JSONB returns
     /// NULL identically for missing keys, JSON `null`, and
     /// non-stringifiable values, so use `.is_null()` /
     /// `.is_not_null()` for the explicit absence check.
     #[must_use = "JsonbPathRef is lazy — dropping one silently omits the filter"]
     pub fn #field_ident(self) -> ::djogi::jsonb::JsonbPathRef<M, #effective_ty> {
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
    // `Option<NestedSchema>` peels to `NestedSchema` per the comment
    // above — traversal semantics are identical at the JSONB layer.
    Some(quote! {
     /// Typed JSONB path accessor for this nested schema field.
     /// Returns the nested type's `Path<M>` with the path accumulator
     /// extended by the JSON key for this field. Further field accesses
     /// descend into the nested schema. `Option<NestedSchema>` is
     /// transparent at the JSONB layer — the `->`/`->>` chain returns
     /// NULL when the key is absent or the value is JSON null.
     #[must_use = "path handles are lazy — dropping one silently omits the filter"]
     pub fn #field_ident(self) -> <#effective_ty as ::djogi::jsonb::JsonbSchema>::Path<M> {
      // Extend the base path by the JSON key for this field.
      let mut extended: ::std::vec::Vec<&'static str> =
       ::std::vec::Vec::from(self.base_path);
      extended.push(#json_key_str);
      // Intern the extended segment slice — bounded by unique paths,
      // never leaks per call (Fix 1: path-slice interning).
      let interned_slice =
       ::djogi::jsonb::schema::intern_path_slice(&extended);
      <#effective_ty as ::djogi::jsonb::JsonbSchema>::__new_from_slice::<M>(
       self.base_column,
       interned_slice,
      )
     }
    })
   }
  })
 .collect();

    // Surface any serde-flatten / `#[jsonb(...)]` parse errors collected
    // above. Both lists are folded into one combined diagnostic so the
    // caller sees every violation at once instead of one-at-a-time.
    let all_errors: Vec<Error> = serde_errors.into_iter().chain(jsonb_errors).collect();
    if !all_errors.is_empty() {
        let combined = all_errors
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
     /// Each method descends one level into the JSONB structure. Scalar
     /// fields return a
     /// [`JsonbPathRef`](::djogi::jsonb::JsonbPathRef) for comparisons;
     /// nested fields return the nested type's `Path<M>`.
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

/// Walk the struct's container-level attributes and extract the `rename_all`
/// rule from `#[serde(rename_all = "...")]`, if present.
/// Returns `None` if no serde `rename_all` is set. Unknown or unparseable
/// rename_all values are silently ignored (consistent with how serde handles
/// them at the user level — the Rust compiler will surface an error when
/// serde itself processes the attribute).
/// Only the first valid `rename_all` value wins; duplicates are ignored.
fn inspect_serde_container(attrs: &[syn::Attribute]) -> Option<RenameAll> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        if let Some(rule) = extract_serde_rename_all(attr) {
            return Some(rule);
        }
    }
    None
}

/// Extract the `RenameAll` rule from a `#[serde(rename_all = "...")]`
/// container attribute.
/// Returns `None` if the attribute does not contain `rename_all` or the value
/// is not one of the seven supported rule strings.
fn extract_serde_rename_all(attr: &syn::Attribute) -> Option<RenameAll> {
    let Meta::List(list) = &attr.meta else {
        return None;
    };
    let nested = list
        .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        .ok()?;
    for item in &nested {
        if let Meta::NameValue(nv) = item
            && nv.path.is_ident("rename_all")
            && let syn::Expr::Lit(expr_lit) = &nv.value
            && let Lit::Str(s) = &expr_lit.lit
        {
            // Use span() from the string literal so errors point at the value.
            return RenameAll::from_str(&s.value(), s.span()).ok();
        }
    }
    None
}

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
/// Rules:
/// - `#[serde(flatten)]` -> `SerdeFieldInfo::Flatten`.
/// - `#[serde(rename = "X")]` -> `SerdeFieldInfo::Rename("X")`.
/// - Any other serde attr (e.g. `skip_serializing_if`, `default`) -> ignored.
/// - No serde attr -> `SerdeFieldInfo::NoRename`.
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
/// `#[serde(word, other =...)]` but NOT `#[serde(word = "value")]`.
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
// `#[jsonb(...)]` field-attribute inspection
// ---------------------------------------------------------------------------

/// Parsed `#[jsonb(...)]` markers on a single struct field.
/// Today the only supported marker is the bare word `scalar`. Future
/// markers (if any) extend this struct rather than introducing a new
/// attribute name. The struct stays simple by design — `#[jsonb(...)]`
/// is an escape hatch, not a general configuration surface.
#[derive(Default, Debug)]
struct JsonbFieldInfo {
    /// `#[jsonb(scalar)]` was present — emit a `JsonbPathRef<M, FieldType>`
    /// leaf instead of treating the field type as a nested `JsonbSchema`.
    scalar: bool,
}

/// Walk a field's `#[jsonb(...)]` attributes and collect the parsed
/// markers. Returns an error span-anchored on the offending attribute
/// when an unrecognised key, an unsupported value form (e.g.
/// `#[jsonb(scalar = "foo")]`), or a duplicate marker is encountered.
fn inspect_jsonb_field(field: &syn::Field) -> syn::Result<JsonbFieldInfo> {
    let mut info = JsonbFieldInfo::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("jsonb") {
            continue;
        }
        let Meta::List(list) = &attr.meta else {
            return Err(Error::new_spanned(
                attr,
                "expected `#[jsonb(...)]` with a parenthesised marker list \
     (e.g. `#[jsonb(scalar)]`)",
            ));
        };
        let nested = list.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        for item in &nested {
            match item {
                Meta::Path(p) if p.is_ident("scalar") => {
                    if info.scalar {
                        return Err(Error::new_spanned(
                            item,
                            "duplicate `#[jsonb(scalar)]` marker on this field",
                        ));
                    }
                    info.scalar = true;
                }
                Meta::Path(p) => {
                    let name = p
                        .get_ident()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "<unknown>".to_string());
                    return Err(Error::new_spanned(
                        item,
                        format!(
                            "unknown `#[jsonb({name})]` marker; the only supported \
        marker today is the bare word `scalar`"
                        ),
                    ));
                }
                // reject `#[jsonb(scalar = "...")]` and
                // `#[jsonb(scalar(...))]` explicitly. The scalar marker is
                // a bare-word toggle; admitting a value form would invite
                // adopters to pass arbitrary SQL cast text through the
                // macro layer, which is the exact anti-pattern this
                // escape hatch was designed to avoid.
                Meta::NameValue(nv) => {
                    return Err(Error::new_spanned(
                        nv,
                        "the `#[jsonb(scalar)]` marker takes no value; \
       remove the `=...` suffix. Postgres cast selection \
       flows through `FieldType: IntoFilterValue`, not \
       through adopter-supplied SQL strings",
                    ));
                }
                Meta::List(l) => {
                    return Err(Error::new_spanned(
                        l,
                        "the `#[jsonb(scalar)]` marker takes no nested list; \
       use the bare form `#[jsonb(scalar)]`",
                    ));
                }
            }
        }
    }
    Ok(info)
}

// ---------------------------------------------------------------------------
// Scalar type detection
// ---------------------------------------------------------------------------

/// Determine whether a field type is a scalar from the cast-matrix allowlist.
/// The allowlist matches `jsonb_sql_cast_for_type` in `djogi::jsonb::path`. Scalar
/// types produce a `JsonbPathRef<M, FieldType>` leaf; all other types are
/// assumed to implement `JsonbSchema` (nested struct).
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

/// If `ty` is `Option<Inner>` (or `std::option::Option<Inner>` /
/// `core::option::Option<Inner>`), return `Inner`; otherwise return `None`.
/// Closes GH issue #28: `#[derive(JsonbSchema)]` previously rejected
/// `Option<T>` fields because it tried to resolve `Option<T>: JsonbSchema`
/// as a trait bound, which fails (no blanket impl exists). The fix is to
/// peel `Option` off at macro-expansion time and treat the inner `T` as
/// the field's effective type. This matches Postgres JSONB semantics:
/// `(col->>'key')` returns NULL whether the key is missing, the JSON
/// value is `null`, or the value is non-stringifiable. Users who need
/// to distinguish those cases call `.is_null()` / `.is_not_null()` on
/// the resulting `JsonbPathRef`.
/// Recognised forms (matched by rendered token-string equality
/// matches the same byte-level discipline `is_scalar_type` uses):
/// - `Option<T>` (the common case)
/// - `std::option::Option<T>`
/// - `core::option::Option<T>`
/// - `::std::option::Option<T>`
/// - `::core::option::Option<T>`
fn unwrap_option(ty: &Type) -> Option<Type> {
    use syn::{GenericArgument, PathArguments};

    let Type::Path(type_path) = ty else {
        return None;
    };
    if type_path.qself.is_some() {
        return None;
    }
    let segments = &type_path.path.segments;
    let last = segments.last()?;
    if last.ident != "Option" {
        return None;
    }
    // Reject paths whose head is anything other than `option` /
    // `core::option` / `std::option` / leaf `Option`. This avoids
    // accidentally peeling, say, `my_module::Option<T>`.
    let prefix: Vec<String> = segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .map(|s| s.ident.to_string())
        .collect();
    let prefix_ok = prefix.is_empty()
        || matches!(prefix.as_slice(), [a] if a == "option")
        || matches!(
         prefix.as_slice(),
         [a, b] if (a == "std" || a == "core") && b == "option"
        );
    if !prefix_ok {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    if args.args.len() != 1 {
        return None;
    }
    let GenericArgument::Type(inner) = args.args.first()? else {
        return None;
    };
    Some(inner.clone())
}

/// Scalar type name strings as they appear in rendered token streams.
/// The list mirrors `jsonb_sql_cast_for_type` in `djogi::jsonb::path`. Qualified
/// forms (`time::OffsetDateTime`) and short forms (`OffsetDateTime`) are both
/// listed because users may import with a `use` statement or not.
/// Kept alphabetical for readability when scanning the matrix; the
/// runtime lookup uses `iter().any(...)` (in `is_scalar_type`), not
/// `binary_search`, so the ordering is convention-only.
// Polish (GH issue #29): added narrow-integer entries
// (`u8`, `u16`, `u32`, `i8`) so `#[derive(JsonbSchema)]` accepts these
// Rust-idiomatic types as scalar fields. Each narrow type widens at
// the filter-binding boundary via `IntoFilterValue` (see
// `djogi::query::field`), and the path emitter casts to the smallest
// Postgres int type that fits its full range (see `jsonb_sql_cast_for_type`
// in `djogi::jsonb::path`).
// `u64` exceeds `int8`'s positive range. adds
// `u64 => JsonbSqlCast::Numeric` so the path emitter casts
// `(col->>'key')::numeric` before comparing — aligning with the
// `IntoFilterValue for u64` → `FilterValue::Decimal` bind path.
// Pre-#161 no Postgres type fit the full u64 range and JSONB
// comparisons silently fell back to text.
const SCALAR_TYPE_PATTERNS: &[&str] = &[
    "&str",
    "Date",
    "DateTime",
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
    "i8",
    "rust_decimal::Decimal",
    "serde_json::Value",
    "str",
    "time::Date",
    "time::OffsetDateTime",
    "u16",
    "u32",
    "u64",
    "u8",
    "uuid::Uuid",
    "::djogi::Date",
    "::djogi::DateTime",
    "::djogi::types::Date",
    "::djogi::types::DateTime",
    "::djogi::types::HeerId",
    "::djogi::types::RanjId",
    "djogi::Date",
    "djogi::DateTime",
    "djogi::types::Date",
    "djogi::types::DateTime",
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

    // GH issue #40 — `djogi::DateTime` / `djogi::Date` are canonical aliases
    // for `time::OffsetDateTime` / `time::Date`. Both the unqualified short
    // forms (visible after `use djogi::prelude::*`) and the various
    // qualified spellings must be recognised as scalar leaves so authors
    // do not have to drop down to the `time` crate just to satisfy the
    // derive's pattern matcher.
    #[test]
    fn djogi_datetime_alias_unqualified_is_scalar() {
        let ty: Type = syn::parse_str("DateTime").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_date_alias_unqualified_is_scalar() {
        let ty: Type = syn::parse_str("Date").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_datetime_alias_qualified_is_scalar() {
        let ty: Type = syn::parse_str("djogi::DateTime").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_date_alias_qualified_is_scalar() {
        let ty: Type = syn::parse_str("djogi::Date").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_types_datetime_alias_is_scalar() {
        let ty: Type = syn::parse_str("djogi::types::DateTime").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_types_date_alias_is_scalar() {
        let ty: Type = syn::parse_str("djogi::types::Date").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_datetime_alias_absolute_is_scalar() {
        let ty: Type = syn::parse_str("::djogi::DateTime").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn djogi_types_datetime_alias_absolute_is_scalar() {
        let ty: Type = syn::parse_str("::djogi::types::DateTime").unwrap();
        assert!(is_scalar_type(&ty));
    }

    #[test]
    fn serde_json_value_is_scalar() {
        let ty: Type = syn::parse_str("serde_json::Value").unwrap();
        assert!(is_scalar_type(&ty));
    }

    // ── serde attribute inspection ─────────────────────────────────────────

    // ── container-level serde inspection ──────────────────────────────────────

    #[test]
    fn inspect_serde_container_no_attr_returns_none() {
        let attrs: Vec<syn::Attribute> = vec![];
        assert!(inspect_serde_container(&attrs).is_none());
    }

    #[test]
    fn inspect_serde_container_rename_all_camel_case() {
        // Simulate `#[serde(rename_all = "camelCase")]` on the struct.
        let attr: syn::Attribute = syn::parse_quote! { #[serde(rename_all = "camelCase")] };
        let attrs = vec![attr];
        let rule = inspect_serde_container(&attrs);
        assert!(
            rule.is_some(),
            "camelCase rename_all must be detected at container level"
        );
        // Verify the rule applies correctly to a snake_case field name.
        let result = rule.unwrap().apply_to_field("engine_type");
        assert_eq!(result, "engineType");
    }

    #[test]
    fn inspect_serde_container_rename_all_kebab_case() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(rename_all = "kebab-case")] };
        let attrs = vec![attr];
        let rule = inspect_serde_container(&attrs);
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().apply_to_field("engine_type"), "engine-type");
    }

    #[test]
    fn inspect_serde_container_rename_all_screaming_snake() {
        let attr: syn::Attribute =
            syn::parse_quote! { #[serde(rename_all = "SCREAMING_SNAKE_CASE")] };
        let attrs = vec![attr];
        let rule = inspect_serde_container(&attrs);
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().apply_to_field("engine_type"), "ENGINE_TYPE");
    }

    #[test]
    fn inspect_serde_container_unknown_value_ignored() {
        // An unknown rename_all value is silently ignored (returns None).
        let attr: syn::Attribute = syn::parse_quote! { #[serde(rename_all = "not_a_rule")] };
        let attrs = vec![attr];
        assert!(inspect_serde_container(&attrs).is_none());
    }

    #[test]
    fn extract_serde_rename_all_returns_rule() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(rename_all = "snake_case")] };
        let rule = extract_serde_rename_all(&attr);
        assert!(rule.is_some());
        // snake_case on a snake_case field is a no-op.
        assert_eq!(rule.unwrap().apply_to_field("engine_type"), "engine_type");
    }

    #[test]
    fn extract_serde_rename_all_none_when_absent() {
        let attr: syn::Attribute = syn::parse_quote! { #[serde(skip)] };
        assert!(extract_serde_rename_all(&attr).is_none());
    }

    // ── field-level serde inspection ──────────────────────────────────────────

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

    fn unwrap_option_string(input: &str) -> Option<String> {
        let ty: Type = syn::parse_str(input).unwrap();
        let inner = unwrap_option(&ty)?;
        Some(quote!(#inner).to_string().replace(' ', ""))
    }

    #[test]
    fn unwrap_option_strips_bare_option_wrapper() {
        assert_eq!(unwrap_option_string("Option<i32>"), Some("i32".to_string()));
        assert_eq!(
            unwrap_option_string("Option<String>"),
            Some("String".to_string())
        );
        assert_eq!(
            unwrap_option_string("Option<Profile>"),
            Some("Profile".to_string())
        );
        assert_eq!(
            unwrap_option_string("Option<my_module::Profile>"),
            Some("my_module::Profile".to_string())
        );
    }

    #[test]
    fn unwrap_option_strips_qualified_option_wrappers() {
        assert_eq!(
            unwrap_option_string("std::option::Option<i32>"),
            Some("i32".to_string())
        );
        assert_eq!(
            unwrap_option_string("core::option::Option<i32>"),
            Some("i32".to_string())
        );
        assert_eq!(
            unwrap_option_string("::std::option::Option<i32>"),
            Some("i32".to_string())
        );
        assert_eq!(
            unwrap_option_string("::core::option::Option<i32>"),
            Some("i32".to_string())
        );
        // Relative-path form `option::Option<T>` — accepted because some
        // module-scoped imports reach Option through the `option`
        // sibling-module spelling.
        // prefix had no dedicated test even though the matcher already
        // covered it.
        assert_eq!(
            unwrap_option_string("option::Option<i32>"),
            Some("i32".to_string())
        );
    }

    #[test]
    fn unwrap_option_returns_none_for_non_option() {
        assert_eq!(unwrap_option_string("i32"), None);
        assert_eq!(unwrap_option_string("Vec<i32>"), None);
        assert_eq!(unwrap_option_string("HashMap<String, i32>"), None);
    }

    #[test]
    fn unwrap_option_rejects_lookalike_paths() {
        // Non-canonical `Option` shadows are not peeled — the user's
        // local `my_module::Option<T>` could be a different type
        // entirely. Conservative: only the canonical `Option` /
        // `core::option::Option` / `std::option::Option` are stripped.
        assert_eq!(unwrap_option_string("my_module::Option<i32>"), None);
        assert_eq!(unwrap_option_string("foo::bar::Option<i32>"), None);
    }

    // ── `#[jsonb(scalar)]` parsing ────────────────────────────

    #[test]
    fn inspect_jsonb_field_no_attr_returns_default() {
        let field: syn::Field = syn::parse_quote! { pub id: MyAppId };
        let info = inspect_jsonb_field(&field).expect("no attr must parse cleanly");
        assert!(!info.scalar);
    }

    #[test]
    fn inspect_jsonb_field_scalar_marker_detected() {
        let field: syn::Field = syn::parse_quote! {
         #[jsonb(scalar)]
         pub id: MyAppId
        };
        let info = inspect_jsonb_field(&field).expect("scalar marker must parse");
        assert!(info.scalar, "scalar flag must be set");
    }

    #[test]
    fn inspect_jsonb_field_rejects_scalar_with_value() {
        // Adopter-supplied SQL cast text via the macro is the anti-
        // pattern this escape hatch was designed to avoid; the parser
        // must refuse the `#[jsonb(scalar = "...")]` shape.
        let field: syn::Field = syn::parse_quote! {
         #[jsonb(scalar = "::int8")]
         pub id: MyAppId
        };
        let err = inspect_jsonb_field(&field).expect_err("scalar = \"...\" must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("takes no value"),
            "expected 'takes no value' diagnostic, got: {msg}"
        );
    }

    #[test]
    fn inspect_jsonb_field_rejects_scalar_with_nested_list() {
        let field: syn::Field = syn::parse_quote! {
         #[jsonb(scalar(int8))]
         pub id: MyAppId
        };
        let err =
            inspect_jsonb_field(&field).expect_err("scalar(...) nested list must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("nested list") || msg.contains("bare form"),
            "expected nested-list diagnostic, got: {msg}"
        );
    }

    #[test]
    fn inspect_jsonb_field_rejects_unknown_marker() {
        let field: syn::Field = syn::parse_quote! {
         #[jsonb(future_marker)]
         pub id: MyAppId
        };
        let err = inspect_jsonb_field(&field).expect_err("unknown marker must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown") && msg.contains("future_marker"),
            "expected 'unknown... future_marker' diagnostic, got: {msg}"
        );
    }

    #[test]
    fn inspect_jsonb_field_rejects_duplicate_scalar() {
        let field: syn::Field = syn::parse_quote! {
         #[jsonb(scalar, scalar)]
         pub id: MyAppId
        };
        let err =
            inspect_jsonb_field(&field).expect_err("duplicate scalar marker must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate"),
            "expected 'duplicate' diagnostic, got: {msg}"
        );
    }
}
