//! Phase 7-Zero-2 T7 — emit `{Visage}Fields` + `{Visage}Filter` per visage
//! plus the `DjogiVisageOf<M>` seal impl.
//!
//! # `{Visage}Fields`
//!
//! A ZST whose associated functions return [`FieldRef<Model, V>`] handles
//! for every column that appears in the visage's scope — framework
//! columns (`id` / `created_at` / `updated_at`, gated the same way the
//! visage's own struct is) plus user fields exposed via the scalar form
//! `expose(scope)` or the relation form `expose(scope -> Peer)`.
//!
//! Non-exposed fields are ABSENT by construction. Referencing one in a
//! closure (e.g. `UserPublicFields::password_hash()` when `password_hash`
//! was declared `expose(none)`) fails with a rustc "no function or
//! associated item named …" error at the call site. That is the
//! compile-time enforcement that makes the visage a genuine data-access
//! boundary, not a soft naming convention.
//!
//! # Design note: `FieldRef<Model, V>` over `FieldRef<Visage, V>`
//!
//! `FieldRef<M, V>` carries `M: Model`, and visages do NOT impl `Model`
//! (they are projections, not tables). Emitting
//! `FieldRef<UserPublic, String>` would therefore fail the trait bound.
//! The T7 emission types accessors on the **source model** — e.g.
//! `FieldRef<User, String>` — and links the visage ↔ model pairing
//! separately via the `DjogiVisageOf<M>` seal. T8 introduces
//! visage-scoped traversal combinators; the surface type may evolve
//! then (e.g. a new `VisageFieldRef<V, T>` that carries a SQL path
//! prefix for joined access). The plan's v3 fixture shape is deferred
//! — kept as a forward-compatible shift in T8.
//!
//! # `{Visage}Filter`
//!
//! Emitted as a placeholder ZST in T7. T8 adds the closure-based builder
//! shape (`UserPublic::filter(|f: &UserPublicFields| Condition)`). The
//! placeholder is sufficient for T7's goal — name the type so callers
//! can reference it in their generic code ahead of T8 landing the real
//! builder surface.
//!
//! # `DjogiVisageOf<M>` seal
//!
//! For each emitted visage the macro emits:
//!
//! ```ignore
//! impl ::djogi::__private::VisageSealed<SourceModel> for VisageIdent {}
//! impl ::djogi::__private::DjogiVisageOf<SourceModel> for VisageIdent {}
//! ```
//!
//! The first satisfies the seal supertrait; the second is the
//! user-facing marker trait. Downstream code cannot add its own
//! `DjogiVisageOf` impls because the seal supertrait is closed-world
//! (only `djogi` and `#[model]`-emitted code implement it).
//!
//! [`FieldRef<Model, V>`]: ::djogi::query::FieldRef

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, ItemStruct};

