//! Proxy-model attribute parsing and SQL lowering — Phase 8β T3.
//!
//! Holds parsers and the SQL-lowering pass for the three `#[model(...)]`
//! keys that opt a model into proxy semantics:
//!
//! - `proxy_for = ParentType` — bare-identifier path naming the parent
//!   model (whose table this proxy shares).
//! - `default_order = [(field, dir), ...]` — list of `(ident, OrderDir)`
//!   pairs declaring the default ordering applied to every
//!   `QuerySet<ProxyModel>` on construction.
//! - `default_filter = |f| ...` — closure expression lowered to a SQL
//!   fragment and AND-composed into every `QuerySet<ProxyModel>` via the
//!   `Model::default_filter_condition` override.
//!
//! Parsing: `proxy_for`, `default_order`, `default_filter` attribute keys
//! parsed from `#[model(...)]`. Cross-attribute validation rejects orphan
//! `default_order`/`default_filter` without `proxy_for`.
//!
//! SQL lowering: `lower_default_filter_to_sql` walks the closure body via
//! recursive descent over `syn::Expr` and lowers recognised patterns
//! (eq/neq/null/range/literal and/or chains) to a SQL fragment string.
//! Unrecognised patterns surface a span-precise compile error.
//!
//! QuerySet wiring: the lowered SQL fragment feeds into
//! `Model::default_filter_condition` which `QuerySet::new()` AND-composes
//! with any user-supplied filter at queryset construction time.
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

use syn::{Expr, ExprClosure};

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
/// walk via recursive descent and lower to SQL.
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

    /// Exactly 63-byte identifier is accepted — boundary of the Postgres
    /// unquoted-identifier cap (≤ 63 bytes). Locks the `<=` not `<` check.
    #[test]
    fn validates_63_byte_ident() {
        // 63 ASCII letter 'a's — exactly at the cap.
        let name = "a".repeat(63);
        let id: syn::Ident = syn::parse_str(&name).unwrap();
        assert!(
            validate_proxy_for_ident(&id).is_ok(),
            "63-byte ident must be accepted"
        );
    }

    /// 64-byte identifier is rejected — one byte over the Postgres cap.
    /// Locks the off-by-one boundary at the enforcement site.
    #[test]
    fn rejects_64_byte_ident() {
        // 64 ASCII letter 'a's — one over the cap.
        let name = "a".repeat(64);
        let id: syn::Ident = syn::parse_str(&name).unwrap();
        let err = validate_proxy_for_ident(&id).expect_err("64-byte ident rejected");
        assert!(
            err.to_string().contains("exceeds 63 bytes"),
            "error must mention the 63-byte cap"
        );
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

// ── T3.3 — SQL lowering for the captured `default_filter` closure ─────────
//
// Recognises a closed grammar of accessor-based predicates against the
// `{Model}Fields` binding and lowers each into a SQL fragment string that
// becomes the `&'static str` value of `ModelDescriptor.default_filter_sql`.
//
// # Why a closed grammar (not a full Rust expression walker)
//
// Per the lens (`feedback_decision_priorities.md`, `feedback_no_regex_in_djogi.md`)
// and v3 plan §T3.3 §7 #3 (resolved 2026-05-03): inline-only literal RHS
// for v0.1.0. The macro recognises a small, explicit grammar and returns
// span-precise errors for anything outside it. The grammar:
//
// ```text
// pred  := <field>.<op>(<lit>)
//        | <field>.<op>(<lit>, <lit>)               // for `between`
//        | <field>.<unary_op>()                     // for `is_null`/`is_not_null`
//        | <pred>.and_with(<pred>)                  // explicit AND
//        | <pred>.or_with(<pred>)                   // explicit OR
//        | (<pred>)
//
// field := <fbind>.<column_ident>                   // `f.active`, etc.
//
// op    := eq | neq | gt | gte | lt | lte
//        | is_null | is_not_null | between
//
// lit   := bool_lit | int_lit | float_lit | string_lit | null_kw
// ```
//
// Anything else — runtime variable references, unrecognised method names,
// arithmetic, function calls — is rejected with a span-precise error
// pointing the adopter at the offending node.
//
// # Why no `IN (...)` / `LIKE` / regex in the grammar
//
// `In` would require a list-literal lowering and bind-encoded values.
// `Like` shapes carry a wildcard convention that's better reasoned about
// at typed call sites than via raw SQL fragments. Both can be added in a
// later phase once the macro pipeline has grown an `Expr<T>`-typed
// closure walker (T6 cluster 8γ); v0.1.0 ships the equality/range/null
// surface adopters most often want for proxy default filters.
//
// # Output format
//
// A single SQL fragment string suitable for splicing into a WHERE clause
// after AND-composition with user filters. The fragment is parens-wrapped
// at every emission site by the runtime composer (T3.4) — the lowered
// string here never adds outer parens for a single leaf, but DOES wrap
// every binary boolean composition (`AND`, `OR`) in its own parens to
// keep operator precedence stable across user filter composition.

/// Lower a parsed `default_filter` closure body to a SQL fragment.
///
/// Walks the closure's body via match-and-recurse over the grammar
/// described above. The closure's single-input binding — the user's
/// `{Model}Fields` accessor name (conventionally `f`) — is captured at
/// entry so the walker can verify every field reference goes through it.
///
/// Returns the SQL fragment string suitable for embedding as a
/// `&'static str` literal in the emitted descriptor. Returns an error
/// with a span-precise diagnostic when:
///
/// - The closure body uses a runtime-bound (non-literal) RHS.
/// - The accessor binding is mis-spelled or shadowed.
/// - An unrecognised method name appears in the predicate position.
/// - A literal value type is outside the supported set
///   (`bool`/integer/float/string/`null`).
pub fn lower_default_filter_to_sql(closure: &ExprClosure) -> syn::Result<String> {
    // The single input pattern must be a simple ident — the binding the
    // walker uses to recognise field-accessor expressions.
    if closure.inputs.len() != 1 {
        return Err(syn::Error::new_spanned(
            closure,
            "`default_filter` closure must take exactly one parameter \
             (the `{Model}Fields` accessor binding, conventionally `f`)",
        ));
    }
    let f_binding = match &closure.inputs[0] {
        syn::Pat::Ident(p) => p.ident.clone(),
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "`default_filter` closure parameter must be a simple \
                 identifier — e.g. `|f| f.active.eq(true)`",
            ));
        }
    };
    lower_pred_expr(&closure.body, &f_binding)
}

