//! Struct-level `#[derived(name, ty, scopes, sql, rust, doc)]` parser.
//! #231 — visage-derived fields, Tier 1 (read-time
//! projection). A derived field is a projection entry on a visage that
//! does not correspond to a model column: it is computed from one or
//! more model columns by a paired SQL expression (evaluated server-side
//! at fetch time) and Rust expression (evaluated in-memory when
//! constructing the visage from a `&Model` reference).
//! Spec: `docs/spec/visage-derived-fields.md`. See [§Declaration] for
//! why parsing lives in `#[model]` (not on `#[derive(Model)]`), and the
//! [error taxonomy] for the `E_DJG_VDF_*` codes the validators emit.
//! # Helper-attribute contract
//! `#[derived(...)]` is a **helper attribute** on `#[derive(Model)]`
//! it is not an independent attribute macro. `derive_model` registers
//! the helper (`attributes(field, derived)`) so rustc accepts the
//! token at parse time; `#[model(...)]` walks `item_struct.attrs` for
//! every outer attribute whose `path()` is `derived`, parses the
//! payload through this module, validates the captured state, and
//! strips the attribute before re-emitting the struct.
//! # No regex
//! Per `feedback_no_regex_in_djogi`: every byte-level check below uses
//! stdlib primitives (`u8::is_ascii_lowercase`, sorted-const-slice
//! `binary_search`). No `regex` / `regex-lite` / `fancy-regex` /
//! `regex-automata` dependency is added.
//! [§Declaration]: ../docs/spec/visage-derived-fields.md#derived-is-a-helper-attribute-not-an-attribute-macro
//! [error taxonomy]: ../docs/spec/visage-derived-fields.md#error-taxonomy

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token, punctuated::Punctuated};

/// One parsed `#[derived(...)]` attribute.
/// Captured during `#[model(...)]` expansion, before the attribute is
/// stripped from the re-emitted struct. The five required keys
/// (`name`, `ty`, `scopes`, `sql`, `rust`) are present at this point;
/// `doc` is `None` when not declared.
#[derive(Debug, Clone)]
pub struct DerivedAttr {
    /// Output field identifier on every scoped visage struct.
    /// Validated against the identifier-shape rules at parse time
    /// (E_DJG_VDF_004 / _005 / _012 / _014).
    pub name: syn::Ident,
    /// Rust type of the output field (the entry's `ty = ...`).
    /// Captured as a `syn::Type` so codegen can splice it into struct
    /// fields, accessor types, and the `where Ty: PartialEq` bound
    /// the parity helper emits.
    pub ty: syn::Type,
    /// Source-order scope identifiers from `scopes = [...]`. At least
    /// one entry; duplicates rejected at parse time
    /// (E_DJG_VDF_013).
    pub scopes: Vec<DerivedScope>,
    /// SQL expression as written in the attribute (verbatim). The
    /// per-row scalar SQL surface; rendered into PROJECTION_LIST with
    /// outer parentheses and an `AS <name>` alias at codegen time.
    pub sql: String,
    /// Span of the SQL string literal — used to anchor SQL-validation
    /// diagnostics (statement separator, leading DDL, `$N` reservation,
    /// aggregate-keyword detection) at the offending source text.
    /// Captured at parse time for the Tier-2 predicate-rendering
    /// path to anchor cross-attribute SQL-side diagnostics; the
    /// current Tier-1 emission renders the SQL fragment into
    /// `PROJECTION_LIST` directly without re-walking the captured
    /// span.
    #[allow(dead_code)]
    pub sql_span: Span,
    /// Rust expression as written in the attribute (verbatim).
    /// Spliced into the `From<&Model>` / `TryFrom<&Model>` init block
    /// for the derived field with a `let model: &Self::Model = src;`
    /// rebind so the adopter writes against `model.<field>` syntax.
    pub rust: String,
    /// Span of the Rust string literal — kept so the codegen pass
    /// can anchor parse-related diagnostics back to the source span.
    #[allow(dead_code)] // wired up in codegen pass below
    pub rust_span: Span,
    /// Optional rustdoc captured from `doc = "..."`. Attached to every
    /// emitted visage struct field via `#[doc = "..."]`.
    pub doc: Option<String>,
    /// Span of the `#[derived(...)]` attribute as a whole — used for
    /// structural diagnostics (e.g. E_DJG_VDF_015 when the host model
    /// has `pk = None`).
    pub attr_span: Span,
}

/// One scope identifier captured from a `scopes = [...]` list.
/// Carries both the lowered string and the original span so the
/// `E_DJG_VDF_013` duplicate diagnostic can anchor at the second
/// occurrence rather than the attribute as a whole.
#[derive(Debug, Clone)]
pub struct DerivedScope {
    /// Canonical lowercase scope key (`"public"` / `"self_view"` /
    /// `"admin"` / `"export"`).
    pub key: &'static str,
    /// Span of the source identifier that produced this scope. Used
    /// for span-precise duplicate diagnostics; held for future
    /// cross-scope analysis (Tier-2 predicate rendering).
    #[allow(dead_code)]
    pub span: Span,
}

/// The four canonical scope identifiers. Kept as a sorted const slice
/// for a `binary_search`-style membership check — no regex, no hash
/// allocation.
const KNOWN_SCOPES: &[&str] = &["admin", "export", "public", "self_view"];

/// Maximum byte length of a derived `name` identifier. Matches the
/// Postgres unquoted-identifier cap that already governs column names
/// (`crate::ident::MAX_IDENT_LEN`) — duplicated here because that const
/// is `pub(crate)` to its file and re-import would add a coupling
/// across modules. The matching identifier-shape rule is enforced by
/// `validate_name_shape` below.
const MAX_DERIVED_NAME_LEN: usize = 63;

/// SQL keyword classes rejected at parse time when they appear as the
/// leading non-whitespace, non-comment token (E_DJG_VDF_007). Sorted
/// lowercase; lookup via `binary_search`.
const LEADING_KEYWORDS: &[&str] = &[
    "alter", "copy", "create", "delete", "drop", "grant", "insert", "merge", "revoke", "truncate",
    "update", "with",
];

/// Aggregate function names rejected by the best-effort guard
/// (E_DJG_VDF_009). Sorted lowercase; lookup via `binary_search`.
/// Token detection is case-insensitive — uppercase forms (`COUNT(`)
/// lowercase to the entries here before lookup.
const AGGREGATE_KEYWORDS: &[&str] = &[
    "array_agg",
    "avg",
    "bit_and",
    "bit_or",
    "bool_and",
    "bool_or",
    "count",
    "every",
    "json_agg",
    "json_object_agg",
    "jsonb_agg",
    "jsonb_object_agg",
    "max",
    "min",
    "multirange_agg",
    "range_agg",
    "string_agg",
    "sum",
    "xmlagg",
];

/// Walk `struct_item.attrs` and collect every parsed
/// `#[derived(...)]` attribute on the host struct.
/// Returns one `DerivedAttr` per attribute, in source order. Errors
/// surface as `syn::Error` with span-precise diagnostics anchored
/// at the offending token. Caller is responsible for stripping the
/// attribute from the re-emitted struct (see
/// `model::mod::expand_inner`).
/// The validations performed here are the structural and SQL-shape
/// checks that do not require visibility of the model's column set
/// or other derived attributes — namely:
/// - Required-key presence ([E_DJG_VDF_001]).
/// - `name` identifier shape ([E_DJG_VDF_004], [E_DJG_VDF_005],
///   [E_DJG_VDF_012], [E_DJG_VDF_014]).
/// - `scopes` membership + per-list duplicate detection
///   ([E_DJG_VDF_006], [E_DJG_VDF_013]).
/// - SQL surface: statement separator, leading DDL/DML, `$N`
///   reservation, aggregate guard ([E_DJG_VDF_007]–[E_DJG_VDF_009]).
///   Cross-attribute checks (column collisions per scope, derived ↔
///   derived collisions, model-level structural rules like `pk = None`
///   incompatibility) run later in `cross_check` once the call site has
///   the model's full attribute set in scope.
///   [E_DJG_VDF_001]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_004]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_005]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_006]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_007]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_008]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_009]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_012]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_013]: ../docs/spec/visage-derived-fields.md#error-taxonomy
///   [E_DJG_VDF_014]: ../docs/spec/visage-derived-fields.md#error-taxonomy
pub fn parse_derived_attrs(struct_item: &syn::ItemStruct) -> syn::Result<Vec<DerivedAttr>> {
    let mut out = Vec::new();
    for attr in &struct_item.attrs {
        if !attr.path().is_ident("derived") {
            continue;
        }
        out.push(parse_one(attr)?);
    }
    Ok(out)
}