/// Emit `{Visage}Fields`, `{Visage}Filter`, and the sealed
/// `DjogiVisageOf<SourceModel>` impl for a single visage.
///
/// Called from `visages::expand_one_visage` after the visage struct and
/// its conversion impls have been emitted. All three emission shapes
/// share the same scope gate (`scalar_hit || relation_hit`) the visage
/// emitter itself uses, so the accessor set on `{Visage}Fields` matches
/// the field set on the visage struct exactly.
///
/// # Parameters
///
/// - `source`: the source `#[model]` struct's ident (`User`).
/// - `visage_ident`: the visage's ident (`UserPublic`).
/// - `scope`: the scope name (`"public"`) — used to look up scalar and
///   relation exposure on each field.
/// - `struct_item`: the post-injection struct (framework columns prepended).
/// - `field_attrs`: per-user-field attribute container (indexed in source order).
/// - `model_attrs`: used to gate framework-column emission the same way
///   `framework_field_decls` gates it on the visage struct itself.
/// - `n_framework`: the number of framework columns prepended by
///   `inject::expand` (2 for `pk = None`, 3 otherwise).
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

    // `pk = None` gate — `FieldRef<M, V>` carries `M: Model`, and
    // `crud::expand` does NOT emit `impl Model` for `pk = None` models
    // (mirrors `stubs::expand`, which applies the identical gate on
    // `{Model}Fields`). Emitting accessors or the `DjogiVisageOf<Source>`
    // seal for a model without a `Model` impl would fail the trait bound.
    //
    // Under `pk = None` we still emit the visage struct itself (the
    // `visages::emit_projection_for_scope` path above), but the
    // associated `{Visage}Fields` / `{Visage}Filter` / seal are
    // suppressed. A future phase that unlocks `pk = None` models on the
    // `Model` trait (composite or user-managed PKs) can relax this gate
    // in lock-step with `stubs::expand`.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return TokenStream::new();
    }

    // Framework accessors — `id` / `created_at` / `updated_at`. Under the
    // `pk = None` early return above, this block is unreachable, so
    // `id` is always emitted with the model's actual PK type here.
    let mut accessors: Vec<TokenStream> = Vec::new();

    let id_ty = pk_type_tokens(&model_attrs.pk);
    accessors.push(quote! {
        #[doc = "Typed `id` column handle for this visage's source model."]
        #[inline]
        pub fn id() -> ::djogi::query::FieldRef<#source, #id_ty> {
            ::djogi::query::field::__macro_support::__make_field_ref::<
                #source,
                #id_ty,
            >("id")
        }
    });

    accessors.push(quote! {
        #[doc = "Typed `created_at` column handle for this visage's source model."]
        #[inline]
        pub fn created_at() -> ::djogi::query::FieldRef<#source, ::djogi::types::DateTime> {
            ::djogi::query::field::__macro_support::__make_field_ref::<
                #source,
                ::djogi::types::DateTime,
            >("created_at")
        }
    });
    accessors.push(quote! {
        #[doc = "Typed `updated_at` column handle for this visage's source model."]
        #[inline]
        pub fn updated_at() -> ::djogi::query::FieldRef<#source, ::djogi::types::DateTime> {
            ::djogi::query::field::__macro_support::__make_field_ref::<
                #source,
                ::djogi::types::DateTime,
            >("updated_at")
        }
    });

    // Per-user-field accessors — scope gate mirrors
    // `visages::emit_projection_for_scope` exactly. Suppressed fields
    // (`expose(none)` / `expose(internal)`) are skipped; fields not
    // present in this scope are also skipped. For relation-form entries
    // the accessor resolves to the peer's `{PeerVisage}Fields` struct so
    // downstream code can dot into it (even though T7 only emits the
    // type — T8 wires the actual traversal semantics).
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
        let is_relation = detect_relation(fty).is_some();

        // Raw identifiers strip `r#` for the SQL column literal the same
        // way `stubs.rs` handles them. The method name keeps the raw
        // spelling so callers can still write `.r#type(...)`.
        let raw = fname.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();

        match (scalar_hit, relation_hit, is_relation) {
            (false, None, _) => continue,

            // Scalar form on scalar field — emit a `FieldRef<Source, Ty>`
            // accessor. This is the T7 happy path.
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
                    pub fn #fname() -> ::djogi::query::FieldRef<#source, #fty> {
                        ::djogi::query::field::__macro_support::__make_field_ref::<
                            #source,
                            #fty,
                        >(#column)
                    }
                });
            }

            // Scalar form on relation field / relation form on scalar field
            // — rejected by the visage emitter itself with a span-precise
            // compile error. Skip here; the visage-side error is enough.
            (true, None, true) | (false, Some(_), false) => continue,

            // Parser rejects mixed scalar+relation on the same scope.
            (true, Some(_), _) => continue,

            // Relation form on relation field — T7 emits an accessor that
            // returns the peer's `{PeerVisage}Fields` type. The peer path
            // the user wrote after `->` may be a narrow visage
            // (`DepartmentPublic`) or a full peer model (`Department`);
            // both are types that have (or will have) a `Fields` sibling.
            //
            // For full-peer targets we append `Fields` to the LAST segment
            // of the peer path (e.g. `crate::models::Department` →
            // `crate::models::DepartmentFields` via `stubs.rs` emission on
            // the peer model). For narrow-visage targets the same rule
            // applies (e.g. `DepartmentPublic` → `DepartmentPublicFields`).
            //
            // Returning the Fields ZST (not a `FieldRef`) mirrors the
            // plan's T8 traversal shape; T8 will thread a SQL-alias path
            // through it. For T7 the return value is purely a type seed —
            // callers cannot SQL-traverse through it yet, but referencing
            // the method at all requires the relation to be in-scope,
            // which is what T7's absence-by-construction test pins.
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
                    // The last segment in a type-position path must not
                    // carry angle-bracketed args here (relation peer
                    // paths never do); if it did, we'd strip them.
                    last.arguments = syn::PathArguments::None;
                }

                let doc = format!(
                    "Relation-scoped accessor for `{column}` — returns the peer visage's \
                     `Fields` ZST. T7 emits the accessor; T8 wires the SQL-alias path \
                     threading for visage-scoped traversal."
                );
                accessors.push(quote! {
                    #[doc = #doc]
                    #[inline]
                    pub fn #fname() -> #peer_fields_path {
                        #peer_fields_path
                    }
                });
            }
        }
    }

    // The `DjogiVisageOf<Source>` seal + the sealed supertrait impl.
    // Both are keyed on the SOURCE model (not on the visage's own ident)
    // so generic code bounded on `V: DjogiVisageOf<M>` composes — a
    // visage of `User` satisfies `DjogiVisageOf<User>`, never
    // `DjogiVisageOf<Post>`.
    let seal_impls = quote! {
        impl ::djogi::__private::VisageSealed<#source> for #visage_ident {}
        impl ::djogi::__private::DjogiVisageOf<#source> for #visage_ident {}
    };

    let fields_doc = format!(
        "Typed field accessors scoped to the `{visage_ident}` visage. One \
         associated function per exposed column; non-exposed fields are \
         absent by construction. See `DjogiVisageOf<{source}>` for the \
         visage ↔ model seal."
    );
    let filter_doc = format!(
        "Placeholder filter builder for the `{visage_ident}` visage (T7). \
         The closure-based entry point (`{visage_ident}::filter(|f| …)`) \
         and programmatic setters land in T8."
    );

    quote! {
        #[doc = #fields_doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #fields_ident;

        impl #fields_ident {
            #(#accessors)*
        }

        #[doc = #filter_doc]
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #filter_ident;

        #seal_impls
    }
}

/// Render the PK type tokens used by the visage's `id` accessor. Matches
/// `visages::framework_field_decls` for the same `PkStrategy` — a
/// mismatch here would emit an accessor whose return type disagreed with
/// the visage struct's own `id` field.
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
