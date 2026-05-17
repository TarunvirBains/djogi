//! Emit visage structs + conversion impls from `#[model]`.
//!
//! For each `#[model]` struct, generate four visage structs:
//! `{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`.
//! Each struct carries (in source order):
//!
//! 1. Framework columns (`id`, `created_at`, `updated_at`) — always
//!    included in every visage, regardless of user annotations.
//! 2. User fields annotated with `expose(scope)` (scalar) or
//!    `expose(scope -> Peer)` (relation — narrow visage or full peer
//!    embed; see below). The deprecated `expose(scope = "Peer")`
//!    string-literal form is still parsed for backward compat but
//!    lowers to the same `RelationExposure` shape.
//!
//! Each visage derives `Debug`, `Clone`, `serde::Serialize`,
//! `serde::Deserialize` unconditionally. Conversion impls:
//!
//! - **Scalar-only** visage (no relation-form entries): `impl From<&Model>`
//!   — infallible straight-line construction.
//! - **Relation-nesting** visage (at least one `expose(scope -> Peer)`
//!   entry on a relation field): `impl TryFrom<&Model>` with
//!   `Error = VisageError`. Optional FK relations emit
//!   `Option<PeerVisage>` and route the resolved relation through
//!   `<PeerVisage as TryFrom<&Target>>::try_from` only when `Some`.
//!
//! ## `expose(scope -> Peer)` grammar
//!
//! Selects one of two embed shapes based on the peer path's last segment:
//!
//! - **Narrow visage** — last segment is a `{Model}{Scope}` shape
//!   (`DepartmentPublic`); peer constructed via fallible `TryFrom`.
//! - **Full peer model** — last segment matches the relation target's
//!   ident (`Department`); peer cloned out of the resolved relation,
//!   serde derives delegate to the target's own (de)serialise impls.
//!
//! Optional FKs emit `Option<PeerVisage>` honestly at the type level.
//!
//! Path routing: every type reference in emitted code goes through
//! `::djogi::*` so users never depend on `serde` / `time` / `heeranjid`
//! directly.

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use crate::model::derived::{DerivedAttr, FallibilityShape, detect_fallibility_shape};
use crate::model::visage_ctx::{
    ScopeMembership, VisageEmitContext, classify_field_for_scope, is_full_peer_for,
};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Every scope emits one generated visage struct, in this fixed order.
pub(crate) const SCOPES: &[(&str, &str)] = &[
    ("public", "Public"),
    ("self_view", "SelfView"),
    ("admin", "Admin"),
    ("export", "Export"),
];

pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
    derived_attrs: &[DerivedAttr],
) -> TokenStream {
    let source_name = &struct_item.ident;

    let n_framework = model_attrs.framework_field_count();

    let visages: Vec<TokenStream> = SCOPES
        .iter()
        .map(|(scope, suffix)| {
            let ctx = VisageEmitContext {
                source: source_name,
                visage_ident: format_ident!("{source_name}{suffix}"),
                scope,
                struct_item,
                field_attrs,
                model_attrs,
                n_framework,
                derived_attrs,
            };
            emit_projection_for_scope(&ctx)
        })
        .collect();

    quote! {
        #(#visages)*
    }
}