fn parse_one(attr: &syn::Attribute) -> syn::Result<DerivedAttr> {
    // `#[derived]` bare form — no payload, no required keys.
    if matches!(attr.meta, Meta::Path(_)) {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[derived]` requires `name`, `ty`, `scopes`, `sql`, and `rust` \
             keys; see docs/spec/visage-derived-fields.md \
             (E_DJG_VDF_001)",
        ));
    }

    let metas = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

    let mut name: Option<syn::Ident> = None;
    let mut ty: Option<syn::Type> = None;
    let mut scopes_raw: Option<(Vec<DerivedScope>, Span)> = None;
    let mut sql: Option<(String, Span)> = None;
    let mut rust: Option<(String, Span)> = None;
    let mut doc: Option<String> = None;

    for meta in &metas {
        match meta {
            // `name = facility_site` — bare identifier on the RHS.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("name") => {
                if name.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `name = ...` key in `#[derived(...)]`",
                    ));
                }
                name = Some(parse_name_value(value)?);
            }
            // `ty = Site` — bare type expression on the RHS, captured
            // as a parsed `syn::Type` so codegen can splice it directly
            // into struct fields and `where`-bound emission.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("ty") => {
                if ty.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `ty = ...` key in `#[derived(...)]`",
                    ));
                }
                ty = Some(parse_ty_value(value)?);
            }
            // `scopes = [public, admin, export]` — bracketed list of
            // bare identifiers. Empty list, unknown identifiers, and
            // per-list duplicates are rejected.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("scopes") => {
                if scopes_raw.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `scopes = [...]` key in `#[derived(...)]`",
                    ));
                }
                let span = value.span_or_call_site();
                scopes_raw = Some((parse_scopes_value(value)?, span));
            }
            // `sql = "..."` — string literal carrying the verbatim SQL
            // expression. Surface validation (separator / keyword / `$N`
            // / aggregate) runs after the full parse loop.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("sql") => {
                if sql.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `sql = \"...\"` key in `#[derived(...)]`",
                    ));
                }
                let (s, span) = parse_string_value(value, "sql")?;
                sql = Some((s, span));
            }
            // `rust = "..."` — string literal carrying the verbatim
            // Rust expression. Captured for the From/TryFrom emission;
            // the expression itself is re-parsed at codegen time to
            // detect fallibility shape.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("rust") => {
                if rust.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `rust = \"...\"` key in `#[derived(...)]`",
                    ));
                }
                let (s, span) = parse_string_value(value, "rust")?;
                rust = Some((s, span));
            }
            // `doc = "..."` — optional. The captured string is the
            // rustdoc body, attached verbatim to the generated visage
            // field via `#[doc = "..."]`.
            Meta::NameValue(MetaNameValue { path, value, .. }) if path.is_ident("doc") => {
                if doc.is_some() {
                    return Err(syn::Error::new_spanned(
                        path,
                        "duplicate `doc = \"...\"` key in `#[derived(...)]`",
                    ));
                }
                let (s, _) = parse_string_value(value, "doc")?;
                doc = Some(s);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "unknown key in `#[derived(...)]`; supported keys: \
                     `name`, `ty`, `scopes`, `sql`, `rust`, `doc`",
                ));
            }
        }
    }

    // Required-key presence — E_DJG_VDF_001.
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(
            attr,
            "`#[derived(...)]` missing required `name` key (E_DJG_VDF_001)",
        )
    })?;
    let ty = ty.ok_or_else(|| {
        syn::Error::new_spanned(
            attr,
            "`#[derived(...)]` missing required `ty` key (E_DJG_VDF_001)",
        )
    })?;
    let (scopes, _scopes_span) = scopes_raw.ok_or_else(|| {
        syn::Error::new_spanned(
            attr,
            "`#[derived(...)]` missing required `scopes = [...]` key (E_DJG_VDF_001)",
        )
    })?;
    let (sql_value, sql_span) = sql.ok_or_else(|| {
        syn::Error::new_spanned(
            attr,
            "`#[derived(...)]` missing required `sql = \"...\"` key (E_DJG_VDF_001)",
        )
    })?;
    let (rust_value, rust_span) = rust.ok_or_else(|| {
        syn::Error::new_spanned(
            attr,
            "`#[derived(...)]` missing required `rust = \"...\"` key (E_DJG_VDF_001)",
        )
    })?;

    // Identifier-shape validation — runs on the captured ident so the
    // diagnostic anchors at the source token (`name = facility_site`).
    validate_name_shape(&name)?;

    // SQL surface validation — leading-keyword, separator, `$N`,
    // aggregate guard. Diagnostics anchor at the string literal.
    validate_sql_surface(&sql_value, sql_span)?;

    Ok(DerivedAttr {
        name,
        ty,
        scopes,
        sql: sql_value,
        sql_span,
        rust: rust_value,
        rust_span,
        doc,
        attr_span: attr.span(),
    })
}

fn parse_name_value(value: &Expr) -> syn::Result<syn::Ident> {
    // Accept either a bare identifier path (the spec's expected form)
    // or a string literal whose contents parse as an identifier
    // (defensive fallback so adopters who reach for the more familiar
    // string form get a parse-time hit rather than a confusing
    // "expected ident" diagnostic). The string-literal route still
    // routes the captured ident through the same shape validator, so
    // grammar enforcement does not weaken.
    match value {
        Expr::Path(p) if p.path.get_ident().is_some() => Ok(p.path.get_ident().unwrap().clone()),
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => {
            let ident: syn::Ident = s.parse().map_err(|_| {
                syn::Error::new_spanned(
                    s,
                    "`name = \"...\"` value is not a valid Rust identifier \
                     (E_DJG_VDF_004)",
                )
            })?;
            Ok(ident)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            "`name = ...` expects a bare identifier (e.g. `name = facility_site`)",
        )),
    }
}

