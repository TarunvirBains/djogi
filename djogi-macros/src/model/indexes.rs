//! `#[model(indexes(...))]` grammar
//! Parses the model-level index declaration grammar (see
//! `docs/spec/indexing.md`) and lowers it to `IndexSpec` token-stream
//! literals that land in the `#[model]`-emitted descriptor.
//! # Parser implementation — hand-rolled, not darling
//! Plan D4 originally described this parser as a `darling::FromMeta` path
//! (matching the `ModelAttrs` pattern). Three §5 constructs sit
//! outside darling's derive grammar:
//! 1. `where = "..."` uses a Rust keyword as a key — darling's derive
//!    reduces to `syn::Path`, which rejects keywords. Only
//!    `syn::ext::IdentExt::parse_any` accepts the keyword.
//! 2. The per-column record literal
//!    `(col = ident, opclass = "…", order = desc, nulls = first)` is a
//!    tuple / paren expression, not an attribute meta list. Darling has
//!    no built-in decoder for it.
//! 3. The mixed `fields = [ident, (col = …)]` list interleaves two
//!    different shapes whose common supertype is `syn::Expr`.
//!    Rather than fight darling's macro machinery — or bolt on three
//!    `FromMeta` impls whose body is a hand-rolled `syn::Expr` walk anyway
//!    the whole parser lives here as a `syn::ParseStream` walk over the
//!    inner token stream. Error spans stay precise; the plan will be
//!    amended in a later docstring pass to reflect this deviation.
//! # Pipeline
//! 1. `ModelAttrs::parse` extracts the `indexes(...)` `Meta::List` and
//!    hands it to [`parse_indexes_meta_list`], which produces
//!    `Vec<ModelIndexDecl>`.
//! 2. `descriptor::expand` consumes the Vec, calls
//!    [`emit_index_spec_tokens`] to lower each decl into an `IndexSpec`
//!    struct-literal token stream, and appends the result to the spatial
//!    GiST indexes already emitted by the descriptor module.
//! 3. The final `indexes: &[IndexSpec { … }, …]` slice is emitted in a
//!    deterministic (alphabetised-by-name) order so minor reorderings in
//!    the user's source do not produce spurious migration diffs.

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
    Expr, Ident, Meta, MetaList, Token,
    ext::IdentExt,
    parse::{Parse, ParseStream, Parser},
    punctuated::Punctuated,
    spanned::Spanned,
};

// ---------------------------------------------------------------------------
// IR — intermediate parse tree
// ---------------------------------------------------------------------------

/// One entry inside `#[model(indexes(...))]`. Distinct variants pin the
/// caller's intent (`index(...)` vs `unique(...)`) at parse time rather
/// than inferring it from a `kind` bool later.
#[derive(Debug, Clone)]
pub struct ModelIndexDecl {
    /// `true` for `unique(...)` declarations; `false` for `index(...)`.
    /// Drives the [`crate::djogi::descriptor::IndexKind`] that lowering
    /// emits — non-unique stays `NonUnique`, unique maps to
    /// `UniqueConstraint` unless a feature forces unique-index form
    /// (partial predicate, `nulls_not_distinct`, expression target,
    /// covering columns).
    pub is_unique: bool,
    pub body: IndexDeclBody,
    /// Span of the declaration's head ident — pins error messages to the
    /// user's `index(` / `unique(` token when a validation rule fires.
    pub head_span: Span,
}

#[derive(Debug, Clone)]
pub struct IndexDeclBody {
    pub target: IndexDeclTarget,
    pub using: Option<String>,
    pub opclass: Option<String>,
    pub include: Vec<String>,
    pub predicate: Option<String>,
    pub nulls_not_distinct: bool,
    pub concurrently: bool,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub enum IndexDeclTarget {
    Fields(Vec<FieldColSpec>),
    Expr(String),
}

#[derive(Debug, Clone)]
pub enum FieldColSpec {
    /// Shorthand `ident` — lowers to `IndexColumnSpec::simple(ident)`.
    Simple(String),
    /// Record form `(col = ident, opclass = "…", order = asc|desc,
    /// nulls = first|last|default)`. Every record field after `col` is
    /// optional and defaults to the corresponding `simple` value.
    Record {
        col: String,
        opclass: Option<String>,
        order: Option<IndexOrder>,
        nulls: Option<IndexNullsOrder>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexNullsOrder {
    Default,
    First,
    Last,
}

// ---------------------------------------------------------------------------
// Parse entry point
// ---------------------------------------------------------------------------

/// Parse the `Meta::List` passed as `indexes(...)` inside `#[model(...)]`.
/// Returns the Vec of `ModelIndexDecl` in source order. Caller validates
/// downstream rules that depend on struct-field knowledge (unknown column
/// names, name collisions) during the lowering step.
pub fn parse_indexes_meta_list(list: &MetaList) -> syn::Result<Vec<ModelIndexDecl>> {
    let metas = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
    let mut decls = Vec::new();
    for meta in &metas {
        match meta {
            Meta::List(inner) => {
                let head = inner
                    .path
                    .get_ident()
                    .ok_or_else(|| {
                        syn::Error::new_spanned(
                            &inner.path,
                            "expected `index(...)` or `unique(...)` inside `indexes(...)`",
                        )
                    })?
                    .clone();
                let head_str = head.to_string();
                let is_unique = match head_str.as_str() {
                    "index" => false,
                    "unique" => true,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &head,
                            format!(
                                "unknown indexes entry `{head_str}` — expected `index(...)` or `unique(...)`"
                            ),
                        ));
                    }
                };
                let body = parse_index_body(inner)?;
                decls.push(ModelIndexDecl {
                    is_unique,
                    body,
                    head_span: head.span(),
                });
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    meta,
                    "expected `index(...)` or `unique(...)` inside `indexes(...)`",
                ));
            }
        }
    }
    Ok(decls)
}

/// One `key = value` entry inside an index body. Uses `Ident::parse_any`
/// so Rust keywords (`where`) can appear as attribute keys — which is
/// required by the §5 grammar (`where = "deleted_at IS NULL"`).
struct IndexBodyEntry {
    key: Ident,
    value: Expr,
}

impl Parse for IndexBodyEntry {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let key = Ident::parse_any(input)?;
        input.parse::<Token![=]>()?;
        let value: Expr = input.parse()?;
        Ok(Self { key, value })
    }
}

