//! Generates `{Model}Filter` — the programmatic filter builder.
//! # What
//! For every `#[model]` struct, emit a plain `{Model}Filter` struct with
//! one setter method per **user** field (framework columns `id` /
//! `created_at` / `updated_at` are filtered through the typed closure
//! path, which is already sufficient for those columns). Each setter
//! takes a [`Lookup<V>`] whose `V` generic matches the field's declared
//! Rust type — the macro reads the type verbatim from the post-injection
//! struct, so `Option<T>` / `Jsonb<T>` / user-defined wrapper types
//! propagate through without a translation table.
//! # Why separate from `{Model}Fields`?
//! `{Model}Fields` (stamped by `stubs.rs`) is a **typed** bag: it returns
//! [`FieldRef<M, V>`] handles that carry zero runtime state but bind the
//! closure-API's `.eq` / `.gte` / … lookups to the column's value type
//! at compile time. `{Model}Filter` is the **erased** counterpart: a
//! setter call projects `Lookup<V>` through `IntoFilterValue` into a
//! [`FilterClause`] and pushes it into a `Vec<FilterClause>`. The closure
//! path and builder path preserve the same query result semantics, but
//! they no longer always converge on the same internal `Condition` tree:
//! the generated `IntoQ` bridge lazily reconstructs portable `Q` leaves
//! for owner-approved `bool`/`String` Eq/Neq/In/NotIn cases and
//! conservatively falls back to `Q::Condition` otherwise. The erased
//! builder shape is what makes closure-free callers (shell, admin,
//! dynamic UIs) possible at all.
//! Setters consume `self` and return `Self` — the idiomatic Rust
//! owned-builder pattern that matches [`QuerySet`]'s own chain shape.
//! Dropping an intermediate builder is fine; only the final call's
//! chain matters.
//! # How (emitted code)
//! For `struct Post { title: String, published: bool, view_count: i32, ... }`:
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
//! # Typed setters and unsupported column types
//! Each setter is typed against the column's declared Rust type. At
//! the call site, `Lookup<__V>` must infer `__V = #field_ty` — the
//! emitted `#field_ty: SameAs<__V>` bound is the reflexive
//! type-equality witness that pins it. That puts `{Model}Filter` on
//! the same compile-time footing as the closure API — passing the
//! wrong value type to a setter (for example
//! `PostFilter::new().view_count(Lookup::Eq("42"))` for an `i32`
//! column) fails at the call site, not later at bind time.
//! The `IntoFilterValue` bound is emitted on the **method generic
//! `__V`**, not on the concrete `#field_ty`. Two consequences:
//! 1. A column whose declared Rust type does not yet implement
//!    `IntoFilterValue` (for example `Decimal` when the
//!    `rust_decimal` feature is off, `Vec<String>`, a user-defined
//!    JSONB payload wrapper) still gets a setter emitted. Rust
//!    checks a concrete-type `where` clause at impl-definition time
//!    (issue #48214), which would whole-model-reject a model
//!    containing such a column. Keeping the bound on `__V` defers
//!    the check to monomorphization, so only the unsupported
//!    setter's call site fails — with a localized
//!    "trait bound `…: IntoFilterValue` is not satisfied" rather
//!    than a whole-model reject.
//! 2. Nullable columns (outer `Option<T>`) get two extra monomorphic
//!    setters — `<field>_is_null()` and `<field>_is_not_null()` — emitted
//!    alongside the generic setter. These call the hidden
//!    `filter_clause_is_null` / `filter_clause_is_not_null` helpers
//!    (`::djogi::__private::query::*`), which build `IS NULL` /
//!    `IS NOT NULL` clauses with no value and **no `IntoFilterValue`
//!    bound**. So a nullable column whose inner type does not implement
//!    `IntoFilterValue` (`Option<Jsonb<P>>`, `Option<GeoPoint>`, user
//!    newtypes) can still be null-checked through the struct-filter path —
//!    full parity with the closure API's [`FieldRef<M, V>`]`::is_null`. The
//!    null setters are additive alongside the existing generic setter, and
//!    provide the ONLY supported struct-filter path for null checks —
//!    `PostFilter::new().score(Lookup::Eq(Some(7)))` does not compile
//!    through the generic setter, since its `__V: IntoFilterValue` bound
//!    (pinned to `Option<T>` by the `SameAs` witness) is unsatisfiable:
//!    `IntoFilterValue` has no `Option<T>` impl. Detection is by
//!    [`unwrap_option`] on the field's outer type, covering every
//!    outer-`Option<T>` field — not just the portable subset. A null helper
//!    whose name would collide with an existing user-field setter is
//!    skipped, preserving backwards compatibility for models with a field
//!    named e.g. `score_is_null`.
//! # `pk = None` gate
//! `crud::expand` does not emit `impl Model` for `pk = None` models
//! (`Model::Pk: Encode` cannot be satisfied without a real PK — see
//! `crud.rs` for the rationale). The `{Model}Filter` struct itself, its
//! setters, and the `ModelFilter` impl have no `Model` dependency — the
//! clauses are erased `FilterClause` records, not `FieldRef<M, V>`
//! handles — so they compile for every pk strategy.
//! The `IntoQ<#name>` bridge is the one piece that
//! does require `#name: Model` (because `Q<T: Model>` carries that
//! bound). Skip the bridge emission for `pk = None` models — the
//! `QuerySet::filter_struct` / `exclude_struct` entry points are
//! unreachable on a model with no `Model` impl anyway, so the missing
//! `IntoQ` impl is not observable.
//! # Path routing
//! All emitted type references go through `::djogi::*` rather than
//! reaching into sub-modules directly. Macro output compiles in the
//! user's crate, which depends only on `djogi`; routing through the
//! top-level re-exports means a single dep is sufficient.
//! [`Lookup<V>`]: ::djogi::Lookup
//! [`FilterClause`]: ::djogi::FilterClause
//! [`FieldRef<M, V>`]: ::djogi::FieldRef
//! [`QuerySet`]: ::djogi::QuerySet
//! [`unwrap_option`]: crate::model::attrs::unwrap_option

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, unwrap_option};
use crate::model::portable_field_emit::{PortableFieldEmitInfo, PortableFieldKind};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::ItemStruct;

