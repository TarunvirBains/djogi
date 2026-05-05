//! Proxy-model attribute parsing — Phase 8β T3.2.
//!
//! Holds the syntactic-only parsers for the three `#[model(...)]` keys
//! that opt a model into proxy semantics:
//!
//! - `proxy_for = ParentType` — bare-identifier path naming the parent
//!   model (whose table this proxy shares).
//! - `default_order = [(field, dir), ...]` — list of `(ident, OrderDir)`
//!   pairs declaring the default ordering applied to every
//!   `QuerySet<ProxyModel>` on construction.
//! - `default_filter = |f| ...` — closure expression lowered to a SQL
//!   fragment by T3.3 and AND-composed into every `QuerySet<ProxyModel>`
//!   via the `Model::default_filter_condition` override (T3.4).
//!
//! T3.2 captures syntactic state only — no SQL lowering, no runtime
//! wiring. The closure body parses as a `syn::ExprClosure`; its single
//! input parameter (`f`) is the user's binding for `{Model}Fields`. T3.3
//! walks the closure body via `syn::visit::Visit` and lowers recognised
//! patterns to a SQL fragment string. T3.4 wires the fragment into
//! `QuerySet<T>::new()` so the filter is AND-composed transparently.
//!
//! # Identifier validation
//!
//! Per `feedback_no_regex_in_djogi` — no regex engine, no regex notation
//! in this module's text. The `proxy_for` identifier validator uses the
//! same byte-level rules as
//! [`crate::ident::check_table_name`]: ASCII letter or underscore first
//! byte, ASCII alphanumerics or underscores after, ≤ 63 bytes — the
//! Postgres unquoted-identifier cap. Validation runs at parse time so
//! the trybuild compile-fail fixtures get span-precise diagnostics
//! before any expansion code runs.
//!
//! # Why bare-identifier `proxy_for` (not string-literal)?
//!
//! Per the lens (`feedback_decision_priorities.md`), bare-identifier
//! catches typos at compile time — the path either resolves to a
//! declared type or rustc emits an unresolved-type error at the
//! emission site. The string-literal alternative would silently accept
//! a typo and only surface the failure at descriptor lookup time.
//! Bare-identifier also matches the existing `pk = HeerIdRecencyBiased`
//! convention (`attrs.rs:836-839`), so the rule "all `key = TypePath`
//! attrs are paths, all `key = "literal"` attrs are strings" stays
//! consistent across the macro surface.

use syn::ExprClosure;

/// Order direction for one entry in `#[model(default_order = [...])]`.
///
/// Mirrors the SQL-side `ASC` / `DESC` modifier without coupling to any
/// `OrderExpr` runtime type at parse time — T3.4 lowers `OrderDir` into
/// the canonical `crate::query::OrderExpr` value when emitting the
/// `Model::default_order_by` override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    /// `ASC` — ascending; the SQL default.
    Asc,
    /// `DESC` — descending. Used when the proxy wants the most-recent
    /// rows first or any other reverse ordering by default.
    Desc,
}

/// Validates a `proxy_for` parent-type identifier.
///
/// The plan-side rule (`cluster-8beta-granular.md` §T3.2) and djogi's
/// no-regex policy both apply here. The byte-level grammar:
///
/// 1. Non-empty.
/// 2. ≤ 63 bytes (Postgres `NAMEDATALEN - 1`).
/// 3. First byte is an ASCII letter (uppercase or lowercase) or
///    underscore.
/// 4. Every remaining byte is ASCII alphanumeric or underscore.
///
/// Spelled out in plain English per `feedback_no_regex_in_djogi`. The
/// byte-level implementation mirrors `check_ident` in
/// `djogi-macros/src/ident.rs` so the safety contract stays in sync
/// with the column / table validators.
///
/// Returns `Err` with a span-precise diagnostic on the offending span
/// when the identifier shape is wrong.
pub fn validate_proxy_for_ident(ident: &syn::Ident) -> syn::Result<()> {
    let s = ident.to_string();
    let bytes = s.as_bytes();

    if bytes.is_empty() {
        return Err(syn::Error::new_spanned(
            ident,
            "`proxy_for = …` parent type name must not be empty",
        ));
    }

    if bytes.len() > 63 {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`proxy_for = …` parent type name {s:?} exceeds 63 bytes \
                 (Postgres unquoted-identifier cap)",
            ),
        ));
    }

    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`proxy_for = …` parent type name {s:?} must start with an \
                 ASCII letter or underscore",
            ),
        ));
    }

    if !bytes
        .iter()
        .skip(1)
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`proxy_for = …` parent type name {s:?} must contain only ASCII \
                 alphanumerics or underscores after the first byte",
            ),
        ));
    }

    Ok(())
}