fn emit_projection_for_scope(ctx: &VisageEmitContext<'_>) -> TokenStream {
    let source = ctx.source;
    let proj_name = &ctx.visage_ident;
    let scope = ctx.scope;
    let struct_item = ctx.struct_item;
    let field_attrs = ctx.field_attrs;
    let model_attrs = ctx.model_attrs;
    let n_framework = ctx.n_framework;
    let source_name_str = source.to_string();

    let fw_fields = framework_field_decls(model_attrs);
    let fw_inits = framework_field_inits(model_attrs);

    // Phase 8.5 #231 — pre-compute fallibility shape per scope-included
    // derived entry. The matched shape decides per-entry emission shape
    // (Shape1 propagates inner `?`; Shapes 2–5 add an outer `?`;
    // Infallible passes through unchanged). On parse failure of the
    // adopter's `rust` expression, fall through to a compile error
    // token stream so the diagnostic reaches the user crate.
    let mut derived_shapes: Vec<(usize, FallibilityShape)> = Vec::new();
    let scoped_derived: Vec<&DerivedAttr> = ctx.scope_derived().collect();
    for (i, d) in scoped_derived.iter().enumerate() {
        match detect_fallibility_shape(&d.rust, d.rust_span) {
            Ok(s) => derived_shapes.push((i, s)),
            Err(e) => return e.to_compile_error(),
        }
    }
    let any_fallible = derived_shapes.iter().any(|(_, s)| {
        matches!(
            s,
            FallibilityShape::Shape1TrailingQuestion | FallibilityShape::Shape2to5Result
        )
    });

    let mut user_fields: Vec<TokenStream> = Vec::new();
    let mut user_inits: Vec<TokenStream> = Vec::new();
    let mut has_relation_entry = false;

    let user_field_pairs: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    for (field, attrs) in user_field_pairs {
        let fname = field
            .ident
            .as_ref()
            .expect("named-field structs only — enforced in inject::expand");
        let fty = &field.ty;

        match classify_field_for_scope(field, attrs, scope) {
            ScopeMembership::Absent => continue,

            // Shape mismatch — emit a span-precise compile error. The
            // `msg` field carries a human-readable description of what
            // went wrong (scalar on relation or vice versa). We format
            // the scope into it at the call site because `classify_*`
            // does not have `scope` in context for the message string.
            ScopeMembership::Reject { .. } => {
                // Determine which direction the mismatch is, to give the
                // best error message.
                let msg = if detect_relation(fty).is_some() {
                    format!(
                        "relation fields require an explicit peer visage name; \
                         write `expose({scope} = \"PeerProjection\")`"
                    )
                } else {
                    format!(
                        "the `expose({scope} = \"...\")` form is only valid on \
                         relation fields (`ForeignKey<T>` / `OneToOneField<T>`)"
                    )
                };
                return syn::Error::new_spanned(field, msg).to_compile_error();
            }

            // Scalar form on scalar field — happy path.
            ScopeMembership::Scalar => {
                user_fields.push(quote! { pub #fname: #fty, });
                user_inits.push(quote! {
                    #fname: ::std::clone::Clone::clone(&src.#fname),
                });
            }

            // Relation form on relation field — optional relations emit
            // `pub field: Option<PeerVisage>`, with the init match-folding
            // `src.field.resolved()` through the peer's fallible
            // `TryFrom<&Target>` impl.
            //
            // Full-peer vs narrow embed:
            //
            // - If the user wrote `expose(scope -> Department)` and the
            //   relation's target ident is `Department`, embed the full
            //   peer model (clone the resolved target).
            // - Otherwise (path's last segment doesn't match the target
            //   ident), treat as a narrow visage and route through
            //   `<Peer as TryFrom<&Target>>::try_from(resolved)?`.
            ScopeMembership::RelationEmbed { exposure, nullable } => {
                has_relation_entry = true;
                let relation_info = detect_relation(fty)
                    .expect("RelationEmbed implies detect_relation returned Some");
                let fname_str = fname.to_string();
                let peer_path = &exposure.peer;
                let is_full_peer = is_full_peer_for(exposure, &relation_info);

                let peer_init_expr = if is_full_peer {
                    // Full-peer: clone the resolved target value.
                    quote! { ::std::clone::Clone::clone(__djogi_peer) }
                } else {
                    // Narrow visage: dispatch via TryFrom<&Target>.
                    quote! {
                        <#peer_path as ::std::convert::TryFrom<&_>>::try_from(__djogi_peer)?
                    }
                };

                if nullable {
                    user_fields.push(quote! { pub #fname: ::std::option::Option<#peer_path>, });
                    user_inits.push(quote! {
                        #fname: match src.#fname.as_ref() {
                            ::std::option::Option::Some(__djogi_rel) => match __djogi_rel.resolved() {
                                ::std::option::Option::Some(__djogi_peer) => {
                                    ::std::option::Option::Some(#peer_init_expr)
                                }
                                ::std::option::Option::None => {
                                    return ::std::result::Result::Err(
                                        ::djogi::VisageError::UnresolvedRelation {
                                            model: #source_name_str,
                                            field: #fname_str,
                                            scope: #scope,
                                        }
                                    );
                                }
                            },
                            ::std::option::Option::None => ::std::option::Option::None,
                        },
                    });
                } else {
                    user_fields.push(quote! { pub #fname: #peer_path, });
                    user_inits.push(quote! {
                        #fname: {
                            let __djogi_peer = src.#fname.resolved().ok_or(
                                ::djogi::VisageError::UnresolvedRelation {
                                    model: #source_name_str,
                                    field: #fname_str,
                                    scope: #scope,
                                }
                            )?;
                            #peer_init_expr
                        },
                    });
                }
            }
        }
    }

    // Phase 8.5 #231 — append derived-field decls and inits AFTER all
    // user column entries so the visage struct's field order is:
    //
    //     framework cols, user cols (in scope), derived entries.
    //
    // The same order drives PROJECTION_LIST emission, FromPgRow
    // positional decode, and the From/TryFrom init body.
    let mut derived_field_decls: Vec<TokenStream> = Vec::new();
    let mut derived_field_inits: Vec<TokenStream> = Vec::new();
    for (i, d) in scoped_derived.iter().enumerate() {
        let name = &d.name;
        let ty = &d.ty;
        let doc = match &d.doc {
            Some(s) => quote! { #[doc = #s] },
            None => quote! {},
        };
        derived_field_decls.push(quote! {
            #doc
            pub #name: #ty,
        });

        // Splice the adopter's `rust` expression. The `let model: &Source
        // = src;` rebind makes `model.<field>` syntax work without
        // retouching the existing emitter's `src` parameter binding.
        let rust_src = &d.rust;
        // The captured Rust expression is a single token stream — parse
        // once, splice once. Errors here surface as compile diagnostics
        // attached to the rust_span so the adopter sees the right
        // source-line anchor.
        let expr_tokens: TokenStream = match syn::parse_str::<syn::Expr>(rust_src) {
            Ok(e) => quote! { #e },
            Err(e) => {
                return syn::Error::new(
                    d.rust_span,
                    format!("`#[derived]` `rust` failed to parse as an expression: {e}"),
                )
                .to_compile_error();
            }
        };

        // Per-entry emission shape — see `DerivedAttr` fallibility
        // discussion in the spec's "From<&Model> / TryFrom<&Model>
        // emission" section.
        let shape = derived_shapes
            .get(i)
            .copied()
            .map(|(_, s)| s)
            .unwrap_or(FallibilityShape::Infallible);
        let init = match (any_fallible, shape) {
            // Whole visage is infallible — block returns T directly.
            (false, _) => quote! {
                #name: {
                    let model: &#source = src;
                    #expr_tokens
                },
            },
            // Visage is fallible, this entry is also fallible Shape 1
            // (trailing `?`). The inner `?` propagates from the splice
            // block to the surrounding try_from body; no outer `?`.
            (true, FallibilityShape::Shape1TrailingQuestion) => quote! {
                #name: {
                    let model: &#source = src;
                    #expr_tokens
                },
            },
            // Visage is fallible, this entry returns `Result<T, E>` —
            // unwrap via outer `?`. The `?` desugars to
            // `Err(From::from(e))`, requiring `VisageError: From<E>`.
            (true, FallibilityShape::Shape2to5Result) => quote! {
                #name: {
                    let model: &#source = src;
                    #expr_tokens
                }?,
            },
            // Visage is fallible but this entry is infallible — block
            // returns T; no outer `?` (no Result to unwrap).
            (true, FallibilityShape::Infallible) => quote! {
                #name: {
                    let model: &#source = src;
                    #expr_tokens
                },
            },
        };
        derived_field_inits.push(init);
    }

    // Serde's derive macros emit paths into `::serde::*` internally; the
    // `#[serde(crate = "...")]` attribute redirects them so the emitted
    // code routes through `::djogi::__private::serde::*` and no direct
    // `serde` dependency is required in the user's crate.
    let derive_path = quote! {
        #[derive(
            ::std::fmt::Debug,
            ::std::clone::Clone,
            ::djogi::__private::serde::Serialize,
            ::djogi::__private::serde::Deserialize,
        )]
        #[serde(crate = "::djogi::__private::serde")]
    };

    // Dispatch on relation-nesting + derived-fallibility presence.
    //
    // - **Scalar-only** visages with all-infallible derived entries
    //   (or no derived entries at all) emit `impl From<&Source>`. The
    //   stdlib provides a blanket `impl<T, U> TryFrom<U> for T where
    //   U: Into<T>` (with `Error = Infallible`), so a scalar-only
    //   visage automatically satisfies `TryFrom<&Source>` too.
    //
    //   Emitting an explicit `TryFrom<&Source, Error = VisageError>`
    //   here would conflict with the stdlib blanket (E0119) — don't.
    //
    // - **Relation-nesting** visages OR scalar-only visages with at
    //   least one fallible derived entry emit
    //   `impl TryFrom<&Source>` with `Error = VisageError`. Phase
    //   8.5 #231 adds derived fallibility as a second trigger for
    //   the TryFrom branch; the relation-nesting trigger is
    //   unchanged. A scalar `From` is unsound when any of the
    //   per-field init expressions may fail.
    let needs_try_from = has_relation_entry || any_fallible;
    let conv_impl = if needs_try_from {
        quote! {
            impl ::std::convert::TryFrom<&#source> for #proj_name {
                type Error = ::djogi::VisageError;
                fn try_from(src: &#source) -> ::std::result::Result<Self, Self::Error> {
                    ::std::result::Result::Ok(Self {
                        #(#fw_inits)*
                        #(#user_inits)*
                        #(#derived_field_inits)*
                    })
                }
            }
        }
    } else {
        quote! {
            impl ::std::convert::From<&#source> for #proj_name {
                fn from(src: &#source) -> Self {
                    Self {
                        #(#fw_inits)*
                        #(#user_inits)*
                        #(#derived_field_inits)*
                    }
                }
            }
        }
    };

    // Emit the visage's sibling `Fields` + `Filter` types plus the
    // `DjogiVisageOf<Source>` seal. The emitter mirrors the same scope gate
    // used above so the accessor set on `{Visage}Fields` matches the field
    // set on the visage struct exactly.
    let fields_filter_seal = crate::model::visage_fields::expand(ctx);

    // Emit the visage's `::filter(...)` queryset entry block + the visage's
    // narrow `FromPgRow` impl. The emitter bails out for relation-embed
    // visages (the SELECT projection can't represent an embedded peer as a
    // single column) and for `pk = None` source models (no
    // `Model::table_name()` to reach).
    let queryset_entry = crate::model::visage_query::expand(ctx);

    // Phase 8.5 #231 — emit the `DjogiVisage` trait impl + the
    // `assert_derived_parity` inherent method, when applicable.
    let djogi_visage_impl = emit_djogi_visage_impl(ctx, &scoped_derived);
    let (parity_impl, visage_descriptor) = if scoped_derived.is_empty() {
        (TokenStream::new(), TokenStream::new())
    } else {
        (
            emit_assert_derived_parity(proj_name, &scoped_derived),
            emit_visage_descriptor(ctx, &scoped_derived),
        )
    };

    quote! {
        #derive_path
        pub struct #proj_name {
            #(#fw_fields)*
            #(#user_fields)*
            #(#derived_field_decls)*
        }

        #conv_impl

        #fields_filter_seal

        #queryset_entry

        #djogi_visage_impl

        #parity_impl

        #visage_descriptor
    }
}

