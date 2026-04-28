//! Emit `{Visage}Fields`, `{Visage}Filter`, and the `DjogiVisageOf<SourceModel>`
//! seal impl for a single visage.
//!
//! # Design
//!
//! `{Visage}Fields` is a state-carrying struct that threads a SQL-alias path
//! prefix through traversal chains:
//!
//! ```ignore
//! pub struct UserPublicFields { pub __djogi_path: Option<&'static str> }
//! ```
//!
//! - Root construction: `UserPublicFields::default()` sets `__djogi_path = None`.
//! - Traversal construction: `UserPublicFields::with_path("owner")` sets
//!   `__djogi_path = Some("owner")` so the peer's scalar accessors produce
//!   `FieldRef`s whose column path is `"owner.{column}"`.
//!
//! Accessors are `&self` methods so the path state is available inside every call:
//!
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
//!
//! # Optional-FK accessor shape
//!
//! A relation-form entry on a nullable FK / O2O field emits an accessor
//! returning [`OptionalRelationRef<PeerFields>`]. The wrapper's `map_filter(|a| …)`
//! combinator emits `author_id IS NOT NULL AND <inner>` so the nullability
//! is honoured at the SQL level.
//!
//! Required FKs (non-`Option`) keep the plain `PeerFields` return type.
//!
//! # `FieldRef<Model, V>` over `FieldRef<Visage, V>`
//!
//! Visages do not impl `Model` (they are projections, not tables). Accessors
//! are typed on the **source model** — e.g. `FieldRef<User, String>` — and
//! the visage ↔ model pairing is tracked separately via `DjogiVisageOf<M>`.
//!
//! # Non-exposed fields are absent by construction
//!
//! Referencing a non-exposed field in a closure fails at compile time with
//! a "no method named …" error. That is the compile-time enforcement that
//! makes the visage a genuine data-access boundary.
//!
//! [`OptionalRelationRef<PeerFields>`]: ::djogi::query::OptionalRelationRef

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy};
use crate::model::visage_ctx::{ScopeMembership, classify_field_for_scope, peer_fields_path};
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
        source,
        &format_ident!("id"),
        "id",
        &id_ty,
    ));
    accessors.push(emit_scalar_accessor(
        source,
        &format_ident!("created_at"),
        "created_at",
        &quote! { ::djogi::types::DateTime },
    ));
    accessors.push(emit_scalar_accessor(
        source,
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

        // Raw identifiers strip `r#` for the SQL column literal.
        let raw = fname.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();

        match classify_field_for_scope(field, attrs, scope) {
            ScopeMembership::Absent | ScopeMembership::Reject { .. } => continue,

            // Scalar form on scalar field — emit a `FieldRef<Source, Ty>`
            // accessor, path-aware.
            ScopeMembership::Scalar => {
                let fty_ts = quote! { #fty };
                accessors.push(emit_scalar_accessor(source, fname, &column, &fty_ts));
            }

            // Relation form on relation field — emit a path-threaded
            // accessor returning the peer's `{PeerVisage}Fields` (required
            // FK) or `OptionalRelationRef<{PeerVisage}Fields>` (optional
            // FK / O2O).
            ScopeMembership::RelationEmbed { exposure, nullable } => {
                let pfp = peer_fields_path(exposure);

                // The SQL-alias path emitted into the peer's `Fields` is
                // the FK column name itself. For `department:
                // ForeignKey<Dept>` the accessor emits
                // `DeptPublicFields::with_path("department")`, so the
                // peer's scalar accessor yields `"department.name"`.
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
                        pub fn #fname(&self) -> ::djogi::query::OptionalRelationRef<#pfp> {
                            ::djogi::query::field::optional_relation_support::__make_optional_relation_ref(
                                #column,
                                <#pfp>::with_path(#column),
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
                        pub fn #fname(&self) -> #pfp {
                            <#pfp>::with_path(#column)
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
            // `pub` for cross-crate macro emission; use `with_path` from
            // hand-written code.
            #[doc(hidden)]
            pub __djogi_path: ::core::option::Option<&'static str>,
        }

        impl #fields_ident {
            /// Construct a root-scope `Fields` handle with no SQL-alias path.
            /// Equivalent to the `Default` impl.
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

/// Emit a scalar `FieldRef` accessor that routes through the merged
/// `__make_field_ref` helper (with optional path prefix).
fn emit_scalar_accessor(
    source: &Ident,
    fname: &Ident,
    column: &str,
    ty: &TokenStream,
) -> TokenStream {
    let doc = format!(
        "Typed handle for the `{column}` column (visage-scoped). Returns a \
         [`FieldRef`](::djogi::query::FieldRef) bound to the source model. \
         Absent on visage-scope Fields types where the field is not exposed — \
         see the `expose(...)` annotation on the source struct."
    );
    quote! {
        #[doc = #doc]
        #[inline]
        pub fn #fname(&self) -> ::djogi::query::FieldRef<#source, #ty> {
            ::djogi::query::field::__macro_support::__make_field_ref::<#source, #ty>(
                self.__djogi_path,
                #column,
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
