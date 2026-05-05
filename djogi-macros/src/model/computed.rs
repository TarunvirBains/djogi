//! Computed-field attribute parsing — Phase 8β T4.3.
//!
//! Parses `#[computed(sql = "...")]` annotations on struct fields and
//! captures the per-field metadata (`sql` source, return type, the
//! optional `stored` keyword which is **always rejected** in v0.1.0
//! with a deferral error pointing at Phase 8.5).
//!
//! T4.3 captures syntactic state only — no Rust-side getter emission
//! (T4.4) and no `{Model}Computed` ZST emission (T4.5). The parser
//! runs alongside the existing `FieldAttrs::parse` walker so adopters
//! can mix `#[field(...)]` and `#[computed(...)]` annotations on the
//! same struct without macro-pipeline interference; the descriptor
//! emitter (T4.5) cross-references computed names against regular
//! field names for collision detection.
//!
//! # Field-level annotation, not struct-level
//!
//! Per the lens (`feedback_decision_priorities.md`, plan §7 #7
//! resolved 2026-05-03): field-level annotation matches canonical
//! Rust derive conventions (`#[serde(skip)]`, `#[doc = "..."]`) and
//! keeps locality of reasoning — the computed declaration sits next
//! to its sibling fields. The struct-level alternative
//! (`#[derive(Model)] #[computed(name = ..., sql = ...)]`) would
//! force adopters to repeat the field name in the attribute and
//! split the computed declaration from its return type.
//!
//! # Stored variant (`#[computed(sql, stored)]`)
//!
//! Always rejected at parse time with an explicit Phase 8.5 deferral
//! message per `feedback_anchored_deferrals` — the migration differ
//! has not yet accumulated long-running stability evidence post-
//! publish, so generating column DDL from a computed attribute is
//! out of scope for v0.1.0. Adopters who need stored computed
//! columns ship a non-stored computed for now and revisit when the
//! deferral lifts.
//!
//! # No regex
//!
//! Per `feedback_no_regex_in_djogi` — every parser path uses byte-
//! level checks against `syn`-parsed tokens; the SQL fragment is
//! captured verbatim as a `String` and emitted into the descriptor
//! as a `&'static str` literal at expand time. T4.4's token-level
//! validation pass walks the SQL string byte-by-byte to confirm
//! every `\w+`-shape token resolves to a declared field, again
//! without regex.

use syn::{Expr, Lit, Meta, MetaNameValue, Token, punctuated::Punctuated};

/// One parsed `#[computed(sql = "...")]` annotation.
///
/// Captured at field-walk time alongside the regular `FieldAttrs`
/// parser. The field's Rust type (`return_type`) is captured from the
/// declared `syn::Field::ty` so T4.4 wires the auto-emitted Rust-side
/// getter stub signature to match and T4.5 threads the type through
/// the typed `Expr<T>::raw_sql_fragment(...)` constructor.
#[derive(Debug, Clone)]
pub struct ComputedAttr {
    /// SQL expression source as written in the attribute.
    pub sql: String,
    /// Rust return type of the computed getter — sourced from the
    /// `syn::Field::ty` of the field carrying the annotation. T4.4
    /// uses this for the getter stub signature; T4.5 threads it into
    /// the typed `Expr<T>::raw_sql_fragment(...)` carrier in the
    /// `{Model}Computed` accessor emission.
    pub return_type: syn::Type,
}

