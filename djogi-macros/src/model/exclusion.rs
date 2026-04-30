//! `#[model(exclusion(...))]` grammar — Phase 7.5 PR 7 task 2.
//!
//! Parses model-level `EXCLUDE` constraint declarations and lowers them
//! to `ExclusionConstraintSpec` token-stream literals that land in the
//! `#[model]`-emitted descriptor.
//!
//! # Grammar (per PR 7 plan, §macro surface)
//!
//! ```ignore
//! #[model(
//!     table = "bookings",
//!     exclusion(
//!         name = "no_overlap",
//!         using = "gist",
//!         elements = ["room_id WITH =", "period WITH &&"],
//!         where = "is_active",        // optional
//!         deferrable = true,           // optional, default false
//!         initially_deferred = false   // optional, default false
//!     ),
//!     exclusion(name = "...", using = "btree", elements = ["..."])
//! )]
//! ```
//!
//! Multiple `exclusion(...)` entries per model are collected into a `Vec`
//! and de-duplicated by name at parse time.
//!
//! # Why a hand-rolled parser
//!
//! Same reason as the `indexes(...)` parser sibling: `where` is a Rust
//! keyword and only `Ident::parse_any` accepts it as a key. The remaining
//! grammar (string literals, bool literals, string-array elements) could
//! ride a smaller darling derive, but unifying all keys behind one
//! `ParseStream` walk keeps the diagnostics shape consistent.
//!
//! # Element decomposition
//!
//! Each entry in `elements = [...]` is a string literal of shape
//! `"<expr> WITH <op>"`. The macro splits byte-level on the literal
//! `" WITH "` (uppercase, single space on each side) — exactly two
//! non-empty halves produces an `ExclusionElement`. Anything else
//! (missing delimiter, extra delimiter, empty halves, leading/trailing
//! whitespace artefacts) is a parse error. Per
//! `feedback_no_regex_in_djogi`, no regex engine and no regex notation.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
    Expr, Ident, MetaList, Token,
    ext::IdentExt,
    parse::{Parse, ParseStream, Parser},
    punctuated::Punctuated,
    spanned::Spanned,
};

use crate::syn_util::{require_bool_lit, require_string_lit};

// ---------------------------------------------------------------------------
// IR — intermediate parse tree
// ---------------------------------------------------------------------------

/// One `exclusion(...)` declaration parsed from the `#[model]` attribute
/// list. Lowered to a `ExclusionConstraintSpec` struct-literal token
/// stream by [`emit_exclusion_spec_tokens`].
#[derive(Debug, Clone)]
pub struct ExclusionDecl {
    /// Constraint name (required). ASCII identifier, ≤ 63 bytes.
    pub name: String,
    /// Index method (required), e.g. `"gist"` / `"btree"`. Emitted
    /// verbatim into `EXCLUDE USING <method>`.
    pub using: String,
    /// One element per `WITH` pair in declaration order.
    pub elements: Vec<ExclusionElementDecl>,
    /// Optional `WHERE` predicate.
    pub where_clause: Option<String>,
    /// `true` emits `DEFERRABLE`; default `false`.
    pub deferrable: bool,
    /// `true` emits `INITIALLY DEFERRED`; only meaningful when
    /// `deferrable` is `true`. The macro rejects the inconsistent pairing
    /// at parse time.
    pub initially_deferred: bool,
    /// Span of the `exclusion(` token — pinned to the head of the entry
    /// so error spans land near the user's source.
    pub head_span: Span,
}

/// One `expr WITH op` element split out of an `elements = [...]` entry.
#[derive(Debug, Clone)]
pub struct ExclusionElementDecl {
    pub expr: String,
    pub with_operator: String,
}

// ---------------------------------------------------------------------------
// Parse entry point
// ---------------------------------------------------------------------------

