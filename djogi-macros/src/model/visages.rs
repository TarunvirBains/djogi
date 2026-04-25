//! Phase 4.5 — emit visage structs + conversion impls from `#[model]`.
//!
//! For each `#[model]` struct, generate four visage structs:
//! `{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`.
//! Each struct carries (in source order):
//!
//! 1. Framework columns (`id`, `created_at`, `updated_at`) — always
//!    included in every visage, regardless of user annotations (Q13).
//! 2. User fields annotated with `expose(scope)` (scalar) or
//!    `expose(scope -> Peer)` (relation — narrow visage or full peer
//!    embed; see below). The deprecated `expose(scope = "Peer")`
//!    string-literal form is still parsed for backward compat but
//!    lowers to the same `RelationExposure` shape.
//!
//! Each visage derives `Debug`, `Clone`, `serde::Serialize`,
//! `serde::Deserialize` unconditionally (D3). Conversion impls:
//!
//! - **Scalar-only** visage (no relation-form entries): `impl From<&Model>`
//!   — infallible straight-line construction.
//! - **Relation-nesting** visage (at least one `expose(scope -> Peer)`
//!   entry on a relation field): `impl TryFrom<&Model>` with
//!   `Error = VisageError`. Optional FK relations emit
//!   `Option<PeerVisage>` and route the resolved relation through
//!   `<PeerVisage as TryFrom<&Target>>::try_from` only when `Some`.
//!
//! ## Phase 7-Zero-2 T6 grammar
//!
//! `expose(scope -> Peer)` selects one of two embed shapes based on the
//! peer path's last segment:
//!
//! - **Narrow visage** — last segment is a `{Model}{Scope}` shape
//!   (`DepartmentPublic`); peer constructed via fallible `TryFrom`.
//! - **Full peer model** — last segment matches the relation target's
//!   ident (`Department`); peer cloned out of the resolved relation,
//!   serde derives delegate to the target's own (de)serialise impls.
//!
//! Optional FKs emit `Option<PeerVisage>` honestly at the type level —
//! the prior Phase 4.5 deferral that rejected `expose` on
//! `Option<ForeignKey<T>>` was lifted in T6.
//!
//! Path routing: every type reference in emitted code goes through
//! `::djogi::*` (`feedback_macro_path_routing.md`) so users never depend on
//! `serde` / `time` / `heeranjid` directly.

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, RelationExposure, detect_relation};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, ItemStruct};

/// Every scope emits one generated visage struct, in this fixed order.
const SCOPES: &[(&str, &str)] = &[
    ("public", "Public"),
    ("self_view", "SelfView"),
    ("admin", "Admin"),
    ("export", "Export"),
];

pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> TokenStream {
    let source_name = &struct_item.ident;

    // Framework columns are prepended by `inject::expand`; n_framework
    // matches the count pushed there (id gated on `pk != none` + the two
    // timestamp columns).
    let n_framework = match &model_attrs.pk {
        PkStrategy::None => 2,
        _ => 3,
    };

    let visages: Vec<TokenStream> = SCOPES
        .iter()
        .map(|(scope, suffix)| {
            emit_projection_for_scope(
                source_name,
                suffix,
                scope,
                struct_item,
                field_attrs,
                model_attrs,
                n_framework,
            )
        })
        .collect();

    quote! {
        #(#visages)*
    }
}