/// Compute the visage's projection-entry list: `(name, is_derived_alias)` pairs.
///
/// The order matches the visage struct's field order — framework
/// columns first (`id`, `created_at`, `updated_at`), then user columns
/// in declaration order (filtered to those exposed in the scope), then
/// derived entries in attribute declaration order. The `is_derived`
/// flag carries the discriminant the SELECT renderer uses to wrap
/// derived entries with `(<sql>) AS <alias>`.
///
/// Relation-embed entries are intentionally excluded — the visage's
/// SELECT projection cannot represent an embedded peer as a flat
/// column. `emit_djogi_visage_impl` and `emit_projection_list_string`
/// both honour this gate via the `RelationEmbed` skip in the scope
/// classifier.
pub(crate) fn projection_entries(ctx: &VisageEmitContext<'_>) -> Vec<(String, bool, String)> {
    // (name-or-alias, is_derived, sql-fragment-or-column-name)
    let model_attrs = ctx.model_attrs;
    let struct_item = ctx.struct_item;
    let field_attrs = ctx.field_attrs;
    let n_framework = ctx.n_framework;
    let scope = ctx.scope;

    let mut out: Vec<(String, bool, String)> = Vec::new();
    if matches!(model_attrs.pk, PkStrategy::None) {
        return out;
    }

    // Framework columns — present only for model-backed flat projections.
    out.push(("id".to_string(), false, "id".to_string()));
    out.push(("created_at".to_string(), false, "created_at".to_string()));
    out.push(("updated_at".to_string(), false, "updated_at".to_string()));

    let user_field_pairs: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    let mut saw_relation_embed = false;
    for (field, attrs) in &user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        match classify_field_for_scope(field, attrs, scope) {
            ScopeMembership::Absent | ScopeMembership::Reject { .. } => continue,
            ScopeMembership::Scalar => {
                let col = crate::syn_util::column_name_from_ident(fname);
                out.push((col.clone(), false, col));
            }
            ScopeMembership::RelationEmbed { .. } => {
                saw_relation_embed = true;
                break;
            }
        }
    }

    // Relation-embed visages do not get a flat projection — clear the
    // list so the consumer (DjogiVisage trait impl, projection-list
    // emitter) knows to bail. The visage struct + From/TryFrom
    // conversion still exist; only the SELECT path is unsupported.
    if saw_relation_embed {
        return Vec::new();
    }

    // Append derived entries in attribute declaration order. The third
    // tuple slot carries the verbatim adopter `sql` so the SELECT
    // renderer can wrap it with `(<sql>) AS <alias>`.
    for d in ctx.scope_derived() {
        let name = d.name.to_string();
        let name = name.strip_prefix("r#").unwrap_or(name.as_str()).to_string();
        out.push((name, true, d.sql.clone()));
    }

    out
}

