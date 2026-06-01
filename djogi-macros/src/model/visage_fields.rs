//! Emit `{Visage}Fields`, `{Visage}Filter`, and the `DjogiVisageOf<SourceModel>`
//! seal impl for a single visage.
//! # Design
//! `{Visage}Fields` is a state-carrying struct that threads a SQL-alias path
//! prefix through traversal chains:
//! ```ignore
//! pub struct UserPublicFields<RootModel = User> {
//!     pub __djogi_path: Option<&'static str>,
//!     pub __djogi_root: PhantomData<fn() -> RootModel>,
//! }
//! ```
//! - Root construction: `UserPublicFields::default` sets `__djogi_path = None`
//! and defaults `RootModel = User`.
//! - Traversal construction: `UserPublicFields::with_path("owner")` sets
//! `__djogi_path = Some("owner")` so the peer's scalar accessors produce
//! `FieldRef`s whose column path is `"owner.{column}"`.
//! - Traversal typing: `UserPublicFields<Post>` means "the peer visage fields
//! for `User`, but predicates built from them still target the owning
//! `Post` root model."
//! Accessors are `&self` methods so the path state is available inside every call:
//! ```ignore
//! impl UserPublicFields {
//!     pub fn display_name(&self) -> FieldRef<User, String> {
//!         __make_field_ref(self.__djogi_path, "display_name")
//!     }
//!     pub fn owner(&self) -> OwnerPublicFields {
//!         OwnerPublicFields::with_path("owner")
//!     }
//! }
//! ```
//! Protected scalar fields that route through a per-scope presentation codec
//! still use the same path-threading, but their accessors return
//! `PresentationFieldRef<Source, Codec, StorageTy>` so predicate / ordering
//! methods are gated by the codec traits instead of leaking the raw
//! `FieldRef` surface.
//! # Optional-FK accessor shape
//! A relation-form entry on a nullable FK / O2O field emits an accessor
//! returning [`OptionalRelationRef<PeerFields>`]. The wrapper's `map_filter(|a| …)`
//! combinator emits `author_id IS NOT NULL AND <inner>` so the nullability
//! is honoured at the SQL level.
//! Required FKs (non-`Option`) keep the plain `PeerFields` return type.
//! # `FieldRef<RootModel, V>` over `FieldRef<Visage, V>`
//! Visages do not impl `Model` (they are projections, not tables). Accessors
//! are typed on the **owning root model** — e.g. `UserPublicFields<Post>::name`
//! yields `FieldRef<Post, String>` — and the visage ↔ source-model pairing
//! is tracked separately via `DjogiVisageOf<M>`.
//! # Non-exposed fields are absent by construction
//! Referencing a non-exposed field in a closure fails at compile time with
//! a "no method named …" error. That is the compile-time enforcement that
//! makes the visage a genuine data-access boundary.
//! [`OptionalRelationRef<PeerFields>`]: ::djogi::query::OptionalRelationRef

use crate::model::attrs::PkStrategy;
use crate::model::visage_ctx::{
    ScopeMembership, VisageEmitContext, classify_field_for_scope, is_full_peer_for,
    peer_traversal_fields_path,
};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Ident;