/// Walk a struct's fields and pull out every `#[computed(sql = "...")]`
/// annotation. Returns a vec of `(field_ident, ComputedAttr)` pairs in
/// declared order — T4.5 emits the `{Model}Computed` accessors in this
/// order so the descriptor entries and the ZST methods stay aligned.
///
/// Errors surface span-precise diagnostics on:
///
/// - The `stored` keyword (rejected with a Phase 8.5 deferral message).
/// - Empty `sql = ""` (silent-no-op surface; rejected at parse time).
/// - Unknown keys inside `#[computed(...)]` (e.g. `index = ...`).
/// - The bare `#[computed]` form (without the required `sql = "..."`).
/// - A field with both `#[computed(...)]` and `#[field(...)]` —
///   computed fields are virtual and must not double up with regular
///   field metadata.
pub fn parse_computed_attrs(
    struct_item: &syn::ItemStruct,
) -> syn::Result<Vec<(syn::Ident, ComputedAttr)>> {
    let mut out = Vec::new();
    for field in &struct_item.fields {
        let Some(field_ident) = field.ident.as_ref() else {
            // Tuple-struct fields have no ident; the macro pipeline
            // rejects tuple/unit structs upstream, but bail safely.
            continue;
        };

        let mut found: Option<ComputedAttr> = None;
        let mut had_field_attr = false;
        for attr in &field.attrs {
            if attr.path().is_ident("field") {
                had_field_attr = true;
                continue;
            }
            if !attr.path().is_ident("computed") {
                continue;
            }
            // `#[computed]` (bare) — no `sql = "..."` payload.
            if matches!(attr.meta, Meta::Path(_)) {
                return Err(syn::Error::new_spanned(
                    attr,
                    "`#[computed]` requires `sql = \"...\"` — e.g. \
                     `#[computed(sql = \"base_price * (1.0 + tax_rate)\")]`",
                ));
            }
            let attr_parsed =
                attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
            let parsed = parse_computed_args(&attr_parsed, attr)?;
            if found.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate `#[computed(...)]` attribute on the same \
                     field — every computed field declares its SQL once",
                ));
            }
            found = Some(ComputedAttr {
                sql: parsed,
                return_type: field.ty.clone(),
            });
        }

        if let Some(attr) = found {
            if had_field_attr {
                return Err(syn::Error::new_spanned(
                    field_ident,
                    "field carries both `#[field(...)]` and `#[computed(...)]` \
                     — computed fields are virtual (no storage) and must not \
                     mix with regular-field metadata; remove one of the \
                     attributes",
                ));
            }
            out.push((field_ident.clone(), attr));
        }
    }
    Ok(out)
}

/// Parse the `sql = "..."` (and reject the `stored` keyword) inside
/// the `#[computed(...)]` attribute argument list.
fn parse_computed_args(
    args: &Punctuated<Meta, Token![,]>,
    attr_for_span: &syn::Attribute,
) -> syn::Result<String> {
    let mut sql: Option<String> = None;
    for meta in args {
        match meta {
            // `sql = "..."` — required key.
            Meta::NameValue(MetaNameValue {
                path,
                value: Expr::Lit(lit_expr),
                ..
            }) if path.is_ident("sql") => {
                let Lit::Str(lit_str) = &lit_expr.lit else {
                    return Err(syn::Error::new_spanned(
                        &lit_expr.lit,
                        "`sql = ...` value must be a string literal — \
                         e.g. `#[computed(sql = \"base_price * 2\")]`",
                    ));
                };
                let value = lit_str.value();
                if value.trim().is_empty() {
                    return Err(syn::Error::new_spanned(
                        lit_str,
                        "`sql = \"\"` is empty — either provide a non-empty \
                         SQL expression or remove the `#[computed(...)]` \
                         attribute",
                    ));
                }
                if sql.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `sql = \"...\"` key in #[computed(...)]",
                    ));
                }
                sql = Some(value);
            }
            // `stored` — flag form, always rejected with deferral message.
            Meta::Path(path) if path.is_ident("stored") => {
                return Err(syn::Error::new_spanned(
                    path,
                    "`stored` computed columns are deferred to Phase 8.5 — \
                     the migration differ has not yet accumulated 6+ months \
                     of long-running stability evidence post-publish, so \
                     generating column DDL from `#[computed(stored)]` is \
                     out of scope for v0.1.0; ship a non-stored computed \
                     (drop the `stored` keyword) and revisit when the \
                     deferral lifts",
                ));
            }
            // Reject `stored = true|false` and any other shape.
            Meta::NameValue(nv) if nv.path.is_ident("stored") => {
                return Err(syn::Error::new_spanned(
                    nv,
                    "`stored` computed columns are deferred to Phase 8.5 — \
                     v0.1.0 accepts neither `stored` (flag) nor \
                     `stored = ...` shapes; ship a non-stored computed",
                ));
            }
            Meta::NameValue(nv) if nv.path.is_ident("sql") => {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "`sql = ...` value must be a string literal — \
                     e.g. `#[computed(sql = \"base_price * 2\")]`",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported key in `#[computed(...)]`; only `sql = \"...\"` \
                     is accepted in v0.1.0 (`stored` is deferred to Phase 8.5)",
                ));
            }
        }
    }
    let sql = sql.ok_or_else(|| {
        syn::Error::new_spanned(
            attr_for_span,
            "`#[computed(...)]` requires `sql = \"...\"` — e.g. \
             `#[computed(sql = \"base_price * (1.0 + tax_rate)\")]`",
        )
    })?;
    Ok(sql)
}