/// Parse one `Meta::List` of shape `exclusion(name = "...", ...)` —
/// returns the populated [`ExclusionDecl`].
///
/// Caller (`ModelAttrs::parse`) accumulates the per-model Vec and runs
/// duplicate-name detection across the whole list.
pub fn parse_exclusion_meta_list(list: &MetaList) -> syn::Result<ExclusionDecl> {
    let head_span = list.path.span();

    let entries: Punctuated<ExclusionBodyEntry, Token![,]> =
        Punctuated::<ExclusionBodyEntry, Token![,]>::parse_terminated
            .parse2(list.tokens.clone())?;

    let mut name: Option<String> = None;
    let mut name_span: Option<Span> = None;
    let mut using: Option<String> = None;
    let mut using_span: Option<Span> = None;
    let mut elements: Option<Vec<ExclusionElementDecl>> = None;
    let mut where_clause: Option<String> = None;
    let mut deferrable: Option<bool> = None;
    let mut deferrable_span: Option<Span> = None;
    let mut initially_deferred: Option<bool> = None;
    let mut initially_deferred_span: Option<Span> = None;

    for ExclusionBodyEntry { key, value } in &entries {
        let key_str = key.unraw().to_string();
        match key_str.as_str() {
            "name" => {
                if name.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `name = \"..\"`"));
                }
                let lit = require_string_lit(value, "name")?;
                let val = lit.value();
                validate_constraint_name(&val, lit.span())?;
                name = Some(val);
                name_span = Some(lit.span());
            }
            "using" => {
                if using.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `using = \"..\"`"));
                }
                let lit = require_string_lit(value, "using")?;
                let val = lit.value();
                if val.is_empty() {
                    return Err(syn::Error::new(
                        lit.span(),
                        "`using = \"\"` is not allowed — index method must be a non-empty \
                         string (e.g. `\"gist\"`, `\"btree\"`)",
                    ));
                }
                using = Some(val);
                using_span = Some(lit.span());
            }
            "elements" => {
                if elements.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `elements = [..]`"));
                }
                elements = Some(parse_elements_array(value)?);
            }
            "where" => {
                if where_clause.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `where = \"..\"`"));
                }
                let lit = require_string_lit(value, "where")?;
                let val = lit.value();
                if val.is_empty() {
                    return Err(syn::Error::new(
                        lit.span(),
                        "`where = \"\"` is not allowed — predicate must be a non-empty SQL \
                         fragment (omit the key entirely if the constraint applies to every row)",
                    ));
                }
                where_clause = Some(val);
            }
            "deferrable" => {
                if deferrable.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `deferrable = ..`"));
                }
                deferrable = Some(require_bool_lit(value, "deferrable")?);
                deferrable_span = Some(key.span());
            }
            "initially_deferred" => {
                if initially_deferred.is_some() {
                    return Err(syn::Error::new_spanned(
                        key,
                        "duplicate `initially_deferred = ..`",
                    ));
                }
                initially_deferred = Some(require_bool_lit(value, "initially_deferred")?);
                initially_deferred_span = Some(key.span());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    key,
                    format!(
                        "unknown key `{other}` inside exclusion(...); \
                         expected one of: name, using, elements, where, \
                         deferrable, initially_deferred"
                    ),
                ));
            }
        }
    }

    let _ = name_span;
    let _ = using_span;

    let name = name.ok_or_else(|| {
        syn::Error::new(
            head_span,
            "exclusion(...) requires `name = \"..\"` — every EXCLUDE constraint must carry an \
             explicit name",
        )
    })?;
    let using = using.ok_or_else(|| {
        syn::Error::new(
            head_span,
            "exclusion(...) requires `using = \"..\"` — index method (e.g. `\"gist\"`) is \
             required",
        )
    })?;
    let elements = elements.ok_or_else(|| {
        syn::Error::new(
            head_span,
            "exclusion(...) requires `elements = [..]` — list at least one `\"<expr> WITH <op>\"` \
             entry",
        )
    })?;
    if elements.is_empty() {
        return Err(syn::Error::new(
            head_span,
            "`elements = []` is not allowed — list at least one `\"<expr> WITH <op>\"` entry",
        ));
    }

    let deferrable = deferrable.unwrap_or(false);
    let initially_deferred = initially_deferred.unwrap_or(false);

    if initially_deferred && !deferrable {
        let span = initially_deferred_span
            .or(deferrable_span)
            .unwrap_or(head_span);
        return Err(syn::Error::new(
            span,
            "`initially_deferred = true` requires `deferrable = true` on the same exclusion(...) \
             entry — INITIALLY DEFERRED is meaningless on a non-deferrable constraint",
        ));
    }

    Ok(ExclusionDecl {
        name,
        using,
        elements,
        where_clause,
        deferrable,
        initially_deferred,
        head_span,
    })
}

/// One `key = value` entry inside an `exclusion(...)` body. Uses
/// `Ident::parse_any` so the Rust keyword `where` can appear as a key.
struct ExclusionBodyEntry {
    key: Ident,
    value: Expr,
}

impl Parse for ExclusionBodyEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key = Ident::parse_any(input)?;
        input.parse::<Token![=]>()?;
        let value: Expr = input.parse()?;
        Ok(Self { key, value })
    }
}

fn parse_elements_array(value: &Expr) -> syn::Result<Vec<ExclusionElementDecl>> {
    let Expr::Array(arr) = value else {
        return Err(syn::Error::new_spanned(
            value,
            "`elements` must be a bracketed list of strings, e.g. \
             `elements = [\"room_id WITH =\", \"period WITH &&\"]`",
        ));
    };
    let mut out = Vec::with_capacity(arr.elems.len());
    for elem in &arr.elems {
        let lit = require_string_lit(elem, "elements entry")?;
        let raw = lit.value();
        let element = decompose_element(&raw, lit.span())?;
        out.push(element);
    }
    Ok(out)
}

