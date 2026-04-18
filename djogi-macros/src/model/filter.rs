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
//!     pub fn title(mut self, lookup: ::djogi::Lookup<String>) -> Self { ... }
//!     pub fn published(mut self, lookup: ::djogi::Lookup<bool>) -> Self { ... }
//!     pub fn view_count(mut self, lookup: ::djogi::Lookup<i32>) -> Self { ... }
//! }
//!
//! impl ::djogi::ModelFilter for PostFilter { ... }
//! ```
//!
//! # Nullable columns and generic setters
//!
//! Each setter is generic in `V: IntoFilterValue` rather than bound to the
//! field's declared Rust type. Two reasons:
//!
//!   1. Fields whose type does not implement `IntoFilterValue` (for example
//!      `Decimal` when the `rust_decimal` feature is off, `Vec<String>`,
//!      JSONB payload wrappers) still get a setter — the trait bound is
//!      only checked at call time, so defining the setter does not require
//!      an `IntoFilterValue` impl for every column type.
//!   2. Nullable columns (`Option<T>`) take `Lookup<T>` directly — users
//!      write `Lookup::Eq("hello".to_string())` rather than
//!      `Lookup::Eq(Some("hello".to_string()))`, matching the closure
//!      API's ergonomics. Explicit NULL checks go through `Lookup::IsNull`
//!      / `Lookup::IsNotNull`, which carry no value and work regardless of
//!      the column's declared nullability (Postgres can produce NULL for
//!      any column through outer joins / CASE / window frames, so these
//!      variants are always meaningful).
//!
//! The trade-off: the compiler accepts `filter.view_count(Lookup::Eq("s"))`
//! even though `view_count` is an `i32` (since `&str: IntoFilterValue`).
//! This is a known consequence of the erased-clause shape. Callers who
//! want strict typed value checks use `QuerySet::filter(|f| ...)` — the
//! closure API is the typed surface and enforces `V` matches the column.
//! `{Model}Filter` is the dynamic surface and accepts anything bindable.
//!
//! # `pk = "none"` gate
//!
//! `crud::expand` does not emit `impl Model` for `pk = "none"` models
//! (`Model::Pk: Encode` cannot be satisfied without a real PK — see
//! `crud.rs` for the rationale). `{Model}Filter` does not depend on the
//! `Model` trait — the clauses are erased `FilterClause` records, not
//! `FieldRef<M, V>` handles — so it compiles for every pk strategy.
//! There is no gate here; the user-field setter emission works the same
//! for `pk = "none"` structs as for the others. Skipping framework
//! fields is parametric on `model_attrs.pk`, matching `descriptor::expand`.
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
    // `pk = "none"` models, `inject::expand` only prepends `created_at`
    // and `updated_at`; everything else prepends `id`, `created_at`,
    // `updated_at`. A mismatch here would emit a `.id(...)` setter that
    // shadows the PK and disagrees with the descriptor — the single
    // source of truth for the schema contract.
    let n_framework = match model_attrs.pk {
        PkStrategy::None => 2,
        _ => 3,
    };

    // Each setter is generic in `V: IntoFilterValue`. Making it generic
    // rather than binding `V` to the field's concrete type at emission
    // time buys two things at the cost of slightly looser compile-time
    // type-checking on the value:
    //
    //   1. Fields whose Rust type does not implement `IntoFilterValue`
    //      (for example `Decimal`, `Vec<String>`, user-defined wrappers
    //      without a ready SQL mapping) still get a setter. The typed
    //      closure path in `{Model}Fields` remains the surface for
    //      strict-typed filtering; `{Model}Filter` is the dynamic path,
    //      and a generic `V` matches its "erased clause Vec" model.
    //   2. Newtype columns — wrappers around a type that DOES implement
    //      `IntoFilterValue` — compose without the macro needing a
    //      per-wrapper pattern. Users pass the inner type's value; the
    //      trait impl handles projection.
    //
    // The downside: `post_filter.view_count(Lookup::Eq("str"))` compiles
    // (since `&str: IntoFilterValue`) even though `view_count` is an
    // `i32`. That's a genuine loss relative to the typed closure API
    // (which would reject this at compile time), but it's the direct
    // consequence of `FilterClause` being an erased Vec<Clause> shape.
    // Callers who want strict-typed value checks use the closure API;
    // callers who want dynamic composition use this one.
    let setters: Vec<TokenStream> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .filter_map(|field| {
            let ident = field.ident.as_ref()?;
            // Raw identifiers: `Ident::to_string` returns `"r#type"` for
            // `r#type`. Strip the prefix for the SQL column literal (same
            // rationale as `stubs.rs`) but keep the raw ident for the
            // method name so users can still call `.r#type(...)`.
            let raw = ident.to_string();
            let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
            let doc = format!(
                "Append a `{column}` lookup to the filter. Accepts any `Lookup<V>` where `V: IntoFilterValue` — generic to keep the erased-clause shape consistent across field types (including `Decimal` / `Vec<T>` / user newtypes that don't round-trip the closure API's typed path)."
            );
            Some(quote! {
                #[doc = #doc]
                #[inline]
                pub fn #ident<__V>(mut self, lookup: ::djogi::Lookup<__V>) -> Self
                where
                    __V: ::djogi::IntoFilterValue,
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
    }
}
