//! Computed-field attribute parsing — 3.
//! Parses `#[computed(sql = "...")]` annotations on struct fields and
//! captures the per-field metadata (`sql` source, return type, the
//! optional `stored` keyword which is **always rejected** in v0.1.0
//! with a deferral error pointing at).
//! The parser captures syntactic state only — no Rust-side getter emission
//! and no `{Model}Computed` ZST emission. The parser
//! runs alongside the existing `FieldAttrs::parse` walker so adopters
//! can mix `#[field(...)]` and `#[computed(...)]` annotations on the
//! same struct without macro-pipeline interference; the descriptor
//! emitter cross-references computed names against regular
//! field names for collision detection.
//! # Field-level annotation, not struct-level
//! Per the lens (`feedback_decision_priorities.md`, plan §7 #7
//! resolved 2026-05-03): field-level annotation matches canonical
//! Rust derive conventions (`#[serde(skip)]`, `#[doc = "..."]`) and
//! keeps locality of reasoning — the computed declaration sits next
//! to its sibling fields. The struct-level alternative
//! (`#[derive(Model)] #[computed(name = ..., sql = ...)]`) would
//! force adopters to repeat the field name in the attribute and
//! split the computed declaration from its return type.
//! # Stored variant (`#[computed(sql, stored)]`)
//! Always rejected at parse time with an explicit deferral
//! message per `feedback_anchored_deferrals` — the migration differ
//! has not yet accumulated long-running stability evidence post-
//! publish, so generating column DDL from a computed attribute is
//! out of scope for v0.1.0. Adopters who need stored computed
//! columns ship a non-stored computed for now and revisit when the
//! deferral lifts.
//! # No regex
//! Per `feedback_no_regex_in_djogi` — every parser path uses byte-
//! level checks against `syn`-parsed tokens; the SQL fragment is
//! captured verbatim as a `String` and emitted into the descriptor
//! as a `&'static str` literal at expand time. The token-level
//! validation pass walks the SQL string byte-by-byte to confirm
//! every `\w+`-shape token resolves to a declared field, again
//! without regex.

use syn::{Expr, Lit, Meta, MetaNameValue, Token, punctuated::Punctuated};

/// One parsed `#[computed(sql = "...")]` annotation.
/// Captured at field-walk time alongside the regular `FieldAttrs`
/// parser. The field's Rust type (`return_type`) is captured from the
/// declared `syn::Field::ty` so the emitter threads the type through the
/// typed `Expr<T>::__raw_sql_fragment(...)` constructor.
#[derive(Debug, Clone)]
pub struct ComputedAttr {
    /// SQL expression source as written in the attribute.
    pub sql: String,
    /// Rust return type of the computed getter — sourced from the
    /// `syn::Field::ty` of the field carrying the annotation. The emitter
    /// threads it into the typed `Expr<T>::__raw_sql_fragment(...)`
    /// carrier in the `{Model}Computed` accessor emission.
    pub return_type: syn::Type,
}