fn parse_ty_value(value: &Expr) -> syn::Result<syn::Type> {
    // Common shape: `ty = Site` parses to `Expr::Path`. We promote the
    // path back into a `syn::Type` via stringification + reparse so
    // generic forms (`ty = Option<Site>`) and qualified paths also
    // work without a custom parser. The reparse never widens the
    // accepted surface — adopter spellings that fail `syn::parse_str`
    // already fail above.
    let tokens = quote::quote! { #value };
    let parsed: syn::Type = syn::parse2(tokens).map_err(|e| {
        syn::Error::new_spanned(
            value,
            format!(
                "`ty = ...` could not be parsed as a Rust type: {e}. \
                 Use a bare type (e.g. `ty = Site` / `ty = Option<Site>`)."
            ),
        )
    })?;
    Ok(parsed)
}

fn parse_scopes_value(value: &Expr) -> syn::Result<Vec<DerivedScope>> {
    // `scopes = [public, admin, export]` — `syn` parses the RHS as
    // `Expr::Array` whose elements are `Expr::Path` ident-only.
    let array = match value {
        Expr::Array(a) => a,
        _ => {
            return Err(syn::Error::new_spanned(
                value,
                "`scopes = [...]` value must be a bracketed list of \
                 identifiers (e.g. `scopes = [public, admin]`)",
            ));
        }
    };

    if array.elems.is_empty() {
        return Err(syn::Error::new_spanned(
            array,
            "`scopes = []` is empty — at least one of `public`, `self_view`, \
             `admin`, `export` is required",
        ));
    }

    let mut out: Vec<DerivedScope> = Vec::with_capacity(array.elems.len());
    for elem in &array.elems {
        let path = match elem {
            Expr::Path(p) => p,
            _ => {
                return Err(syn::Error::new_spanned(
                    elem,
                    "entries in `scopes = [...]` must be bare identifiers",
                ));
            }
        };
        let ident = path.path.get_ident().ok_or_else(|| {
            syn::Error::new_spanned(
                elem,
                "entries in `scopes = [...]` must be a single identifier",
            )
        })?;
        let key_str = ident.to_string();
        let key = match canonical_scope_key(&key_str) {
            Some(k) => k,
            None => {
                return Err(syn::Error::new_spanned(
                    ident,
                    format!(
                        "unknown scope `{key_str}` in `scopes = [...]` — \
                         expected one of `public`, `self_view`, `admin`, `export` \
                         (E_DJG_VDF_006)"
                    ),
                ));
            }
        };
        // Per-list duplicate detection — E_DJG_VDF_013. The post-parse
        // collation deduplicates anyway, but rejecting at parse time
        // keeps the declaration honest and catches copy-paste bugs.
        if out.iter().any(|s| s.key == key) {
            return Err(syn::Error::new_spanned(
                ident,
                format!(
                    "duplicate scope `{key}` in `scopes = [...]` \
                     (E_DJG_VDF_013)"
                ),
            ));
        }
        out.push(DerivedScope {
            key,
            span: ident.span(),
        });
    }
    Ok(out)
}

fn parse_string_value(value: &Expr, key_name: &str) -> syn::Result<(String, Span)> {
    let lit_str = match value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => s,
        _ => {
            return Err(syn::Error::new_spanned(
                value,
                format!("`{key_name} = ...` value must be a string literal"),
            ));
        }
    };
    let v = lit_str.value();
    if v.trim().is_empty() {
        return Err(syn::Error::new_spanned(
            lit_str,
            format!(
                "`{key_name} = \"\"` is empty — either provide a non-empty \
                 expression or remove the `#[derived(...)]` attribute"
            ),
        ));
    }
    Ok((v, lit_str.span()))
}

/// Map a raw scope ident string to the canonical lowercase key. Returns
/// `None` for unknown identifiers — the caller surfaces the
/// E_DJG_VDF_006 diagnostic with the source span.
fn canonical_scope_key(raw: &str) -> Option<&'static str> {
    KNOWN_SCOPES
        .binary_search(&raw)
        .ok()
        .map(|i| KNOWN_SCOPES[i])
}

/// Validate the identifier shape of `name`.
/// Rules enforced (E_DJG_VDF_004 / _005 / _012 / _014):
/// 1. Length 1..=63 bytes.
/// 2. First byte is ASCII lowercase letter or `_`.
/// 3. Every remaining byte is ASCII lowercase letter, digit, or `_`.
/// 4. Not prefixed by `__djogi_` (ASCII case-insensitive).
/// 5. Not a Postgres fully-reserved keyword (reuses the sorted const
///    slice in `crate::ident::RESERVED_KEYWORDS` via the existing
///    `crate::ident::check_one` helper for the keyword-only check).
///    Uppercase bytes have their own diagnostic (E_DJG_VDF_012) so an
///    adopter who reaches for `camelCase` sees the precise rule and not
///    the generic shape rule.
fn validate_name_shape(name: &syn::Ident) -> syn::Result<()> {
    let raw = name.to_string();
    // Strip the `r#` raw-identifier prefix when present (syn renders
    // raw idents as `r#name` in `.to_string()`). The downstream
    // emitter writes the bare identifier into SQL aliases and Rust
    // field names; the raw escape is a Rust-side concern only.
    let stripped = raw.strip_prefix("r#").unwrap_or(raw.as_str());
    let bytes = stripped.as_bytes();
    let span = name.span();

    if bytes.is_empty() {
        return Err(syn::Error::new(
            span,
            "`#[derived]` `name` is empty (E_DJG_VDF_004)",
        ));
    }
    if bytes.len() > MAX_DERIVED_NAME_LEN {
        return Err(syn::Error::new(
            span,
            format!(
                "`#[derived]` `name` `{stripped}` is {len} bytes, exceeding the \
                 {max}-byte Postgres unquoted-identifier cap (E_DJG_VDF_004)",
                len = bytes.len(),
                max = MAX_DERIVED_NAME_LEN,
            ),
        ));
    }

    // Uppercase carve-out: a separate diagnostic anchored at E_DJG_VDF_012
    // catches the common camelCase mistake. Any uppercase byte trips it.
    for &b in bytes {
        if b.is_ascii_uppercase() {
            return Err(syn::Error::new(
                span,
                format!(
                    "`#[derived]` `name` `{stripped}` contains an uppercase byte; \
                     derived aliases must be ASCII lowercase so Postgres's \
                     unquoted-identifier case folding does not silently rename \
                     them (E_DJG_VDF_012). Use snake_case (e.g. `facility_site`)."
                ),
            ));
        }
    }

    // First-byte rule (after the uppercase carve-out): `_` or ASCII
    // lowercase letter.
    let first = bytes[0];
    if !(first == b'_' || first.is_ascii_lowercase()) {
        return Err(syn::Error::new(
            span,
            format!(
                "`#[derived]` `name` `{stripped}` starts with `{first_ch}` — first \
                 byte must be `_` or an ASCII lowercase letter (E_DJG_VDF_004)",
                first_ch = first as char,
            ),
        ));
    }

    // Body bytes: `_`, ASCII lowercase letter, ASCII digit.
    for &b in &bytes[1..] {
        if !(b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit()) {
            return Err(syn::Error::new(
                span,
                format!(
                    "`#[derived]` `name` `{stripped}` contains byte `{ch}` — only \
                     `_`, ASCII lowercase letters, and ASCII digits are permitted \
                     after the first character (E_DJG_VDF_004)",
                    ch = b as char,
                ),
            ));
        }
    }

    // Framework-reserved `__djogi_` prefix — E_DJG_VDF_005. Case-
    // insensitive byte compare (the bytes are already lowercase by
    // this point, but mirror the convention in `crate::ident`).
    const RESERVED_DJOGI_PREFIX: &[u8] = b"__djogi_";
    if bytes.len() >= RESERVED_DJOGI_PREFIX.len()
        && bytes[..RESERVED_DJOGI_PREFIX.len()]
            .iter()
            .zip(RESERVED_DJOGI_PREFIX.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    {
        return Err(syn::Error::new(
            span,
            format!(
                "`#[derived]` `name` `{stripped}` begins with the framework-reserved \
                 `__djogi_` prefix — pick another name (E_DJG_VDF_005)"
            ),
        ));
    }

    // Reserved-keyword check — E_DJG_VDF_014. Routes through the
    // existing `crate::ident::check_one` helper indirectly: we
    // re-run only the keyword half of its rule set so the diagnostic
    // stays anchored at the derived-attribute span (the existing
    // helper returns its own diagnostic prose).
    if is_reserved_postgres_keyword(stripped) {
        return Err(syn::Error::new(
            span,
            format!(
                "`#[derived]` `name` `{stripped}` is a Postgres reserved keyword; \
                 it cannot appear unquoted in generated SQL (E_DJG_VDF_014)"
            ),
        ));
    }

    Ok(())
}

/// Case-insensitive Postgres reserved-keyword check.
/// Mirrors the contract used by `crate::ident::check_one`
/// `binary_search` against a sorted lowercase const slice. The list
/// lives in `crate::ident::RESERVED_KEYWORDS`; importing the same
/// constant would couple this module to a `pub(crate)` symbol that
/// isn't currently re-exported. We re-use the helper through a
/// span-discarding round-trip: build a synthetic ident with the
/// caller's bytes and reuse the existing helper's keyword leg.
fn is_reserved_postgres_keyword(value: &str) -> bool {
    // The existing helper performs four checks (length, first byte,
    // body bytes, reserved keyword). We've already validated the first
    // three on the bytes we hand in, so the only remaining failure
    // mode is the reserved-keyword arm — which is exactly what we
    // want to surface here.
    crate::ident::check_one(value, Span::call_site()).is_err_and(|e| {
        let msg = e.to_string();
        msg.contains("reserved Postgres keyword")
    })
}

/// Validate the surface of the SQL string against the four parse-time
/// guards in the spec (statement separators, leading DDL/DML keyword,
/// reserved `$N` placeholders, aggregate/window guard).
/// The tokeniser is a single-pass byte walker that skips
/// single-quoted strings and dollar-quoted bodies so embedded `;` /
/// `count(` / `$1` inside string literals do not false-positive.
fn validate_sql_surface(sql: &str, span: Span) -> syn::Result<()> {
    // 1. Statement-separator check — E_DJG_VDF_007.
    if contains_unquoted_byte(sql, b';') {
        return Err(syn::Error::new(
            span,
            "derived `sql` contains a `;` statement separator outside string-literal \
             context — derived expressions must be a single per-row scalar \
             (E_DJG_VDF_007)",
        ));
    }

    // 2. Leading DDL/DML keyword — E_DJG_VDF_007. Find the first
    // non-whitespace, non-comment token and compare against the
    // sorted const slice.
    let leading = leading_keyword(sql);
    if let Some(leading) = leading
        && LEADING_KEYWORDS.binary_search(&leading.as_str()).is_ok()
    {
        return Err(syn::Error::new(
            span,
            format!(
                "derived `sql` begins with the DDL/DML keyword `{}` — derived \
                 expressions must be a single per-row scalar (E_DJG_VDF_007)",
                leading.to_ascii_uppercase()
            ),
        ));
    }

    // 3. `$N` placeholder reservation — E_DJG_VDF_008.
    if contains_unquoted_dollar_digit(sql) {
        return Err(syn::Error::new(
            span,
            "derived `sql` contains a `$N` placeholder token — `$1`, `$2`, ... \
             tokens are reserved for future cross-model references and \
             cannot appear in derived expressions in v0.1.0 (E_DJG_VDF_008). \
             If a literal `$N` must appear in the output, route it through \
             `chr(36) || '<digit>'` until proper escaping lands.",
        ));
    }

    // 4. Aggregate / window guard — E_DJG_VDF_009. Best-effort: walk
    // the tokens looking for `<aggregate_name>(` and the bare `OVER`
    // keyword.
    if let Some(hit) = aggregate_or_over_hit(sql) {
        return Err(syn::Error::new(
            span,
            format!(
                "derived `sql` references the aggregate or window construct \
                 `{hit}` — Tier 1 rejects aggregates and window functions in \
                 `#[derived]` `sql` today (derived expressions are per-row \
                 scalars). The future aggregate / window surface is locked but \
                 not yet implemented: Shape Q (QuerySet `.annotate(...)`) and \
                 Shape V (`#[derived(..., aggregate = true)]`); the `aggregate \
                 = true` marker is not accepted by the parser yet \
                 (E_DJG_VDF_009)"
            ),
        ));
    }

    Ok(())
}

/// Walk `sql` byte-by-byte and return `true` if `target` appears
/// outside single-quoted strings and dollar-quoted string bodies.
/// Single-quote handling honours Postgres's `''` (two adjacent
/// quotes) embedded-quote escape. Dollar-quoted bodies use the
/// matching tag the opener declared (`$$` or `$tag$`).
fn contains_unquoted_byte(sql: &str, target: u8) -> bool {
    walk_unquoted_tokens(sql).any(|tok| matches!(tok, SqlTok::Byte(b) if b == target))
}

fn contains_unquoted_dollar_digit(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip single-quoted string.
        if bytes[i] == b'\'' {
            i = skip_single_quoted(bytes, i);
            continue;
        }
        // Skip dollar-quoted body. The dollar-quote tag opener has the
        // shape `$<optional-tag>$` where `<optional-tag>` is an
        // optional identifier-shape sequence followed by another `$`.
        // If the opener parses cleanly, the body runs until the
        // matching closing tag. Anything else (e.g. `$1`) falls
        // through to the placeholder detector below.
        if bytes[i] == b'$' {
            if let Some((tag_len, body_start)) = parse_dollar_open(bytes, i) {
                i = skip_dollar_body(bytes, body_start, &bytes[i..i + tag_len]);
                continue;
            }
            // Detect `$N` — `$` followed by one or more ASCII digits.
            if i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                return true;
            }
        }
        // Skip `--`-style line comment.
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip `/* ... */`-style block comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        i += 1;
    }
    false
}

