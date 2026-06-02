//! Macro-time gating for [`MirJzSON`](djogi::jsonb::MirJzSON) fields
//! #195.
//! `MirJzSON` is Djogi's raw / unschemed JSONB column type, the sibling
//! of [`Jsonb<T>`](djogi::jsonb::Jsonb). It exists precisely for the
//! cases where a payload's schema lives somewhere other than the Rust
//! struct: an upstream partner SDK, a downstream consumer service, a
//! polymorphic blob whose shape varies per row. Whenever an adopter
//! reaches for `MirJzSON` they are stepping off the typed-schema
//! invariant `Jsonb<T>` carries — that step deserves a deliberate,
//! recorded justification.
//! This module owns the macro-side enforcement of that contract:
//! - Every `MirJzSON` and `Option<MirJzSON>` field MUST carry
//!   `#[mirjzson(justification = "...")]`. A missing attribute fails
//!   at expand time with a span-precise error.
//! - The justification literal must be present, non-empty, and not a
//!   placeholder token (`TODO`, `TBD`, `FIXME`, `...`, `none`, etc.).
//!   The denylist is small and ASCII case-insensitive; a minimum
//!   length of 12 trimmed bytes weeds out one-word non-answers.
//! - The attribute is consumed by the macro before the struct is
//!   re-emitted, mirroring how `#[field(...)]` and `#[computed(...)]`
//!   are stripped — rustc has no notion of `mirjzson` as a helper
//!   attribute on the `#[model]` attribute macro, so leaving it in
//!   place produces an `unknown attribute` rustc error rather than the
//!   typed diagnostic this module emits.
//! - `#[mirjzson(...)]` on a non-`MirJzSON` field is rejected at
//!   expand time with a span at the misplaced attribute.
//! - `Jsonb<T>` (the typed-schema sibling) is **not** subject to this
//!   gate. The typed schema IS the justification.
//! # No regex
//! Per `feedback_no_regex_in_djogi.md`: detection uses byte-level
//! checks (`str::eq_ignore_ascii_case`, `str::trim`, last-segment
//! ident comparison) and a sorted const placeholder slice with
//! `iter().any(...)`. No `regex` engine, no regex notation in
//! diagnostics.

use syn::{Expr, ExprLit, Lit, LitStr, Meta, MetaNameValue, Token, punctuated::Punctuated};

/// A single parsed `#[mirjzson(justification = "...")]` annotation.
/// Captured only for fields whose declared Rust type is `MirJzSON` or
/// `Option<MirJzSON>` (last-segment match — covers bare, `djogi::`,
/// `djogi::jsonb::`, `crate::`, `super::`, and `::djogi::*` path forms
/// uniformly).
/// The `justification` field is stored verbatim after validation. v0.1
/// consumers only need to know the attribute is present and well-formed
/// the value is available here so future surfaces (descriptor
/// emission, doc generation, model reflection) can read it without
/// re-parsing. `dead_code` is allowed at the struct level so adding a
/// new consumer downstream does not require touching this declaration.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MirJzSONAttr {
    /// The justification string the adopter wrote. Validated to be
    /// non-empty, non-placeholder, and at least
    /// [`MIN_JUSTIFICATION_BYTES`] trimmed bytes long.
    pub justification: String,
}