/// Emit `{Visage}Fields`, `{Visage}Filter`, and the sealed
/// `DjogiVisageOf<SourceModel>` impl for a single visage.
pub fn expand(ctx: &VisageEmitContext<'_>) -> TokenStream {
    let source = ctx.source;
    let root_ident = format_ident!("RootModel");
    let visage_ident = &ctx.visage_ident;
    let scope = ctx.scope;
    let struct_item = ctx.struct_item;
    let field_attrs = ctx.field_attrs;
    let model_attrs = ctx.model_attrs;
    let n_framework = ctx.n_framework;
    let fields_ident = format_ident!("{visage_ident}Fields");
    let filter_ident = format_ident!("{visage_ident}Filter");
    let filter_doc = format!(
        "Placeholder filter builder for the `{visage_ident}` visage. \
         The closure-based entry point (`{visage_ident}::filter(|f| …)`) \
         and programmatic setters land in a later task."
    );

    // `pk = None` gate — `FieldRef<M, V>` carries `M: Model` and
    // `DjogiVisageOf<M>` likewise bounds `M: Model`. `crud::expand` does
    // not emit `impl Model` for `pk = None` models, so the `{Visage}Fields`
    // accessors and the seal cannot reference the source type. Emit only
    // the placeholder `{Visage}Filter` ZST for those models — it carries
    // no `M: Model` bound and the closure-based builder may still
    // want the name to exist on pk = none surfaces.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return quote! {
            #[doc = #filter_doc]
            #[derive(Debug, Clone, Copy, Default)]
            pub struct #filter_ident;
        };
    }

    let id_ty = pk_type_tokens(&model_attrs.pk);

    // Framework accessors — `id` / `created_at` / `updated_at`. Paths are
    // threaded the same way user-field accessors are — an `id` accessor
    // on a traversal-targeted `Fields` emits `"prefix.id"`, which is
    // what the SQL emitter needs for cross-relation comparisons.
    let mut accessors: Vec<TokenStream> = Vec::new();

    accessors.push(emit_scalar_accessor(
        &root_ident,
        &format_ident!("id"),
        "id",
        &id_ty,
    ));
    accessors.push(emit_scalar_accessor(
        &root_ident,
        &format_ident!("created_at"),
        "created_at",
        &quote! { ::djogi::types::DateTime },
    ));
    accessors.push(emit_scalar_accessor(
        &root_ident,
        &format_ident!("updated_at"),
        "updated_at",
        &quote! { ::djogi::types::DateTime },
    ));

    // Per-user-field accessors — scope gate mirrors
    // `visages::emit_projection_for_scope` exactly.
    let user_field_pairs: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    for (field, attrs) in user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        let fty = &field.ty;

        let column = crate::syn_util::column_name_from_ident(fname);

        match classify_field_for_scope(field, attrs, scope) {
            ScopeMembership::Absent | ScopeMembership::Reject { .. } => continue,

            // Scalar form on scalar field — plain columns emit a path-aware
            // `FieldRef<Source, Ty>` accessor; per-scope presentation codecs
            // emit the codec-gated `PresentationFieldRef<Source, Codec, Ty>`
            // wrapper instead so queryability is enforced through the codec
            // traits.
            ScopeMembership::Scalar => {
                let fty_ts = quote! { #fty };
                if let Some(codec_ty) = lookup_per_scope_codec(attrs, scope) {
                    accessors.push(emit_presentation_scalar_accessor(
                        &root_ident,
                        fname,
                        &column,
                        codec_ty,
                        &fty_ts,
                    ));
                } else {
                    accessors.push(emit_scalar_accessor(&root_ident, fname, &column, &fty_ts));
                }
            }

            // Relation form on relation field — emit a path-threaded
            // accessor returning the peer's `{PeerVisage}Fields` (required
            // FK) or `OptionalRelationRef<{PeerVisage}Fields>` (optional
            // FK / O2O).
            ScopeMembership::RelationEmbed { exposure, nullable } => {
                // After the path-aware peer fields handle
                // depends on the exposure shape: full-peer
                // (`expose(scope -> Department)`) routes through
                // `{Department}SqlFields` because root `{Department}Fields`
                // is now a portable ZST without `__djogi_path` /
                // `with_path`; narrow visage
                // (`expose(scope -> Department::Public)`) keeps using
                // `{Visage}Fields` because the visage struct is
                // path-aware on its own. `peer_traversal_fields_path`
                // resolves the right shape from the relation field.
                let pfp = peer_traversal_fields_path(field, exposure);
                let full_peer = crate::model::attrs::detect_relation(&field.ty)
                    .map(|info| is_full_peer_for(exposure, &info))
                    .unwrap_or(false);

                // The SQL-alias path emitted into the peer's `Fields` is
                // the FK column name itself. For `department:
                // ForeignKey<Dept>` the accessor emits
                // `DeptPublicFields::with_path("department")`, so the
                // peer's scalar accessor yields `"department.name"`.
                if nullable {
                    let doc = format!(
                        "Optional-relation accessor for `{column}` — returns an \
                         [`OptionalRelationRef`](::djogi::query::OptionalRelationRef) \
                         over the peer visage's `Fields`. Compose an ordinary \
                         FieldRef-based closure against the peer with \
                         `.map_filter(|p| …)`; when the inner closure yields a broader \
                         query predicate (`Q<RootModel>`, codec-gated \
                         `PresentationFieldRef::eq(...)`, etc.), use \
                         `.map_predicate(|p| …)` instead. Both routes guard on \
                         `{column} IS NOT NULL` before applying the inner predicate."
                    );
                    if full_peer {
                        accessors.push(quote! {
                            #[doc = #doc]
                            #[inline]
                            pub fn #fname(&self) -> ::djogi::query::OptionalRelationRef<#pfp> {
                                ::djogi::query::field::optional_relation_support::__make_optional_relation_ref(
                                    #column,
                                    <#pfp>::with_path(#column),
                                )
                            }
                        });
                    } else {
                        accessors.push(quote! {
                            #[doc = #doc]
                            #[inline]
                            pub fn #fname(
                                &self,
                            ) -> ::djogi::query::OptionalRelationRef<#pfp<#root_ident>> {
                                ::djogi::query::field::optional_relation_support::__make_optional_relation_ref(
                                    #column,
                                    <#pfp<#root_ident>>::with_path(#column),
                                )
                            }
                        });
                    }
                } else {
                    let doc = format!(
                        "Required-relation accessor for `{column}` — returns the peer \
                         visage's `Fields` with SQL-alias path `{column}` threaded through. \
                         Chain a scalar accessor on the return value to compose a traversal \
                         leaf (`FieldRef` whose column path is `{column}.<peer_col>`)."
                    );
                    if full_peer {
                        accessors.push(quote! {
                            #[doc = #doc]
                            #[inline]
                            pub fn #fname(&self) -> #pfp {
                                <#pfp>::with_path(#column)
                            }
                        });
                    } else {
                        accessors.push(quote! {
                            #[doc = #doc]
                            #[inline]
                            pub fn #fname(&self) -> #pfp<#root_ident> {
                                <#pfp<#root_ident>>::with_path(#column)
                            }
                        });
                    }
                }
            }
        }
    }

    // The `DjogiVisageOf<Source>` seal + the sealed supertrait impl.
    let seal_impls = quote! {
        impl ::djogi::__private::VisageSealed<#source> for #visage_ident {}
        impl ::djogi::__private::DjogiVisageOf<#source> for #visage_ident {}
    };

    let fields_doc = format!(
        "Typed field accessors scoped to the `{visage_ident}` visage. One \
         inherent method per exposed column; non-exposed fields are \
         absent by construction. Carries an optional SQL-alias path \
         (`__djogi_path`) so traversal chains (`a.department().name()`) \
         compose into dot-qualified column names at emission time. The \
         public `RootModel = {source}` generic tracks which owning queryset / \
         filter root the emitted predicates target as traversal crosses into \
         peer visages. \
         See `DjogiVisageOf<{source}>` for the visage ↔ model seal."
    );

    quote! {
        #[doc = #fields_doc]
        #[derive(Debug, Clone, Copy)]
        pub struct #fields_ident<#root_ident = #source> {
            // `pub` for cross-crate macro emission; use `with_path` from
            // hand-written code.
            #[doc(hidden)]
            pub __djogi_path: ::core::option::Option<&'static str>,
            #[doc(hidden)]
            pub __djogi_root: ::core::marker::PhantomData<fn() -> #root_ident>,
        }

        impl<#root_ident> #fields_ident<#root_ident> {
            /// Construct a root-scope `Fields` handle with no SQL-alias path.
            /// Equivalent to the `Default` impl.
            #[doc(hidden)]
            #[inline]
            pub const fn new() -> Self {
                Self {
                    __djogi_path: ::core::option::Option::None,
                    __djogi_root: ::core::marker::PhantomData,
                }
            }

            /// Construct a traversal-scope `Fields` handle threaded with
            /// the given SQL-alias path. The caller is the macro's
            /// relation-form accessor on the parent `Fields`; the `path`
            /// is the FK column name on the parent.
            #[doc(hidden)]
            #[inline]
            pub const fn with_path(path: &'static str) -> Self {
                Self {
                    __djogi_path: ::core::option::Option::Some(path),
                    __djogi_root: ::core::marker::PhantomData,
                }
            }
        }

        impl<#root_ident: ::djogi::prelude::Model> #fields_ident<#root_ident> {
            #(#accessors)*
        }

        impl #fields_ident {
            /// Construct a root-scope `Fields` handle targeting the source
            /// model directly.
            /// This inherent method preserves the pre-generic
            /// `{Visage}Fields::default` call shape for root-scope code.
            #[inline]
            pub fn default() -> Self {
                Self::new()
            }
        }

        impl<#root_ident> ::core::default::Default for #fields_ident<#root_ident> {
            #[inline]
            fn default() -> Self {
                Self::new()
            }
        }

        #[doc = #filter_doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #filter_ident;

        #seal_impls
    }
}