/// Emit `{Model}Filter` with one setter per user field plus the
/// `ModelFilter` trait impl.
/// `struct_item` is the post-injection struct: framework columns sit at
/// the front in the order `inject::expand` placed them. We skip past
/// them — filtering by `id` / `created_at` / `updated_at` goes through
/// the typed closure API, which is already sufficient — and iterate user
/// fields in source order. Skip count is keyed off `model_attrs.pk` the
/// same way `descriptor::expand` does it, keeping the single
/// framework-field contract consistent across generated code.
/// `_field_attrs` is threaded through for forward compatibility
/// per-field rename hints, validation, or column-override keys may alter
/// the emitted setter names in a later phase. Unused today.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
    portable_field_info: &[PortableFieldEmitInfo],
) -> TokenStream {
    let name = &struct_item.ident;
    let filter_name = format_ident!("{}Filter", name);

    // Framework-field skip count — mirrors `descriptor::expand`. For
    // `pk = None` models, `inject::expand` only prepends `created_at`
    // and `updated_at`; everything else prepends `id`, `created_at`,
    // `updated_at`. A mismatch here would emit a `.id(...)` setter that
    // shadows the PK and disagrees with the descriptor — the single
    // source of truth for the schema contract. `framework_field_count`
    // is the single source: `2` for `PkStrategy::None`, `3` otherwise.
    let n_framework = model_attrs.framework_field_count();

    // Typed setters: the value's generic `__V` is pinned to the
    // column's declared Rust type through a reflexive `SameAs<#ty>`
    // bound. A user passing the wrong value type
    // `PostFilter::new().view_count(Lookup::Eq("42"))` for an `i32`
    // column — infers `__V = &str`, which then fails the
    // `i32: SameAs<__V>` bound with a clear "expected `i32`" error at
    // the call site. Matches the closure API's compile-time discipline.
    // # Why not `where #ty: IntoFilterValue` directly?
    // Rust rejects concrete bounds in method `where` clauses whose
    // subject isn't a method generic (issue #48214: "where clauses on
    // method must reference a type parameter"). An emission like
    // `where Option<String>: IntoFilterValue` fails at impl-definition
    // time — the whole `{Model}Filter` refuses to compile for any
    // model whose columns include a type without an `IntoFilterValue`
    // impl. That would be a whole-model regression, not the localized
    // call-site failure the review called for.
    // # Deferred-bound pattern
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
    // The helper trait lives in `djogi::__private::SameAs` to keep it
    // out of the public namespace — users compose filters through
    // `Lookup<V>` and never need to name `SameAs` themselves.
    // # Nullable columns: dedicated null-only setters
    // For outer-`Option<T>` fields the macro emits `<field>_is_null()` /
    // `<field>_is_not_null()` below (issue #318), backed by the hidden
    // `filter_clause_is_null` / `filter_clause_is_not_null` helpers —
    // value-free, no `IntoFilterValue` bound. The generic
    // `Lookup::<#ty>::IsNull` path through `from_lookup` still exists but is
    // only reachable when `#ty: IntoFilterValue`; the dedicated setters are
    // the supported way to null-check ANY nullable column, matching the
    // closure API's `.is_null()`. A null helper colliding with an existing
    // user-field setter name is skipped.
    //
    // Names of every user-field setter the macro will emit (stripped column
    // names). A null helper whose name collides with one of these is skipped
    // so a model with a field literally named e.g. `score_is_null` keeps its
    // own generic setter and does not get a duplicate-method conflict. This
    // deduplicates only *within* the macro-generated `impl {Model}Filter`
    // block; a method the adopter defines in their own separate `impl` block
    // is invisible to the macro (see the generated null-setter doc comments
    // and the module-level design notes on that unavoidable limitation).
    let user_method_names: HashSet<String> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .filter_map(|field| field.ident.as_ref())
        .map(crate::syn_util::column_name_from_ident)
        .collect();

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

            let generic_setter = quote! {
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
            };

            // Outer-`Option<T>` fields additionally get two value-free
            // null-only setters backed by `filter_clause_is_null` /
            // `filter_clause_is_not_null`. They impose NO `IntoFilterValue`
            // bound, so non-portable inners (`Option<Jsonb<P>>`,
            // `Option<GeoPoint>`, user newtypes) can be null-checked through
            // the struct-filter path (issue #318). Detection is
            // `unwrap_option` (outer-Option only), covering ALL nullable
            // fields — not just the `PortableFieldKind::is_optional()`
            // portable subset.
            let (_inner, is_optional) = unwrap_option(ty);
            if !is_optional {
                return Some(generic_setter);
            }

            let mut methods = vec![generic_setter];

            // Method names derive from the stripped column base so they line
            // up with the SQL column literal and the collision set.
            let is_null_name = format!("{column}_is_null");
            if !user_method_names.contains(&is_null_name) {
                let is_null_ident = format_ident!("{}", is_null_name);
                let null_doc = format!(
                    "Append a `{column} IS NULL` check to the filter. Value-free — \
                     requires no `IntoFilterValue` impl on the column's inner type, \
                     so it works on every nullable column. Mirrors the closure API's \
                     `f.{column}().is_null()`.\n\n\
                     This is a generated inherent method on the filter struct. If \
                     your crate already defines a method of this name in its own \
                     `impl` block for this filter, the two collide \
                     (`error[E0592]: duplicate definitions`); rename your method to \
                     resolve it."
                );
                methods.push(quote! {
                    #[doc = #null_doc]
                    #[inline]
                    pub fn #is_null_ident(mut self) -> Self {
                        self.clauses.push(
                            ::djogi::__private::query::filter_clause_is_null(#column)
                        );
                        self
                    }
                });
            }

            let is_not_null_name = format!("{column}_is_not_null");
            if !user_method_names.contains(&is_not_null_name) {
                let is_not_null_ident = format_ident!("{}", is_not_null_name);
                let not_null_doc = format!(
                    "Append a `{column} IS NOT NULL` check to the filter. Value-free — \
                     requires no `IntoFilterValue` impl on the column's inner type, \
                     so it works on every nullable column. Mirrors the closure API's \
                     `f.{column}().is_not_null()`.\n\n\
                     This is a generated inherent method on the filter struct. If \
                     your crate already defines a method of this name in its own \
                     `impl` block for this filter, the two collide \
                     (`error[E0592]: duplicate definitions`); rename your method to \
                     resolve it."
                );
                methods.push(quote! {
                    #[doc = #not_null_doc]
                    #[inline]
                    pub fn #is_not_null_ident(mut self) -> Self {
                        self.clauses.push(
                            ::djogi::__private::query::filter_clause_is_not_null(#column)
                        );
                        self
                    }
                });
            }

            Some(quote! { #(#methods)* })
        })
        .collect();

    let struct_doc = format!(
        "Programmatic filter builder for [`{name}`] — one setter per user field. \
         Use with `QuerySet::filter_struct` for closure-free filtering (shell, admin, dynamic UI). \
         The closure API (`QuerySet::filter(|f| ...)`) is the preferred surface when a closure is writable; \
         both paths preserve the same database result semantics."
    );

    // ── `IntoQ<#name>` bridge ───────────────
    // Lifts `{Model}Filter` into the `Q<T>` algebra so it composes with
    // the `QuerySet::filter_struct` / `exclude_struct` signature
    // `<F: IntoQ<T>>`. The impl lazily maps the single stored
    // `Vec<FilterClause>` source into portable Q leaves when the erased
    // column/op/value tuple is known to match the model field. Clauses
    // outside that conservative mapping fall back to `Q::Condition`,
    // preserving existing SQL behavior without storing parallel Q state.
    // Gated on `pk != PkStrategy::None`: `Q<T: Model>` carries a
    // `Model` bound, but `crud::expand` does not emit `impl Model` for
    // `pk = None` models. Skipping the bridge there keeps the rest of
    // `{Model}Filter` (struct, setters, `ModelFilter` impl) usable for
    // pk-less models even though the closure-free filter path is not
    // reachable on them.
    let filter_clause_arms = filter_clause_to_q_arms(name, portable_field_info);

    let into_q_bridge = if matches!(model_attrs.pk, PkStrategy::None) {
        quote! {}
    } else {
        quote! {
            impl ::djogi::__private::__SealedIntoQ for #filter_name {}
            impl ::djogi::__private::query::IntoQ<#name> for #filter_name {
                #[inline]
                fn into_q(self) -> ::djogi::__private::query::Q<#name> {
                    let clauses = <Self as ::djogi::ModelFilter>::into_clauses(self);
                    ::djogi::__private::query::clauses_into_q::<#name, _>(
                        clauses,
                        |__djogi_clause| {
                            let __djogi_parts = __djogi_clause.into_parts();
                            match (__djogi_parts.column, __djogi_parts.op, __djogi_parts.value) {
                                #(#filter_clause_arms)*
                                (__djogi_column, __djogi_op, __djogi_value) => {
                                    ::djogi::__private::query::Q::Condition(
                                        ::djogi::__private::query::FilterClauseParts {
                                            column: __djogi_column,
                                            op: __djogi_op,
                                            value: __djogi_value,
                                        }.into_condition(),
                                    )
                                }
                            }
                        },
                    )
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

fn filter_clause_to_q_arms(
    model_name: &syn::Ident,
    portable_field_info: &[PortableFieldEmitInfo],
) -> Vec<TokenStream> {
    portable_field_info
        .iter()
        .filter_map(|info| {
            if info.tracked_wrapped || info.field_kind.is_optional() {
                return None;
            }
            let ident = &info.rust_ident;
            let column = info.column_name.as_str();
            match info.field_kind {
                PortableFieldKind::Bool => Some(quote! {
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::Eq,
                        ::djogi::__private::query::FilterValue::Bool(__djogi_value),
                    ) => {
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, bool>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.eq(__djogi_value))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::Neq,
                        ::djogi::__private::query::FilterValue::Bool(__djogi_value),
                    ) => {
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, bool>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.neq(__djogi_value))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::In,
                        ::djogi::__private::query::FilterValue::List(__djogi_values),
                    ) if __djogi_values.iter().all(|__djogi_value| matches!(
                        __djogi_value,
                        ::djogi::__private::query::FilterValue::Bool(_)
                    )) => {
                        let __djogi_values = __djogi_values
                            .into_iter()
                            .filter_map(|__djogi_value| {
                                match __djogi_value {
                                    ::djogi::__private::query::FilterValue::Bool(__djogi_value) => {
                                        ::std::option::Option::Some(__djogi_value)
                                    }
                                    _ => ::std::option::Option::None,
                                }
                            })
                            .collect::<::std::vec::Vec<bool>>();
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, bool>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.in_(__djogi_values))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::NotIn,
                        ::djogi::__private::query::FilterValue::List(__djogi_values),
                    ) if __djogi_values.iter().all(|__djogi_value| matches!(
                        __djogi_value,
                        ::djogi::__private::query::FilterValue::Bool(_)
                    )) => {
                        let __djogi_values = __djogi_values
                            .into_iter()
                            .filter_map(|__djogi_value| {
                                match __djogi_value {
                                    ::djogi::__private::query::FilterValue::Bool(__djogi_value) => {
                                        ::std::option::Option::Some(__djogi_value)
                                    }
                                    _ => ::std::option::Option::None,
                                }
                            })
                            .collect::<::std::vec::Vec<bool>>();
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, bool>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.not_in(__djogi_values))
                    }
                }),
                PortableFieldKind::String => Some(quote! {
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::Eq,
                        ::djogi::__private::query::FilterValue::String(__djogi_value),
                    ) => {
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, ::std::string::String>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.eq(__djogi_value))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::Neq,
                        ::djogi::__private::query::FilterValue::String(__djogi_value),
                    ) => {
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, ::std::string::String>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.neq(__djogi_value))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::In,
                        ::djogi::__private::query::FilterValue::List(__djogi_values),
                    ) if __djogi_values.iter().all(|__djogi_value| matches!(
                        __djogi_value,
                        ::djogi::__private::query::FilterValue::String(_)
                    )) => {
                        let __djogi_values = __djogi_values
                            .into_iter()
                            .filter_map(|__djogi_value| {
                                match __djogi_value {
                                    ::djogi::__private::query::FilterValue::String(__djogi_value) => {
                                        ::std::option::Option::Some(__djogi_value)
                                    }
                                    _ => ::std::option::Option::None,
                                }
                            })
                            .collect::<::std::vec::Vec<::std::string::String>>();
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, ::std::string::String>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.in_(__djogi_values))
                    }
                    (
                        #column,
                        ::djogi::__private::query::LookupOp::NotIn,
                        ::djogi::__private::query::FilterValue::List(__djogi_values),
                    ) if __djogi_values.iter().all(|__djogi_value| matches!(
                        __djogi_value,
                        ::djogi::__private::query::FilterValue::String(_)
                    )) => {
                        let __djogi_values = __djogi_values
                            .into_iter()
                            .filter_map(|__djogi_value| {
                                match __djogi_value {
                                    ::djogi::__private::query::FilterValue::String(__djogi_value) => {
                                        ::std::option::Option::Some(__djogi_value)
                                    }
                                    _ => ::std::option::Option::None,
                                }
                            })
                            .collect::<::std::vec::Vec<::std::string::String>>();
                        let __djogi_field = ::djogi::__private::query::__make_djogi_field::<#model_name, ::std::string::String>(
                            #column,
                            |__djogi_row: &#model_name| &__djogi_row.#ident,
                        );
                        ::djogi::__private::query::Q::Portable(__djogi_field.not_in(__djogi_values))
                    }
                }),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn portable_info(
        rust_ident: syn::Ident,
        field_kind: PortableFieldKind,
        tracked_wrapped: bool,
    ) -> PortableFieldEmitInfo {
        PortableFieldEmitInfo {
            rust_ident,
            column_name: "probe".to_string(),
            rust_type: parse_quote!(bool),
            option_inner_type: None,
            field_kind,
            tracked_wrapped,
        }
    }

    #[test]
    fn filter_clause_to_q_arms_skips_tracked_bool_and_string_fields() {
        let model_name: syn::Ident = parse_quote!(Probe);
        let fields = vec![
            portable_info(parse_quote!(tracked_active), PortableFieldKind::Bool, true),
            portable_info(parse_quote!(tracked_label), PortableFieldKind::String, true),
        ];

        let arms = filter_clause_to_q_arms(&model_name, &fields);

        assert!(
            arms.is_empty(),
            "tracked bool/string fields must not emit portable filter_struct arms"
        );
    }

    #[test]
    fn filter_clause_to_q_arms_does_not_emit_optional_bool_or_string_fields() {
        let model_name: syn::Ident = parse_quote!(Probe);
        let fields = vec![
            portable_info(
                parse_quote!(maybe_active),
                PortableFieldKind::OptionBool,
                false,
            ),
            portable_info(
                parse_quote!(maybe_label),
                PortableFieldKind::OptionString,
                false,
            ),
        ];

        let arms = filter_clause_to_q_arms(&model_name, &fields);

        assert!(
            arms.is_empty(),
            "optional bool/string fields are not part of the bool/string-only bridge"
        );
    }

    #[test]
    fn filter_clause_to_q_arms_emits_plain_bool_and_string_fields() {
        let model_name: syn::Ident = parse_quote!(Probe);
        let fields = vec![
            portable_info(parse_quote!(active), PortableFieldKind::Bool, false),
            portable_info(parse_quote!(label), PortableFieldKind::String, false),
        ];

        let arms = filter_clause_to_q_arms(&model_name, &fields);

        assert_eq!(
            arms.len(),
            2,
            "plain bool/string fields should remain on the portable filter_struct path"
        );
    }

    fn probe_model_attrs() -> ModelAttrs {
        // Minimal valid attrs: default PK (HeerIdRecencyBiased), so
        // `framework_field_count()` == 3 and the `IntoQ` bridge IS emitted.
        ModelAttrs::parse(quote::quote!(table = "probe")).expect("valid model attrs")
    }

    #[test]
    fn expand_emits_null_helpers_for_option_fields() {
        // Three framework fields first (id/created_at/updated_at), then
        // user fields. The macro must emit null helpers for every outer
        // `Option<T>`, including the non-portable `Option<Jsonb<_>>` inner.
        let item: syn::ItemStruct = parse_quote! {
            pub struct Probe {
                pub id: ::djogi::HeerId,
                pub created_at: ::time::OffsetDateTime,
                pub updated_at: ::time::OffsetDateTime,
                pub maybe_count: Option<i32>,
                pub maybe_meta: Option<::djogi::Jsonb<Meta>>,
                pub maybe_label: std::option::Option<String>,
            }
        };

        let tokens = expand(&item, &probe_model_attrs(), &[], &[]).to_string();

        assert!(tokens.contains("maybe_count_is_null"));
        assert!(tokens.contains("maybe_count_is_not_null"));
        assert!(tokens.contains("maybe_meta_is_null"));
        assert!(tokens.contains("maybe_meta_is_not_null"));
        assert!(tokens.contains("maybe_label_is_null"));
        assert!(tokens.contains("filter_clause_is_null"));
        assert!(tokens.contains("filter_clause_is_not_null"));
    }

    #[test]
    fn expand_skips_null_helpers_for_non_option_fields() {
        let item: syn::ItemStruct = parse_quote! {
            pub struct Probe {
                pub id: ::djogi::HeerId,
                pub created_at: ::time::OffsetDateTime,
                pub updated_at: ::time::OffsetDateTime,
                pub count: i32,
                pub metadata: ::djogi::Jsonb<Meta>,
            }
        };

        let tokens = expand(&item, &probe_model_attrs(), &[], &[]).to_string();

        assert!(!tokens.contains("count_is_null"));
        assert!(!tokens.contains("metadata_is_null"));
    }

    #[test]
    fn expand_skips_colliding_null_helper_names() {
        // A user field literally named `maybe_count_is_null` makes the macro
        // emit a generic setter `maybe_count_is_null<__V>(Lookup<__V>)`. The
        // null helper for the sibling `maybe_count: Option<i32>` would also
        // generate a no-arg `maybe_count_is_null()`, but collision-skipping
        // must SKIP it so the impl block has exactly one
        // `pub fn maybe_count_is_null` (the user's generic setter), not a
        // duplicate. Same for `maybe_count_is_not_null`.
        let item: syn::ItemStruct = parse_quote! {
            pub struct Probe {
                pub id: ::djogi::HeerId,
                pub created_at: ::time::OffsetDateTime,
                pub updated_at: ::time::OffsetDateTime,
                pub maybe_count: Option<i32>,
                pub maybe_count_is_null: bool,
                pub maybe_count_is_not_null: bool,
            }
        };

        let tokens = expand(&item, &probe_model_attrs(), &[], &[]).to_string();

        // Exactly one `pub fn maybe_count_is_null` must exist (the user's bool
        // field generic setter). The null helper for the `Option<i32>` field is
        // suppressed due to collision-skipping. Same for
        // `maybe_count_is_not_null`.
        assert_eq!(tokens.matches("pub fn maybe_count_is_null").count(), 1);
        assert_eq!(tokens.matches("pub fn maybe_count_is_not_null").count(), 1);
    }
}
