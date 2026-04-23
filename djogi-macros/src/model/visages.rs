//! Phase 4.5 — emit visage structs + conversion impls from `#[model]`.
//!
//! For each `#[model]` struct, generate four visage structs:
//! `{Model}Public`, `{Model}SelfView`, `{Model}Admin`, `{Model}Export`.
//! Each struct carries (in source order):
//!
//! 1. Framework columns (`id`, `created_at`, `updated_at`) — always
//!    included in every visage, regardless of user annotations (Q13).
//! 2. User fields annotated with `expose(scope)` (scalar) or
//!    `expose(scope = "PeerProjection")` (relation — deferred to Task 5).
//!
//! Each visage derives `Debug`, `Clone`, `serde::Serialize`,
//! `serde::Deserialize` unconditionally (D3). Conversion impls:
//!
//! - **Scalar-only** visage (no relation-form entries): `impl From<&Model>`
//!   — infallible straight-line construction.
//! - **Relation-nesting** visage (at least one `expose(scope = "Peer")`
//!   entry on a relation field): `impl TryFrom<&Model>` with
//!   `Error = VisageError` — Task 5 replaces the Task-3 stub.
//!
//! Path routing: every type reference in emitted code goes through
//! `::djogi::*` (`feedback_macro_path_routing.md`) so users never depend on
//! `serde` / `time` / `heeranjid` directly.

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
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
    let n_framework = match model_attrs.pk {
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

            // Relation form on relation field — nest the peer visage
            // via `.resolved()`. Option<FK> / Option<O2O> is deferred to a
            // follow-up phase; reject it at codegen time with a loud error.
            (false, Some(peer), true) => {
                if let Some(info) = &relation_info
                    && info.nullable
                {
                    return syn::Error::new_spanned(
                        field,
                        "`Option<ForeignKey<T>>` / `Option<OneToOneField<T>>` \
                         in relation-form visages is deferred to a \
                         follow-up phase; use a required FK in Phase 4.5",
                    )
                    .to_compile_error();
                }
                has_relation_entry = true;
                let peer_ident: Ident =
                    syn::parse_str(peer).unwrap_or_else(|_| format_ident!("__djogi_invalid_peer"));
                let fname_str = fname.to_string();
                user_fields.push(quote! { pub #fname: #peer_ident, });
                user_inits.push(quote! {
                    #fname: {
                        let resolved = src.#fname.resolved().ok_or(
                            ::djogi::VisageError::UnresolvedRelation {
                                model: #source_name_str,
                                field: #fname_str,
                                scope: #scope,
                            }
                        )?;
                        // Peer construction goes through `TryFrom::try_from(...)?`
                        // uniformly: scalar-only peers satisfy it via the
                        // Phase 4.5-emitted `TryFrom<&Peer, Error = VisageError>`
                        // wrapper that returns `Ok(From::from(...))`; relation-
                        // nesting peers satisfy it via their own fallible
                        // emission. This unifies scalar and nested-relation peer
                        // usage — Vehicle → Owner → Department nesting composes.
                        <#peer_ident as ::std::convert::TryFrom<&_>>::try_from(resolved)?
                    },
                });
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

    quote! {
        #derive_path
        pub struct #proj_name {
            #(#fw_fields)*
            #(#user_fields)*
        }

        #conv_impl
    }
}

fn framework_field_decls(model_attrs: &ModelAttrs) -> Vec<TokenStream> {
    let mut out = Vec::new();
    match model_attrs.pk {
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
    }
    out.push(quote! { pub created_at: ::djogi::types::DateTime, });
    out.push(quote! { pub updated_at: ::djogi::types::DateTime, });
    out
}

fn framework_field_inits(model_attrs: &ModelAttrs) -> Vec<TokenStream> {
    let mut out = Vec::new();
    match model_attrs.pk {
        PkStrategy::None => {}
        _ => {
            out.push(quote! { id: ::std::clone::Clone::clone(&src.id), });
        }
    }
    out.push(quote! { created_at: src.created_at, });
    out.push(quote! { updated_at: src.updated_at, });
    out
}