/// Lower a single predicate expression — recurses through `and_with` /
/// `or_with` combinators and bottoms out at field-accessor predicates.
fn lower_pred_expr(expr: &Expr, f_binding: &syn::Ident) -> syn::Result<String> {
    match expr {
        // Parenthesised — descend.
        Expr::Paren(p) => lower_pred_expr(&p.expr, f_binding),

        // Method call — could be a leaf predicate (`f.col.eq(v)`) or a
        // boolean combinator (`<pred>.and_with(<pred>)`).
        Expr::MethodCall(mc) => lower_method_call(mc, f_binding),

        other => Err(syn::Error::new_spanned(
            other,
            "unsupported expression shape in `default_filter` closure; \
             expected a chain of field-accessor predicates combined with \
             `.and_with(...)` / `.or_with(...)` — e.g. \
             `|f| f.active.eq(true).and_with(f.price.gte(100))`",
        )),
    }
}

/// Lower a method call — dispatches between leaf-predicate emission and
/// boolean-combinator recursion based on the method ident.
fn lower_method_call(mc: &syn::ExprMethodCall, f_binding: &syn::Ident) -> syn::Result<String> {
    let method = mc.method.to_string();

    // Boolean combinators — `<pred>.and_with(<pred>)` /
    // `<pred>.or_with(<pred>)`. Recurse on both sides; wrap the result in
    // outer parens so adopter-side AND-composition with `.filter(...)`
    // never drifts on operator precedence (per T3.4 risk note).
    if method == "and_with" || method == "or_with" {
        if mc.args.len() != 1 {
            return Err(syn::Error::new_spanned(
                mc,
                format!(
                    "`{method}` takes exactly one predicate argument \
                     in `default_filter` closures",
                ),
            ));
        }
        let lhs = lower_pred_expr(&mc.receiver, f_binding)?;
        let rhs = lower_pred_expr(&mc.args[0], f_binding)?;
        let op = if method == "and_with" { "AND" } else { "OR" };
        return Ok(format!("({lhs} {op} {rhs})"));
    }

    // Leaf predicate — receiver must be a field accessor `f.<column>`.
    let column = lower_field_accessor(&mc.receiver, f_binding)?;

    match method.as_str() {
        "eq" | "neq" | "gt" | "gte" | "lt" | "lte" => {
            if mc.args.len() != 1 {
                return Err(syn::Error::new_spanned(
                    mc,
                    format!("`{method}` takes exactly one literal argument"),
                ));
            }
            let lit = lower_literal_arg(&mc.args[0])?;
            let sql_op = match method.as_str() {
                "eq" => "=",
                "neq" => "<>",
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                "lte" => "<=",
                _ => unreachable!(),
            };
            Ok(format!("{column} {sql_op} {lit}"))
        }
        "is_null" => {
            if !mc.args.is_empty() {
                return Err(syn::Error::new_spanned(
                    mc,
                    "`is_null` takes no arguments in `default_filter` closures",
                ));
            }
            Ok(format!("{column} IS NULL"))
        }
        "is_not_null" => {
            if !mc.args.is_empty() {
                return Err(syn::Error::new_spanned(
                    mc,
                    "`is_not_null` takes no arguments in `default_filter` closures",
                ));
            }
            Ok(format!("{column} IS NOT NULL"))
        }
        "between" => {
            if mc.args.len() != 2 {
                return Err(syn::Error::new_spanned(
                    mc,
                    "`between` takes exactly two literal arguments \
                     (lower and upper bound)",
                ));
            }
            let lo = lower_literal_arg(&mc.args[0])?;
            let hi = lower_literal_arg(&mc.args[1])?;
            Ok(format!("{column} BETWEEN {lo} AND {hi}"))
        }
        other => Err(syn::Error::new_spanned(
            &mc.method,
            format!(
                "unsupported predicate `{other}` in `default_filter` closure; \
                 supported: eq, neq, gt, gte, lt, lte, is_null, is_not_null, \
                 between, and_with, or_with",
            ),
        )),
    }
}