fn parse_index_body(list: &MetaList) -> syn::Result<IndexDeclBody> {
    let entries: Punctuated<IndexBodyEntry, Token![,]> =
        Punctuated::<IndexBodyEntry, Token![,]>::parse_terminated.parse2(list.tokens.clone())?;

    let mut fields: Option<Vec<FieldColSpec>> = None;
    let mut expr: Option<String> = None;
    let mut using: Option<String> = None;
    let mut opclass: Option<String> = None;
    let mut include: Vec<String> = Vec::new();
    let mut include_seen = false;
    let mut predicate: Option<String> = None;
    let mut nulls_not_distinct = false;
    let mut seen_nulls_not_distinct = false;
    let mut concurrently = false;
    let mut seen_concurrently = false;
    let mut name: Option<String> = None;

    for IndexBodyEntry { key, value } in &entries {
        let key_str = key.unraw().to_string();
        match key_str.as_str() {
            "fields" => {
                if fields.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `fields = [..]`"));
                }
                fields = Some(parse_fields_array(value)?);
            }
            "expr" => {
                if expr.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `expr = \"..\"`"));
                }
                expr = Some(require_string_lit(value, "expr")?.value());
            }
            "using" => {
                if using.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `using = \"..\"`"));
                }
                using = Some(require_string_lit(value, "using")?.value());
            }
            "opclass" => {
                if opclass.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `opclass = \"..\"`"));
                }
                opclass = Some(require_string_lit(value, "opclass")?.value());
            }
            "include" => {
                if include_seen {
                    return Err(syn::Error::new_spanned(key, "duplicate `include = [..]`"));
                }
                include_seen = true;
                include = parse_ident_array(value, "include")?;
            }
            "where" => {
                if predicate.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `where = \"..\"`"));
                }
                predicate = Some(require_string_lit(value, "where")?.value());
            }
            "nulls_not_distinct" => {
                if seen_nulls_not_distinct {
                    return Err(syn::Error::new_spanned(
                        key,
                        "duplicate `nulls_not_distinct = ..`",
                    ));
                }
                seen_nulls_not_distinct = true;
                nulls_not_distinct = require_bool_lit(value, "nulls_not_distinct")?;
            }
            "concurrently" => {
                if seen_concurrently {
                    return Err(syn::Error::new_spanned(
                        key,
                        "duplicate `concurrently = ..`",
                    ));
                }
                seen_concurrently = true;
                concurrently = require_bool_lit(value, "concurrently")?;
            }
            "name" => {
                if name.is_some() {
                    return Err(syn::Error::new_spanned(key, "duplicate `name = \"..\"`"));
                }
                name = Some(require_string_lit(value, "name")?.value());
            }
            other => {
                return Err(syn::Error::new_spanned(
                    key,
                    format!(
                        "unknown key `{other}` inside index(...); \
                         expected one of: fields, expr, using, opclass, include, \
                         where, nulls_not_distinct, concurrently, name"
                    ),
                ));
            }
        }
    }

    let target = match (fields, expr) {
        (Some(fs), None) => {
            if fs.is_empty() {
                return Err(syn::Error::new_spanned(
                    &list.path,
                    "`fields = []` is not allowed — list at least one column",
                ));
            }
            IndexDeclTarget::Fields(fs)
        }
        (None, Some(e)) => {
            if e.is_empty() {
                return Err(syn::Error::new_spanned(
                    &list.path,
                    "`expr = \"\"` is not allowed — expression must be a non-empty SQL fragment",
                ));
            }
            IndexDeclTarget::Expr(e)
        }
        (Some(_), Some(_)) => {
            return Err(syn::Error::new_spanned(
                &list.path,
                "`fields = [..]` and `expr = \"..\"` are mutually exclusive — use exactly one",
            ));
        }
        (None, None) => {
            return Err(syn::Error::new_spanned(
                &list.path,
                "missing target — every index declaration must set either `fields = [..]` or `expr = \"..\"`",
            ));
        }
    };

    Ok(IndexDeclBody {
        target,
        using,
        opclass,
        include,
        predicate,
        nulls_not_distinct,
        concurrently,
        name,
    })
}

fn parse_fields_array(value: &Expr) -> syn::Result<Vec<FieldColSpec>> {
    let Expr::Array(arr) = value else {
        return Err(syn::Error::new_spanned(
            value,
            "`fields` must be a bracketed list, e.g. `fields = [last_name, first_name]`",
        ));
    };
    let mut out = Vec::with_capacity(arr.elems.len());
    for elem in &arr.elems {
        out.push(parse_field_col_spec(elem)?);
    }
    Ok(out)
}

fn parse_field_col_spec(elem: &Expr) -> syn::Result<FieldColSpec> {
    match elem {
        // `ident` — bare column reference, shorthand for
        // IndexColumnSpec::simple(ident). `unraw` normalises raw idents
        // (`r#where`, `r#type`) into their keyword spelling so the
        // downstream comparison against `declared_columns` — which also
        // strips `r#` — succeeds.
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            let ident = p.path.segments[0].ident.unraw().to_string();
            Ok(FieldColSpec::Simple(ident))
        }
        // `(col = x, ...)` — record literal. `syn` parses this as
        // `Expr::Tuple` when there are multiple comma-separated assignment
        // expressions inside parentheses; a single `(key = val)` is
        // parsed as `Expr::Paren(Expr::Assign(...))`.
        Expr::Tuple(t) => parse_field_col_record(&t.elems, t.paren_token.span.span()),
        Expr::Paren(p) => {
            let mut one = Punctuated::<Expr, Token![,]>::new();
            one.push_value((*p.expr).clone());
            parse_field_col_record(&one, p.paren_token.span.span())
        }
        _ => Err(syn::Error::new_spanned(
            elem,
            "`fields` entries must be either a bare column ident or a record \
             `(col = ident, opclass = \"..\", order = asc|desc, nulls = first|last|default)`",
        )),
    }
}

