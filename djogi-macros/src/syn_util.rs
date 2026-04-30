//! Shared `syn`-level parser helpers used across djogi-macros.
//!
//! This module hosts small attribute-parsing utilities that more than one
//! macro in `djogi-macros` reaches for. Keep the surface narrow — anything
//! macro-specific belongs in the macro's own module. Identifier-shape
//! validation that needs reserved-keyword rejection lives in
//! [`crate::ident`], not here.

use syn::{Expr, ExprLit, Field, Lit, LitStr};

/// Require a string literal at the right-hand side of `key = …`.
///
/// Returns the cloned [`LitStr`] so callers that need the span (most
/// macros do, for downstream error attribution) keep it. Callers that
/// only need the bare string should call `.value()` on the result.
pub(crate) fn require_string_lit(value: &Expr, key: &str) -> syn::Result<LitStr> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Str(s), ..
    }) = value
    {
        Ok(s.clone())
    } else {
        Err(syn::Error::new_spanned(
            value,
            format!("`{key} = …` must be a string literal"),
        ))
    }
}

/// SQL column name for a Rust identifier — strips the `r#` prefix that
/// `Ident::to_string()` carries on raw identifiers (e.g. `r#type` → `type`).
/// Plain idents pass through unchanged.
pub(crate) fn column_name_from_ident(ident: &syn::Ident) -> String {
    let raw = ident.to_string();
    raw.strip_prefix("r#").unwrap_or(&raw).to_string()
}

/// SQL column name for a named-struct field.
///
/// The macro convention: a Rust raw-identifier field (`r#type`) maps to the
/// unprefixed column name (`type`). Plain idents pass through unchanged.
/// Panics if `field` belongs to a tuple/unit struct — callers in this crate
/// guard with `validate_shape` upstream so this is unreachable from the
/// emission path.
pub(crate) fn column_name_from_field(field: &Field) -> String {
    let ident = field.ident.as_ref().expect("only named structs supported");
    column_name_from_ident(ident)
}

/// Require a boolean literal at the right-hand side of `key = …`.
pub(crate) fn require_bool_lit(value: &Expr, key: &str) -> syn::Result<bool> {
    if let Expr::Lit(ExprLit {
        lit: Lit::Bool(b), ..
    }) = value
    {
        Ok(b.value)
    } else {
        Err(syn::Error::new_spanned(
            value,
            format!("`{key}` expects a boolean literal (`true` or `false`)"),
        ))
    }
}