/// Look up the per-scope presentation codec type (if any) for `scope`.
fn lookup_per_scope_codec<'a>(
    attrs: &'a crate::model::attrs::FieldAttrs,
    scope: &str,
) -> Option<&'a syn::Path> {
    attrs
        .protected
        .as_ref()
        .and_then(|spec| spec.per_scope.iter().find(|entry| entry.scope == scope))
        .map(|entry| &entry.codec_type)
}

/// Emit a scalar `FieldRef` accessor that routes through the merged
/// `__make_field_ref` helper (with optional path prefix).
fn emit_scalar_accessor(
    root: &Ident,
    fname: &Ident,
    column: &str,
    ty: &TokenStream,
) -> TokenStream {
    let doc = format!(
        "Typed handle for the `{column}` column (visage-scoped). Returns a \
         [`FieldRef`](::djogi::query::FieldRef) bound to the owning \
         `RootModel`. \
         Absent on visage-scope Fields types where the field is not exposed — \
         see the `expose(...)` annotation on the source struct."
    );
    quote! {
        #[doc = #doc]
        #[inline]
        pub fn #fname(&self) -> ::djogi::query::FieldRef<#root, #ty> {
            ::djogi::query::field::__macro_support::__make_field_ref::<#root, #ty>(
                self.__djogi_path,
                #column,
            )
        }
    }
}