fn leading_keyword(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    loop {
        // Skip ASCII whitespace.
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        // Skip `--`-style line comment.
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip `/* ... */`-style block comment.
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        break;
    }
    if i >= bytes.len() {
        return None;
    }
    // Read a maximal run of ASCII letter / digit / underscore.
    let start = i;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some(sql[start..i].to_ascii_lowercase())
}

fn aggregate_or_over_hit(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip quoted contexts.
        if bytes[i] == b'\'' {
            i = skip_single_quoted(bytes, i);
            continue;
        }
        if bytes[i] == b'$'
            && let Some((tag_len, body_start)) = parse_dollar_open(bytes, i)
        {
            i = skip_dollar_body(bytes, body_start, &bytes[i..i + tag_len]);
            continue;
        }
        // Skip line / block comments.
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // Read identifier-shape tokens. We require the previous byte
        // (if any) to NOT be alphanumeric/underscore so an embedded
        // `count` inside a longer identifier (`countdown`) does not
        // false-positive.
        let is_word_start = bytes[i].is_ascii_alphabetic() || bytes[i] == b'_';
        let prev_is_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if is_word_start && !prev_is_word {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word: String = sql[start..i].to_ascii_lowercase();
            // Aggregate detection: word + zero-or-more whitespace + `(`.
            let mut peek = i;
            while peek < bytes.len() && bytes[peek].is_ascii_whitespace() {
                peek += 1;
            }
            let followed_by_open = peek < bytes.len() && bytes[peek] == b'(';
            if followed_by_open && AGGREGATE_KEYWORDS.binary_search(&word.as_str()).is_ok() {
                return Some(word.to_ascii_uppercase());
            }
            // `OVER` keyword detection — case-insensitive bare keyword,
            // typically followed by `(` or `<window_name>`.
            if word == "over" {
                return Some("OVER".to_string());
            }
            continue;
        }
        i += 1;
    }
    None
}

/// Token shape emitted by `walk_unquoted_tokens` — single-byte
/// position-marked.
enum SqlTok {
    Byte(u8),
}

/// Iterate over the bytes of `sql` outside single-quoted / dollar-
/// quoted contexts and outside line / block comments.
/// Returns the byte at each non-quoted position. Used for the
/// statement-separator check above; callers that need richer token
/// shape should hand-roll their own walker (see
/// `aggregate_or_over_hit` for the word-token shape).
fn walk_unquoted_tokens(sql: &str) -> impl Iterator<Item = SqlTok> + '_ {
    let bytes = sql.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                i = skip_single_quoted(bytes, i);
                continue;
            }
            if bytes[i] == b'$'
                && let Some((tag_len, body_start)) = parse_dollar_open(bytes, i)
            {
                i = skip_dollar_body(bytes, body_start, &bytes[i..i + tag_len]);
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            let b = bytes[i];
            i += 1;
            return Some(SqlTok::Byte(b));
        }
        None
    })
}

