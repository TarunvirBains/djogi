//! Generates `{Model}Filter` — the programmatic filter builder.
//!
//! # What
//!
//! For every `#[model]` struct, emit a plain `{Model}Filter` struct with
//! one setter method per **user** field (framework columns `id` /
//! `created_at` / `updated_at` are filtered through the typed closure
//! path, which is already sufficient for those columns). Each setter
//! takes a [`Lookup<V>`] whose `V` generic matches the field's declared
//! Rust type — the macro reads the type verbatim from the post-injection
//! struct, so `Option<T>` / `Jsonb<T>` / user-defined wrapper types
//! propagate through without a translation table.
//!
//! # Why separate from `{Model}Fields`?
//!
//! `{Model}Fields` (stamped by `stubs.rs`) is a **typed** bag: it returns
//! [`FieldRef<M, V>`] handles that carry zero runtime state but bind the
//! closure-API's `.eq` / `.gte` / … lookups to the column's value type
//! at compile time. `{Model}Filter` is the **erased** counterpart: a
//! setter call projects `Lookup<V>` through `IntoFilterValue` into a
//! [`FilterClause`] and pushes it into a `Vec<FilterClause>`. Both paths
//! converge on the same `Condition` tree — the integration test in
//! `tests/integration/phase2_queryset.rs` asserts row-count parity — but
//! the erased shape is what makes closure-free callers (shell, admin,
//! dynamic UIs) possible at all.
//!
//! Setters consume `self` and return `Self` — the idiomatic Rust
//! owned-builder pattern that matches [`QuerySet`]'s own chain shape.
//! Dropping an intermediate builder is fine; only the final call's
//! chain matters.
//!
//! # How (emitted code)
//!
//! For `struct Post { title: String, published: bool, view_count: i32, ... }`:
//!
//! ```ignore
//! pub struct PostFilter {
//!     clauses: ::std::vec::Vec<::djogi::FilterClause>,
//! }
//!
//! impl PostFilter {
//!     pub fn new() -> Self { ... }
//!     pub fn title<__V>(mut self, lookup: ::djogi::Lookup<__V>) -> Self
//!     where
//!         __V: ::djogi::IntoFilterValue,
//!         String: ::djogi::__private::SameAs<__V>,
//!     { ... }
//!     pub fn published<__V>(mut self, lookup: ::djogi::Lookup<__V>) -> Self
//!     where
//!         __V: ::djogi::IntoFilterValue,
//!         bool: ::djogi::__private::SameAs<__V>,
//!     { ... }
//!     pub fn view_count<__V>(mut self, lookup: ::djogi::Lookup<__V>) -> Self
//!     where
//!         __V: ::djogi::IntoFilterValue,
//!         i32: ::djogi::__private::SameAs<__V>,
//!     { ... }
//! }
//!
//! impl ::djogi::ModelFilter for PostFilter { ... }
//! ```
//!
//! The `SameAs<__V>` bound is a reflexive type-equality witness — the
//! blanket `impl<T: ?Sized> SameAs<T> for T` means `A: SameAs<B>` holds
//! if and only if `A == B`. That pins `__V = #field_ty` at every call
//! site, so passing the wrong value type fails at the call site with a
//! clean "expected `#field_ty`, got `…`" error. The `IntoFilterValue`
//! bound is kept on the method generic `__V` rather than on the
//! concrete `#field_ty` so Rust defers checking it to monomorphization
//! (issue #48214 rejects concrete where-clauses that don't reference a
//! method generic). Practical consequence: columns whose declared Rust
//! type does not yet implement `IntoFilterValue` — `Decimal` without
//! the feature flag, `Vec<T>`, JSONB payload wrappers — still have
//! their setter emitted, and only its call site fails with a localized
//! trait-bound error. The whole `{Model}Filter` remains compilable and
//! composable.
//!
//! # Typed setters and unsupported column types
//!
//! Each setter is typed against the column's declared Rust type. At
//! the call site, `Lookup<__V>` must infer `__V = #field_ty` — the
//! emitted `#field_ty: SameAs<__V>` bound is the reflexive
//! type-equality witness that pins it. That puts `{Model}Filter` on
//! the same compile-time footing as the closure API — passing the
//! wrong value type to a setter (for example
//! `PostFilter::new().view_count(Lookup::Eq("42"))` for an `i32`
//! column) fails at the call site, not later at bind time.
//!
//! The `IntoFilterValue` bound is emitted on the **method generic
//! `__V`**, not on the concrete `#field_ty`. Two consequences:
//!
//!   1. A column whose declared Rust type does not yet implement
//!      `IntoFilterValue` (for example `Decimal` when the
//!      `rust_decimal` feature is off, `Vec<String>`, a user-defined
//!      JSONB payload wrapper) still gets a setter emitted. Rust
//!      checks a concrete-type `where` clause at impl-definition time
//!      (issue #48214), which would whole-model-reject a model
//!      containing such a column. Keeping the bound on `__V` defers
//!      the check to monomorphization, so only the unsupported
//!      setter's call site fails — with a localized
//!      "trait bound `…: IntoFilterValue` is not satisfied" rather
//!      than a whole-model reject.
//!   2. Explicit NULL checks via `Lookup::IsNull` / `Lookup::IsNotNull`
//!      still go through `IntoFilterValue` (because
//!      `Lookup<V>::into_op_value` is bounded on `V`). For columns
//!      without such an impl, those setters are unusable — the
//!      closure API's `.is_null()` remains the escape hatch.
//!
//! Nullable columns (`Option<T>`) take `Lookup<Option<T>>` directly —
//! the field type is read verbatim, so users write
//! `Lookup::Eq(Some("hello".to_string()))` /
//! `Lookup::<Option<T>>::IsNull`. The closure API has the same shape
//! for nullable columns, so the two surfaces stay symmetric. (A later
//! phase may add a sugar layer that re-emits `Option<T>` setters as
//! `Lookup<T>` for the `Eq` / `Neq` variants specifically; that is
//! purely additive and does not need a change to this module.)
//!
//! # `pk = None` gate
//!
//! `crud::expand` does not emit `impl Model` for `pk = None` models
//! (`Model::Pk: Encode` cannot be satisfied without a real PK — see
//! `crud.rs` for the rationale). The `{Model}Filter` struct itself, its
//! setters, and the `ModelFilter` impl have no `Model` dependency — the
//! clauses are erased `FilterClause` records, not `FieldRef<M, V>`
//! handles — so they compile for every pk strategy.
//!
//! The Cluster 8γ Stage 2 `IntoQ<#name>` bridge is the one piece that
//! does require `#name: Model` (because `Q<T: Model>` carries that
//! bound). Skip the bridge emission for `pk = None` models — the
//! `QuerySet::filter_struct` / `exclude_struct` entry points are
//! unreachable on a model with no `Model` impl anyway, so the missing
//! `IntoQ` impl is not observable.
//!
//! # Path routing
//!
//! All emitted type references go through `::djogi::*` rather than
//! reaching into sub-modules directly. Macro output compiles in the
//! user's crate, which depends only on `djogi`; routing through the
//! top-level re-exports means a single dep is sufficient.
//!
//! [`Lookup<V>`]: ::djogi::Lookup
//! [`FilterClause`]: ::djogi::FilterClause
//! [`FieldRef<M, V>`]: ::djogi::FieldRef
//! [`QuerySet`]: ::djogi::QuerySet
//! [`unwrap_option`]: crate::model::attrs::unwrap_option

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}Filter` with one setter per user field plus the
/// `ModelFilter` trait impl.
///
/// `struct_item` is the post-injection struct: framework columns sit at
/// the front in the order `inject::expand` placed them. We skip past
/// them — filtering by `id` / `created_at` / `updated_at` goes through
/// the typed closure API, which is already sufficient — and iterate user
/// fields in source order. Skip count is keyed off `model_attrs.pk` the
/// same way `descriptor::expand` does it, keeping the single
/// framework-field contract consistent across generated code.
///
/// `_field_attrs` is threaded through for forward compatibility —
/// per-field rename hints, validation, or column-override keys may alter
/// the emitted setter names in a later phase. Unused today.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    let name = &struct_item.ident;
    let filter_name = format_ident!("{}Filter", name);

    // Framework-field skip count — mirrors `descriptor::expand`. For
    // `pk = None` models, `inject::expand` only prepends `created_at`
    // and `updated_at`; everything else prepends `id`, `created_at`,
    // `updated_at`. A mismatch here would emit a `.id(...)` setter that
    // shadows the PK and disagrees with the descriptor — the single
    // source of truth for the schema contract.
    let n_framework = match &model_attrs.pk {
        PkStrategy::None => 2,
        _ => 3,
    };

    // Typed setters: the value's generic `__V` is pinned to the
    // column's declared Rust type through a reflexive `SameAs<#ty>`
    // bound. A user passing the wrong value type —
    // `PostFilter::new().view_count(Lookup::Eq("42"))` for an `i32`
    // column — infers `__V = &str`, which then fails the
    // `i32: SameAs<__V>` bound with a clear "expected `i32`" error at
    // the call site. Matches the closure API's compile-time discipline.
    //
    // # Why not `where #ty: IntoFilterValue` directly?
    //
    // Rust rejects concrete bounds in method `where` clauses whose
    // subject isn't a method generic (issue #48214: "where clauses on
    // method must reference a type parameter"). An emission like
    // `where Option<String>: IntoFilterValue` fails at impl-definition
    // time — the whole `{Model}Filter` refuses to compile for any
    // model whose columns include a type without an `IntoFilterValue`
    // impl. That would be a whole-model regression, not the localized
    // call-site failure the review called for.
    //
    // # Deferred-bound pattern
    //
    // Introduce a method generic `__V` and tie it to `#ty` via a
    // reflexive trait: `trait SameAs<T: ?Sized> {}` with a blanket
    // `impl<T: ?Sized> SameAs<T> for T`. Since the only way for
    // `A: SameAs<B>` to hold is `A == B`, writing
    // `#ty: SameAs<__V>` pins `__V = #ty` whenever the setter is
    // called. The `__V: IntoFilterValue` bound is now genuinely
    // generic, so Rust defers checking it to monomorphization at the
    // call site — exactly the lazy check we want. The whole
    // `{Model}Filter` compiles regardless of which columns have
    // `IntoFilterValue` impls, and the error message on a call to a
    // setter whose column type lacks the impl points at the call
    // site with "the trait bound `#ty: IntoFilterValue` is not
    // satisfied".
    //
    // The helper trait lives in `djogi::__private::SameAs` to keep it
    // out of the public namespace — users compose filters through
    // `Lookup<V>` and never need to name `SameAs` themselves.
    //
    // # Raw `Lookup::<#ty>::IsNull` still works
    //
    // `Lookup::IsNull` carries no value, but the variant is still
    // generic in `V` — the user writes `Lookup::<#ty>::IsNull` or
    // lets inference fill `__V = #ty` from the setter's signature.
    // Once `__V = #ty` is pinned, the `__V: IntoFilterValue` bound
    // applies; columns whose type doesn't implement that trait cannot
    // use `IsNull` through the clause path and fall back to the
    // closure API's `.is_null()` (which has its own set of impls to
    // consult).
    let setters: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .filter_map(|field| {
            let ident = field.ident.as_ref()?;
            // Strip the `r#` prefix for the SQL column literal but keep
            // the raw ident for the method name so users can still call
            // `.r#type(...)` on the filter struct.
            let column = crate::syn_util::column_name_from_ident(ident);
            // Read the field type verbatim — `Option<T>` / `Jsonb<T>` /
            // user newtypes propagate without a translation table,
            // same as `stubs.rs` emits into `FieldRef<M, V>`.
            let ty = &field.ty;
            let doc = format!(
                "Append a `{column}` lookup to the filter. Typed on the column's declared Rust type — passing the wrong value type fails at the call site with a type-mismatch error. For columns whose type does not implement `IntoFilterValue` (e.g. `Decimal` without the feature flag, `Vec<T>`, user newtypes), this setter is defined but unusable; calling it fails at the call site with a trait-bound error. Reach for the closure API (`QuerySet::filter(|f| …)`) or `ctx.raw_execute` / `ctx.raw_scalar` directly in those cases."
            );
            Some(quote! {
                #[doc = #doc]
                #[inline]
                pub fn #ident<__V>(mut self, lookup: ::djogi::Lookup<__V>) -> Self
                where
                    __V: ::djogi::IntoFilterValue,
                    #ty: ::djogi::__private::SameAs<__V>,
                {
                    self.clauses.push(::djogi::FilterClause::from_lookup(#column, lookup));
                    self
                }
            })
        })
        .collect();

    let struct_doc = format!(
        "Programmatic filter builder for [`{name}`] — one setter per user field. \
         Use with `QuerySet::filter_struct` for closure-free filtering (shell, admin, dynamic UI). \
         The closure API (`QuerySet::filter(|f| ...)`) is the preferred surface when a closure is writable; \
         both paths produce structurally equivalent condition trees."
    );

    // ── `IntoQ<#name>` bridge (Cluster 8γ Stage 2 — T6.7) ───────────────
    //
    // Lifts `{Model}Filter` into the `Q<T>` algebra so it composes with
    // the `QuerySet::filter_struct` / `exclude_struct` signature
    // `<F: IntoQ<T>>`. The impl folds the accumulated clauses through
    // `clauses_into_condition` and wraps the result as
    // `Q::Condition(_)`. Character-for-character SQL parity is
    // preserved by construction.
    //
    // Gated on `pk != PkStrategy::None`: `Q<T: Model>` carries a
    // `Model` bound, but `crud::expand` does not emit `impl Model` for
    // `pk = None` models. Skipping the bridge there keeps the rest of
    // `{Model}Filter` (struct, setters, `ModelFilter` impl) usable for
    // pk-less models even though the closure-free filter path is not
    // reachable on them.
    let into_q_bridge = if matches!(model_attrs.pk, PkStrategy::None) {
        quote! {}
    } else {
        quote! {
            impl ::djogi::__private::__SealedIntoQ for #filter_name {}
            impl ::djogi::__private::query::IntoQ<#name> for #filter_name {
                #[inline]
                fn into_q(self) -> ::djogi::__private::query::Q<#name> {
                    let clauses = <Self as ::djogi::ModelFilter>::into_clauses(self);
                    let cond = ::djogi::__private::query::clauses_into_condition(clauses);
                    ::djogi::__private::query::Q::Condition(cond)
                }
            }
        }
    };

    quote! {
        #[doc = #struct_doc]
        #[derive(Debug, Clone, Default)]
        pub struct #filter_name {
            // Pushed in setter-call order. The clause-fold helper
            // preserves that order so SQL emission is deterministic
            // across runs — important for `EXPLAIN` parity and for
            // query-plan caching.
            clauses: ::std::vec::Vec<::djogi::FilterClause>,
        }

        impl #filter_name {
            /// Construct an empty filter. Equivalent to `Self::default()`.
            #[must_use]
            #[inline]
            pub fn new() -> Self {
                Self::default()
            }

            #(#setters)*
        }

        impl ::djogi::ModelFilter for #filter_name {
            #[inline]
            fn into_clauses(self) -> ::std::vec::Vec<::djogi::FilterClause> {
                self.clauses
            }
        }

        #into_q_bridge
    }
}