/// Parse a `default_order = [(field, Asc), (field2, Desc), ...]` list.
///
/// Each list entry is a tuple expression `(ident, dir_ident)` where
/// `dir_ident` is the bare identifier `Asc` or `Desc`. The list itself
/// must be non-empty (an empty `default_order = []` would silently emit
/// no override — surface that as a parse error rather than letting the
/// adopter wonder why their default ordering disappeared).
///
/// Runs at parse time. Field-existence validation (i.e. "does the
/// model actually have a field named `name`?") is deferred to the
/// descriptor emitter at T3.3 / T3.4 because the user-field list is
/// only available at expansion time, not in this attribute-only parser.
pub fn parse_default_order_list(expr: &syn::Expr) -> syn::Result<Vec<(syn::Ident, OrderDir)>> {
    // The expected shape is `[(ident, dir_ident), ...]` — a Rust array
    // expression containing tuple expressions. Anything else is a
    // parse error pointing at the offending span.
    let array = match expr {
        syn::Expr::Array(arr) => arr,
        _ => {
            return Err(syn::Error::new_spanned(
                expr,
                "`default_order = …` value must be an array of \
                 `(field, Asc|Desc)` tuples — e.g. \
                 `default_order = [(name, Asc), (created_at, Desc)]`",
            ));
        }
    };

    if array.elems.is_empty() {
        return Err(syn::Error::new_spanned(
            expr,
            "`default_order = []` is empty — either provide at least one \
             `(field, Asc|Desc)` tuple or omit the attribute entirely",
        ));
    }

    let mut out = Vec::with_capacity(array.elems.len());
    for elem in &array.elems {
        let tuple = match elem {
            syn::Expr::Tuple(t) => t,
            _ => {
                return Err(syn::Error::new_spanned(
                    elem,
                    "each `default_order` entry must be a `(field, Asc|Desc)` \
                     tuple",
                ));
            }
        };

        if tuple.elems.len() != 2 {
            return Err(syn::Error::new_spanned(
                tuple,
                "each `default_order` tuple must have exactly two elements: \
                 `(field, Asc|Desc)`",
            ));
        }

        // First element: the field identifier (an `Expr::Path` with a
        // single-segment ident).
        let field_ident = expect_ident_expr(&tuple.elems[0], "field name")?;

        // Second element: `Asc` or `Desc` — also an `Expr::Path`.
        let dir_ident = expect_ident_expr(&tuple.elems[1], "direction")?;
        let dir = match dir_ident.to_string().as_str() {
            "Asc" => OrderDir::Asc,
            "Desc" => OrderDir::Desc,
            other => {
                return Err(syn::Error::new_spanned(
                    &dir_ident,
                    format!(
                        "expected `Asc` or `Desc` as the direction \
                         identifier; got `{other}`",
                    ),
                ));
            }
        };

        out.push((field_ident, dir));
    }

    Ok(out)
}

/// Parse a `default_filter = |f| <expr>` closure value.
///
/// At T3.2 we validate only the shape: the value must be a single-input
/// closure expression. The closure body is captured verbatim for T3.3 to
/// walk via `syn::visit::Visit` and lower to SQL.
///
/// We accept any closure with exactly one input — the descriptor
/// emitter (T3.3) cross-checks that the input binding matches the
/// model's `{Model}Fields` accessor pattern and that the body uses only
/// the recognised SQL-projectable operations.
pub fn parse_default_filter_closure(expr: &syn::Expr) -> syn::Result<ExprClosure> {
    match expr {
        syn::Expr::Closure(c) => {
            if c.inputs.len() != 1 {
                return Err(syn::Error::new_spanned(
                    c,
                    "`default_filter` closure must take exactly one parameter \
                     (the `{Model}Fields` accessor binding, conventionally `f`)",
                ));
            }
            Ok(c.clone())
        }
        _ => Err(syn::Error::new_spanned(
            expr,
            "`default_filter = …` value must be a closure expression — e.g. \
             `default_filter = |f| f.active.eq(true)`",
        )),
    }
}