/// Skip a single-quoted string starting at `bytes[start]` (the opening
/// `'`). Returns the index past the closing quote. Honours Postgres's
/// `''` embedded-quote escape.
fn skip_single_quoted(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            // `''` is an escaped single quote — keep going.
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Parse a dollar-quote opener at `bytes[start]` (the opening `$`).
/// Returns `(tag_len_including_dollars, body_start)` on success — the
/// tag byte span is `bytes[start..start + tag_len_including_dollars]`.
/// Returns `None` if the opener does not match (e.g. `$1`, lone `$`).
fn parse_dollar_open(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    // Bare `$$` opener — minimum two-byte tag.
    if start + 1 < bytes.len() && bytes[start + 1] == b'$' {
        return Some((2, start + 2));
    }
    // `$tag$` opener — tag is identifier-shape (letters / digits /
    // underscore), terminated by another `$`. Empty tag is the bare
    // form handled above.
    let mut i = start + 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i > start + 1 && i < bytes.len() && bytes[i] == b'$' {
        return Some((i - start + 1, i + 1));
    }
    None
}

/// Skip the body of a dollar-quoted string. `body_start` is the byte
/// just after the opening `$tag$`; `tag_bytes` is the full
/// `$tag$` opener (including both `$` characters). Returns the index
/// past the closing tag.
fn skip_dollar_body(bytes: &[u8], body_start: usize, tag_bytes: &[u8]) -> usize {
    let mut i = body_start;
    while i + tag_bytes.len() <= bytes.len() {
        if &bytes[i..i + tag_bytes.len()] == tag_bytes {
            return i + tag_bytes.len();
        }
        i += 1;
    }
    bytes.len()
}

// ─────────────────────────────────────────────────────────────────────────
// Cross-attribute validation
// ─────────────────────────────────────────────────────────────────────────

/// Cross-attribute validation pass — runs after every `#[derived(...)]`
/// attribute has been individually parsed and after the model's column
/// set is known to the caller.
/// Checks performed:
/// - **Per-scope column collision (E_DJG_VDF_002)** — for each derived
///   entry, every scope in `scopes = [...]` must not contain a model
///   column with the same name. The column set is supplied as a
///   `(column_name, exposed_scopes)` list since the call site
///   already has the parsed `FieldAttrs::expose` shape.
/// - **Per-scope derived collision (E_DJG_VDF_003)** — two derived
///   entries with overlapping `scopes` cannot share a `name`.
/// - **Relation-form overlap (E_DJG_VDF_010)** — a derived entry
///   cannot target a scope that also embeds a peer visage via
///   `expose(scope -> Peer)`.
/// - **Model-level pk = None incompatibility (E_DJG_VDF_015)**
///   the caller supplies the model's PK strategy; `pk = None` rejects
///   the entire attribute.
///   Returns `Ok(())` when every check passes; any failure returns a
///   span-precise `syn::Error` pointing at the offending derived
///   attribute.
pub fn cross_check(
    derived: &[DerivedAttr],
    column_exposures: &[(String, Vec<&'static str>)],
    storage_field_types: &[(String, syn::Type)],
    relation_form_exposures: &[(String, Vec<&'static str>)],
    pk_is_none: bool,
) -> syn::Result<()> {
    let _ = storage_field_types;

    // E_DJG_VDF_015 — `pk = None` host model.
    if pk_is_none && !derived.is_empty() {
        return Err(syn::Error::new(
            derived[0].attr_span,
            "`#[derived(...)]` is incompatible with `#[model(pk = None)]` — \
             derived visages hydrate per-row identified by primary key, and \
             a `pk = None` model has no `id` column, no `Model::Pk` \
             associated type, and no visage queryset to filter against \
             (E_DJG_VDF_015)",
        ));
    }

    // E_DJG_VDF_010 — relation-form visages use a separate projector
    // path that does not yet render derived SQL expressions.
    for d in derived {
        for scope in &d.scopes {
            if let Some((field_name, _)) = relation_form_exposures
                .iter()
                .find(|(_, exposed_scopes)| exposed_scopes.contains(&scope.key))
            {
                return Err(syn::Error::new(
                    scope.span,
                    format!(
                        "derived visage scope `{}` overlaps relation-form exposure `{field_name}` \
                         in the same scope; derived fields and relation-form embedding are not \
                         combinable until the relation projector emits derived expressions \
                         (E_DJG_VDF_010)",
                        scope.key
                    ),
                ));
            }
        }
    }

    // E_DJG_VDF_002 — collision against an exposed model column in any
    // overlapping scope.
    for d in derived {
        let d_name = d.name.to_string();
        let d_name_str = d_name.strip_prefix("r#").unwrap_or(d_name.as_str());
        for (col, exposed_scopes) in column_exposures {
            if col != d_name_str {
                continue;
            }
            // Find the first overlap between the derived scope set and
            // the column's exposed scope set.
            let overlap = d.scopes.iter().find(|s| exposed_scopes.contains(&s.key));
            if let Some(scope) = overlap {
                return Err(syn::Error::new(
                    d.name.span(),
                    format!(
                        "derived `name = {d_name_str}` collides with the exposed \
                         model column `{col}` in scope `{}` (E_DJG_VDF_002)",
                        scope.key
                    ),
                ));
            }
        }
    }

    // E_DJG_VDF_003 — collision against another derived entry in any
    // overlapping scope.
    for (a_idx, a) in derived.iter().enumerate() {
        let a_name = a.name.to_string();
        let a_name_str = a_name.strip_prefix("r#").unwrap_or(a_name.as_str());
        for b in &derived[a_idx + 1..] {
            let b_name = b.name.to_string();
            let b_name_str = b_name.strip_prefix("r#").unwrap_or(b_name.as_str());
            if a_name_str != b_name_str {
                continue;
            }
            let overlap = a
                .scopes
                .iter()
                .find(|sa| b.scopes.iter().any(|sb| sa.key == sb.key));
            if let Some(scope) = overlap {
                return Err(syn::Error::new(
                    b.name.span(),
                    format!(
                        "derived `name = {a_name_str}` declared twice in scope `{}` — \
                         each derived entry's name must be unique within every scope \
                         it targets (E_DJG_VDF_003)",
                        scope.key
                    ),
                ));
            }
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Fallibility detection — codegen consumer
// ─────────────────────────────────────────────────────────────────────────

/// Syntactic shape recognised by [`detect_fallibility_shape`].
/// Matches the closed set documented in
/// [§Fallibility detection]. Each shape determines the emission shape
/// in the From/TryFrom body — Shape1 already contains the inner `?`,
/// so the outer block is emitted without an additional `?`; the other
/// fallible shapes evaluate to `Result<T, E>` and need the outer `?`
/// to unwrap.
/// [§Fallibility detection]: ../docs/spec/visage-derived-fields.md#fallibility-detection-syntactic-tail-not-type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallibilityShape {
    /// Infallible — block evaluates to `T`.
    Infallible,
    /// `<expr>?` at outermost tail — inner `?` propagates; no outer `?`.
    Shape1TrailingQuestion,
    /// `match` / `if`/`else` / block / `Ok(...)` / `Err(...)` at
    /// outermost tail — block evaluates to `Result<T, E>`; outer `?`
    /// required.
    Shape2to5Result,
}

impl FallibilityShape {
    /// `true` iff the shape produces a fallible expression (Shape1 or
    /// any of Shapes 2–5).
    pub fn is_fallible(self) -> bool {
        !matches!(self, FallibilityShape::Infallible)
    }
}

/// Detect the fallibility shape of `rust_source` per the spec's closed
/// set. Operates in token space — the macro cannot inspect type
/// information, so the detector reads the expression's syntactic tail.
/// Returns `Err(syn::Error)` when the expression does not parse as a
/// `syn::Expr`; the caller surfaces it as a span-anchored diagnostic
/// pointing at the `rust = "..."` literal.
pub fn detect_fallibility_shape(rust_source: &str, span: Span) -> syn::Result<FallibilityShape> {
    let expr: syn::Expr = syn::parse_str(rust_source).map_err(|e| {
        syn::Error::new(
            span,
            format!(
                "`#[derived]` `rust` expression failed to parse: {e}. \
                 The string must be a valid Rust expression — e.g. \
                 `rust = \"model.field.clone()\"`"
            ),
        )
    })?;
    Ok(classify_expr(&expr))
}

fn classify_expr(expr: &syn::Expr) -> FallibilityShape {
    // Strip outer parentheses (`(expr)`) — they are semantically
    // invisible to fallibility detection.
    let inner = match expr {
        syn::Expr::Paren(p) => return classify_expr(&p.expr),
        _ => expr,
    };
    match inner {
        // Shape 1 — trailing `?` at the outermost tail.
        syn::Expr::Try(_) => FallibilityShape::Shape1TrailingQuestion,
        // Shape 5 — bare `Ok(...)` / `Err(...)` call.
        syn::Expr::Call(call) => {
            if let syn::Expr::Path(p) = call.func.as_ref()
                && let Some(seg) = p.path.segments.last()
            {
                let id = seg.ident.to_string();
                if id == "Ok" || id == "Err" {
                    return FallibilityShape::Shape2to5Result;
                }
            }
            FallibilityShape::Infallible
        }
        // Shape 2 — `match` whose every arm body recursively classifies
        // as a fallible shape.
        syn::Expr::Match(m) => {
            if m.arms
                .iter()
                .all(|arm| classify_expr(&arm.body).is_fallible())
            {
                FallibilityShape::Shape2to5Result
            } else {
                FallibilityShape::Infallible
            }
        }
        // Shape 3 — `if`/`else` chain whose every branch's tail
        // recursively classifies as fallible.
        syn::Expr::If(if_expr) => classify_if(if_expr),
        // Shape 4 — block whose tail (last statement) recursively
        // classifies as fallible. An empty block falls back to
        // infallible.
        syn::Expr::Block(block) => match block.block.stmts.last() {
            Some(syn::Stmt::Expr(e, _)) => classify_expr(e),
            _ => FallibilityShape::Infallible,
        },
        _ => FallibilityShape::Infallible,
    }
}

fn classify_if(if_expr: &syn::ExprIf) -> FallibilityShape {
    // Then-branch tail.
    let then_tail = match if_expr.then_branch.stmts.last() {
        Some(syn::Stmt::Expr(e, _)) => classify_expr(e),
        _ => FallibilityShape::Infallible,
    };
    if !then_tail.is_fallible() {
        return FallibilityShape::Infallible;
    }
    // Else-branch tail. Missing else => `()` => infallible.
    let else_tail = match &if_expr.else_branch {
        Some((_, expr)) => classify_expr(expr),
        None => FallibilityShape::Infallible,
    };
    if !else_tail.is_fallible() {
        return FallibilityShape::Infallible;
    }
    FallibilityShape::Shape2to5Result
}

// `syn::Expr` does not expose a `span_or_call_site` shortcut; provide
// one for our `Expr` references so the meta-walker above can fall
// back to a sensible span when the value-side of a `MetaNameValue`
// is itself spanless (rare but possible inside `Path` exprs).
trait ExprSpan {
    fn span_or_call_site(&self) -> Span;
}

impl ExprSpan for syn::Expr {
    fn span_or_call_site(&self) -> Span {
        self.span()
    }
}

/// Condition 1 of [E_DJG_VDF_017]: is the derived `sql` literal a *simple
/// reference* to the storage column named `ident`?
///
/// Two spellings count as simple references, after trimming ASCII
/// whitespace from both ends of `sql`:
/// 1. the bare unquoted identifier byte-identical to `ident`;
/// 2. a simple double-quoted identifier — the trimmed literal begins and
///    ends with `"`, contains no embedded `"` between them, and the
///    unquoted body is byte-identical to `ident`.
///
/// Any compound expression (parentheses, function calls, operators,
/// subqueries, doubly-quoted or embedded-quote forms) is NOT a simple
/// reference and returns `false` — those are adopter-owned compound
/// territory caught at runtime by the parity gate, not at parse time.
/// No regex, no SQL parser (`feedback_no_regex_in_djogi.md`).
///
/// [E_DJG_VDF_017]: ../docs/spec/jsonb-per-audience-schema.md#error-taxonomy-extension
fn sql_is_simple_reference_to(sql: &str, ident: &str) -> bool {
    let trimmed = sql.trim();
    // Spelling 1: bare unquoted ident.
    if trimmed == ident {
        return true;
    }
    // Spelling 2: simple quoted ident. Must be at least `""` plus a body,
    // begin and end with `"`, and have no `"` anywhere in the interior.
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let body = &trimmed[1..trimmed.len() - 1];
        if !body.is_empty() && !body.as_bytes().contains(&b'"') {
            return body == ident;
        }
    }
    false
}

/// Strip a single prelude `Option<_>` wrapper, returning the inner type.
///
/// Mirrors `attrs::unwrap_option` (`attrs.rs:4050-4059`): only the prelude
/// `Option` path forms (`Option<T>`, `std::option::Option<T>`,
/// `core::option::Option<T>`) are stripped. A user type named `Option` in
/// the adopter's own module is left unchanged — treating it as nullable
/// would be wrong, and for the guard it would be an unsound strip. Returns
/// the input unchanged (by reference-clone semantics via the caller) when
/// the type is not a prelude `Option<_>`.
fn unwrap_prelude_option(ty: &syn::Type) -> syn::Type {
    if let syn::Type::Path(syn::TypePath { qself: None, path }) = ty {
        if is_prelude_option_path(path) {
            if let Some(last) = path.segments.last() {
                if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                        return inner.clone();
                    }
                }
            }
        }
    }
    ty.clone()
}

/// True iff `path` is one of the three prelude `Option` spellings:
/// bare `Option`, `std::option::Option`, or `core::option::Option`.
/// Mirrors the prelude-only recognition `attrs::unwrap_option` uses
/// (via `attrs::is_prelude_option_path`); duplicated here because that
/// helper is private to the `attrs` module.
fn is_prelude_option_path(path: &syn::Path) -> bool {
    // The last segment must be `Option` in every accepted spelling.
    // `syn::Ident` compares against `&str` directly (PartialEq<str>),
    // matching how the rest of the macro crate checks segment idents.
    if !path.segments.last().is_some_and(|seg| seg.ident == "Option") {
        return false;
    }
    match path.segments.len() {
        // Bare `Option`.
        1 => true,
        // `std::option::Option` / `core::option::Option`. Compare each
        // segment ident as a place (`seg.ident == "lit"`), matching how
        // the rest of the macro crate checks idents and avoiding any
        // `&String`/`&str` comparison pitfall.
        3 => {
            let first_is_std_or_core =
                path.segments[0].ident == "std" || path.segments[0].ident == "core";
            first_is_std_or_core && path.segments[1].ident == "option"
        }
        _ => false,
    }
}

/// Condition 2 of [E_DJG_VDF_017]: does the storage column's declared Rust
/// type denote `Jsonb<...>` at its outermost path (after stripping a
/// prelude `Option<_>`)?
///
/// True iff, after stripping a single prelude `Option<_>`, the type is a
/// `syn::Type::Path` with no `qself` whose rightmost `::`-separated path
/// segment is the identifier `Jsonb` carrying angle-bracketed generic
/// arguments. So `Jsonb<X>`, `djogi::types::Jsonb<X>`, `djogi::Jsonb<X>`,
/// `crate::Jsonb<X>`, `super::Jsonb<X>`, `::djogi::Jsonb<X>`, and
/// `Option<Jsonb<X>>` all match; `AdminMeta`, `String`, `NotJsonb<X>`, and a
/// bare `Jsonb` without generics do not.
///
/// This is a structural `syn::Type` match, NOT a token-string match — it
/// mirrors the shipped `descriptor.rs::is_jsonb_type` (which also strips
/// `Option` first) and the structural `Jsonb<…>` arm of
/// `attrs::rust_type_to_sql` (`attrs.rs:3563-3582`), so the guard's JSONB
/// detection is exactly as strong as the descriptor / migration emitter's.
/// Proc macros still cannot resolve type *aliases* — a storage column
/// spelled `type AdminMeta = Jsonb<...>; pub metadata: AdminMeta;` is
/// intentionally missed here and caught at runtime by the parity gate
/// (spec §OQ-5).
///
/// [E_DJG_VDF_017]: ../docs/spec/jsonb-per-audience-schema.md#error-taxonomy-extension
fn type_is_jsonb(ty: &syn::Type) -> bool {
    let inner = unwrap_prelude_option(ty);
    let syn::Type::Path(syn::TypePath { qself: None, path }) = &inner else {
        return false;
    };
    path.segments.last().is_some_and(|seg| {
        seg.ident == "Jsonb"
            && matches!(seg.arguments, syn::PathArguments::AngleBracketed(_))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;
    use syn::parse2;

    fn parse_struct(ts: proc_macro2::TokenStream) -> syn::ItemStruct {
        parse2(ts).expect("struct parses")
    }

    fn first_attr_err(ts: proc_macro2::TokenStream) -> String {
        let s = parse_struct(ts);
        let err = parse_derived_attrs(&s).expect_err("expected error");
        err.to_string()
    }

    #[test]
    fn parses_minimal_derived_attribute() {
        let s = parse_struct(quote! {
            #[derived(
                name   = facility_site,
                ty     = Site,
                scopes = [public, admin],
                sql    = "CASE WHEN direction = 'inbound' THEN inbound_site ELSE outbound_site END",
                rust   = "model.inbound_site.clone()",
            )]
            struct Consignment {
                pub direction: String,
                pub inbound_site: String,
                pub outbound_site: String,
            }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        assert_eq!(parsed.len(), 1);
        let d = &parsed[0];
        assert_eq!(d.name.to_string(), "facility_site");
        assert_eq!(d.scopes.len(), 2);
        assert_eq!(d.scopes[0].key, "public");
        assert_eq!(d.scopes[1].key, "admin");
        assert!(d.sql.contains("CASE WHEN"));
        assert!(d.doc.is_none());
    }

    #[test]
    fn rejects_missing_required_keys() {
        // Missing `rust`.
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_001"), "got: {msg}");
        assert!(msg.contains("rust"), "got: {msg}");
    }

    #[test]
    fn rejects_uppercase_name_byte() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = facilitySite,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_012"), "got: {msg}");
    }

    #[test]
    fn rejects_reserved_djogi_prefix() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = __djogi_secret,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_005"), "got: {msg}");
    }

    #[test]
    fn rejects_reserved_pg_keyword() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = order,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_014"), "got: {msg}");
    }

    #[test]
    fn rejects_unknown_scope() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [unicorn],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_006"), "got: {msg}");
    }

    #[test]
    fn rejects_duplicate_scope_within_list() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public, public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_013"), "got: {msg}");
    }

    #[test]
    fn rejects_empty_scopes_list() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("empty"), "got: {msg}");
    }

    #[test]
    fn rejects_statement_separator_in_sql() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "1; DROP TABLE x",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_007"), "got: {msg}");
    }

    #[test]
    fn allows_semicolon_inside_string_literal() {
        // `';'` inside a quoted SQL string is not a statement separator.
        let s = parse_struct(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "CASE WHEN c = ';' THEN 1 ELSE 0 END",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        parse_derived_attrs(&s).expect("ok");
    }

    #[test]
    fn rejects_leading_ddl_keyword() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "DELETE FROM x",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_007"), "got: {msg}");
    }

    #[test]
    fn rejects_dollar_placeholder_token() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "$1 + 1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_008"), "got: {msg}");
    }

    #[test]
    fn allows_dollar_quoted_body() {
        // `$tag$ ... $tag$` is a Postgres dollar-quoted string literal,
        // not a `$N` placeholder.
        let s = parse_struct(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "$tag$ inner; text $tag$",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        parse_derived_attrs(&s).expect("ok");
    }

    #[test]
    fn rejects_aggregate_call() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "COUNT(*) + 1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        assert!(msg.contains("E_DJG_VDF_009"), "got: {msg}");
    }

    #[test]
    fn rejects_over_keyword() {
        let msg = first_attr_err(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "rank() OVER (ORDER BY id)",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        // `rank` itself is not in our aggregate list; the OVER keyword
        // is what trips the detector.
        assert!(msg.contains("E_DJG_VDF_009"), "got: {msg}");
    }

    #[test]
    fn aggregate_inside_quotes_does_not_trip() {
        let s = parse_struct(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "'COUNT(*)'",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        parse_derived_attrs(&s).expect("ok");
    }

    #[test]
    fn cross_check_rejects_pk_none() {
        let s = parse_struct(quote! {
            #[derived(
                name = x,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        let err = cross_check(&parsed, &[], &[], &[], true).expect_err("pk = None must reject");
        assert!(err.to_string().contains("E_DJG_VDF_015"));
    }

    #[test]
    fn cross_check_rejects_collision_with_exposed_column() {
        let s = parse_struct(quote! {
            #[derived(
                name = display_name,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        let columns = vec![("display_name".to_string(), vec!["public"])];
        let err = cross_check(&parsed, &columns, &[], &[], false).expect_err("collision must reject");
        assert!(err.to_string().contains("E_DJG_VDF_002"));
    }

    #[test]
    fn cross_check_rejects_relation_form_scope_overlap() {
        let s = parse_struct(quote! {
            #[derived(
                name = department,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        let columns = vec![("department".to_string(), vec!["public"])];
        let relation_forms = vec![("department".to_string(), vec!["public"])];
        let err = cross_check(&parsed, &columns, &[], &relation_forms, false)
            .expect_err("relation-form overlap must reject before column collision");
        assert!(err.to_string().contains("E_DJG_VDF_010"));
    }

    #[test]
    fn cross_check_allows_column_collision_outside_overlapping_scope() {
        // Column exposed only to `admin`; derived only in `public`
        // no overlap, no collision.
        let s = parse_struct(quote! {
            #[derived(
                name = display_name,
                ty = i32,
                scopes = [public],
                sql = "1",
                rust = "1",
            )]
            struct M { f: i32 }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        let columns = vec![("display_name".to_string(), vec!["admin"])];
        cross_check(&parsed, &columns, &[], &[], false).expect("no collision");
    }

    #[test]
    fn cross_check_rejects_derived_collision_in_overlapping_scope() {
        let s = parse_struct(quote! {
            #[derived(name = x, ty = i32, scopes = [public, admin], sql = "1", rust = "1")]
            #[derived(name = x, ty = i32, scopes = [admin], sql = "2", rust = "2")]
            struct M { f: i32 }
        });
        let parsed = parse_derived_attrs(&s).expect("ok");
        let err = cross_check(&parsed, &[], &[], &[], false).expect_err("collision must reject");
        assert!(err.to_string().contains("E_DJG_VDF_003"));
    }

    #[test]
    fn detect_fallibility_recognises_trailing_question() {
        let s = detect_fallibility_shape("compute(model)?", Span::call_site()).unwrap();
        assert_eq!(s, FallibilityShape::Shape1TrailingQuestion);
    }

    #[test]
    fn detect_fallibility_recognises_ok_call() {
        let s = detect_fallibility_shape("Ok(42)", Span::call_site()).unwrap();
        assert_eq!(s, FallibilityShape::Shape2to5Result);
    }

    #[test]
    fn detect_fallibility_recognises_match_with_ok_arms() {
        let src = "match x { 1 => Ok(1), _ => Err(42) }";
        let s = detect_fallibility_shape(src, Span::call_site()).unwrap();
        assert_eq!(s, FallibilityShape::Shape2to5Result);
    }

    #[test]
    fn detect_fallibility_recognises_infallible() {
        let s = detect_fallibility_shape("model.field.clone()", Span::call_site()).unwrap();
        assert_eq!(s, FallibilityShape::Infallible);
    }

    #[test]
    fn detect_fallibility_recognises_block_tail_question() {
        // Outer block whose tail expr is `compute(model)?` — Shape 1
        // bubbles through the block wrapper.
        let src = "{ let m = model; compute(m)? }";
        let s = detect_fallibility_shape(src, Span::call_site()).unwrap();
        assert_eq!(s, FallibilityShape::Shape1TrailingQuestion);
    }

    // ──────────────────────────────────────────────────────────────
    // E_DJG_VDF_004 — general identifier-shape coverage.
    // The validator's three rejection arms (length cap, leading byte,
    // body byte) all anchor at E_DJG_VDF_004 in their diagnostic. The
    // length cap is reachable via a normal ASCII identifier longer
    // than 63 bytes. The leading-byte and body-byte arms are
    // unreachable via macro-time parser input that originates as a
    // rustc-validated identifier, BUT the validator must still reject
    // such names if a future caller hands it a synthesised
    // `syn::Ident` carrying non-ASCII bytes (Rust 1.53+ accepts
    // Unicode XID identifiers — `école` and `café` both parse as
    // valid `syn::Ident`s).
    // These tests exercise `validate_name_shape` directly. The
    // earlier `rejects_uppercase_name_byte`, `rejects_reserved_…`,
    // and similar tests cover the parser end-to-end via
    // `parse_derived_attrs`; the byte-level rules below are split
    // out here because the >63-byte path is awkward to write inline
    // in a `quote! { ... }` block (and would also force the test
    // file itself to carry a 64-char ident, which clippy-style
    // tooling tends to dislike).
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn rejects_name_exceeding_63_bytes() {
        // Maximum allowed length is 63 bytes. A 64-byte ASCII
        // lowercase identifier trips the length cap arm.
        let raw = "a".repeat(MAX_DERIVED_NAME_LEN + 1);
        let ident = syn::Ident::new(&raw, Span::call_site());
        let err = super::validate_name_shape(&ident).expect_err("length cap must fire");
        let msg = err.to_string();
        assert!(msg.contains("E_DJG_VDF_004"), "got: {msg}");
        assert!(msg.contains("exceeding"), "got: {msg}");
    }

    #[test]
    fn accepts_name_at_63_byte_cap() {
        // Boundary check — 63 bytes exactly is the maximum the
        // Postgres unquoted-identifier cap permits. The validator
        // must accept this length, not reject it.
        let raw = "a".repeat(MAX_DERIVED_NAME_LEN);
        let ident = syn::Ident::new(&raw, Span::call_site());
        super::validate_name_shape(&ident).expect("63-byte cap is inclusive");
    }

    #[test]
    fn rejects_non_ascii_leading_byte() {
        // `é` U+00E9 is a valid Unicode XID_Start character, so syn
        // accepts it as an identifier; its UTF-8 encoding starts
        // with 0xC3, which is neither `_` nor ASCII-lowercase, so the
        // validator's leading-byte arm fires E_DJG_VDF_004.
        let ident = syn::Ident::new("école", Span::call_site());
        let err = super::validate_name_shape(&ident).expect_err("leading non-ASCII must fire");
        let msg = err.to_string();
        assert!(msg.contains("E_DJG_VDF_004"), "got: {msg}");
        assert!(
            msg.contains("first byte must be"),
            "expected leading-byte diagnostic, got: {msg}"
        );
    }

    #[test]
    fn rejects_non_ascii_body_byte() {
        // `café` starts with the ASCII letter `c` (lower), so the
        // first-byte check passes; the third character `é` encodes
        // as UTF-8 bytes 0xC3 0xA9, which trip the body-byte arm of
        // E_DJG_VDF_004 because neither byte is `_`, ASCII
        // lowercase, or ASCII digit.
        let ident = syn::Ident::new("café", Span::call_site());
        let err = super::validate_name_shape(&ident).expect_err("body non-ASCII must fire");
        let msg = err.to_string();
        assert!(msg.contains("E_DJG_VDF_004"), "got: {msg}");
        assert!(
            msg.contains("after the first character"),
            "expected body-byte diagnostic, got: {msg}"
        );
    }

    #[test]
    fn vdf017_sql_is_simple_reference_to() {
        // Bare unquoted ident matches.
        assert!(sql_is_simple_reference_to("metadata", "metadata"));
        assert!(sql_is_simple_reference_to("  metadata  ", "metadata")); // trimmed
        // Simple quoted ident matches.
        assert!(sql_is_simple_reference_to("\"metadata\"", "metadata"));
        assert!(sql_is_simple_reference_to("  \"metadata\"  ", "metadata"));
        // Wrong column name does not match.
        assert!(!sql_is_simple_reference_to("metadata", "other_col"));
        assert!(!sql_is_simple_reference_to("\"metadata\"", "other_col"));
        // Compound / non-simple shapes do NOT match (adopter-owned territory).
        assert!(!sql_is_simple_reference_to("(metadata)", "metadata"));
        assert!(!sql_is_simple_reference_to("coalesce(metadata, '{}'::jsonb)", "metadata"));
        assert!(!sql_is_simple_reference_to("metadata || '{}'::jsonb", "metadata"));
        assert!(!sql_is_simple_reference_to("jsonb_set(metadata, '{}', '{}')", "metadata"));
        assert!(!sql_is_simple_reference_to("(SELECT metadata)", "metadata"));
        assert!(!sql_is_simple_reference_to("jsonb_build_object('a', metadata)", "metadata"));
        // Doubly-quoted / embedded-quote shapes are NOT simple quoted idents.
        assert!(!sql_is_simple_reference_to("\"\"metadata\"\"", "metadata"));
        assert!(!sql_is_simple_reference_to("\"meta\"\"data\"", "metadata"));
        // Empty / lone-quote edge cases.
        assert!(!sql_is_simple_reference_to("", "metadata"));
        assert!(!sql_is_simple_reference_to("\"\"", ""));
    }

    #[test]
    fn vdf017_type_is_jsonb() {
        // Helper to parse a Rust type from source for the test table.
        fn ty(s: &str) -> syn::Type {
            syn::parse_str::<syn::Type>(s).expect("type parses")
        }

        // Direct and qualified Jsonb<…> forms all match.
        assert!(type_is_jsonb(&ty("Jsonb<ProfileMetaAdmin>")));
        assert!(type_is_jsonb(&ty("djogi::types::Jsonb<X>")));
        assert!(type_is_jsonb(&ty("djogi::Jsonb<X>")));
        assert!(type_is_jsonb(&ty("crate::Jsonb<X>")));
        assert!(type_is_jsonb(&ty("super::Jsonb<X>")));
        assert!(type_is_jsonb(&ty("::djogi::Jsonb<X>")));
        // Nullable Jsonb<_> matches — Option<_> is stripped first:
        // a nullable JSONB storage column leaks
        // identically to a non-null one when the row is non-NULL, so the
        // guard MUST treat it as a JSONB column.
        assert!(type_is_jsonb(&ty("Option<Jsonb<X>>")));
        assert!(type_is_jsonb(&ty("std::option::Option<Jsonb<X>>")));
        // Not Jsonb.
        assert!(!type_is_jsonb(&ty("AdminMeta")));
        assert!(!type_is_jsonb(&ty("String")));
        assert!(!type_is_jsonb(&ty("NotJsonb<X>"))); // rightmost segment is NotJsonb
        assert!(!type_is_jsonb(&ty("Jsonb")));       // no angle-bracket generics
        // A user type literally named `Option` in their own module is not the
        // prelude Option, so it is NOT stripped; its last segment is `Option`,
        // not `Jsonb`, so it does not match (mirrors unwrap_option's prelude-
        // only recognition at attrs.rs:4040-4059).
        assert!(!type_is_jsonb(&ty("my_crate::Option<Jsonb<X>>")));
    }
}