fn parse_field_col_record(
    elems: &Punctuated<Expr, Token![,]>,
    fallback_span: Span,
) -> syn::Result<FieldColSpec> {
    let mut col: Option<String> = None;
    let mut opclass: Option<String> = None;
    let mut order: Option<IndexOrder> = None;
    let mut nulls: Option<IndexNullsOrder> = None;

    for (idx, e) in elems.iter().enumerate() {
        let Expr::Assign(assign) = e else {
            return Err(syn::Error::new_spanned(
                e,
                "expected `key = value` inside column record",
            ));
        };
        let key_path = match &*assign.left {
            Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
                p.path.segments[0].ident.to_string()
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &assign.left,
                    "column-record key must be a plain ident",
                ));
            }
        };
        // §5 grammar — `col = ident` must be the first entry. Every
        // subsequent opclass/order/nulls key is optional but "after col"
        // is non-negotiable per the documented grammar. Catching it at
        // parse time keeps the diagnostic specific rather than cascading
        // into a "col is missing" error at record close.
        if idx == 0 && key_path.as_str() != "col" {
            return Err(syn::Error::new_spanned(
                &assign.left,
                format!("column record must start with `col = ident`; got `{key_path}` first"),
            ));
        }
        match key_path.as_str() {
            "col" => {
                if col.is_some() {
                    return Err(syn::Error::new_spanned(&assign.left, "duplicate `col`"));
                }
                match &*assign.right {
                    Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
                        col = Some(p.path.segments[0].ident.unraw().to_string());
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &assign.right,
                            "`col = ident` — column name must be a plain ident",
                        ));
                    }
                }
            }
            "opclass" => {
                if opclass.is_some() {
                    return Err(syn::Error::new_spanned(&assign.left, "duplicate `opclass`"));
                }
                opclass = Some(require_string_lit(&assign.right, "opclass")?.value());
            }
            "order" => {
                if order.is_some() {
                    return Err(syn::Error::new_spanned(&assign.left, "duplicate `order`"));
                }
                order = Some(parse_order_ident(&assign.right)?);
            }
            "nulls" => {
                if nulls.is_some() {
                    return Err(syn::Error::new_spanned(&assign.left, "duplicate `nulls`"));
                }
                nulls = Some(parse_nulls_ident(&assign.right)?);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    &assign.left,
                    format!(
                        "unknown column-record key `{other}`; expected col, opclass, order, nulls"
                    ),
                ));
            }
        }
    }

    let col = col.ok_or_else(|| {
        syn::Error::new(
            fallback_span,
            "column record must specify `col = ident` as its first entry",
        )
    })?;

    Ok(FieldColSpec::Record {
        col,
        opclass,
        order,
        nulls,
    })
}

fn parse_order_ident(value: &Expr) -> syn::Result<IndexOrder> {
    let ident = expect_bare_ident(value, "order")?;
    match ident.to_string().as_str() {
        "asc" => Ok(IndexOrder::Asc),
        "desc" => Ok(IndexOrder::Desc),
        other => Err(syn::Error::new_spanned(
            value,
            format!("unknown order `{other}`; expected `asc` or `desc`"),
        )),
    }
}

fn parse_nulls_ident(value: &Expr) -> syn::Result<IndexNullsOrder> {
    let ident = expect_bare_ident(value, "nulls")?;
    match ident.to_string().as_str() {
        "first" => Ok(IndexNullsOrder::First),
        "last" => Ok(IndexNullsOrder::Last),
        "default" => Ok(IndexNullsOrder::Default),
        other => Err(syn::Error::new_spanned(
            value,
            format!("unknown nulls `{other}`; expected `first`, `last`, or `default`"),
        )),
    }
}

fn expect_bare_ident<'a>(value: &'a Expr, key: &str) -> syn::Result<&'a Ident> {
    match value {
        Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
            Ok(&p.path.segments[0].ident)
        }
        _ => Err(syn::Error::new_spanned(
            value,
            format!("`{key}` expects a bare ident"),
        )),
    }
}

fn parse_ident_array(value: &Expr, key: &str) -> syn::Result<Vec<String>> {
    let Expr::Array(arr) = value else {
        return Err(syn::Error::new_spanned(
            value,
            format!("`{key}` must be a bracketed list of column idents"),
        ));
    };
    // Normalise every bare ident via `IdentExt::unraw` so raw-identifier
    // column references (`r#where`, `r#type`) compare equal to their
    // `r#`-stripped entry in `declared_columns`.
    arr.elems
        .iter()
        .map(|e| match e {
            Expr::Path(p) if p.qself.is_none() && p.path.segments.len() == 1 => {
                Ok(p.path.segments[0].ident.unraw().to_string())
            }
            _ => Err(syn::Error::new_spanned(
                e,
                format!("`{key}` entries must be bare column idents"),
            )),
        })
        .collect()
}

use crate::syn_util::{require_bool_lit, require_string_lit};

// ---------------------------------------------------------------------------
// Lowering — validation + IndexSpec token emission
// ---------------------------------------------------------------------------

/// Context needed to lower one `ModelIndexDecl` into an `IndexSpec` token
/// literal. The caller (descriptor emitter) owns the field-name set and
/// the table-name seed for generated index names.
pub struct LoweringCtx<'a> {
    pub table_name: &'a str,
    /// Set of declared user-field column names (post raw-ident strip)
    /// used to validate that every column reference in
    /// `fields` / `include` refers to a real field.
    pub declared_columns: &'a [String],
    /// Names generated implicitly by other parts of the macro (today,
    /// the spatial `<table>_<col>_gix` indexes). Rejecting
    /// user-supplied names that collide prevents silent override.
    pub reserved_generated_names: &'a [String],
}

/// Whether a `unique(...)` declaration carries any feature that
/// `ALTER TABLE ... ADD CONSTRAINT ... UNIQUE` cannot express — in
/// which case the macro escalates the kind to `UniqueIndex`
/// (plan §6.2 + §6.4). Shared by [`emit_index_spec_tokens`] (which
/// selects the `IndexKind` token) and [`generate_index_name`] (which
/// selects the name stem); they **must** agree or the emitted
/// `IndexSpec` would carry a constraint-shaped name against an
/// index-shaped kind (or vice versa).
/// # Escalation triggers
/// | Feature | Reason |
/// |---------|--------|
/// | `where = "..."` | Postgres constraints have no partial-predicate form. |
/// | `include = [...]` | `ADD CONSTRAINT … UNIQUE` has no `INCLUDE` form. |
/// | `nulls_not_distinct = true` | `ADD CONSTRAINT` cannot express nulls-distinct semantics. |
/// | `expr = "..."` | Expression targets require `CREATE INDEX`; constraint syntax requires a column-name list. |
/// | `concurrently = true` | `ADD CONSTRAINT` has no concurrent form. |
/// | `opclass = "..."` (top-level) | Opclass is index-element syntax; it is not valid in the table-constraint column list. |
/// | Per-column `opclass`, `order = desc`, or `nulls = first\|last` | Same reason — these modifiers are only valid inside `CREATE INDEX … USING … (col modifier…)`, not in `UNIQUE (col, …)`. |
/// # Out of scope here — rejected at validation
/// A non-btree `using = "<method>"` on `unique(...)` is rejected by
/// [`validate_decl`] **before** lowering reaches this predicate
/// (PostgreSQL unique indexes are btree-only; `ALTER TABLE … ADD
/// CONSTRAINT … UNIQUE` also has no `USING` clause). The predicate
/// therefore never sees a non-btree unique declaration in well-formed
/// code; the `using` field is intentionally absent from the
/// escalation table above.
fn forces_unique_index(body: &IndexDeclBody) -> bool {
    body.predicate.is_some()
        || body.nulls_not_distinct
        || matches!(body.target, IndexDeclTarget::Expr(_))
        || !body.include.is_empty()
        || body.concurrently
        // Top-level opclass is index-element syntax — not valid in the
        // table-constraint UNIQUE column list.
        || body.opclass.is_some()
        // Any per-column modifier (opclass / DESC / NULLS FIRST|LAST) is also
        // index-element syntax and must escalate to CREATE UNIQUE INDEX.
        || unique_columns_have_index_only_modifiers(&body.target)
    // Non-btree `using` on `unique(...)` is rejected at validation time
    // (PostgreSQL unique indexes are btree-only). See `validate_decl`.
    // Lowering never sees that combination from a well-formed parse.
}