/// Walk a field-accessor expression `f.<column_ident>` and return the
/// column name. Rejects anything else (chained accessors, non-`f` bindings,
/// computed paths) with a span-precise diagnostic.
fn lower_field_accessor(expr: &Expr, f_binding: &syn::Ident) -> syn::Result<String> {
    match expr {
        Expr::Field(fld) => {
            let base = match &*fld.base {
                Expr::Path(p) => p.path.get_ident().cloned().ok_or_else(|| {
                    syn::Error::new_spanned(
                        &fld.base,
                        "`default_filter` field accessor base must be the \
                         single-ident closure binding (conventionally `f`)",
                    )
                })?,
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "`default_filter` field accessor base must be the \
                         closure's input ident (conventionally `f`); \
                         multi-segment paths are not supported",
                    ));
                }
            };
            if base != *f_binding {
                return Err(syn::Error::new_spanned(
                    &fld.base,
                    format!(
                        "`default_filter` field accessor must use the \
                         closure's input binding `{f_binding}`; got `{base}`",
                    ),
                ));
            }
            let col_ident = match &fld.member {
                syn::Member::Named(id) => id.clone(),
                syn::Member::Unnamed(u) => {
                    return Err(syn::Error::new_spanned(
                        u,
                        "`default_filter` field accessor must use a named \
                         field — tuple-index access is not supported",
                    ));
                }
            };
            // Validate the column name byte-level — same rule as
            // `validate_proxy_for_ident`. Unquoted Postgres identifier:
            // ASCII letter or underscore, alphanumerics or underscores,
            // ≤ 63 bytes. The macro never quotes the column at emission
            // time, so an out-of-grammar name would be a SQL injection
            // surface if it ever reached here. Defense in depth.
            let col = col_ident.to_string();
            let bytes = col.as_bytes();
            if bytes.is_empty() || bytes.len() > 63 {
                return Err(syn::Error::new_spanned(
                    &col_ident,
                    format!(
                        "`default_filter` column name {col:?} length \
                         outside the 1..=63-byte unquoted-identifier range",
                    ),
                ));
            }
            if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
                return Err(syn::Error::new_spanned(
                    &col_ident,
                    format!(
                        "`default_filter` column name {col:?} must start \
                         with an ASCII letter or underscore",
                    ),
                ));
            }
            if !bytes
                .iter()
                .skip(1)
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                return Err(syn::Error::new_spanned(
                    &col_ident,
                    format!(
                        "`default_filter` column name {col:?} must contain \
                         only ASCII alphanumerics or underscores after the \
                         first byte",
                    ),
                ));
            }
            Ok(col)
        }
        other => Err(syn::Error::new_spanned(
            other,
            "`default_filter` predicate receiver must be a single \
             field accessor — e.g. `f.active` or `f.price`",
        )),
    }
}

