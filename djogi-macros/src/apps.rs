//! `djogi::apps! { … }` function-like proc macro.
//! Lowers a block of `#[app(...)] pub struct Foo;` declarations into:
//! 1. The unit structs themselves, with every `#[app(...)]` attribute
//! stripped (the emitted struct is plain Rust; all app metadata is
//! captured in the [`App`] impl).
//! 2. One `impl ::djogi::apps::App for <Struct>` per entry, carrying
//! the hidden seal witness required by `App`, with
//! `LABEL` / `DATABASE` / `DESCRIPTOR` associated constants fully
//! resolvable at const-eval time.
//! 3. One `inventory::submit!` of the struct's
//! `::djogi::apps::AppDescriptor` per entry for migration and
//! ledger consumers to discover.
//! 4. A single zero-sized invocation sentinel emitted exactly once
//! per `djogi::apps!` call. Two invocations in the same crate
//! collide on the sentinel's name and rustc raises `duplicate
//! definition`.
//! # Parser
//! Hand-rolled, matching the model-index parser shape. Darling's
//! derive grammar cannot express "a block of top-level item
//! declarations, each with its own attribute" — this is closer to a
//! mini-module-level parser than an attribute form. The parser walks
//! `syn::ParseStream`, accepting any number of
//! `#[app(...)] pub struct Ident;` items. Visibility other than `pub`
//! (including no visibility at all) is accepted verbatim and
//! preserved in the emitted struct; field form other than unit
//! (tuple or named) is rejected with a precise span.
//! # ASCII validation
//! Every label shape check uses byte-level primitives. Default label
//! derivation calls `str::to_ascii_lowercase`; explicit overrides go
//! through `validate_label_shape` before any token emission.
//! # Path routing
//! Every macro-emitted path starts with `::djogi::` per
//! `feedback_macro_path_routing.md`. The macro never references
//! `::heeranjid::*` / `::time::*` / `::inventory::*` directly.

use crate::syn_util::require_string_lit;
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
    Attribute, Ident, LitStr, Meta, Token, Visibility,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
};

// ---------------------------------------------------------------------------
// IR
// ---------------------------------------------------------------------------

/// One app declaration parsed from inside `djogi::apps! { … }`.
#[derive(Debug)]
struct AppDecl {
    /// Visibility token(s) — preserved verbatim in the emitted
    /// struct. Typically `pub`; users may use `pub(crate)` or nothing.
    vis: Visibility,
    /// The unit-struct identifier (e.g. `Vehicles`).
    ident: Ident,
    /// The explicit `label = "…"` override, if any. `None` means "use
    /// the lowercased struct identifier". Kept as `LitStr` so the
    /// original literal's span is preserved for validation diagnostics.
    label_override: Option<LitStr>,
    /// The `database = "…"` target — required. The macro rejects
    /// entries missing this key before any lowering happens.
    database: String,
    /// The prior label this app was renamed from, if any. Set via
    /// `#[app(renamed_from = "old_label")]`. Carries the **old string
    /// label**, not a type — the old type may no longer exist in
    /// source. Mutually exclusive with `tombstone`.
    renamed_from: Option<LitStr>,
    /// Span of `#[app(tombstone)]`, when present. Tombstoned apps
    /// surface `AppDescriptor.tombstone = true`; active models must
    /// not reference them except as historical move metadata.
    tombstone: Option<Span>,
    /// Span of the struct identifier — used to pin derived-label
    /// validation errors back to the user-declared name when no
    /// explicit `label = "…"` override exists.
    ident_span: Span,
}

// ---------------------------------------------------------------------------
// Parser — outer block
// ---------------------------------------------------------------------------

/// Outer block: a sequence of `#[app(...)] <vis> struct Ident;` items.
struct AppsBlock {
    decls: Vec<AppDecl>,
}