/// Helper — extract a single-segment ident from an `Expr::Path`.
///
/// Used by the tuple-element parsers in `parse_default_order_list` to
/// pull the field-name and direction-name idents out of a `(name, Asc)`
/// tuple expression. Multi-segment paths and non-path expressions are
/// rejected with a span-precise error pointing at the offending node.
fn expect_ident_expr(expr: &syn::Expr, role: &str) -> syn::Result<syn::Ident> {
    match expr {
        syn::Expr::Path(p) => p.path.get_ident().cloned().ok_or_else(|| {
            syn::Error::new_spanned(
                p,
                format!(
                    "expected a single-segment identifier for {role}; \
                         got a multi-segment path",
                ),
            )
        }),
        _ => Err(syn::Error::new_spanned(
            expr,
            format!("expected an identifier for {role}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse2;

    /// Bare-identifier `proxy_for = Vehicle` parses cleanly through the
    /// validator. Locks the byte-level grammar against accidental
    /// loosening.
    #[test]
    fn validates_simple_parent_ident() {
        let id: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        assert!(validate_proxy_for_ident(&id).is_ok());
    }

    /// Underscore start is permitted (mirrors Postgres unquoted-ident
    /// grammar — a leading underscore is legal).
    #[test]
    fn validates_underscore_start_ident() {
        let id: syn::Ident = parse2(quote! { _Vehicle }).unwrap();
        assert!(validate_proxy_for_ident(&id).is_ok());
    }

    /// Empty array `default_order = []` is rejected with a span-precise
    /// error — silently emitting no override would be a phantom-bug
    /// surface.
    #[test]
    fn rejects_empty_default_order_list() {
        let expr: syn::Expr = parse2(quote! { [] }).unwrap();
        let err = parse_default_order_list(&expr).expect_err("empty list rejected");
        assert!(err.to_string().contains("empty"));
    }

    /// Single-entry list parses to a one-element vec with the right
    /// direction. Round-trip locks the parser shape.
    #[test]
    fn parses_single_entry_default_order_list() {
        let expr: syn::Expr = parse2(quote! { [(name, Asc)] }).unwrap();
        let parsed = parse_default_order_list(&expr).expect("single entry parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.to_string(), "name");
        assert_eq!(parsed[0].1, OrderDir::Asc);
    }

    /// Multi-entry list preserves source order; both Asc and Desc
    /// resolve correctly.
    #[test]
    fn parses_multi_entry_default_order_list() {
        let expr: syn::Expr = parse2(quote! { [(name, Asc), (created_at, Desc)] }).unwrap();
        let parsed = parse_default_order_list(&expr).expect("multi entries parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0.to_string(), "name");
        assert_eq!(parsed[0].1, OrderDir::Asc);
        assert_eq!(parsed[1].0.to_string(), "created_at");
        assert_eq!(parsed[1].1, OrderDir::Desc);
    }

    /// Non-array RHS (`default_order = name`) is rejected with a
    /// diagnostic pointing at the value.
    #[test]
    fn rejects_non_array_default_order() {
        let expr: syn::Expr = parse2(quote! { name }).unwrap();
        let err = parse_default_order_list(&expr).expect_err("non-array rejected");
        assert!(err.to_string().contains("array"));
    }

    /// Wrong direction identifier (`Up` instead of `Asc`/`Desc`) is
    /// rejected with a diagnostic naming the expected values.
    #[test]
    fn rejects_unknown_direction() {
        let expr: syn::Expr = parse2(quote! { [(name, Up)] }).unwrap();
        let err = parse_default_order_list(&expr).expect_err("unknown direction rejected");
        let msg = err.to_string();
        assert!(msg.contains("Asc"));
        assert!(msg.contains("Desc"));
    }

    /// Tuple with wrong arity (single-element or three-element) is
    /// rejected.
    #[test]
    fn rejects_tuple_with_wrong_arity() {
        let expr: syn::Expr = parse2(quote! { [(name,)] }).unwrap();
        let err = parse_default_order_list(&expr).expect_err("single-element tuple rejected");
        assert!(err.to_string().contains("two elements"));
    }

    /// Single-input closure parses to an `ExprClosure` with one input.
    #[test]
    fn parses_single_input_closure() {
        let expr: syn::Expr = parse2(quote! { |f| f.active.eq(true) }).unwrap();
        let closure = parse_default_filter_closure(&expr).expect("closure parses");
        assert_eq!(closure.inputs.len(), 1);
    }

    /// Zero-input closure rejected — the macro pipeline relies on the
    /// single `f` binding to thread `{Model}Fields` access.
    #[test]
    fn rejects_zero_input_closure() {
        let expr: syn::Expr = parse2(quote! { || true }).unwrap();
        let err = parse_default_filter_closure(&expr).expect_err("zero-input closure rejected");
        assert!(err.to_string().contains("one parameter"));
    }

    /// Two-input closure rejected.
    #[test]
    fn rejects_two_input_closure() {
        let expr: syn::Expr = parse2(quote! { |f, g| true }).unwrap();
        let err = parse_default_filter_closure(&expr).expect_err("two-input closure rejected");
        assert!(err.to_string().contains("one parameter"));
    }

    /// Non-closure RHS rejected.
    #[test]
    fn rejects_non_closure_default_filter() {
        let expr: syn::Expr = parse2(quote! { true }).unwrap();
        let err = parse_default_filter_closure(&expr).expect_err("non-closure rejected");
        assert!(err.to_string().contains("closure"));
    }
}