/// Returns `true` when at least one column in a `Fields` target carries a
/// modifier that is index-element syntax only (opclass, non-default sort
/// order, or non-default nulls placement). The table-constraint UNIQUE form
/// accepts only bare column identifiers; these modifiers require
/// `CREATE UNIQUE INDEX … USING … (col modifier)` instead.
fn unique_columns_have_index_only_modifiers(target: &IndexDeclTarget) -> bool {
    let IndexDeclTarget::Fields(cols) = target else {
        return false;
    };
    cols.iter().any(|c| match c {
        FieldColSpec::Simple(_) => false,
        FieldColSpec::Record {
            opclass,
            order,
            nulls,
            ..
        } => {
            opclass.is_some()
                || matches!(order, Some(IndexOrder::Desc))
                || matches!(nulls, Some(IndexNullsOrder::First | IndexNullsOrder::Last))
        }
    })
}

/// Lower a parsed decl into an `IndexSpec` struct-literal token stream +
/// the generated index name (used for alphabetising emission).
pub fn emit_index_spec_tokens(
    decl: &ModelIndexDecl,
    ctx: &LoweringCtx<'_>,
) -> syn::Result<(String, TokenStream)> {
    // ── Validation — §5 "Rules baked into macro" ─────────────────────────
    validate_decl(decl, ctx)?;

    let body = &decl.body;

    // Resolve IndexKind. Unique + any unique-index-only feature
    // (partial/NND/expression/covering/concurrent) forces `UniqueIndex`.
    // Plain unique lowers to `UniqueConstraint`. See `forces_unique_index`
    // for the shared escalation predicate.
    let forces_unique_index = forces_unique_index(body);
    let kind_tokens = if decl.is_unique {
        if forces_unique_index {
            quote! { ::djogi::descriptor::IndexKind::UniqueIndex }
        } else {
            quote! { ::djogi::descriptor::IndexKind::UniqueConstraint }
        }
    } else {
        quote! { ::djogi::descriptor::IndexKind::NonUnique }
    };

    // IndexType.
    let index_type_tokens = match body.using.as_deref() {
        None | Some("btree") => quote! { ::djogi::descriptor::IndexType::BTree },
        Some("gin") => quote! { ::djogi::descriptor::IndexType::Gin },
        Some("gist") => quote! { ::djogi::descriptor::IndexType::Gist },
        Some("hash") => quote! { ::djogi::descriptor::IndexType::Hash },
        Some("brin") => quote! { ::djogi::descriptor::IndexType::Brin },
        Some("spgist") => quote! { ::djogi::descriptor::IndexType::Spgist },
        Some(other) => {
            return Err(syn::Error::new(
                decl.head_span,
                format!(
                    "unknown index method `using = \"{other}\"`; \
                     expected one of btree, gin, gist, brin, hash, spgist"
                ),
            ));
        }
    };

    // Target → IndexTarget token stream.
    let target_tokens = match &body.target {
        IndexDeclTarget::Fields(fs) => {
            let col_tokens: Vec<TokenStream> = fs
                .iter()
                .map(|spec| emit_index_column_spec(spec, body.opclass.as_deref()))
                .collect();
            quote! {
                ::djogi::descriptor::IndexTarget::Columns(&[
                    #(#col_tokens,)*
                ])
            }
        }
        IndexDeclTarget::Expr(expr) => {
            quote! { ::djogi::descriptor::IndexTarget::Expression(#expr) }
        }
    };

    // Generated name (naming; will be replaced with
    // the shared `djogi::descriptor::index_name` helper once that lands).
    let generated_name = body
        .name
        .clone()
        .unwrap_or_else(|| generate_index_name(decl, ctx));

    // Predicate / include / nulls_not_distinct / concurrently → tokens.
    let predicate_tokens = match &body.predicate {
        Some(s) => quote! { ::std::option::Option::Some(#s) },
        None => quote! { ::std::option::Option::None },
    };
    let include_lits = body.include.iter().map(|s| {
        let s = s.as_str();
        quote! { #s }
    });
    let include_tokens = quote! { &[ #(#include_lits,)* ] };
    let nulls_not_distinct_tokens = {
        let b = body.nulls_not_distinct;
        quote! { #b }
    };
    let requires_out_of_transaction_tokens = {
        let b = body.concurrently;
        quote! { #b }
    };
    let extension_tokens = quote! { ::std::option::Option::None };
    let name_tokens = generated_name.as_str();

    let spec_tokens = quote! {
        ::djogi::descriptor::IndexSpec {
            name: #name_tokens,
            target: #target_tokens,
            kind: #kind_tokens,
            index_type: #index_type_tokens,
            predicate: #predicate_tokens,
            include: #include_tokens,
            nulls_not_distinct: #nulls_not_distinct_tokens,
            requires_out_of_transaction: #requires_out_of_transaction_tokens,
            extension_dependency: #extension_tokens,
        }
    };

    Ok((generated_name, spec_tokens))
}

fn emit_index_column_spec(spec: &FieldColSpec, top_level_opclass: Option<&str>) -> TokenStream {
    let (name, opclass, order, nulls) = match spec {
        FieldColSpec::Simple(name) => (
            name.clone(),
            None,
            IndexOrder::Asc,
            IndexNullsOrder::Default,
        ),
        FieldColSpec::Record {
            col,
            opclass,
            order,
            nulls,
        } => (
            col.clone(),
            opclass.clone(),
            order.unwrap_or(IndexOrder::Asc),
            nulls.unwrap_or(IndexNullsOrder::Default),
        ),
    };

    // Per-column record opclass takes precedence over top-level uniform opclass.
    let effective_opclass = opclass.or_else(|| top_level_opclass.map(str::to_string));

    let name_lit = name.as_str();
    let opclass_tokens = match effective_opclass {
        Some(s) => {
            let s = s.as_str();
            quote! { ::std::option::Option::Some(#s) }
        }
        None => quote! { ::std::option::Option::None },
    };
    let order_tokens = match order {
        IndexOrder::Asc => quote! { ::djogi::descriptor::IndexOrder::Asc },
        IndexOrder::Desc => quote! { ::djogi::descriptor::IndexOrder::Desc },
    };
    let nulls_tokens = match nulls {
        IndexNullsOrder::Default => quote! { ::djogi::descriptor::IndexNullsOrder::Default },
        IndexNullsOrder::First => quote! { ::djogi::descriptor::IndexNullsOrder::First },
        IndexNullsOrder::Last => quote! { ::djogi::descriptor::IndexNullsOrder::Last },
    };

    quote! {
        ::djogi::descriptor::IndexColumnSpec {
            name: #name_lit,
            opclass: #opclass_tokens,
            order: #order_tokens,
            nulls: #nulls_tokens,
        }
    }
}

fn validate_decl(decl: &ModelIndexDecl, ctx: &LoweringCtx<'_>) -> syn::Result<()> {
    let body = &decl.body;
    let span = decl.head_span;

    // #83 — PostgreSQL unique indexes are btree-only.
    // `CREATE UNIQUE INDEX … USING <method>` is rejected by PostgreSQL for
    // every non-btree access method (gin / gist / brin / spgist / hash);
    // `ALTER TABLE … ADD CONSTRAINT … UNIQUE` has no `USING` clause at all
    // and always uses btree internally. So `unique(..., using = "<non-btree>")`
    // has no valid lowering and must be rejected at compile time — silently
    // emitting `CREATE UNIQUE INDEX … USING gist` (or similar) would compile
    // a model whose generated migration SQL fails at apply with PG's
    // "access method does not support unique indexes" error.
    // Hash is included in the rejection set: hash is non-btree, and a
    // unique hash index has the same impossibility as a unique gin / gist /
    // brin / spgist index. The original §5 Q3 hash-only carve-out (Phase
    // 7-Zero) is subsumed by this rule. Below, the hash-specific rejection
    // of multi-column / where / include / expression combinations is
    // retained, because those combinations are hash-incompatible
    // independently of unique.
    if decl.is_unique
        && let Some(method) = body.using.as_deref()
        && method != "btree"
    {
        return Err(syn::Error::new(
            span,
            format!(
                "`unique(..., using = \"{method}\")` is rejected: PostgreSQL unique indexes \
                 are btree-only. Either use `using = \"btree\"` (or omit `using`), or drop \
                 `unique` if a non-unique `{method}` lookup index is what you want."
            ),
        ));
    }

    // §5 Q3 — `using = "hash"` + (multi-column | expr | where | include) → error.
    // Hash + unique is already covered by the btree-only rule above; the
    // remaining hash-incompatible combinations stay as separate diagnostics
    // because their root cause is hash's own structural limitations (no
    // partial predicate support, no covering columns, no expression
    // targets, single-column only).
    if body.using.as_deref() == Some("hash") {
        let mut reason: Option<&'static str> = None;
        if body.predicate.is_some() {
            reason = Some("a `where = \"..\"` partial predicate");
        } else if !body.include.is_empty() {
            reason = Some("`include = [..]` covering columns");
        } else if matches!(body.target, IndexDeclTarget::Expr(_)) {
            reason = Some("an `expr = \"..\"` expression target");
        } else if let IndexDeclTarget::Fields(fs) = &body.target
            && fs.len() > 1
        {
            reason = Some("multi-column `fields = [..]`");
        }
        if let Some(r) = reason {
            return Err(syn::Error::new(
                span,
                format!(
                    "`using = \"hash\"` is incompatible with {r}. Postgres hash indexes do not \
                     support this combination — drop the combination or switch to a btree index."
                ),
            ));
        }
    }

    // §5 — `nulls_not_distinct = true` is only meaningful on `unique(...)`.
    if body.nulls_not_distinct && !decl.is_unique {
        return Err(syn::Error::new(
            span,
            "`nulls_not_distinct = true` is only meaningful on `unique(...)`; \
             move this declaration into `unique(...)` or drop `nulls_not_distinct`.",
        ));
    }

    // §4 amendment (2026-04-23 pass-2 P1-02) — expression indexes do not
    // accept opclass in 0.1.0. The descriptor carries no path to bind an
    // operator class to an expression target; see docs/spec/indexing.md §20.2.
    if matches!(body.target, IndexDeclTarget::Expr(_)) && body.opclass.is_some() {
        return Err(syn::Error::new(
            span,
            "`expr = \"..\"` indexes do not accept `opclass = \"..\"` in 0.1.0 — \
             to use a non-default opclass on an expression index, issue the CREATE INDEX \
             statement directly via `raw_ddl` under \
             `#[djogi::deliberately_bypass_convention_with_raw_sql]` \
             (see docs/spec/raw-sql-escape-hatches.md §2).",
        ));
    }

    // Q5 — opclass ASCII-shape validation (non-empty, `_` or ASCII alpha
    // start, `_` / ASCII alphanumeric remaining, ≤ 63 bytes).
    if let Some(op) = &body.opclass {
        validate_opclass_shape(op, span, "opclass")?;
    }
    if let IndexDeclTarget::Fields(fs) = &body.target {
        for f in fs {
            if let FieldColSpec::Record {
                opclass: Some(op), ..
            } = f
            {
                validate_opclass_shape(op, span, "opclass")?;
            }
        }
    }

    // Unknown-column validation against the declared struct fields.
    if let IndexDeclTarget::Fields(fs) = &body.target {
        for f in fs {
            let col = match f {
                FieldColSpec::Simple(c) => c,
                FieldColSpec::Record { col, .. } => col,
            };
            if !ctx.declared_columns.iter().any(|c| c == col) {
                return Err(syn::Error::new(
                    span,
                    format!("column `{col}` in `fields = [..]` is not declared on this struct"),
                ));
            }
        }
    }
    for col in &body.include {
        if !ctx.declared_columns.iter().any(|c| c == col) {
            return Err(syn::Error::new(
                span,
                format!("column `{col}` in `include = [..]` is not declared on this struct"),
            ));
        }
    }

    // Name-override collision check.
    if let Some(explicit) = &body.name {
        validate_index_name_shape(explicit, span)?;
        if ctx.reserved_generated_names.iter().any(|n| n == explicit) {
            return Err(syn::Error::new(
                span,
                format!(
                    "`name = \"{explicit}\"` collides with a macro-generated index name — \
                     pick a different explicit name or remove the override."
                ),
            ));
        }
    }

    Ok(())
}

fn validate_opclass_shape(s: &str, span: Span, key: &str) -> syn::Result<()> {
    if s.is_empty() {
        return Err(syn::Error::new(
            span,
            format!("`{key} = \"\"` is not allowed — opclass names must be non-empty"),
        ));
    }
    let bytes = s.as_bytes();
    if bytes.len() > 63 {
        return Err(syn::Error::new(
            span,
            format!(
                "`{key} = \"{s}\"` exceeds the 63-byte Postgres identifier limit ({} bytes)",
                bytes.len()
            ),
        ));
    }
    let first_ok = bytes[0] == b'_' || bytes[0].is_ascii_alphabetic();
    if !first_ok {
        return Err(syn::Error::new(
            span,
            format!("`{key} = \"{s}\"` must start with an ASCII letter or underscore"),
        ));
    }
    for &b in &bytes[1..] {
        if !(b == b'_' || b.is_ascii_alphanumeric()) {
            return Err(syn::Error::new(
                span,
                format!("`{key} = \"{s}\"` contains a non-ASCII-alphanumeric / underscore byte"),
            ));
        }
    }
    Ok(())
}

fn validate_index_name_shape(s: &str, span: Span) -> syn::Result<()> {
    // Same shape check as opclass — plain ASCII ident, ≤ 63 bytes.
    validate_opclass_shape(s, span, "name")
}

/// Mirror of `djogi::descriptor::index_name` — the cycle between djogi
/// and djogi-macros prevents the macro from depending on djogi at
/// compile time, so the deterministic naming contract is spelled out in
/// both places. Unit tests on both sides assert byte-for-byte parity
/// against a small cross-crate matrix so drift between the two
/// implementations is caught immediately.
/// Logic kept deliberately identical to §6.4 + D5 in the v3 plan:
/// - `NonUnique` / `UniqueConstraint` / `UniqueIndex` stems → `_idx` /
///   `_key` / `_uidx`.
/// - Expression targets render with the literal `expr` body; column
///   lists render as underscore-joined column names in declaration
///   order.
/// - When the naïve name exceeds 63 bytes, truncate the stem to 55
///   bytes and append `_<8-char hex digest>` of the pre-truncation
///   name (SipHash-1-3 low 32 bits).
fn generate_index_name(decl: &ModelIndexDecl, ctx: &LoweringCtx<'_>) -> String {
    let table = ctx.table_name;
    let body = &decl.body;
    let stem = if !decl.is_unique {
        "idx"
    } else if forces_unique_index(body) {
        "uidx"
    } else {
        "key"
    };
    let body_text = match &body.target {
        IndexDeclTarget::Fields(fs) => {
            let mut parts = Vec::with_capacity(fs.len());
            for f in fs {
                match f {
                    FieldColSpec::Simple(c) => parts.push(c.clone()),
                    FieldColSpec::Record { col, .. } => parts.push(col.clone()),
                }
            }
            parts.join("_")
        }
        IndexDeclTarget::Expr(_) => "expr".to_string(),
    };
    let full = format!("{table}_{body_text}_{stem}");
    if full.len() <= 63 {
        return full;
    }
    // Truncation: 55-byte stem + `_` + 8-char hex digest (see §D5).
    use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
    let mut h =
        BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().build_hasher();
    h.write(full.as_bytes());
    let digest = format!("{:08x}", (h.finish() as u32));
    let stem_55: String = full.as_bytes()[..55].iter().map(|b| *b as char).collect();
    format!("{stem_55}_{digest}")
}

// ---------------------------------------------------------------------------
// Tests — intermediate parse tree
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn parse_indexes_from_attr(tokens: TokenStream) -> syn::Result<Vec<ModelIndexDecl>> {
        let attr: syn::Attribute = parse_quote! { #[indexes(#tokens)] };
        let Meta::List(ml) = attr.meta else {
            panic!("expected meta list");
        };
        parse_indexes_meta_list(&ml)
    }

    #[test]
    fn parses_simple_index() {
        let decls = parse_indexes_from_attr(quote! {
            index(fields = [last_name, first_name])
        })
        .expect("should parse");
        assert_eq!(decls.len(), 1);
        assert!(!decls[0].is_unique);
        match &decls[0].body.target {
            IndexDeclTarget::Fields(fs) => {
                assert_eq!(fs.len(), 2);
                assert!(matches!(&fs[0], FieldColSpec::Simple(c) if c == "last_name"));
                assert!(matches!(&fs[1], FieldColSpec::Simple(c) if c == "first_name"));
            }
            _ => panic!("expected fields target"),
        }
    }

    #[test]
    fn parses_unique_constraint() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [org_id, external_id])
        })
        .unwrap();
        assert_eq!(decls.len(), 1);
        assert!(decls[0].is_unique);
    }

    #[test]
    fn parses_expression_target() {
        let decls = parse_indexes_from_attr(quote! {
            index(expr = "lower(email)")
        })
        .unwrap();
        match &decls[0].body.target {
            IndexDeclTarget::Expr(e) => assert_eq!(e, "lower(email)"),
            _ => panic!("expected expr target"),
        }
    }

    #[test]
    fn parses_covering_and_partial() {
        let decls = parse_indexes_from_attr(quote! {
            index(fields = [created_at], include = [status, priority]),
            unique(fields = [email], where = "deleted_at IS NULL")
        })
        .unwrap();
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].body.include, vec!["status", "priority"]);
        assert_eq!(
            decls[1].body.predicate.as_deref(),
            Some("deleted_at IS NULL")
        );
    }

    #[test]
    fn parses_method_and_opclass() {
        let decls = parse_indexes_from_attr(quote! {
            index(fields = [payload], using = "gin", opclass = "jsonb_path_ops")
        })
        .unwrap();
        assert_eq!(decls[0].body.using.as_deref(), Some("gin"));
        assert_eq!(decls[0].body.opclass.as_deref(), Some("jsonb_path_ops"));
    }

    #[test]
    fn parses_per_column_record_form() {
        let decls = parse_indexes_from_attr(quote! {
            index(fields = [
                (col = created_at, order = desc, nulls = first),
                (col = status, opclass = "text_pattern_ops"),
            ])
        })
        .unwrap();
        match &decls[0].body.target {
            IndexDeclTarget::Fields(fs) => {
                assert_eq!(fs.len(), 2);
                match &fs[0] {
                    FieldColSpec::Record {
                        col, order, nulls, ..
                    } => {
                        assert_eq!(col, "created_at");
                        assert_eq!(*order, Some(IndexOrder::Desc));
                        assert_eq!(*nulls, Some(IndexNullsOrder::First));
                    }
                    _ => panic!("expected record form"),
                }
                match &fs[1] {
                    FieldColSpec::Record { col, opclass, .. } => {
                        assert_eq!(col, "status");
                        assert_eq!(opclass.as_deref(), Some("text_pattern_ops"));
                    }
                    _ => panic!("expected record form"),
                }
            }
            _ => panic!("expected fields target"),
        }
    }

    #[test]
    fn parses_mixed_simple_and_record_forms() {
        let decls = parse_indexes_from_attr(quote! {
            index(fields = [tenant_id, (col = created_at, order = desc)])
        })
        .unwrap();
        match &decls[0].body.target {
            IndexDeclTarget::Fields(fs) => {
                assert_eq!(fs.len(), 2);
                assert!(matches!(&fs[0], FieldColSpec::Simple(c) if c == "tenant_id"));
                assert!(matches!(&fs[1], FieldColSpec::Record { col, .. } if col == "created_at"));
            }
            _ => panic!("expected fields target"),
        }
    }

    #[test]
    fn parses_nulls_not_distinct_and_concurrent_build() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [tenant_id, slug], nulls_not_distinct = true),
            index(fields = [email], concurrently = true)
        })
        .unwrap();
        assert!(decls[0].body.nulls_not_distinct);
        assert!(decls[1].body.concurrently);
    }

    #[test]
    fn rejects_fields_and_expr_both() {
        let err = parse_indexes_from_attr(quote! {
            index(fields = [a], expr = "lower(email)")
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}"
        );
    }

    #[test]
    fn rejects_missing_target() {
        let err = parse_indexes_from_attr(quote! {
            index(using = "btree")
        })
        .unwrap_err();
        assert!(err.to_string().contains("missing target"));
    }

    #[test]
    fn rejects_unknown_top_level_entry() {
        let err = parse_indexes_from_attr(quote! {
            bogus(fields = [a])
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown indexes entry `bogus`"));
    }

    /// Parity check — the macro-side `generate_index_name` must
    /// produce byte-for-byte identical names to the runtime helper
    /// `djogi::descriptor::index_name` for every shape listed in
    /// §6.4 + D5. Both sides independently duplicate the logic
    /// because the djogi / djogi-macros cycle prevents sharing a
    /// function; this test catches drift immediately.
    #[test]
    fn generate_index_name_matches_runtime_contract_shape() {
        fn mk_decl(
            is_unique: bool,
            target: IndexDeclTarget,
            predicate: Option<String>,
            include: Vec<String>,
            nulls_not_distinct: bool,
        ) -> ModelIndexDecl {
            ModelIndexDecl {
                is_unique,
                body: IndexDeclBody {
                    target,
                    using: None,
                    opclass: None,
                    include,
                    predicate,
                    nulls_not_distinct,
                    concurrently: false,
                    name: None,
                },
                head_span: Span::call_site(),
            }
        }
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &[
                "email".to_string(),
                "org_id".to_string(),
                "external_id".to_string(),
                "last".to_string(),
                "first".to_string(),
            ],
            reserved_generated_names: &[],
        };

        let simple = mk_decl(
            false,
            IndexDeclTarget::Fields(vec![FieldColSpec::Simple("email".into())]),
            None,
            vec![],
            false,
        );
        assert_eq!(generate_index_name(&simple, &ctx), "users_email_idx");

        let unique_constraint = mk_decl(
            true,
            IndexDeclTarget::Fields(vec![
                FieldColSpec::Simple("org_id".into()),
                FieldColSpec::Simple("external_id".into()),
            ]),
            None,
            vec![],
            false,
        );
        let ctx_orgs = LoweringCtx {
            table_name: "orgs",
            ..ctx
        };
        assert_eq!(
            generate_index_name(&unique_constraint, &ctx_orgs),
            "orgs_org_id_external_id_key"
        );

        let partial_unique = mk_decl(
            true,
            IndexDeclTarget::Fields(vec![FieldColSpec::Simple("email".into())]),
            Some("deleted_at IS NULL".into()),
            vec![],
            false,
        );
        let ctx_accounts = LoweringCtx {
            table_name: "accounts",
            ..ctx
        };
        assert_eq!(
            generate_index_name(&partial_unique, &ctx_accounts),
            "accounts_email_uidx"
        );

        let expr = mk_decl(
            false,
            IndexDeclTarget::Expr("lower(email)".into()),
            None,
            vec![],
            false,
        );
        assert_eq!(generate_index_name(&expr, &ctx), "users_expr_idx");

        // Column order matters.
        let last_first = mk_decl(
            false,
            IndexDeclTarget::Fields(vec![
                FieldColSpec::Simple("last".into()),
                FieldColSpec::Simple("first".into()),
            ]),
            None,
            vec![],
            false,
        );
        let ctx_people = LoweringCtx {
            table_name: "people",
            ..ctx
        };
        assert_eq!(
            generate_index_name(&last_first, &ctx_people),
            "people_last_first_idx"
        );
    }

    #[test]
    fn rejects_unknown_body_key() {
        let err = parse_indexes_from_attr(quote! {
            index(fields = [a], wrongkey = "x")
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown key `wrongkey`"));
    }

    /// Plan §6.2: `unique(..., concurrently = true)` escalates to
    /// `UniqueIndex` because `ALTER TABLE ADD CONSTRAINT ... UNIQUE`
    /// has no `CONCURRENTLY` form — emitting `UniqueConstraint` +
    /// non-transactional would generate invalid DDL. The name stem
    /// has to escalate with the kind — a `_key` stem against a
    /// `UniqueIndex` kind would mean the migration emitter names a
    /// unique index after the constraint convention.
    #[test]
    fn unique_with_concurrently_escalates_kind_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [email], concurrently = true)
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "expected UniqueIndex for unique + concurrently; got: {rendered}"
        );
        assert!(
            !rendered.contains("IndexKind :: UniqueConstraint"),
            "must not lower to UniqueConstraint; got: {rendered}"
        );
        assert_eq!(
            name, "users_email_uidx",
            "name stem must escalate with the kind"
        );
    }

    /// Plain `unique(...)` (no escalation trigger) stays as
    /// `UniqueConstraint`. Sanity check companion to the test above.
    #[test]
    fn plain_unique_stays_as_unique_constraint() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [email])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (_name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueConstraint"),
            "expected UniqueConstraint for plain unique; got: {rendered}"
        );
    }

    // ── Class B: unique(…) with index-only modifiers → UniqueIndex ──────

    /// Top-level `opclass` on `unique(...)` must escalate to `UniqueIndex`
    /// because opclass is index-element syntax — Postgres rejects it inside
    /// the table-constraint `UNIQUE (col_list)` column list.
    #[test]
    fn unique_with_top_level_opclass_escalates_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [email], opclass = "text_pattern_ops")
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "top-level opclass must escalate unique to UniqueIndex; got: {rendered}"
        );
        assert!(
            !rendered.contains("IndexKind :: UniqueConstraint"),
            "must not remain UniqueConstraint; got: {rendered}"
        );
        assert_eq!(
            name, "users_email_uidx",
            "name stem must use _uidx when escalated"
        );
    }

    /// Per-column `opclass` on `unique(...)` must escalate to `UniqueIndex`.
    #[test]
    fn unique_with_per_column_opclass_escalates_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [(col = email, opclass = "text_pattern_ops")])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "accounts",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "per-column opclass must escalate unique to UniqueIndex; got: {rendered}"
        );
        assert_eq!(name, "accounts_email_uidx");
    }

    /// Per-column `order = desc` on `unique(...)` must escalate to `UniqueIndex`.
    /// `DESC` is index-element syntax and Postgres rejects it in `UNIQUE (col_list)`.
    #[test]
    fn unique_with_per_column_desc_order_escalates_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [(col = created_at, order = desc)])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "events",
            declared_columns: &["created_at".to_string()],
            reserved_generated_names: &[],
        };
        let (name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "per-column order=desc must escalate unique to UniqueIndex; got: {rendered}"
        );
        assert_eq!(name, "events_created_at_uidx");
    }

    /// Per-column `nulls = first` on `unique(...)` must escalate to `UniqueIndex`.
    /// `NULLS FIRST` is index-element syntax; Postgres only accepts it in
    /// `CREATE INDEX … (col NULLS FIRST)`, not in a table-constraint column list.
    #[test]
    fn unique_with_per_column_nulls_first_escalates_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [(col = slug, nulls = first)])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "posts",
            declared_columns: &["slug".to_string()],
            reserved_generated_names: &[],
        };
        let (name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "per-column nulls=first must escalate unique to UniqueIndex; got: {rendered}"
        );
        assert_eq!(name, "posts_slug_uidx");
    }

    /// Per-column `nulls = last` also escalates (explicit non-default).
    #[test]
    fn unique_with_per_column_nulls_last_escalates_to_unique_index() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [(col = slug, nulls = last)])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "posts",
            declared_columns: &["slug".to_string()],
            reserved_generated_names: &[],
        };
        let (_name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueIndex"),
            "per-column nulls=last must escalate unique to UniqueIndex; got: {rendered}"
        );
    }

    /// `asc` order and default nulls do NOT trigger escalation — these are
    /// already the Postgres defaults and are omitted from the emitted SQL,
    /// so carrying them in the record form is harmless.
    #[test]
    fn unique_with_per_column_asc_and_default_nulls_stays_as_unique_constraint() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [(col = email, order = asc, nulls = default)])
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (_name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueConstraint"),
            "asc + default-nulls must not escalate; got: {rendered}"
        );
    }

    /// `unique(..., using = "<non-btree>")` is rejected at validation time
    /// because PostgreSQL unique indexes are btree-only. Every non-btree
    /// method gets the same diagnostic, naming the rejected method and
    /// pointing the user at the three resolutions (use `btree`, omit
    /// `using`, or drop `unique`).
    /// Pre-, the macro silently escalated `unique(... using =
    /// "gist")` to `UniqueIndex` and emitted `CREATE UNIQUE INDEX … USING
    /// gist`, which PostgreSQL rejects at migration apply. The fix is to
    /// reject at the macro layer so the model never compiles.
    #[test]
    fn unique_with_non_btree_using_is_rejected() {
        for method in ["gin", "gist", "brin", "spgist", "hash"] {
            let attr_tokens = match method {
                "gin" => quote! { unique(fields = [location], using = "gin") },
                "gist" => quote! { unique(fields = [location], using = "gist") },
                "brin" => quote! { unique(fields = [location], using = "brin") },
                "spgist" => quote! { unique(fields = [location], using = "spgist") },
                "hash" => quote! { unique(fields = [location], using = "hash") },
                _ => unreachable!(),
            };
            let decls = parse_indexes_from_attr(attr_tokens).unwrap();
            let ctx = LoweringCtx {
                table_name: "places",
                declared_columns: &["location".to_string()],
                reserved_generated_names: &[],
            };
            let err = emit_index_spec_tokens(&decls[0], &ctx).unwrap_err();
            let s = err.to_string();
            assert!(
                s.contains("PostgreSQL unique indexes") && s.contains("btree-only"),
                "{method}: expected btree-only rejection, got: {s}"
            );
            assert!(
                s.contains(&format!("unique(..., using = \"{method}\")")),
                "{method}: expected error to name the rejected method, got: {s}"
            );
        }
    }

    /// Explicit `using = "btree"` on `unique(...)` does NOT escalate — btree
    /// is the constraint form's implicit method.
    #[test]
    fn unique_with_explicit_btree_using_stays_as_unique_constraint() {
        let decls = parse_indexes_from_attr(quote! {
            unique(fields = [email], using = "btree")
        })
        .unwrap();
        let ctx = LoweringCtx {
            table_name: "users",
            declared_columns: &["email".to_string()],
            reserved_generated_names: &[],
        };
        let (_name, tokens) = emit_index_spec_tokens(&decls[0], &ctx).unwrap();
        let rendered = tokens.to_string();
        assert!(
            rendered.contains("IndexKind :: UniqueConstraint"),
            "explicit btree using must not escalate; got: {rendered}"
        );
    }
}