impl Parse for AppsBlock {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut decls = Vec::new();
        while !input.is_empty() {
            decls.push(parse_app_decl(input)?);
        }
        Ok(AppsBlock { decls })
    }
}

fn parse_app_decl(input: ParseStream<'_>) -> syn::Result<AppDecl> {
    // One or more outer attributes — at least one `#[app(...)]`
    // is required so the parser can surface the `database` target.
    let attrs: Vec<Attribute> = input.call(Attribute::parse_outer)?;

    let vis: Visibility = input.parse()?;
    input.parse::<Token![struct]>()?;
    let ident: Ident = input.parse()?;
    let ident_span = ident.span();
    // Reject `struct Foo(...)` and `struct Foo { ... }` — apps are
    // zero-sized unit structs only.
    let lookahead = input.lookahead1();
    if lookahead.peek(Token![;]) {
        input.parse::<Token![;]>()?;
    } else if lookahead.peek(syn::token::Paren) || lookahead.peek(syn::token::Brace) {
        return Err(syn::Error::new(
            ident_span,
            format!(
                "`djogi::apps!` declarations must be unit structs — \
                 `pub struct {ident};` with no fields. Tuple and named \
                 structs are not allowed."
            ),
        ));
    } else {
        return Err(lookahead.error());
    }

    let parsed = parse_app_attribute(&attrs, &ident)?;

    Ok(AppDecl {
        vis,
        ident,
        label_override: parsed.label_override,
        database: parsed.database,
        renamed_from: parsed.renamed_from,
        tombstone: parsed.tombstone,
        ident_span,
    })
}

// ---------------------------------------------------------------------------
// Parser — per-decl `#[app(...)]` attribute
// ---------------------------------------------------------------------------

/// Walks the outer-attribute list on one decl, finds the `#[app(...)]`
/// entry, and extracts:
/// - `label = "…"` (optional override),
/// - `database = "…"` (required),
/// - `renamed_from = "old_label"` (optional lifecycle marker),
/// - `tombstone` (optional lifecycle flag).
/// Errors on duplicates, unknown keys, non-string values, or when
/// `renamed_from` and `tombstone` both appear (mutually exclusive
/// a tombstoned app is being retired, not renamed).
struct ParsedAppAttr {
    label_override: Option<LitStr>,
    database: String,
    renamed_from: Option<LitStr>,
    tombstone: Option<Span>,
}