/// Emit the per-visage `DjogiVisage` trait impl, when the projection
/// has a flat (non-relation-embed) shape. Relation-embed visages skip
/// the impl — their SELECT path is unsupported, so the trait constants
/// would carry stale data.
fn emit_djogi_visage_impl(
    ctx: &VisageEmitContext<'_>,
    _scoped_derived: &[&DerivedAttr],
) -> TokenStream {
    let entries = projection_entries(ctx);
    if entries.is_empty() {
        return TokenStream::new();
    }
    let proj_name = &ctx.visage_ident;
    let scope = ctx.scope;

    // `COLUMNS` — every ordinal position's name. Column entries: the
    // raw column name. Derived entries: the alias.
    let columns_lits: Vec<TokenStream> = entries
        .iter()
        .map(|(name, _, _)| {
            let n = name.as_str();
            quote! { #n }
        })
        .collect();

    // `PROJECTIONS` — sealed `ProjectionEntry` list. Column entries
    // lower to `Column("<name>")`; derived entries lower to
    // `Derived { alias, sql }`.
    let projection_entry_lits: Vec<TokenStream> = entries
        .iter()
        .map(|(name, is_derived, payload)| {
            let n = name.as_str();
            let p = payload.as_str();
            if *is_derived {
                quote! {
                    ::djogi::__private::ProjectionEntry::Derived {
                        alias: #n,
                        sql: #p,
                    }
                }
            } else {
                quote! { ::djogi::__private::ProjectionEntry::Column(#n) }
            }
        })
        .collect();

    // `PROJECTION_LIST` — rendered comma-joined SELECT-list string.
    // Column entries pass through verbatim; derived entries render as
    // `(<sql>) AS <alias>`.
    let mut projection_list = String::new();
    for (i, (name, is_derived, payload)) in entries.iter().enumerate() {
        if i > 0 {
            projection_list.push_str(", ");
        }
        if *is_derived {
            projection_list.push('(');
            projection_list.push_str(payload);
            projection_list.push_str(") AS ");
            projection_list.push_str(name);
        } else {
            projection_list.push_str(name);
        }
    }

    let source = ctx.source;
    quote! {
        // Phase 8.5 #231 reconciliation — emit `type Model = #source`
        // so generic `V: DjogiVisage` consumers reach the source model
        // (and the source table via `<V::Model as
        // ::djogi::prelude::Model>::table_name()`) without threading
        // the model in as a separate type parameter. The seal is the
        // existing `DjogiVisageOf<#source>` impl emitted alongside the
        // `{Visage}Fields` machinery in `visage_fields::expand` — no
        // standalone metadata seal is required.
        impl ::djogi::DjogiVisage for #proj_name {
            type Model = #source;
            const SCOPE: &'static str = #scope;
            const COLUMNS: &'static [&'static str] = &[ #(#columns_lits),* ];
            const PROJECTIONS: &'static [::djogi::__private::ProjectionEntry] = &[
                #(#projection_entry_lits),*
            ];
            const PROJECTION_LIST: &'static str = #projection_list;
        }
    }
}

/// Emit the `assert_derived_parity` inherent method AND the parallel
/// `impl DerivedParity for {Visage}` trait impl on a visage. Both are
/// only emitted when the visage has at least one derived entry in its
/// scope.
///
/// Compares ONLY the derived fields between two pre-constructed
/// visages; framework columns and storage columns are intentionally
/// not compared (their round-trip lossy `DateTime` truncation would
/// false-positive on high-precision timestamps regardless of any
/// derived drift). Short-circuits at the first mismatch.
///
/// Emits a `where <Ty>: PartialEq` bound per distinct derived type so
/// rustc's E0277 diagnostic anchors at the impl block (E_DJG_VDF_016
/// per the spec) rather than at the inner `!=` token.
///
/// # Two surfaces, one body
///
/// - **Inherent method** — `visage.assert_derived_parity(&other)`
///   resolves via Rust's inherent-method-first method resolution. No
///   trait import required at the call site; this is the ergonomic
///   shape integration tests use.
/// - **Trait impl** — `impl ::djogi::testing::DerivedParity for V`
///   carries the same body. Reachable from generic code that bounds
///   `where V: DerivedParity` — required by the async
///   [`::djogi::testing::assert_derived_parity_fetched`] free helper
///   (Phase 8.5 #231 reconciliation: CTO-required async convenience).
///
/// Method resolution in Rust prefers inherent methods over trait
/// methods for unqualified calls (`v.foo()`); the trait method is
/// reachable through generic bounds. Both surfaces share the same
/// comparison body, so adopters never see different behaviour
/// depending on which surface they reach.
fn emit_assert_derived_parity(
    proj_name: &syn::Ident,
    scoped_derived: &[&DerivedAttr],
) -> TokenStream {
    let proj_str = proj_name.to_string();

    let comparisons: Vec<TokenStream> = scoped_derived
        .iter()
        .map(|d| {
            let name = &d.name;
            let name_str = name.to_string();
            let stripped = name_str
                .strip_prefix("r#")
                .unwrap_or(name_str.as_str())
                .to_string();
            quote! {
                if self.#name != other.#name {
                    return ::std::result::Result::Err(
                        ::djogi::testing::DerivedParityError::Drift {
                            visage: #proj_str,
                            field: #stripped,
                        }
                    );
                }
            }
        })
        .collect();

    // Deduplicate ty tokens for the `where`-bound list. Token-level
    // equality is sufficient — distinct spellings of the same type
    // produce distinct bounds; the dedupe keeps the impl block from
    // listing the same bound twice.
    let mut seen: Vec<String> = Vec::new();
    let mut where_bounds: Vec<TokenStream> = Vec::new();
    for d in scoped_derived {
        let ty = &d.ty;
        let key = quote! { #ty }.to_string();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        where_bounds.push(quote! { #ty: ::std::cmp::PartialEq });
    }
    let where_clause = if where_bounds.is_empty() {
        TokenStream::new()
    } else {
        quote! { where #(#where_bounds),* }
    };

    let doc = format!(
        " Compare derived fields between two `{proj_str}` instances and \
         return `Err(DerivedParityError::Drift {{ ... }})` on the first \
         mismatch. Framework columns (`id`, `created_at`, `updated_at`) \
         and storage columns are NEVER compared — only fields populated \
         from `#[derived(...)]` declarations whose `scopes = [...]` list \
         includes this visage's scope.\n\n\
         Phase 8.5 issue #231 — see \
         `docs/spec/visage-derived-fields.md` for the parity-helper \
         design rationale (the per-visage emission is the macro's \
         answer to round-trip-lossy timestamp false positives + the \
         absence of an auto-derived `PartialEq` on visages)."
    );

    // The seal-only supertrait impl carries no body; the constraint
    // ladder it satisfies lives entirely on `DerivedParitySealed`'s
    // own definition site. The trait impl below carries the same
    // `where` bounds as the inherent so the `<Ty>: PartialEq`
    // diagnostic anchors at one stable site (E_DJG_VDF_016).
    quote! {
        // Phase 8.5 #231 reconciliation — route the seal through
        // `::djogi::__private::DerivedParitySealed` per
        // `feedback_macro_path_routing.md` (macro paths route through
        // `::djogi::*` only; never through `::djogi::testing::*`
        // submodules). The `__private` re-export aliases the same
        // `djogi::testing::private::DerivedParitySealed` trait, so
        // the seal closure does not change — only the path the
        // macro emits.
        impl ::djogi::__private::DerivedParitySealed for #proj_name {}

        impl #proj_name #where_clause {
            #[doc = #doc]
            pub fn assert_derived_parity(
                &self,
                other: &Self,
            ) -> ::std::result::Result<(), ::djogi::testing::DerivedParityError> {
                #(#comparisons)*
                ::std::result::Result::Ok(())
            }
        }

        impl ::djogi::testing::DerivedParity for #proj_name #where_clause {
            fn assert_derived_parity(
                &self,
                other: &Self,
            ) -> ::std::result::Result<(), ::djogi::testing::DerivedParityError> {
                #(#comparisons)*
                ::std::result::Result::Ok(())
            }
        }
    }
}