/// Walk a struct's fields and validate the MirJzSON gate.
/// Returns the validated `(field_ident, MirJzSONAttr)` pairs in
/// declared order so future consumers (descriptor emission, doc
/// generation) can read the justification without re-parsing.
/// Errors with a span-precise diagnostic on:
/// - A `MirJzSON` / `Option<MirJzSON>` field without a
///   `#[mirjzson(...)]` attribute.
/// - A `#[mirjzson(...)]` attribute on a field whose type is not
///   `MirJzSON` / `Option<MirJzSON>`.
/// - A missing, malformed, or placeholder `justification` value
///   (see [`validate_justification`]).
/// - A duplicate `#[mirjzson(...)]` attribute on the same field.
/// - The bare `#[mirjzson]` form (no argument list).
/// - Any key other than `justification` inside the argument list.
///   Returns `Ok(Vec::new())` for structs that declare no `MirJzSON`
///   fields and carry no stray `#[mirjzson(...)]` attributes.
pub fn parse_mirjzson_attrs(
    struct_item: &syn::ItemStruct,
) -> syn::Result<Vec<(syn::Ident, MirJzSONAttr)>> {
    let mut out = Vec::new();
    for field in &struct_item.fields {
        let Some(field_ident) = field.ident.as_ref() else {
            // Tuple-struct fields have no ident; the macro pipeline
            // rejects tuple/unit structs upstream, but bail safely.
            continue;
        };

        let is_mirjzson_typed = is_mirjzson_type(&field.ty);
        let mut found: Option<(syn::Attribute, MirJzSONAttr)> = None;

        for attr in &field.attrs {
            if !attr.path().is_ident("mirjzson") {
                continue;
            }

            // Reject when the attribute lands on a field whose declared
            // Rust type is NOT `MirJzSON` / `Option<MirJzSON>`. The
            // attribute is a `MirJzSON` gate, not a generic JSONB knob
            // `Jsonb<T>` has its typed schema as the justification, and
            // any other type has no MirJzSON semantics to gate.
            if !is_mirjzson_typed {
                let ty = &field.ty;
                let type_str = quote::quote!(#ty).to_string().replace(' ', "");
                return Err(syn::Error::new_spanned(
                    attr,
                    format!(
                        "`#[mirjzson(...)]` is only valid on `MirJzSON` or `Option<MirJzSON>` \
                         fields; `{field_ident}` is `{type_str}`. \
                         For `Jsonb<T>` the typed schema IS the justification — drop the \
                         `#[mirjzson(...)]` attribute. For any other type the attribute has \
                         no meaning."
                    ),
                ));
            }

            // `#[mirjzson]` (bare path) — reject. The attribute exists
            // solely to record the justification.
            if matches!(attr.meta, Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[mirjzson]` requires `justification = \"...\"` — \
                     e.g. `#[mirjzson(justification = \"payload schema is \
                     owned by the upstream partner SDK\")]`",
                ));
            }

            let args = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            let attr_value = parse_mirjzson_args(&args, attr)?;

            if found.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    format!(
                        "duplicate `#[mirjzson(...)]` attribute on field `{field_ident}` — \
                         declare the justification once per MirJzSON field"
                    ),
                ));
            }
            found = Some((attr.clone(), attr_value));
        }

        match (is_mirjzson_typed, found) {
            (true, Some((_, attr_value))) => {
                out.push((field_ident.clone(), attr_value));
            }
            (true, None) => {
                return Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "`{field_ident}: MirJzSON` requires \
                         `#[mirjzson(justification = \"...\")]` on the field. \
                         `MirJzSON` is Djogi's raw / unschemed JSONB escape hatch; every \
                         use site must record why the schema is not represented as a \
                         typed `Jsonb<T>` (e.g. \"payload is externally owned by partner \
                         API\"). For typed schemas, switch the field to `Jsonb<YourSchema>`."
                    ),
                ));
            }
            (false, _) => {
                // Already handled inside the loop — a `#[mirjzson(...)]`
                // on a non-MirJzSON field would have errored before
                // reaching this match arm.
            }
        }
    }
    Ok(out)
}

/// Parse the argument list inside `#[mirjzson(...)]`. Returns the
/// validated [`MirJzSONAttr`].
fn parse_mirjzson_args(
    args: &Punctuated<Meta, Token![,]>,
    attr_for_span: &syn::Attribute,
) -> syn::Result<MirJzSONAttr> {
    let mut justification: Option<(LitStr, String)> = None;
    for meta in args {
        match meta {
            // `justification = "..."` — required, string-literal-only.
            Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: Lit::Str(lit_str),
                        ..
                    }),
                ..
            }) if path.is_ident("justification") => {
                if justification.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `justification = \"...\"` key in `#[mirjzson(...)]`",
                    ));
                }
                justification = Some((lit_str.clone(), lit_str.value()));
            }
            Meta::NameValue(nv) if nv.path.is_ident("justification") => {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "`justification = ...` value must be a string literal — \
                     e.g. `#[mirjzson(justification = \"payload is externally \
                     owned by partner API\")]`",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported key in `#[mirjzson(...)]`; only \
                     `justification = \"...\"` is accepted",
                ));
            }
        }
    }
    let (lit_str, value) = justification.ok_or_else(|| {
        syn::Error::new_spanned(
            attr_for_span,
            "`#[mirjzson(...)]` requires `justification = \"...\"` — \
             e.g. `#[mirjzson(justification = \"payload schema is owned by \
             the upstream partner SDK\")]`",
        )
    })?;
    validate_justification(&lit_str, &value)?;
    Ok(MirJzSONAttr {
        justification: value,
    })
}

