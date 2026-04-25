//! Phase 7-Zero-2 T7 + T8 — emit `{Visage}Fields` + `{Visage}Filter` per
//! visage plus the `DjogiVisageOf<M>` seal impl.
//!
//! # T8 shift: state-carrying `{Visage}Fields`
//!
//! T7 emitted `{Visage}Fields` as a unit-struct ZST with associated
//! functions (`UserPublicFields::display_name()`). T8 converts it to a
//! state-carrying struct that threads a SQL-alias path prefix through
//! traversal chains:
//!
//! ```ignore
//! pub struct UserPublicFields { __djogi_path: Option<&'static str> }
//! ```
//!
//! - Root construction: `UserPublicFields::default()` (equivalently,
//!   `UserPublicFields::new()`) sets `__djogi_path = None`.
//! - Traversal construction: `UserPublicFields::with_path("owner")` sets
//!   `__djogi_path = Some("owner")` so the peer's scalar accessors
//!   produce `FieldRef`s whose column path is `"owner.{column}"`.
//!
//! Accessors are `&self` methods (not associated fns) so the path state
//! is available inside every call:
//!
//! ```ignore
//! impl UserPublicFields {
//!     pub fn display_name(&self) -> FieldRef<User, String> {
//!         match self.__djogi_path {
//!             Some(prefix) => __make_field_ref_with_path(prefix, "display_name"),
//!             None         => __make_field_ref("display_name"),
//!         }
//!     }
//!     pub fn owner(&self) -> OwnerPublicFields {
//!         OwnerPublicFields::with_path("owner")
//!     }
//! }
//! ```
//!
//! # T8 shift: optional-FK accessor shape
//!
//! A relation-form entry on a nullable FK / O2O field (e.g.
//! `author: Option<ForeignKey<User>>` with `expose(public -> UserPublic)`)
//! emits an accessor returning
//! [`OptionalRelationRef<UserPublicFields>`]. The wrapper's
//! `map_filter(|a| …)` combinator emits `author_id IS NOT NULL AND <inner>`
//! so the nullability is honoured at the SQL level without leaking an
//! `Option` into the filter tree.
//!
//! Required FKs (non-`Option`) keep the plain `PeerFields` return type —
//! no wrapper, no guard clause, one less layer at the call site.
//!
//! # T7 carry-over: `FieldRef<Model, V>` over `FieldRef<Visage, V>`
//!
//! `FieldRef<M, V>` carries `M: Model`, and visages do NOT impl `Model`
//! (they are projections, not tables). Emitting
//! `FieldRef<UserPublic, String>` would therefore fail the trait bound.
//! T7+T8 types accessors on the **source model** — e.g.
//! `FieldRef<User, String>` — and links the visage ↔ model pairing
//! separately via the `DjogiVisageOf<M>` seal.
//!
//! # Non-exposed fields are ABSENT
//!
//! Non-exposed fields are ABSENT by construction. Referencing one in a
//! closure (e.g. `fields.password_hash()` when `password_hash` was
//! declared `expose(none)`) fails with a rustc "no method named …"
//! error at the call site. That is the compile-time enforcement that
//! makes the visage a genuine data-access boundary.
//!
//! # `{Visage}Filter`
//!
//! Emitted as a placeholder ZST in T7. T8 keeps the placeholder — the
//! closure-based filter entry point (`{Visage}::filter(|f| …)`) is T10's
//! concern.
//!
//! [`OptionalRelationRef<UserPublicFields>`]: ::djogi::query::OptionalRelationRef

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, ItemStruct};