/// Lower a literal argument to its SQL fragment form.
///
/// Accepts: bool, integer, float, string, the bare `null` ident.
/// Rejects every other expression shape (variable references, function
/// calls, arithmetic, etc.) with a span-precise diagnostic — that's the
/// "no runtime-bound values" rule from v3 line 250 / §7 resolution #3.
///
/// # String escaping
///
/// String literals are rendered as Postgres single-quoted form with
/// embedded single quotes doubled: `O'Brien` becomes `'O''Brien'`. Per
/// the no-regex policy and `feedback_decision_priorities.md`, this is a
/// byte-level scan rather than a fancy escaping helper. ASCII-only
/// strings are accepted; non-ASCII bytes (which would still be valid
/// UTF-8 in Postgres but require careful encoding) emit an error
/// pointing the adopter at a manual `default_filter_condition()` impl.
/// Conservative for v0.1.0 — broaden if production use cases warrant.
fn lower_literal_arg(expr: &Expr) -> syn::Result<String> {
    match expr {
        // `null` as a bare ident — render as the SQL keyword.
        Expr::Path(p) if p.path.is_ident("null") => Ok("NULL".to_string()),

        Expr::Lit(lit_expr) => match &lit_expr.lit {
            syn::Lit::Bool(b) => Ok(if b.value {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }),
            syn::Lit::Int(i) => {
                // Re-emit the raw digits — `LitInt::base10_digits` strips
                // any suffix (e.g. `100i64` → `"100"`). Negative literals
                // flow through the `Expr::Unary(Neg, ...)` arm below; here
                // we only see the magnitude.
                Ok(i.base10_digits().to_string())
            }
            syn::Lit::Float(f) => Ok(f.base10_digits().to_string()),
            syn::Lit::Str(s) => escape_sql_string(&s.value()).map_err(|msg| {
                syn::Error::new_spanned(s, format!("`default_filter` string literal: {msg}"))
            }),
            other => Err(syn::Error::new_spanned(
                other,
                "`default_filter` literal must be one of: bool, integer, \
                 float, string, or the bare keyword `null`",
            )),
        },

        // Negative numeric literal — `Expr::Unary(Neg, Expr::Lit(...))`.
        // Re-emit with the leading minus so the SQL fragment carries
        // the sign through. Other unary ops (Not, Deref) are not allowed
        // in literal position.
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => {
            let inner = lower_literal_arg(&u.expr)?;
            Ok(format!("-{inner}"))
        }

        other => Err(syn::Error::new_spanned(
            other,
            "`default_filter` argument must be an inline literal \
             (bool/integer/float/string/`null`); runtime-bound values \
             are not supported in v0.1.0 — for non-literal RHS, \
             implement `Model::default_filter_condition` by hand",
        )),
    }
}