fn emit_projection_for_scope(
    source: &Ident,
    suffix: &str,
    scope: &str,
    struct_item: &ItemStruct,
    field_attrs: &[FieldAttrs],
    model_attrs: &ModelAttrs,
    n_framework: usize,
) -> TokenStream {
    let proj_name = format_ident!("{source}{suffix}");
    let source_name_str = source.to_string();

    let fw_fields = framework_field_decls(model_attrs);
    let fw_inits = framework_field_inits(model_attrs);

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

        if attrs.expose.suppressed {
            continue;
        }

        let scalar_hit = attrs.expose.scalar_scopes.contains(scope);
        let relation_hit = attrs.expose.relation_scopes.get(scope);
        let relation_info = detect_relation(fty);
        let is_relation = relation_info.is_some();

        match (scalar_hit, relation_hit, is_relation) {
            // Field does not appear in this scope.
            (false, None, _) => continue,

            // Parser already rejects this within a single attribute; a
            // cross-attribute duplicate also errors in FieldAttrs::parse.
            (true, Some(_), _) => unreachable!(
                "parser rejects mixed scalar+relation on same scope at ExposeSpec::parse_list / FieldAttrs::parse"
            ),

            // Scalar form on scalar field — Task 3 happy path.
            (true, None, false) => {
                user_fields.push(quote! { pub #fname: #fty, });
                user_inits.push(quote! {
                    #fname: ::std::clone::Clone::clone(&src.#fname),
                });
            }

            // Scalar form on relation field — reject at codegen time.
            (true, None, true) => {
                let msg = format!(
                    "relation fields require an explicit peer visage name; \
                     write `expose({scope} = \"PeerProjection\")`"
                );
                return syn::Error::new_spanned(field, msg).to_compile_error();
            }

            // Relation form on scalar field — reject at codegen time.
            (false, Some(_), false) => {
                let msg = format!(
                    "the `expose({scope} = \"...\")` form is only valid on \
                     relation fields (`ForeignKey<T>` / `OneToOneField<T>`)"
                );
                return syn::Error::new_spanned(field, msg).to_compile_error();
            }

            // Relation form on relation field — Phase 7-Zero-2 T6 lifts
            // the prior `Option<FK>` / `Option<O2O>` rejection. Optional
            // relations now emit `pub field: Option<PeerVisage>`, with the
            // init match-folding `src.field.resolved()` through the peer's
            // fallible `TryFrom<&Target>` impl.
            //
            // Full-peer vs narrow embed:
            //
            // - If the user wrote `expose(scope -> Department)` and the
            //   relation's target ident is `Department`, embed the full
            //   peer model (clone the resolved target).
            // - Otherwise (path's last segment doesn't match the target
            //   ident), treat as a narrow visage and route through
            //   `<Peer as TryFrom<&Target>>::try_from(resolved)?`.
            (false, Some(exposure), true) => {
                has_relation_entry = true;
                let info = relation_info
                    .as_ref()
                    .expect("is_relation == true implies Some(relation_info)");
                let fname_str = fname.to_string();
                let nullable = info.nullable;
                let peer_path = &exposure.peer;
                let is_full_peer = is_full_peer_path(exposure, info);

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

    // Dispatch on relation-nesting presence.
    //
    // - **Scalar-only** visages emit `impl From<&Source>`. The
    //   stdlib provides a blanket `impl<T, U> TryFrom<U> for T where
    //   U: Into<T>` (with `Error = Infallible`), so a scalar-only
    //   visage automatically satisfies `TryFrom<&Source>` too. The
    //   relation-nesting emitter above calls
    //   `<Peer as TryFrom<&_>>::try_from(resolved)?` uniformly; the `?`
    //   coerces the blanket's `Infallible` error into `VisageError`
    //   via the `impl From<Infallible> for VisageError` glue in
    //   `djogi/src/visage.rs`. That is what makes transitive
    //   nesting (Vehicle → Owner → Department) compose without the
    //   relation-nesting emitter knowing each peer's shape.
    //
    //   Emitting an explicit `TryFrom<&Source, Error = VisageError>`
    //   here would conflict with the stdlib blanket (E0119) — don't.
    //
    // - **Relation-nesting** visages emit only `impl TryFrom<&Source>`
    //   with `Error = VisageError`. A scalar `From` is unsound for
    //   this case because the `.resolved()` probe is fallible.
    let conv_impl = if has_relation_entry {
        quote! {
            impl ::std::convert::TryFrom<&#source> for #proj_name {
                type Error = ::djogi::VisageError;
                fn try_from(src: &#source) -> ::std::result::Result<Self, Self::Error> {
                    ::std::result::Result::Ok(Self {
                        #(#fw_inits)*
                        #(#user_inits)*
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
                    }
                }
            }
        }
    };

    // Phase 7-Zero-2 T7 — emit the visage's sibling `Fields` + `Filter`
    // types plus the `DjogiVisageOf<Source>` seal. The emitter mirrors the
    // same scope gate used above so the accessor set on `{Visage}Fields`
    // matches the field set on the visage struct exactly.
    let fields_filter_seal = crate::model::visage_fields::expand(
        source,
        &proj_name,
        scope,
        struct_item,
        field_attrs,
        model_attrs,
        n_framework,
    );

    // Phase 7-Zero-2 T10 — emit the visage's `::filter(...)` queryset
    // entry block + the visage's narrow `FromPgRow` impl. The emitter
    // bails out for relation-embed visages (the SELECT projection
    // can't represent an embedded peer as a single column) and for
    // `pk = None` source models (no `Model::table_name()` to reach).
    let queryset_entry = crate::model::visage_query::expand(
        source,
        &proj_name,
        scope,
        struct_item,
        field_attrs,
        model_attrs,
        n_framework,
    );

    quote! {
        #derive_path
        pub struct #proj_name {
            #(#fw_fields)*
            #(#user_fields)*
        }

        #conv_impl

        #fields_filter_seal

        #queryset_entry
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

/// Phase 7-Zero-2 T6 — decide whether the user's `expose(scope -> Peer)`
/// path resolves to the relation's full target model (full-peer embed) or
/// to a narrow `{Model}{Scope}` visage.
///
/// Heuristic: compare the *last* segment of the user-written peer path
/// against the relation target's ident (e.g. `Department` for a
/// `ForeignKey<Department>`). If they match exactly, the user is asking
/// for a full-peer embed; otherwise it's a narrow visage and the emitter
/// dispatches through `TryFrom<&Target>` for fallible peer construction.
///
/// The check inspects the path's last segment only, mirroring how
/// [`detect_relation`] tolerates fully-qualified spellings (e.g.
/// `crate::models::Department`). Disambiguation by full path is not
/// attempted here — the conservative choice for T6 is to anchor on the
/// last-segment ident, which is what the user-visible name binding does.
///
/// Edge cases:
/// - `Peer` matches the target's bare ident → full-peer.
/// - `module::Peer` where `Peer` matches the target ident → full-peer
///   (the user is reaching for the same model through a re-export).
/// - `DepartmentPublic` where target is `Department` → narrow.
/// - Anything else (last segment differs from target ident) → narrow.
///   If the resulting `{Peer}` doesn't exist as a type, the user gets a
///   span-carrying compile error from the emitted `TryFrom<&Target>`
///   call — Rust's name resolution surfaces it cleanly.
fn is_full_peer_path(
    exposure: &RelationExposure,
    info: &crate::model::attrs::RelationInfo,
) -> bool {
    let Some(last) = exposure.peer.segments.last() else {
        return false;
    };
    last.ident == info.target_name
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