/// Emit one `inventory::submit!(VisageDescriptor { ... })` block for the
/// `(Source, Scope)` pair when at least one derived entry is in scope —
/// Phase 8.5 issue #231 reconciliation (BLOCK-1).
///
/// Structurally separate from the [`ModelDescriptor`] inventory the
/// migration differ walks: registers against
/// [`::djogi::descriptor::VisageDescriptor`]'s own
/// `inventory::collect!` collection, which migration / snapshot /
/// `build.rs` paths never iterate. The boundary mirrors the storage-
/// vs-projection split the rest of the visage-derived-fields surface
/// establishes.
///
/// # Per-entry contents
///
/// - `name` — derived field name (the `name = ...` key).
/// - `ty_path` — token-string rendering of the entry's `ty = ...`,
///   captured via `quote! { #ty }.to_string()`. The exact text
///   includes token-level whitespace (`"Option < String >"`) — that
///   is the source spelling documentation generators want.
/// - `sql` — adopter's SQL expression, verbatim.
/// - `rust` — adopter's Rust expression source, verbatim.
/// - `doc` — `Some("...")` when the entry declared `doc = "..."`,
///   `None` otherwise. The macro emits `None` / `Some("...")`
///   literally inside the const literal so the slice is fully
///   `&'static`.
/// - `scopes` — every scope the entry was declared against, in
///   source order.
fn emit_visage_descriptor(
    ctx: &VisageEmitContext<'_>,
    scoped_derived: &[&DerivedAttr],
) -> TokenStream {
    // Bail early if this scope's visage is not flat-projected. Same
    // gate the `DjogiVisage` trait impl uses — a relation-embed
    // visage has no flat SELECT shape, so a per-`(Model, scope)`
    // descriptor of its derived entries would describe a projection
    // that does not exist.
    let entries = projection_entries(ctx);
    if entries.is_empty() {
        return TokenStream::new();
    }
    if matches!(ctx.model_attrs.pk, PkStrategy::None) {
        return TokenStream::new();
    }

    let source_str = ctx.source.to_string();
    let scope_str = ctx.scope;
    let visage_str = ctx.visage_ident.to_string();

    let derived_entry_lits: Vec<TokenStream> = scoped_derived
        .iter()
        .map(|d| {
            // The name on the wire / struct field side strips `r#`
            // raw-prefix; the descriptor consumer expects the same
            // shape `COLUMNS` / `PROJECTION_LIST` carry.
            let raw_name = d.name.to_string();
            let name_stripped = raw_name
                .strip_prefix("r#")
                .unwrap_or(raw_name.as_str())
                .to_string();
            // Render the `syn::Type` to a token-string. Token-level
            // whitespace (`Option < String >`) is acceptable — this
            // is documentation-consumer surface, not a re-parsed
            // form.
            let ty_path_str = {
                let ty = &d.ty;
                quote! { #ty }.to_string()
            };
            let sql_str = &d.sql;
            let rust_str = &d.rust;
            let doc_tokens = match &d.doc {
                Some(s) => quote! { ::std::option::Option::Some(#s) },
                None => quote! { ::std::option::Option::None },
            };
            let scope_lits: Vec<TokenStream> = d
                .scopes
                .iter()
                .map(|s| {
                    let k = s.key;
                    quote! { #k }
                })
                .collect();
            quote! {
                ::djogi::descriptor::DerivedProjection {
                    name:    #name_stripped,
                    ty_path: #ty_path_str,
                    sql:     #sql_str,
                    rust:    #rust_str,
                    doc:     #doc_tokens,
                    scopes:  &[ #(#scope_lits),* ],
                }
            }
        })
        .collect();

    quote! {
        ::djogi::__private::inventory::submit! {
            ::djogi::descriptor::VisageDescriptor {
                model_name:  #source_str,
                scope:       #scope_str,
                visage_name: #visage_str,
                derived:     &[ #(#derived_entry_lits),* ],
            }
        }
    }
}

fn framework_field_decls(model_attrs: &ModelAttrs) -> Vec<TokenStream> {
    let mut out = Vec::new();
    match &model_attrs.pk {
        PkStrategy::HeerId => {
            out.push(quote! { pub id: ::djogi::types::HeerId, });
        }
        PkStrategy::RanjId => {
            out.push(quote! { pub id: ::djogi::types::RanjId, });
        }
        PkStrategy::HeerIdDesc => {
            out.push(quote! { pub id: ::djogi::types::HeerIdDesc, });
        }
        PkStrategy::RanjIdDesc => {
            out.push(quote! { pub id: ::djogi::types::RanjIdDesc, });
        }
        PkStrategy::Serial => {
            out.push(quote! { pub id: i32, });
        }
        PkStrategy::None => {}
        PkStrategy::Custom(path) => {
            out.push(quote! { pub id: #path, });
        }
    }
    out.push(quote! { pub created_at: ::djogi::types::DateTime, });
    out.push(quote! { pub updated_at: ::djogi::types::DateTime, });
    out
}

fn framework_field_inits(model_attrs: &ModelAttrs) -> Vec<TokenStream> {
    let mut out = Vec::new();
    match &model_attrs.pk {
        PkStrategy::None => {}
        _ => {
            out.push(quote! { id: ::std::clone::Clone::clone(&src.id), });
        }
    }
    out.push(quote! { created_at: src.created_at, });
    out.push(quote! { updated_at: src.updated_at, });
    out
}