/// Lowercase-ASCII placeholder tokens that fail the justification gate.
/// Kept short, deterministic, and easy to extend. The matcher uses
/// [`str::eq_ignore_ascii_case`] against the WHOLE trimmed value, so
/// adopters who write a real explanation that happens to CONTAIN one
/// of these tokens (e.g. "TODO is the wrong word here, payload is...")
/// still pass.
/// Slice held sorted by hand for human readability; ordering does not
/// affect correctness since the matcher is linear-search-with-equality.
const PLACEHOLDER_JUSTIFICATIONS: &[&str] = &[
    "?",
    "??",
    "???",
    ".",
    "..",
    "...",
    "-",
    "--",
    "explained elsewhere",
    "explained later",
    "external",
    "fix me",
    "fixme",
    "later",
    "n/a",
    "na",
    "no",
    "none",
    "ok",
    "placeholder",
    "raw",
    "raw json",
    "raw jsonb",
    "see above",
    "see below",
    "see comment",
    "see comments",
    "tba",
    "tbd",
    "test",
    "to be added",
    "to be determined",
    "todo",
    "todo.",
    "tbd.",
    "wip",
    "x",
    "xxx",
    "yes",
];

/// Minimum trimmed byte length for a justification. Anything shorter is
/// effectively single-word and fails the "specific reason" bar.
/// Chosen by hand to admit "external SDK." (12 bytes) — a borderline
/// honest answer — while excluding "external" (8 bytes), "tbd later"
/// (9 bytes), and similar placeholders that slip past the denylist.
const MIN_JUSTIFICATION_BYTES: usize = 12;

/// Verify a justification value is present, non-empty, and not a
/// placeholder.
/// Steps (in order):
/// 1. Trim leading/trailing ASCII whitespace.
/// 2. Reject empty-after-trim with `"justification is empty"`.
/// 3. Reject the trimmed value if it matches one of
///    [`PLACEHOLDER_JUSTIFICATIONS`] under ASCII case-insensitive
///    comparison.
/// 4. Reject when the trimmed length is below
///    [`MIN_JUSTIFICATION_BYTES`] with a "too short" message.
///    Errors carry the `lit_str` span so the diagnostic underlines the
///    adopter's literal, not the enclosing attribute.
fn validate_justification(lit_str: &LitStr, value: &str) -> syn::Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(syn::Error::new_spanned(
            lit_str,
            "`#[mirjzson(justification = \"\")]` — the justification must be a \
             non-empty, specific reason for reaching for `MirJzSON` instead of \
             `Jsonb<T>` (e.g. \"payload is externally owned by partner API\")",
        ));
    }
    if PLACEHOLDER_JUSTIFICATIONS
        .iter()
        .any(|p| trimmed.eq_ignore_ascii_case(p))
    {
        return Err(syn::Error::new_spanned(
            lit_str,
            format!(
                "`#[mirjzson(justification = \"{trimmed}\")]` — that value is a \
                 placeholder, not a specific reason. Replace it with the actual \
                 reason `MirJzSON` is preferable to `Jsonb<T>` for this field \
                 (e.g. \"payload schema is owned by the upstream partner SDK\")"
            ),
        ));
    }
    if trimmed.len() < MIN_JUSTIFICATION_BYTES {
        return Err(syn::Error::new_spanned(
            lit_str,
            format!(
                "`#[mirjzson(justification = \"{trimmed}\")]` is too short — \
                 give a specific reason `MirJzSON` is preferable to `Jsonb<T>` \
                 for this field (minimum {MIN_JUSTIFICATION_BYTES} characters \
                 after trim; the spec example is \"payload is externally owned \
                 by partner API\")"
            ),
        ));
    }
    Ok(())
}