/// Walk a struct's fields and pull out every `#[computed(sql = "...")]`
/// annotation. Returns a vec of `(field_ident, ComputedAttr)` pairs in
/// declared order — the macro emits the `{Model}Computed` accessors in this
/// order so the descriptor entries and the ZST methods stay aligned.
/// Errors surface span-precise diagnostics on:
/// - The `stored` keyword (rejected with a deferral message).
/// - Empty `sql = ""` (silent-no-op surface; rejected at parse time).
/// - Unknown keys inside `#[computed(...)]` (e.g. `index = ...`).
/// - The bare `#[computed]` form (without the required `sql = "..."`).
/// - A field with both `#[computed(...)]` and `#[field(...)]`
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
            // `stored` — flag form, always rejected with "not yet supported" message.
            Meta::Path(path) if path.is_ident("stored") => {
                return Err(syn::Error::new_spanned(
                    path,
                    "`stored` computed columns are not yet supported — \
                     the migration differ has not yet accumulated 6+ months \
                     of long-running stability evidence, so generating \
                     column DDL from `#[computed(stored)]` is out of scope \
                     for now; ship a non-stored computed (drop the `stored` \
                     keyword)",
                ));
            }
            // Reject `stored = true|false` and any other shape.
            Meta::NameValue(nv) if nv.path.is_ident("stored") => {
                return Err(syn::Error::new_spanned(
                    nv,
                    "`stored` computed columns are not yet supported — \
                     neither `stored` (flag) nor `stored = ...` shapes are \
                     accepted; ship a non-stored computed",
                ));
            }
            Meta::NameValue(nv) if nv.path.is_ident("sql") => {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    "`sql = ...` value must be a string literal — \
                     e.g. `#[computed(sql = \"base_price * 2\")]`",
                ));
            }
            // #225 — `expose = "..."` / `expose(...)` is
            // not accepted inside `#[computed(...)]`. The Path A draft
            // entertained an `expose` sub-key on the model-side
            // computed attribute; Path B (issue #231) reshapes visage
            // exposure into a struct-level `#[derived(...)]` attribute
            // that owns its own SQL + Rust + scopes triple. The two
            // surfaces represent two distinct concepts (model-side
            // virtual/stored column vs. visage-side projection-only
            // entry), and conflating them was exactly the conceptual
            // error the Path B reshape eliminated.
            // The diagnostic surfaces a span-anchored hard rejection
            // (the Path A spec was
            // never adopted publicly so there is no compatibility
            // surface to preserve) with a remediation pointer to
            // `#[derived(...)]` and the visage-derived-fields spec.
            Meta::NameValue(nv) if nv.path.is_ident("expose") => {
                return Err(syn::Error::new_spanned(
                    &nv.path,
                    "`expose = ...` is not accepted inside `#[computed(...)]` — \
                     visage exposure is declared as a struct-level \
                     `#[derived(name, ty, scopes, sql, rust)]` attribute \
                     instead. See docs/spec/visage-derived-fields.md \
                     (issue #225 / #231).",
                ));
            }
            Meta::List(list) if list.path.is_ident("expose") => {
                return Err(syn::Error::new_spanned(
                    &list.path,
                    "`expose(...)` is not accepted inside `#[computed(...)]` — \
                     visage exposure is declared as a struct-level \
                     `#[derived(name, ty, scopes, sql, rust)]` attribute \
                     instead. See docs/spec/visage-derived-fields.md \
                     (issue #225 / #231).",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unsupported key in `#[computed(...)]`; only `sql = \"...\"` \
                     is accepted (`stored` is not yet supported; \
                     `expose` was reshaped to the struct-level `#[derived(...)]` \
                     attribute per issue #225 / #231 — see \
                     docs/spec/visage-derived-fields.md)",
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

// ── Rust-side getter emission (intentionally a no-op) ─────────────
// Earlier shapes of this task emitted one inherent method per
// `#[computed(sql = "...")]` field with an `unimplemented!()` body so
// adopters could "override the stub" with a hand-written
// `pub fn total_price(&self) -> f64 { ... }`. That design rests on a
// false premise: Rust does not allow two inherent methods with the
// same name on the same type. Adding a second
// `pub fn total_price(&self) -> f64 { self.base_price * ... }`
// would be E0201 (duplicate definition), not a silent override.
// The Rust-side getter emission is removed entirely.
// The SQL-side path stays the canonical surface: adopters call
// `Vehicle::computed().total_price()` for an `Expr<f64>` that composes
// in `.annotate()` / `.filter_expr()` / `.order_by()`. For a Rust-side
// computation, adopters write a plain inherent method with any name
// of their choosing — no macro-emitted boilerplate to override.
// Per the lens (`feedback_decision_priorities.md`, plan §7 #8 resolved
// 2026-05-03): production stability + simple-to-use both apply; on
// this decision the deciding consideration is that a home-grown
// arithmetic SQL parser would ship bug-for-bug copies of Postgres
// semantics (rounding divergence between Rust `f64` and Postgres
// `numeric`, NULL-coalescing edge cases, integer overflow semantics).
// Removing the stub honors that resolution — there is no auto-derived
// Rust-side path to be wrong about, and adopters who need one author
// it themselves with the semantics they actually want.

/// No-op (kept as a stable call point for the
/// orchestrator in `model::mod`). Returns an empty token stream
/// regardless of input. The previous shape emitted one
/// `pub fn <field>(&self) -> <T> { unimplemented!() }` per computed
/// field, but that surface conflicted with hand-written getters under
/// E0201 — Rust does not allow two inherent methods with the same
/// name on the same type. The accompanying guide
/// (`docs/guide/computed.md`) explains the call-site pattern.
pub fn emit_rust_getters(
    _struct_name: &syn::Ident,
    _computed_attrs: &[(syn::Ident, ComputedAttr)],
) -> proc_macro2::TokenStream {
    proc_macro2::TokenStream::new()
}

// ── `{Model}Computed` ZST emission for SQL projection ─────────────
// Emits a ZST `{Model}Computed` whose accessors return
// `Expr<V>` (where `V` is the computed field's Rust return type) so
// adopters can use computed fields in `.annotate()`, `.filter_expr()`,
// and `.order_by`. The ZST is
// constructed via `Vehicle::computed()` (an inherent method we also
// emit on `#name`), giving adopters the call-site pattern:
// ```rust
// Vehicle::objects()
// .filter_expr(|_| Vehicle::computed().total_price().gte(Expr::literal(100.0)))
// .fetch_all(ctx).await?;
// ```
// # Why a separate ZST rather than bundling into `T::Fields`
// Plan §7 #10 (resolved 2026-05-03) recommended bundling computed
// accessors directly into `T::Fields` so the call site is
// `f.total_price()` symmetrically with regular fields. That decision
// requires extending `FieldRef` with an internal `Source` enum
// (Column | RawSql), which is a substantial surface-area change
// touching every method. This ships the simpler ZST split for
// v0.1.0 — the call-site difference is `Vehicle::computed()
// .total_price()` vs `f.total_price()`. Adopter ergonomics drift
// slightly but the API surface stays narrow. The Q-Algebra refactor
// is the natural place to bundle if real adopter feedback warrants.

/// Emit the `{Model}Computed` ZST + its accessor methods + the
/// `Vehicle::computed()` inherent constructor.
/// Empty token stream when no computed fields are declared. Non-
/// computed models pay zero emission cost.
pub fn emit_computed_zst(
    struct_name: &syn::Ident,
    computed_attrs: &[(syn::Ident, ComputedAttr)],
) -> proc_macro2::TokenStream {
    if computed_attrs.is_empty() {
        return proc_macro2::TokenStream::new();
    }
    let zst_name = quote::format_ident!("{}Computed", struct_name);
    let accessors: Vec<proc_macro2::TokenStream> = computed_attrs
        .iter()
        .map(|(field_ident, attr)| {
            let return_type = &attr.return_type;
            let sql = &attr.sql;
            let doc_comment = format!(
                " Accessor for the `#[computed(sql = \"{sql}\")]` field.\n\
                 Returns an `Expr<{}>` for use in `.annotate()`, \
                 `.filter_expr()`, and `.order_by()`.",
                quote::quote!(#return_type),
            );
            quote::quote! {
                #[doc = #doc_comment]
                #[allow(clippy::wrong_self_convention)]
                pub fn #field_ident(self) -> ::djogi::expr::Expr<#return_type> {
                    ::djogi::expr::Expr::<#return_type>::__raw_sql_fragment(#sql)
                }
            }
        })
        .collect();
    quote::quote! {
        // {Model}Computed — 5 ZST holding one accessor per
        // computed field. Default + Copy + Clone so adopters can
        // construct it without naming the type.
        #[derive(::core::default::Default, ::core::marker::Copy, ::core::clone::Clone, ::core::fmt::Debug)]
        pub struct #zst_name;

        impl #zst_name {
            #(#accessors)*
        }

        impl #struct_name {
            // Adopter-facing constructor for the computed accessor ZST.
            // Returns the freshly-constructed ZST; the call site is
            // `Vehicle::computed().total_price()` returning `Expr<f64>`.
            #[doc = " Accessor for the model's computed fields."]
            #[doc = ""]
            #[doc = " Returns a `{Model}Computed` ZST whose methods return `Expr<V>`"]
            #[doc = " typed values suitable for `.annotate()`, `.filter_expr()`,"]
            #[doc = " and `.order_by()`. Each method's `V` is the computed field's"]
            #[doc = " declared Rust return type."]
            #[must_use]
            pub fn computed() -> #zst_name {
                #zst_name
            }
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

    /// Rejects the `stored` flag form with a clear "not yet supported" message.
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
        assert!(msg.contains("not yet supported"), "got: {msg}");
    }

    /// `stored = true|false` shape also rejected — same "not yet supported" error.
    #[test]
    fn rejects_computed_stored_assignment() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "x", stored = true)]
                pub stored_value: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("stored = true rejected");
        assert!(err.to_string().contains("not yet supported"));
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

    /// #225 — `expose = "..."` inside `#[computed]`
    /// is rejected with a remediation pointer to `#[derived(...)]`.
    /// (parse-time hard rejection): the Path A draft was
    /// never adopted publicly, so this ships without a deprecation
    /// warning intermediate step.
    #[test]
    fn rejects_expose_assignment_inside_computed_attribute() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2", expose = "public")]
                pub double_price: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("expose= rejected");
        let msg = err.to_string();
        assert!(msg.contains("expose"), "got: {msg}");
        assert!(msg.contains("#[derived"), "got: {msg}");
        assert!(msg.contains("visage-derived-fields"), "got: {msg}");
    }

    /// #225 — `expose(...)` (list form) inside
    /// `#[computed]` is also rejected with the same remediation
    /// pointer. The list form might appear if an adopter copied the
    /// Path A `expose(public, admin)` shape from the earlier draft.
    #[test]
    fn rejects_expose_list_inside_computed_attribute() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2", expose(public, admin))]
                pub double_price: f64,
            }
        });
        let err = parse_computed_attrs(&s).expect_err("expose(...) rejected");
        let msg = err.to_string();
        assert!(msg.contains("expose"), "got: {msg}");
        assert!(msg.contains("#[derived"), "got: {msg}");
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

    /// `emit_rust_getters` is now a no-op. Earlier
    /// shapes emitted `pub fn <field>(&self) -> <T> { unimplemented!() }`
    /// stubs that conflicted with hand-written getters under E0201
    /// (Rust forbids duplicate inherent methods). The canonical
    /// Rust-side path is a plain inherent method written by the
    /// adopter, with any name they choose. The orchestrator still
    /// calls this function so the wiring point stays stable; the body
    /// returns an empty token stream regardless of input.
    #[test]
    fn rust_getters_emit_empty_token_stream() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2")]
                pub double_price: f64,
            }
        });
        let parsed = parse_computed_attrs(&s).expect("ok");
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_rust_getters(&struct_name, &parsed).to_string();
        assert!(
            ts.is_empty(),
            "emit_rust_getters must not emit an inherent impl block, got: {ts}"
        );
    }

    /// Empty input still produces an empty token
    /// stream (regression guard for the no-op contract).
    #[test]
    fn empty_input_emits_empty_token_stream() {
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_rust_getters(&struct_name, &[]).to_string();
        assert!(ts.is_empty());
    }

    /// `emit_computed_zst` emits the `{Model}Computed` ZST
    /// plus accessor methods plus `Vehicle::computed()` constructor.
    /// Each accessor returns `Expr<V>` typed for the field's declared
    /// return type and routes through `Expr::__raw_sql_fragment`.
    #[test]
    fn emits_computed_zst_with_accessors() {
        let s = parse_struct(quote! {
            struct Vehicle {
                #[computed(sql = "base_price * 2")]
                pub double_price: f64,
            }
        });
        let parsed = parse_computed_attrs(&s).expect("ok");
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_computed_zst(&struct_name, &parsed).to_string();
        // ZST declared.
        assert!(ts.contains("struct VehicleComputed"));
        // Accessor method.
        assert!(ts.contains("pub fn double_price"));
        // Routes through `Expr::__raw_sql_fragment` with the SQL.
        assert!(ts.contains("__raw_sql_fragment"));
        assert!(ts.contains("base_price * 2"));
        // Inherent constructor on the parent model.
        assert!(ts.contains("pub fn computed"));
    }

    /// Non-computed model pays zero emission cost.
    #[test]
    fn empty_computed_attrs_skips_zst_emission() {
        let struct_name: syn::Ident = parse2(quote! { Vehicle }).unwrap();
        let ts = emit_computed_zst(&struct_name, &[]).to_string();
        assert!(ts.is_empty());
    }

    /// Multiple computed fields on one struct parse cleanly in declared
    /// order. The emitter relies on the order to keep descriptor
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