// ── T4.4 — Rust-side getter emission ─────────────────────────────────────
//
// Emits one inherent method per `#[computed(sql = "...")]` field:
//
// ```rust
// impl Vehicle {
//     pub fn total_price(&self) -> f64 {
//         unimplemented!(
//             "Rust-side getter for the `total_price` computed field is \
//              not auto-emitted in v0.1.0. Implement this method by hand \
//              if you need to call it after `fetch_one()` without a \
//              re-query — see docs/guide/computed.md for the manual \
//              evaluation pattern."
//         )
//     }
// }
// ```
//
// # Why `unimplemented!()` instead of auto-deriving an arithmetic
// expression
//
// Per the lens (`feedback_decision_priorities.md`, plan §7 #8 resolved
// 2026-05-03): production stability and simple-to-use both apply; on
// this decision the deciding consideration is that a home-grown
// arithmetic SQL parser would ship bug-for-bug copies of Postgres
// semantics (rounding divergence between Rust `f64` and Postgres
// `numeric`, NULL-coalescing edge cases, integer overflow semantics).
// A failing-loud `unimplemented!()` at runtime forces adopters who
// need the Rust-side path to hand-implement the getter — bounded
// escape hatch instead of silent miscompile. Common case (compute the
// expression server-side via `.annotate()` / `.filter()`) is fully
// supported by T4.5's `{Model}Computed` ZST.
//
// # Why we still emit a method (not skip the getter entirely)
//
// Adopter ergonomics: `vehicle.total_price()` is the natural call
// site, mirroring `vehicle.base_price`. Skipping the auto-emitted
// stub would force every adopter to choose between a hand-written
// inherent impl block (boilerplate) and an awkward `Vehicle::computed()
// .total_price().eval(&vehicle)` style. The stub keeps the call site
// uniform; the `unimplemented!()` body fails loud the first time it
// runs so adopters who actually need the path know to hand-implement.