/// Escape a Rust string for embedding as a single-quoted Postgres string
/// literal. Doubles embedded single quotes; rejects non-ASCII bytes.
///
/// Returns the formatted SQL fragment (`'foo'`, `'O''Brien'`) on success
/// or a plain-English error message on failure (the caller wraps it in
/// a `syn::Error` with a span pointing at the offending literal).
fn escape_sql_string(s: &str) -> Result<String, String> {
    // ASCII-only is the v0.1.0 conservative rule. Non-ASCII strings are
    // valid UTF-8 in Postgres but need careful encoding awareness; defer
    // until production use cases warrant the broader surface.
    for (i, b) in s.bytes().enumerate() {
        if !b.is_ascii() {
            return Err(format!(
                "non-ASCII byte at position {i} (0x{b:02X}); \
                 v0.1.0 accepts ASCII-only strings — for non-ASCII \
                 RHS, implement `Model::default_filter_condition` by hand",
            ));
        }
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    Ok(out)
}

#[cfg(test)]
mod sql_lowering_tests {
    use super::*;
    use quote::quote;
    use syn::parse2;

    fn parse_closure(ts: proc_macro2::TokenStream) -> ExprClosure {
        let expr: Expr = parse2(ts).expect("closure parses");
        match expr {
            Expr::Closure(c) => c,
            other => panic!("expected closure, got {other:?}"),
        }
    }

    /// Equality predicate over a bool literal — `eq(true)` lowers to
    /// `active = TRUE`. Anchors the canonical happy path.
    #[test]
    fn lowers_eq_bool_predicate() {
        let c = parse_closure(quote! { |f| f.active.eq(true) });
        assert_eq!(lower_default_filter_to_sql(&c).unwrap(), "active = TRUE");
    }

    /// Greater-than predicate over an integer literal — `gte(100)` lowers
    /// to `price >= 100`.
    #[test]
    fn lowers_gte_int_predicate() {
        let c = parse_closure(quote! { |f| f.price.gte(100) });
        assert_eq!(lower_default_filter_to_sql(&c).unwrap(), "price >= 100");
    }

    /// Equality on a string literal — single quotes wrap the value,
    /// embedded single quotes double-up.
    #[test]
    fn lowers_string_literal_with_quote_doubling() {
        let c = parse_closure(quote! { |f| f.name.eq("O'Brien") });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "name = 'O''Brien'"
        );
    }

    /// `is_null` lowers to the standard SQL form. No arguments accepted.
    #[test]
    fn lowers_is_null_predicate() {
        let c = parse_closure(quote! { |f| f.deleted_at.is_null() });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "deleted_at IS NULL"
        );
    }

    /// `between` lowers to the standard SQL form with both bounds inline.
    #[test]
    fn lowers_between_predicate() {
        let c = parse_closure(quote! { |f| f.price.between(10, 100) });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "price BETWEEN 10 AND 100"
        );
    }

    /// AND combinator wraps both sides in outer parens for stable
    /// operator precedence under further AND-composition.
    #[test]
    fn lowers_and_combinator() {
        let c = parse_closure(quote! {
            |f| f.active.eq(true).and_with(f.price.gte(100))
        });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "(active = TRUE AND price >= 100)"
        );
    }

    /// OR combinator wraps both sides in outer parens.
    #[test]
    fn lowers_or_combinator() {
        let c = parse_closure(quote! {
            |f| f.active.eq(true).or_with(f.archived.eq(false))
        });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "(active = TRUE OR archived = FALSE)"
        );
    }

    /// Negative integer literal preserves the sign through the lowered
    /// fragment.
    #[test]
    fn lowers_negative_integer_literal() {
        let c = parse_closure(quote! { |f| f.balance.gte(-50) });
        assert_eq!(lower_default_filter_to_sql(&c).unwrap(), "balance >= -50");
    }

    /// Bare `null` ident lowers to the SQL keyword.
    #[test]
    fn lowers_null_keyword() {
        let c = parse_closure(quote! { |f| f.deleted_at.eq(null) });
        assert_eq!(
            lower_default_filter_to_sql(&c).unwrap(),
            "deleted_at = NULL"
        );
    }

    /// Runtime-bound RHS rejected — points the adopter at the manual
    /// `default_filter_condition` escape hatch.
    #[test]
    fn rejects_runtime_bound_value() {
        let c = parse_closure(quote! { |f| f.name.eq(some_var) });
        let err = lower_default_filter_to_sql(&c).expect_err("runtime ref rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("inline literal"),
            "diagnostic must mention the inline-literal rule, got: {msg}",
        );
    }

    /// Unrecognised method name rejected with a list of supported names.
    #[test]
    fn rejects_unsupported_method() {
        let c = parse_closure(quote! { |f| f.name.like("foo") });
        let err = lower_default_filter_to_sql(&c).expect_err("unknown predicate rejected");
        let msg = err.to_string();
        assert!(msg.contains("unsupported predicate"));
    }

    /// Wrong accessor binding (closure declares `g`, body uses `f`)
    /// rejected — guards against typos.
    #[test]
    fn rejects_wrong_accessor_binding() {
        // Body uses `f` but closure binding is `g`.
        let c = parse_closure(quote! { |g| f.active.eq(true) });
        let err = lower_default_filter_to_sql(&c).expect_err("wrong binding rejected");
        assert!(err.to_string().contains("closure's input binding"));
    }

    /// Non-ASCII string literal rejected — points the adopter at the
    /// manual escape hatch for the v0.1.0 conservative rule.
    #[test]
    fn rejects_non_ascii_string_literal() {
        let c = parse_closure(quote! { |f| f.name.eq("café") });
        let err = lower_default_filter_to_sql(&c).expect_err("non-ASCII rejected");
        assert!(err.to_string().contains("non-ASCII"));
    }

    /// 64-byte column name rejected — defense-in-depth byte-level cap
    /// on top of the proxy_for ident validator.
    #[test]
    fn rejects_oversized_column_name() {
        // A field accessor with a 64-character column name.
        let long = "a".repeat(64);
        let body = format!("|f| f.{long}.eq(true)");
        let expr: Expr = syn::parse_str(&body).expect("parses");
        let c = match expr {
            Expr::Closure(c) => c,
            _ => unreachable!(),
        };
        let err = lower_default_filter_to_sql(&c).expect_err("oversized column rejected");
        assert!(err.to_string().contains("63-byte"));
    }
}