fn parse_app_attribute(attrs: &[Attribute], ident: &Ident) -> syn::Result<ParsedAppAttr> {
    let mut app_attr: Option<&Attribute> = None;
    for attr in attrs {
        if attr.path().is_ident("app") {
            if app_attr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate `#[app(...)]` — each app declaration carries exactly one",
                ));
            }
            app_attr = Some(attr);
            continue;
        }
        // Unknown attributes on an apps entry are noise we refuse to
        // silently swallow — the macro owns the whole surface.
        return Err(syn::Error::new_spanned(
            attr,
            "only `#[app(...)]` attributes are recognised on `djogi::apps!` entries",
        ));
    }

    let Some(app_attr) = app_attr else {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "`pub struct {ident};` inside `djogi::apps!` needs an \
                 `#[app(database = \"…\")]` attribute declaring its database target"
            ),
        ));
    };

    let metas = match &app_attr.meta {
        Meta::List(list) => {
            list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?
        }
        _ => {
            return Err(syn::Error::new_spanned(
                app_attr,
                "`#[app(...)]` requires parenthesised key = value entries",
            ));
        }
    };

    let mut label_override: Option<LitStr> = None;
    let mut database: Option<String> = None;
    let mut renamed_from: Option<LitStr> = None;
    let mut tombstone: Option<Span> = None;

    for meta in &metas {
        match meta {
            Meta::Path(path) if path.is_ident("tombstone") => {
                if tombstone.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `tombstone` flag inside the same `#[app(...)]`",
                    ));
                }
                tombstone = Some(path.span());
                continue;
            }
            Meta::NameValue(nv) => {
                let key = nv.path.get_ident().ok_or_else(|| {
                    syn::Error::new_spanned(
                        &nv.path,
                        "entries inside `#[app(...)]` must be identifier keys",
                    )
                })?;
                if key == "label" {
                    if label_override.is_some() {
                        return Err(syn::Error::new_spanned(
                            key,
                            "duplicate `label = \"…\"` inside the same `#[app(...)]`",
                        ));
                    }
                    label_override = Some(require_string_lit(&nv.value, "label")?);
                } else if key == "database" {
                    if database.is_some() {
                        return Err(syn::Error::new_spanned(
                            key,
                            "duplicate `database = \"…\"` inside the same `#[app(...)]`",
                        ));
                    }
                    database = Some(require_string_lit(&nv.value, "database")?.value());
                } else if key == "renamed_from" {
                    if renamed_from.is_some() {
                        return Err(syn::Error::new_spanned(
                            key,
                            "duplicate `renamed_from = \"…\"` inside the same `#[app(...)]`",
                        ));
                    }
                    renamed_from = Some(require_string_lit(&nv.value, "renamed_from")?);
                } else {
                    return Err(syn::Error::new_spanned(
                        key,
                        format!(
                            "unknown key `{key}` inside `#[app(...)]`; \
                                 expected one of: label, database, renamed_from, tombstone"
                        ),
                    ));
                }
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    meta,
                    "entries inside `#[app(...)]` must be `key = \"value\"` pairs \
                     or the `tombstone` flag",
                ));
            }
        }
    }

    let database = database.ok_or_else(|| {
        syn::Error::new_spanned(
            app_attr,
            format!(
                "`#[app(...)]` on `{ident}` is missing the required \
                 `database = \"…\"` key (e.g. `database = \"main\"`)"
            ),
        )
    })?;

    if let (Some(tombstone_span), Some(_)) = (tombstone, &renamed_from) {
        return Err(syn::Error::new(
            tombstone_span,
            "`tombstone` and `renamed_from` are mutually exclusive — a \
             tombstoned app is being retired, not renamed",
        ));
    }

    // Validate renamed_from label shape because it becomes an
    // AppDescriptor.renamed_from value matched against ledger rows.
    if let Some(rf) = &renamed_from {
        validate_label_shape(&rf.value(), rf.span(), true)?;
    }

    Ok(ParsedAppAttr {
        label_override,
        database,
        renamed_from,
        tombstone,
    })
}

// ---------------------------------------------------------------------------
// Label derivation + §3 ASCII-shape validation
// ---------------------------------------------------------------------------

/// Resolve the effective label for an app decl: explicit override if
/// present, otherwise the struct identifier lowercased byte-by-byte.
/// The explicit path validates the override against §3; the default
/// path assumes `to_ascii_lowercase` of a valid Rust identifier still
/// satisfies §3 (first byte ASCII-letter or `_`, remaining bytes
/// ASCII-alnum or `_`, ≤ 63 bytes). That assumption holds for every
/// valid Rust identifier — rustc already enforces the superset grammar
/// at parse time — but we re-validate defensively anyway so users get
/// a clear diagnostic in the pathological case (e.g. a non-ASCII Rust
/// identifier, which is legal Rust but not a legal Postgres label).
fn derive_label(decl: &AppDecl) -> syn::Result<String> {
    let (label, span) = match &decl.label_override {
        Some(explicit) => (explicit.value(), explicit.span()),
        None => (decl.ident.to_string().to_ascii_lowercase(), decl.ident_span),
    };
    validate_label_shape(&label, span, decl.label_override.is_some())?;
    Ok(label)
}