/// Split `"<expr> WITH <op>"` byte-level on the literal `" WITH "`
/// (uppercase, single space on each side). Exactly one occurrence; both
/// halves must be non-empty after trimming any surrounding whitespace.
fn decompose_element(raw: &str, span: Span) -> syn::Result<ExclusionElementDecl> {
    // `str::matches` byte-walks; collect occurrences to detect
    // multiple-delimiter cases.
    const DELIM: &str = " WITH ";
    let mut positions = Vec::new();
    let bytes = raw.as_bytes();
    let needle = DELIM.as_bytes();
    if bytes.len() >= needle.len() {
        let mut i = 0;
        while i + needle.len() <= bytes.len() {
            if &bytes[i..i + needle.len()] == needle {
                positions.push(i);
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }

    match positions.as_slice() {
        [] => Err(syn::Error::new(
            span,
            format!(
                "exclusion element {raw:?} is missing the ` WITH ` delimiter — \
                 each element must look like `\"<expr> WITH <op>\"`, e.g. \
                 `\"room_id WITH =\"` / `\"period WITH &&\"`"
            ),
        )),
        [pos] => {
            let expr = raw[..*pos].trim();
            let op = raw[*pos + DELIM.len()..].trim();
            if expr.is_empty() {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "exclusion element {raw:?} has an empty expression before ` WITH ` — \
                         left-hand side must be a column reference or expression"
                    ),
                ));
            }
            if op.is_empty() {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "exclusion element {raw:?} has an empty operator after ` WITH ` — \
                         right-hand side must be an operator class member (e.g. `=`, `&&`)"
                    ),
                ));
            }
            Ok(ExclusionElementDecl {
                expr: expr.to_string(),
                with_operator: op.to_string(),
            })
        }
        _ => Err(syn::Error::new(
            span,
            format!(
                "exclusion element {raw:?} contains more than one ` WITH ` delimiter — \
                 each element must split into exactly one expression and one operator"
            ),
        )),
    }
}

/// ASCII-identifier validator: letter or underscore start, alphanumerics
/// or underscores after, ≤ 63 bytes. Used for the `name` key on each
/// `exclusion(...)` entry. Mirrors the shape rule used in `indexes.rs`
/// (and the framework-wide `crate::ident::check_one`); duplicating the
/// rule here keeps the diagnostic focused on the exclusion grammar.
fn validate_constraint_name(s: &str, span: Span) -> syn::Result<()> {
    if s.is_empty() {
        return Err(syn::Error::new(
            span,
            "`name = \"\"` is not allowed — exclusion constraint names must be non-empty",
        ));
    }
    let bytes = s.as_bytes();
    if bytes.len() > 63 {
        return Err(syn::Error::new(
            span,
            format!(
                "`name = \"{s}\"` exceeds the 63-byte Postgres identifier limit ({} bytes)",
                bytes.len()
            ),
        ));
    }
    let first_ok = bytes[0] == b'_' || bytes[0].is_ascii_alphabetic();
    if !first_ok {
        return Err(syn::Error::new(
            span,
            format!("`name = \"{s}\"` must start with an ASCII letter or underscore"),
        ));
    }
    for &b in &bytes[1..] {
        if !(b == b'_' || b.is_ascii_alphanumeric()) {
            return Err(syn::Error::new(
                span,
                format!("`name = \"{s}\"` contains a non-ASCII-alphanumeric / underscore byte"),
            ));
        }
    }
    Ok(())
}