/// Emit `{Visage}Fields`, `{Visage}Filter`, and the sealed
/// `DjogiVisageOf<SourceModel>` impl for a single visage.
pub fn expand(
    source: &Ident,
    visage_ident: &Ident,
    scope: &str,
    struct_item: &ItemStruct,
    field_attrs: &[FieldAttrs],
    model_attrs: &ModelAttrs,
    n_framework: usize,
) -> TokenStream {
    let fields_ident = format_ident!("{visage_ident}Fields");
    let filter_ident = format_ident!("{visage_ident}Filter");
    let filter_doc = format!(
        "Placeholder filter builder for the `{visage_ident}` visage. \
         The closure-based entry point (`{visage_ident}::filter(|f| …)`) \
         and programmatic setters land in a later task (T10)."
    );

    // `pk = None` gate — `FieldRef<M, V>` carries `M: Model` and
    // `DjogiVisageOf<M>` likewise bounds `M: Model`. `crud::expand` does
    // not emit `impl Model` for `pk = None` models, so the `{Visage}Fields`
    // accessors and the seal cannot reference the source type. Emit only
    // the placeholder `{Visage}Filter` ZST for those models — it carries
    // no `M: Model` bound and the T10 closure-based builder may still
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

    accessors.push(quote! {
        #[doc = "Typed `id` column handle for this visage's source model."]
        #[inline]
        pub fn id(&self) -> ::djogi::query::FieldRef<#source, #id_ty> {
            match self.__djogi_path {
                ::core::option::Option::Some(prefix) => {
                    ::djogi::query::field::__macro_support::__make_field_ref_with_path::<
                        #source,
                        #id_ty,
                    >(prefix, "id")
                }
                ::core::option::Option::None => {
                    ::djogi::query::field::__macro_support::__make_field_ref::<
                        #source,
                        #id_ty,
                    >("id")
                }
            }
        }
    });

    accessors.push(quote! {
        #[doc = "Typed `created_at` column handle for this visage's source model."]
        #[inline]
        pub fn created_at(&self) -> ::djogi::query::FieldRef<#source, ::djogi::types::DateTime> {
            match self.__djogi_path {
                ::core::option::Option::Some(prefix) => {
                    ::djogi::query::field::__macro_support::__make_field_ref_with_path::<
                        #source,
                        ::djogi::types::DateTime,
                    >(prefix, "created_at")
                }
                ::core::option::Option::None => {
                    ::djogi::query::field::__macro_support::__make_field_ref::<
                        #source,
                        ::djogi::types::DateTime,
                    >("created_at")
                }
            }
        }
    });

    accessors.push(quote! {
        #[doc = "Typed `updated_at` column handle for this visage's source model."]
        #[inline]
        pub fn updated_at(&self) -> ::djogi::query::FieldRef<#source, ::djogi::types::DateTime> {
            match self.__djogi_path {
                ::core::option::Option::Some(prefix) => {
                    ::djogi::query::field::__macro_support::__make_field_ref_with_path::<
                        #source,
                        ::djogi::types::DateTime,
                    >(prefix, "updated_at")
                }
                ::core::option::Option::None => {
                    ::djogi::query::field::__macro_support::__make_field_ref::<
                        #source,
                        ::djogi::types::DateTime,
                    >("updated_at")
                }
            }
        }
    });

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

        if attrs.expose.suppressed {
            continue;
        }

        let scalar_hit = attrs.expose.scalar_scopes.contains(scope);
        let relation_hit = attrs.expose.relation_scopes.get(scope);
        let rel_info = detect_relation(fty);
        let is_relation = rel_info.is_some();

        // Raw identifiers strip `r#` for the SQL column literal.
        let raw = fname.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();

        match (scalar_hit, relation_hit, is_relation) {
            (false, None, _) => continue,

            // Scalar form on scalar field — emit a `FieldRef<Source, Ty>`
            // accessor, path-aware.
            (true, None, false) => {
                let doc = format!(
                    "Typed handle for the `{column}` column (visage-scoped). Returns a \
                     [`FieldRef`](::djogi::query::FieldRef) bound to the source model. \
                     Absent on visage-scope Fields types where the field is not exposed — \
                     see the `expose(...)` annotation on the source struct."
                );
                accessors.push(quote! {
                    #[doc = #doc]
                    #[inline]
                    pub fn #fname(&self) -> ::djogi::query::FieldRef<#source, #fty> {
                        match self.__djogi_path {
                            ::core::option::Option::Some(prefix) => {
                                ::djogi::query::field::__macro_support::__make_field_ref_with_path::<
                                    #source,
                                    #fty,
                                >(prefix, #column)
                            }
                            ::core::option::Option::None => {
                                ::djogi::query::field::__macro_support::__make_field_ref::<
                                    #source,
                                    #fty,
                                >(#column)
                            }
                        }
                    }
                });
            }

            // Scalar form on relation field / relation form on scalar field
            // — rejected by the visage emitter with a span-precise error.
            (true, None, true) | (false, Some(_), false) => continue,

            // Parser rejects mixed scalar+relation on the same scope.
            (true, Some(_), _) => continue,

            // Relation form on relation field — T8 emits a path-threaded
            // accessor returning the peer's `{PeerVisage}Fields` (required
            // FK) or `OptionalRelationRef<{PeerVisage}Fields>` (optional
            // FK / O2O).
            (false, Some(exposure), true) => {
                let peer_path = &exposure.peer;
                let peer_fields_ident = format_ident!(
                    "{}Fields",
                    exposure
                        .peer
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default()
                );
                // Rebuild the peer's Fields path by replacing the last
                // segment's ident with `{PeerIdent}Fields`.
                let mut peer_fields_path = peer_path.clone();
                if let Some(last) = peer_fields_path.segments.last_mut() {
                    last.ident = peer_fields_ident.clone();
                    last.arguments = syn::PathArguments::None;
                }

                // The SQL-alias path emitted into the peer's `Fields` is
                // the FK column name itself. For `department:
                // ForeignKey<Dept>` the accessor emits
                // `DeptPublicFields::with_path("department")`, so the
                // peer's scalar accessor yields `"department.name"`.
                //
                // `column` is already the raw-stripped column name;
                // `#column` lowers to a string literal, which the
                // `assert_plain_ident` validator inside
                // `__make_field_ref_with_path` will re-check at runtime.
                //
                // For optional relations the FK column is
                // `{column}_id` in typical convention, but users may
                // declare the column however they wish — use the field
                // name verbatim as the path key (the underlying FK
                // column name is the field name; #[model]'s descriptor
                // emission on relation fields uses the field name as
                // the column identifier).
                let nullable = rel_info.map(|i| i.nullable).unwrap_or(false);

                if nullable {
                    let doc = format!(
                        "Optional-relation accessor for `{column}` — returns an \
                         [`OptionalRelationRef`](::djogi::query::OptionalRelationRef) \
                         over the peer visage's `Fields`. Compose a closure against the \
                         peer with `.map_filter(|p| …)`; the emitted SQL guards on \
                         `{column} IS NOT NULL` before applying the inner predicate."
                    );
                    accessors.push(quote! {
                        #[doc = #doc]
                        #[inline]
                        pub fn #fname(&self) -> ::djogi::query::OptionalRelationRef<#peer_fields_path> {
                            ::djogi::query::field::optional_relation_support::__make_optional_relation_ref(
                                #column,
                                <#peer_fields_path>::with_path(#column),
                            )
                        }
                    });
                } else {
                    let doc = format!(
                        "Required-relation accessor for `{column}` — returns the peer \
                         visage's `Fields` with SQL-alias path `{column}` threaded through. \
                         Chain a scalar accessor on the return value to compose a traversal \
                         leaf (`FieldRef` whose column path is `{column}.<peer_col>`)."
                    );
                    accessors.push(quote! {
                        #[doc = #doc]
                        #[inline]
                        pub fn #fname(&self) -> #peer_fields_path {
                            <#peer_fields_path>::with_path(#column)
                        }
                    });
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
         compose into dot-qualified column names at emission time. \
         See `DjogiVisageOf<{source}>` for the visage ↔ model seal."
    );

    quote! {
        #[doc = #fields_doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #fields_ident {
            // `#[doc(hidden)]` on a public-but-internal state field keeps
            // the emitted struct debuggable (the `Debug` derive includes
            // the path in output) while flagging to rustdoc that this is
            // not part of the stable surface. The field is `pub` so
            // macro-emitted code in other crates can construct the
            // struct (Rust's privacy rules gate per-field access by
            // crate boundary — the `with_path` constructor is the
            // supported path, the raw field is the escape hatch macros
            // need).
            #[doc(hidden)]
            pub __djogi_path: ::core::option::Option<&'static str>,
        }

        impl #fields_ident {
            /// Construct a root-scope `Fields` handle with no SQL-alias
            /// path. Equivalent to the `Default` impl.
            #[doc(hidden)]
            #[inline]
            pub const fn new() -> Self {
                Self { __djogi_path: ::core::option::Option::None }
            }

            /// Construct a traversal-scope `Fields` handle threaded with
            /// the given SQL-alias path. The caller is the macro's
            /// relation-form accessor on the parent `Fields`; the `path`
            /// is the FK column name on the parent.
            #[doc(hidden)]
            #[inline]
            pub const fn with_path(path: &'static str) -> Self {
                Self { __djogi_path: ::core::option::Option::Some(path) }
            }

            #(#accessors)*
        }

        #[doc = #filter_doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #filter_ident;

        #seal_impls
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