/// Byte-level ASCII-shape check.
/// Rules:
/// 1. Non-empty.
/// 2. First byte is `b'_'` or `u8::is_ascii_alphabetic`.
/// 3. Remaining bytes are `b'_'` or `u8::is_ascii_alphanumeric`.
/// 4. Total length ≤ 63 bytes (Postgres `NAMEDATALEN - 1`).
/// `via_override` only changes the error message's advice — "add an
/// explicit `#[app(label = \"…\")]`" is only useful when the current
/// failure came from the default-derivation path.
fn validate_label_shape(label: &str, span: Span, via_override: bool) -> syn::Result<()> {
    let bytes = label.as_bytes();
    if bytes.is_empty() {
        return Err(syn::Error::new(span, "app label must be non-empty"));
    }
    if bytes.len() > 63 {
        return Err(syn::Error::new(
            span,
            format!(
                "app label {label:?} is {len} bytes; Postgres identifier limit is 63",
                len = bytes.len()
            ),
        ));
    }
    let first = bytes[0];
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return Err(label_shape_error(label, span, via_override, "first byte"));
    }
    for &b in &bytes[1..] {
        if !(b == b'_' || b.is_ascii_alphanumeric()) {
            return Err(label_shape_error(
                label,
                span,
                via_override,
                "one or more bytes after the first",
            ));
        }
    }
    Ok(())
}