/// Emit a scalar accessor wrapped in `PresentationFieldRef` so the query /
/// ordering surface is gated by the selected presentation codec traits.
fn emit_presentation_scalar_accessor(
    root: &Ident,
    fname: &Ident,
    column: &str,
    codec: &syn::Path,
    ty: &TokenStream,
) -> TokenStream {
    let doc = format!(
        "Typed handle for the `{column}` column (visage-scoped) governed by its \
         per-scope presentation codec. Returns a \
         [`PresentationFieldRef`](::djogi::presentation::query::PresentationFieldRef) \
         bound to the owning `RootModel` and selected codec. Predicate / ordering \
         methods are available only when that codec implements the matching \
         presentation query traits."
    );
    quote! {
        #[doc = #doc]
        #[inline]
        pub fn #fname(
            &self,
        ) -> ::djogi::presentation::query::PresentationFieldRef<#root, #codec, #ty> {
            ::djogi::presentation::query::PresentationFieldRef::<#root, #codec, #ty>::__new_crate_private(
                ::djogi::query::field::__macro_support::__make_field_ref::<#root, #ty>(
                    self.__djogi_path,
                    #column,
                )
            )
        }
    }
}

/// Render the PK type tokens used by the visage's `id` accessor. Matches
/// `visages::framework_field_decls` for the same `PkStrategy`.
fn pk_type_tokens(pk: &PkStrategy) -> TokenStream {
    match pk {
        PkStrategy::HeerId => quote! { ::djogi::types::HeerId },
        PkStrategy::RanjId => quote! { ::djogi::types::RanjId },
        PkStrategy::HeerIdDesc => quote! { ::djogi::types::HeerIdDesc },
        PkStrategy::RanjIdDesc => quote! { ::djogi::types::RanjIdDesc },
        PkStrategy::Serial => quote! { i32 },
        PkStrategy::None => quote! { () }, // Unreachable — caller gates.
        PkStrategy::Custom(path) => quote! { #path },
    }
}