/// `true` when `ty` is `MirJzSON` or `Option<MirJzSON>` in any of the
/// accepted path forms.
/// Mirrors the last-segment matcher used by [`super::attrs::detect_relation`]
/// and the `Jsonb<T>` recognizer in
/// [`super::attrs::rust_type_to_sql`]: a path is recognised by its last
/// segment ident (`MirJzSON`) regardless of leading prefix
/// (`djogi::`, `djogi::jsonb::`, `crate::`, `super::`, `::djogi::*`).
/// `Option<…>` is stripped exactly once via
/// [`super::attrs::unwrap_option`] before the inner check runs, so
/// `Option<MirJzSON>` and `MirJzSON` both succeed; `Vec<MirJzSON>`,
/// `Option<Vec<MirJzSON>>`, and similar deeper nestings are rejected.
pub fn is_mirjzson_type(ty: &syn::Type) -> bool {
    let (inner, _nullable) = super::attrs::unwrap_option(ty);
    is_mirjzson_path(&inner)
}

/// Bare-ident form of [`is_mirjzson_type`] — checks ONLY whether the
/// path's last segment is `MirJzSON`. Used by the
/// [`is_mirjzson_type`] helper after stripping one optional layer of
/// `Option<…>`.
fn is_mirjzson_path(ty: &syn::Type) -> bool {
    let syn::Type::Path(syn::TypePath {
        path, qself: None, ..
    }) = ty
    else {
        return false;
    };
    let Some(last) = path.segments.last() else {
        return false;
    };
    // `MirJzSON` is nullary — reject any path segment carrying generic
    // arguments. This locks the matcher to the actual type and rejects
    // hypothetical wrappers like `MirJzSON<T>`.
    if !matches!(last.arguments, syn::PathArguments::None) {
        return false;
    }
    last.ident == "MirJzSON"
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse_quote;

    fn parse_struct(ts: proc_macro2::TokenStream) -> syn::ItemStruct {
        syn::parse2(ts).expect("struct parses")
    }

    #[test]
    fn detects_bare_mirjzson_type() {
        let ty: syn::Type = parse_quote!(MirJzSON);
        assert!(is_mirjzson_type(&ty));
    }

    #[test]
    fn detects_optional_mirjzson_type() {
        let ty: syn::Type = parse_quote!(Option<MirJzSON>);
        assert!(is_mirjzson_type(&ty));
    }

    #[test]
    fn detects_qualified_mirjzson_paths() {
        let bare: syn::Type = parse_quote!(MirJzSON);
        let djogi: syn::Type = parse_quote!(djogi::MirJzSON);
        let djogi_jsonb: syn::Type = parse_quote!(djogi::jsonb::MirJzSON);
        let abs_djogi: syn::Type = parse_quote!(::djogi::MirJzSON);
        let abs_jsonb: syn::Type = parse_quote!(::djogi::jsonb::MirJzSON);
        let crate_rel: syn::Type = parse_quote!(crate::MirJzSON);
        let super_rel: syn::Type = parse_quote!(super::MirJzSON);
        assert!(is_mirjzson_type(&bare));
        assert!(is_mirjzson_type(&djogi));
        assert!(is_mirjzson_type(&djogi_jsonb));
        assert!(is_mirjzson_type(&abs_djogi));
        assert!(is_mirjzson_type(&abs_jsonb));
        assert!(is_mirjzson_type(&crate_rel));
        assert!(is_mirjzson_type(&super_rel));
    }

    #[test]
    fn rejects_lookalikes_and_generics() {
        // Last segment is not `MirJzSON`.
        let other: syn::Type = parse_quote!(MyMirJzSON);
        assert!(!is_mirjzson_type(&other));
        // `MirJzSON` nested inside another generic — last segment is `Vec`.
        let in_vec: syn::Type = parse_quote!(Vec<MirJzSON>);
        assert!(!is_mirjzson_type(&in_vec));
        // `Option<Vec<MirJzSON>>` — outer is Option, inner is Vec.
        let opt_vec: syn::Type = parse_quote!(Option<Vec<MirJzSON>>);
        assert!(!is_mirjzson_type(&opt_vec));
        // Hypothetical `MirJzSON<T>` — the matcher requires no generic args.
        let generic: syn::Type = parse_quote!(MirJzSON<i32>);
        assert!(!is_mirjzson_type(&generic));
    }

    #[test]
    fn rejects_unrelated_types() {
        let s: syn::Type = parse_quote!(String);
        let j: syn::Type = parse_quote!(Jsonb<MySchema>);
        let opt_j: syn::Type = parse_quote!(Option<Jsonb<MySchema>>);
        assert!(!is_mirjzson_type(&s));
        assert!(!is_mirjzson_type(&j));
        assert!(!is_mirjzson_type(&opt_j));
    }

    #[test]
    fn accepts_valid_justification() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "payload is externally owned by partner API")]
                pub payload: MirJzSON,
            }
        });
        let parsed = parse_mirjzson_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.to_string(), "payload");
        assert_eq!(
            parsed[0].1.justification,
            "payload is externally owned by partner API"
        );
    }

    #[test]
    fn accepts_valid_justification_on_optional() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "schema lives in the downstream consumer service")]
                pub maybe_payload: Option<MirJzSON>,
            }
        });
        let parsed = parse_mirjzson_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn rejects_mirjzson_field_without_attribute() {
        let s = parse_struct(quote! {
            struct Post {
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        let msg = err.to_string();
        assert!(
            msg.contains("requires"),
            "expected 'requires' in message, got: {msg}"
        );
        assert!(msg.contains("payload"));
        assert!(msg.contains("MirJzSON"));
    }

    #[test]
    fn rejects_optional_mirjzson_field_without_attribute() {
        let s = parse_struct(quote! {
            struct Post {
                pub maybe: Option<MirJzSON>,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("requires"));
    }

    #[test]
    fn rejects_attribute_on_non_mirjzson_field() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "this should not be allowed on a String field")]
                pub title: String,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        let msg = err.to_string();
        assert!(msg.contains("only valid on"));
        assert!(msg.contains("MirJzSON"));
    }

    #[test]
    fn rejects_empty_justification() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "")]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn rejects_whitespace_only_justification() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "   ")]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn rejects_placeholder_justifications() {
        for raw in [
            "TODO",
            "todo",
            "Tbd",
            "FIXME",
            "wip",
            "n/a",
            "N/A",
            "?",
            "??",
            "...",
            "none",
            "external",
            "raw",
            "see comment",
            "TBA",
        ] {
            let attr_ts: proc_macro2::TokenStream = format!(
                "{{ struct Post {{ \
                  #[mirjzson(justification = {value:?})] \
                  pub payload: MirJzSON, \
                 }} }}",
                value = raw,
            )
            .parse()
            .unwrap();
            // Wrap in a block so syn parses cleanly; pull the struct out.
            let block: syn::Block = syn::parse2(attr_ts).unwrap();
            let item = block
                .stmts
                .into_iter()
                .find_map(|stmt| match stmt {
                    syn::Stmt::Item(syn::Item::Struct(s)) => Some(s),
                    _ => None,
                })
                .unwrap();
            let err = parse_mirjzson_attrs(&item).unwrap_err();
            let err_str = err.to_string();
            assert!(
                err_str.contains("placeholder"),
                "expected `placeholder` rejection for {raw:?}, got: {err_str}"
            );
        }
    }

    #[test]
    fn rejects_too_short_justification() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "short")]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn rejects_bare_attribute() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("requires"));
    }

    #[test]
    fn rejects_unknown_key() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(reason = "payload is externally owned by partner API")]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("unsupported key"));
    }

    #[test]
    fn rejects_non_string_justification() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = 42)]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("string literal"));
    }

    #[test]
    fn rejects_duplicate_attribute() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(justification = "payload is externally owned by partner API")]
                #[mirjzson(justification = "second annotation should be rejected as duplicate")]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn rejects_duplicate_justification_key_within_attribute() {
        let s = parse_struct(quote! {
            struct Post {
                #[mirjzson(
                    justification = "payload is externally owned by partner API",
                    justification = "duplicate key should be rejected within one attribute"
                )]
                pub payload: MirJzSON,
            }
        });
        let err = parse_mirjzson_attrs(&s).expect_err("must error");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn accepts_struct_with_no_mirjzson_fields() {
        let s = parse_struct(quote! {
            struct Post {
                pub title: String,
                pub body: String,
            }
        });
        let parsed = parse_mirjzson_attrs(&s).expect("ok");
        assert!(parsed.is_empty());
    }

    #[test]
    fn accepts_mixed_jsonb_and_mirjzson_fields() {
        let s = parse_struct(quote! {
            struct Post {
                pub typed: Jsonb<MySchema>,
                #[mirjzson(justification = "raw audit blob with shape varying per row")]
                pub raw: MirJzSON,
            }
        });
        let parsed = parse_mirjzson_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.to_string(), "raw");
    }
}