fn label_shape_error(label: &str, span: Span, via_override: bool, which: &str) -> syn::Error {
    let advice = if via_override {
        "explicit labels must be ASCII — first byte a letter or underscore, \
         remaining bytes ASCII alphanumerics or underscores"
    } else {
        "the default label is the struct identifier lowercased; \
         if the identifier is non-ASCII, add an explicit \
         `#[app(label = \"…\")]` with an ASCII-only override"
    };
    syn::Error::new(
        span,
        format!("app label {label:?} has a non-conforming {which}. {advice}"),
    )
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Entry point called from `lib.rs`.
pub fn expand(input: TokenStream) -> TokenStream {
    expand_inner(input).unwrap_or_else(|e| e.to_compile_error())
}

fn expand_inner(input: TokenStream) -> syn::Result<TokenStream> {
    let block: AppsBlock = syn::parse2(input)?;

    // First pass: validate labels + detect duplicate identity-pair
    // collisions within this single invocation so the error lands at
    // macro-expansion time rather than as a post-link descriptor-set
    // conflict. Identity is `(database, label)` — two apps with the
    // same label in *different* databases are legitimate (they map
    // to separate `<database>/<label>/` migration directories per
    // `docs/spec/apps-and-database-domains.md`).
    let mut resolved: Vec<(AppDecl, String)> = Vec::with_capacity(block.decls.len());
    for decl in block.decls {
        let label = derive_label(&decl)?;
        if let Some((prior, _)) = resolved
            .iter()
            .find(|(prior, l)| l == &label && prior.database == decl.database)
        {
            return Err(syn::Error::new(
                decl.ident_span,
                format!(
                    "duplicate app identity (database = {database:?}, label = {label:?}) \
                     — also declared by `{prior_ident}` in the same `djogi::apps!` block",
                    database = decl.database,
                    prior_ident = prior.ident,
                ),
            ));
        }
        resolved.push((decl, label));
    }

    // Second pass: validate `renamed_from` does not target a live
    // label in the *same database* within this invocation. A rename
    // from a label that is still present in the same database would
    // leave two simultaneous claimants on the migration
    // `<database>/<label>/` directory. Cross-database same-label
    // coexistence is explicitly legitimate per the (database, label)
    // identity contract (`main/audit/` and `crud_log/audit/` are
    // distinct apps), so we scope the check accordingly.
    for (decl, _) in &resolved {
        if let Some(rf) = &decl.renamed_from {
            let rf_value = rf.value();
            if resolved
                .iter()
                .any(|(other, l)| l == &rf_value && other.database == decl.database)
            {
                return Err(syn::Error::new(
                    rf.span(),
                    format!(
                        "`renamed_from = {rf_value:?}` targets a label still \
                         declared live in the same database in this \
                         `djogi::apps!` block. A rename retires the old label; \
                         remove the prior app (or its explicit \
                         `label = \"{rf_value}\"`) before renaming onto it.",
                    ),
                ));
            }
        }
    }

    // Third pass: emit the tokens for every decl, then the
    // once-per-crate invocation sentinel.
    let mut out = TokenStream::new();
    for (decl, label) in &resolved {
        out.extend(emit_one(decl, label));
    }
    out.extend(emit_invocation_sentinel());
    Ok(out)
}

fn emit_one(decl: &AppDecl, label: &str) -> TokenStream {
    let AppDecl {
        vis,
        ident,
        database,
        renamed_from,
        tombstone,
        ..
    } = decl;
    let ident_hidden = &ident;
    let label_lit = label;
    let database_lit = database.as_str();
    let tombstone_lit = tombstone.is_some();
    let renamed_from_tokens = match renamed_from {
        Some(rf) => {
            let rf_value = rf.value();
            quote! { ::core::option::Option::Some(#rf_value) }
        }
        None => quote! { ::core::option::Option::None },
    };

    quote! {
        #vis struct #ident_hidden;

        impl ::djogi::apps::App for #ident_hidden {
            const __DJOGI_APP_SEAL: ::djogi::apps::SealToken =
                ::djogi::__private::apps_seal::TOKEN;
            const LABEL: &'static str = #label_lit;
            const DATABASE: &'static str = #database_lit;
            const TOMBSTONE: bool = #tombstone_lit;
            const DESCRIPTOR: ::djogi::apps::AppDescriptor = ::djogi::apps::AppDescriptor {
                label: #label_lit,
                database: #database_lit,
                renamed_from: #renamed_from_tokens,
                tombstone: #tombstone_lit,
            };
        }

        ::djogi::__private::inventory::submit! {
            ::djogi::apps::AppDescriptor {
                label: #label_lit,
                database: #database_lit,
                renamed_from: #renamed_from_tokens,
                tombstone: #tombstone_lit,
            }
        }
    }
}

/// Emit a same-scope duplicate-invocation sentinel.
/// Cross-scope duplicate app identities are validated by
/// [`crate::apps::AppRegistry::all`].
fn emit_invocation_sentinel() -> TokenStream {
    quote! {
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        mod __djogi_apps_invocation_sentinel {}
    }
}

// ---------------------------------------------------------------------------
// Unit tests — label shape + helper parsing. Full round-trips live in
// lihaaf compile_pass / compile_fail fixtures.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::Span;

    fn span() -> Span {
        Span::call_site()
    }

    #[test]
    fn label_shape_accepts_typical_names() {
        validate_label_shape("vehicles", span(), false).unwrap();
        validate_label_shape("fleet_logs", span(), true).unwrap();
        validate_label_shape("_private", span(), true).unwrap();
        validate_label_shape("a1", span(), true).unwrap();
    }

    #[test]
    fn label_shape_rejects_empty() {
        assert!(validate_label_shape("", span(), true).is_err());
    }

    #[test]
    fn label_shape_rejects_leading_digit() {
        assert!(validate_label_shape("1abc", span(), true).is_err());
    }

    #[test]
    fn label_shape_rejects_hyphen() {
        assert!(validate_label_shape("bad-name", span(), true).is_err());
    }

    #[test]
    fn label_shape_rejects_space() {
        assert!(validate_label_shape("bad name", span(), true).is_err());
    }

    #[test]
    fn label_shape_rejects_non_ascii() {
        assert!(validate_label_shape("naïve", span(), true).is_err());
    }

    #[test]
    fn label_shape_rejects_over_63_bytes() {
        let long = "a".repeat(64);
        assert!(validate_label_shape(&long, span(), true).is_err());
    }

    #[test]
    fn label_shape_accepts_exactly_63_bytes() {
        let at_limit = "a".repeat(63);
        validate_label_shape(&at_limit, span(), true).unwrap();
    }
}