/// Reject duplicate `name` values across a model's exclusion list.
///
/// Caller passes the freshly-parsed Vec; collision diagnostics span on
/// the second occurrence's `head_span` so the user lands on the
/// duplicate, not on the first valid declaration.
pub fn validate_unique_names(decls: &[ExclusionDecl]) -> syn::Result<()> {
    for (i, decl) in decls.iter().enumerate() {
        if decls[..i].iter().any(|prior| prior.name == decl.name) {
            return Err(syn::Error::new(
                decl.head_span,
                format!(
                    "duplicate exclusion `name = \"{}\"` — every EXCLUDE constraint on a model \
                     must have a unique name",
                    decl.name
                ),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Lowering — emit ExclusionConstraintSpec token literal
// ---------------------------------------------------------------------------

/// Lower one [`ExclusionDecl`] into an `ExclusionConstraintSpec`
/// struct-literal token stream. The descriptor emitter wraps the per-decl
/// streams in a `&[ ... ]` slice literal.
pub fn emit_exclusion_spec_tokens(decl: &ExclusionDecl) -> TokenStream {
    let name = decl.name.as_str();
    let using = decl.using.as_str();
    let element_tokens: Vec<TokenStream> = decl
        .elements
        .iter()
        .map(|e| {
            let expr = e.expr.as_str();
            let op = e.with_operator.as_str();
            quote! {
                ::djogi::descriptor::ExclusionElement {
                    expr: #expr,
                    with_operator: #op,
                }
            }
        })
        .collect();
    let where_tokens = match &decl.where_clause {
        Some(s) => {
            let s = s.as_str();
            quote! { ::std::option::Option::Some(#s) }
        }
        None => quote! { ::std::option::Option::None },
    };
    let deferrable = decl.deferrable;
    let initially_deferred = decl.initially_deferred;

    quote! {
        ::djogi::descriptor::ExclusionConstraintSpec {
            name: #name,
            using: #using,
            elements: &[ #(#element_tokens,)* ],
            where_clause: #where_tokens,
            deferrable: #deferrable,
            initially_deferred: #initially_deferred,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use syn::{Meta, parse_quote};

    fn parse_one(tokens: TokenStream) -> syn::Result<ExclusionDecl> {
        let attr: syn::Attribute = parse_quote! { #[exclusion(#tokens)] };
        let Meta::List(ml) = attr.meta else {
            panic!("expected meta list");
        };
        parse_exclusion_meta_list(&ml)
    }

    #[test]
    fn parses_full_declaration() {
        let decl = parse_one(quote! {
            name = "no_overlap",
            using = "gist",
            elements = ["room_id WITH =", "period WITH &&"],
            where = "is_active",
            deferrable = true,
            initially_deferred = true,
        })
        .expect("should parse");
        assert_eq!(decl.name, "no_overlap");
        assert_eq!(decl.using, "gist");
        assert_eq!(decl.elements.len(), 2);
        assert_eq!(decl.elements[0].expr, "room_id");
        assert_eq!(decl.elements[0].with_operator, "=");
        assert_eq!(decl.elements[1].expr, "period");
        assert_eq!(decl.elements[1].with_operator, "&&");
        assert_eq!(decl.where_clause.as_deref(), Some("is_active"));
        assert!(decl.deferrable);
        assert!(decl.initially_deferred);
    }

    #[test]
    fn defaults_omitted_flags_to_false() {
        let decl = parse_one(quote! {
            name = "x",
            using = "btree",
            elements = ["col WITH ="],
        })
        .unwrap();
        assert!(!decl.deferrable);
        assert!(!decl.initially_deferred);
        assert!(decl.where_clause.is_none());
    }

    #[test]
    fn rejects_missing_name() {
        let err = parse_one(quote! {
            using = "gist",
            elements = ["a WITH ="],
        })
        .unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn rejects_missing_using() {
        let err = parse_one(quote! {
            name = "x",
            elements = ["a WITH ="],
        })
        .unwrap_err();
        assert!(err.to_string().contains("using"));
    }

    #[test]
    fn rejects_missing_elements() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
        })
        .unwrap_err();
        assert!(err.to_string().contains("elements"));
    }

    #[test]
    fn rejects_empty_elements_array() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
            elements = [],
        })
        .unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn rejects_element_without_with_delimiter() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
            elements = ["bad_element"],
        })
        .unwrap_err();
        assert!(err.to_string().contains("missing the ` WITH ` delimiter"));
    }

    #[test]
    fn rejects_element_with_extra_with_delimiter() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
            elements = ["a WITH b WITH c"],
        })
        .unwrap_err();
        assert!(err.to_string().contains("more than one"));
    }

    #[test]
    fn rejects_initially_deferred_without_deferrable() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
            elements = ["a WITH ="],
            initially_deferred = true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("requires `deferrable = true`"));
    }

    #[test]
    fn rejects_unknown_key() {
        let err = parse_one(quote! {
            name = "x",
            using = "gist",
            elements = ["a WITH ="],
            wrongkey = true,
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown key `wrongkey`"));
    }

    #[test]
    fn validate_unique_names_catches_duplicates() {
        let a = parse_one(quote! {
            name = "dup",
            using = "gist",
            elements = ["a WITH ="],
        })
        .unwrap();
        let b = parse_one(quote! {
            name = "dup",
            using = "btree",
            elements = ["b WITH ="],
        })
        .unwrap();
        let err = validate_unique_names(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("duplicate exclusion"));
    }

    #[test]
    fn rejects_empty_using_string() {
        let err = parse_one(quote! {
            name = "x",
            using = "",
            elements = ["a WITH ="],
        })
        .unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn rejects_empty_name_string() {
        let err = parse_one(quote! {
            name = "",
            using = "gist",
            elements = ["a WITH ="],
        })
        .unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }
}