/// Emit one inherent method per computed field. The body is
/// `unimplemented!(...)` with an actionable diagnostic — every site
/// reaching the call-site method without a hand-written override
/// fails loud at runtime per the lens resolution above.
///
/// Returns the full inherent-impl token stream wrapping every emitted
/// getter for `#name`. Empty token stream when no computed fields are
/// declared.
pub fn emit_rust_getters(
    struct_name: &syn::Ident,
    computed_attrs: &[(syn::Ident, ComputedAttr)],
) -> proc_macro2::TokenStream {
    if computed_attrs.is_empty() {
        return proc_macro2::TokenStream::new();
    }
    let getters: Vec<proc_macro2::TokenStream> = computed_attrs
        .iter()
        .map(|(field_ident, attr)| {
            let return_type = &attr.return_type;
            let sql = &attr.sql;
            let panic_msg = format!(
                "Rust-side getter for the `{field_ident}` computed field is \
                 not auto-emitted in v0.1.0. Implement this method by hand \
                 if you need to call it after `fetch_one()` without a \
                 re-query — see docs/guide/computed.md for the manual \
                 evaluation pattern. SQL expression: {sql}",
            );
            let doc_comment = format!(
                " Auto-emitted Rust-side getter stub for the\n\
                 `#[computed(sql = \"{sql}\")]` field. Emits\n\
                 `unimplemented!()` at v0.1.0 — implement by hand when the\n\
                 Rust-side evaluation path is exercised. The SQL-side path\n\
                 (`.annotate()` / `.filter()` / `.order_by()` via\n\
                 `{}::Computed`) works without a hand-written getter.",
                struct_name,
            );
            quote::quote! {
                #[doc = #doc_comment]
                pub fn #field_ident(&self) -> #return_type {
                    ::std::unimplemented!(#panic_msg)
                }
            }
        })
        .collect();
    quote::quote! {
        impl #struct_name {
            #(#getters)*
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse2;

    fn parse_struct(ts: proc_macro2::TokenStream) -> syn::ItemStruct {
        parse2(ts).expect("struct parses")
    }

    /// Bare-minimum `#[computed(sql = "...")]` parses cleanly with the
    /// captured SQL fragment + the field's Rust return type.
    #[test]
    fn parses_computed_sql_attribute() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2")]
                pub double_price: f64,
            }
        });
        let parsed = parse_computed_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 1);
        let (ident, attr) = &parsed[0];
        assert_eq!(ident.to_string(), "double_price");
        assert_eq!(attr.sql, "base_price * 2");
        // Smoke-check the captured return type carries `f64`.
        // ToTokens runs through `quote::ToTokens`, which `syn::Type`
        // implements; converting to a string lets us assert without
        // cracking open the `Path` shape.
        use quote::ToTokens;
        let ty_str = attr.return_type.to_token_stream().to_string();
        assert_eq!(ty_str, "f64");
    }

    /// `stored` flag rejected with a Phase 8.5 deferral message that
    /// names the future phase explicitly per `feedback_anchored_deferrals`.
    #[test]
    fn rejects_computed_stored_keyword() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "x", stored)]
                pub stored_value: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("stored rejected");
        let msg = err.to_string();
        assert!(msg.contains("Phase 8.5"), "got: {msg}");
        assert!(msg.contains("deferred"), "got: {msg}");
    }

    /// `stored = true|false` shape also rejected — same deferral.
    #[test]
    fn rejects_computed_stored_assignment() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "x", stored = true)]
                pub stored_value: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("stored = true rejected");
        assert!(err.to_string().contains("Phase 8.5"));
    }

    /// Empty SQL string rejected — silent-no-op surface.
    #[test]
    fn rejects_empty_sql_string() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "")]
                pub empty: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("empty sql rejected");
        assert!(err.to_string().contains("empty"));
    }

    /// Bare `#[computed]` without the `sql = "..."` payload rejected.
    #[test]
    fn rejects_bare_computed_attribute() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed]
                pub bad: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("bare attribute rejected");
        assert!(err.to_string().contains("sql"));
    }

    /// Unknown key in `#[computed(...)]` rejected with a list of
    /// supported keys.
    #[test]
    fn rejects_unknown_computed_key() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "x", foo = "bar")]
                pub bad: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("unknown key rejected");
        assert!(err.to_string().contains("unsupported key"));
    }

    /// Mixing `#[field(...)]` and `#[computed(...)]` on the same field
    /// rejected — computed fields are virtual; no field-storage knobs.
    #[test]
    fn rejects_mixed_field_and_computed_attrs() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[field(unique)]
                #[computed(sql = "x")]
                pub bad: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("mix rejected");
        assert!(err.to_string().contains("computed"));
    }

    /// T4.4 — `emit_rust_getters` produces an `impl Vehicle { ... }`
    /// block with one `pub fn` per computed field, return type sourced
    /// from the field's declared Rust type, body `unimplemented!()`.
    #[test]
    fn emits_unimplemented_getter_stubs() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2")]
                pub double_price: f64,
            }
        });
        let parsed = parse_computed_attrs(&s).expect("ok");
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_rust_getters(&struct_name, &parsed).to_string();
        assert!(ts.contains("impl Vehicle"));
        assert!(ts.contains("pub fn double_price"));
        assert!(ts.contains("unimplemented"));
        // The doc comment carries the SQL fragment for adopter
        // discoverability — surface as a string contains check.
        assert!(ts.contains("base_price * 2"));
    }

    /// T4.4 — `emit_rust_getters` returns an empty token stream when
    /// no computed fields are declared. Non-computed models pay zero
    /// emission cost.
    #[test]
    fn empty_input_emits_empty_token_stream() {
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_rust_getters(&struct_name, &[]).to_string();
        assert!(ts.is_empty());
    }

    /// Multiple computed fields on one struct parse cleanly in declared
    /// order. T4.5's emitter relies on the order to keep descriptor
    /// entries and ZST accessors aligned.
    #[test]
    fn preserves_declared_order_across_multiple_computed_fields() {
        let s = parse_struct(quote! {
            struct Vehicle {
                pub base_price: f64,
                #[computed(sql = "base_price * 1.1")]
                pub with_tax: f64,
                pub tax_rate: f64,
                #[computed(sql = "base_price * 2.0")]
                pub double_price: f64,
            }
        });
        let parsed = parse_computed_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0.to_string(), "with_tax");
        assert_eq!(parsed[1].0.to_string(), "double_price");
    }
}
