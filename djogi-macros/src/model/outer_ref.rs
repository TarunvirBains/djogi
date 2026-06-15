//! Generates `{Model}OuterRef` — the typed outer-scope column-reference bag
//! used to build correlated subqueries.
//! # `{Model}OuterRef`
//! A ZST whose inherent associated functions (no receiver — they are
//! called as `AccountOuterRef::balance()`, not `account_outer_ref.balance()`)
//! return [`OuterRef<Self, V>`](::djogi::expr::OuterRef) for every column,
//! framework and user alike. The emission order mirrors [`stubs`]:
//! 1. `id` — present for `pk = HeerId | RanjId | HeerIdDesc | RanjIdDesc |
//! Serial`; omitted for `pk = None`.
//! 2. `created_at`, `updated_at` — always emitted.
//! 3. User-declared columns in struct source order.
//! The `OuterRef`'s `V` generic is the user's declared Rust type verbatim
//! exactly like `{Model}Fields`. The typed `V` makes a correlated
//! subquery like `outer_ref_on<V1>().as_expr().eq(field_ref_on<V2>().as_expr())`
//! a compile error unless `V1 == V2`, catching value-type mismatches at
//! the closure site rather than as a Postgres runtime error.
//! # Why associated functions (not methods)
//! `OuterRef` does not carry any per-instance state — it is a typed
//! handle that erases to a `&'static str` column name at construction.
//! The canonical call site is inside a correlated subquery's
//! `filter_expr` closure:
//! ```ignore
//! Account::objects()
//! .filter_expr(|_| Exists::new(
//!   Entry::objects().filter_expr(|inner| {
//!    inner.ledger_id().as_pk_expr()
//!    .eq(AccountOuterRef::id().as_expr())
//!   })
//!  ).as_expr())
//! ```
//! Associated-function syntax (`AccountOuterRef::id()` — no receiver)
//! reads naturally because there is no value to receive on; `Fields`
//! methods take `&self` only because the closure receives a
//! default-constructed `T::Fields` value and dots into it. Outer refs
//! have no such carrier — they reference the enclosing scope, which is
//! a compile-time concept, not a runtime value.
//! # `pk = None`
//! Same gate as [`crate::model::stubs`] / [`crate::model::crud`]: models
//! that opt out of a PK do not get `impl Model`, so `OuterRef<M, V>`
//! (which is bounded `M: Model`) cannot resolve. Emitting accessors here
//! for those models would fail at E0277; mirror the Phase-1 empty-stub
//! gate instead.
//! # Path routing
//! All emitted type references go through `::djogi::expr::*` and
//! `::djogi::types::*` — matching the project rule that macro-emitted
//! code never reaches into `heeranjid` / `time` / `uuid` directly.

use crate::model::attrs::{ModelAttrs, PkStrategy};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}OuterRef` with one associated function per column.
/// `struct_item` is the post-injection struct: its `fields` list already
/// has the framework-injected columns (`id` / `created_at` /
/// `updated_at`) at the front in the same order `descriptor::expand`
/// and `stubs::expand` rely on. The `model_attrs` are consulted solely
/// to gate the emission on `pk = None`; user-column types otherwise
/// come from the struct verbatim.
pub fn expand(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    let name = &struct_item.ident;
    let outer_name = format_ident!("{}OuterRef", name);

    // Mirror `stubs::expand`: `pk = None` keeps the Phase-1 empty-stub
    // behaviour because `OuterRef<M, V>` requires `M: Model`, and
    // `crud::expand` does not emit `impl Model` for `pk = None`
    // models. A later phase that introduces a user-managed / composite
    // PK trait can lift this gate without touching the per-column
    // emission below.
    let accessor_impl: TokenStream = if matches!(model_attrs.pk, PkStrategy::None) {
        TokenStream::new()
    } else {
        // Iterate all fields (framework + user); for each, emit an
        // associated function that returns
        // `::djogi::expr::OuterRef<Self, <type>>` via the sealed
        // macro-entry point. The raw-ident-aware column name rule
        // mirrors `stubs.rs` exactly — a field named `r#type` maps to
        // column `type`.
        let accessors: Vec<TokenStream> = struct_item
            .fields
            .iter()
            .filter_map(|field| {
                let ident = field.ident.as_ref()?;
                let column = crate::syn_util::column_name_from_field(field);
                let ty = &field.ty;
                Some(quote! {
                 /// Typed outer-scope handle for this column — use
                 /// inside a correlated subquery's `filter_expr`
                 /// closure to reference the enclosing scope.
                 /// Returns an [`OuterRef`] carrying the column name
                 /// plus phantom markers that bind it to this model
                 /// and the column's Rust type. Call `.as_expr()` to
                 /// produce an `Expr<V>` for composition with
                 /// `.eq` / arithmetic / other expression-IR
                 /// consumers.
                 /// [`OuterRef`]: ::djogi::expr::OuterRef
                 #[inline]
                 pub fn #ident() -> ::djogi::expr::OuterRef<#name, #ty> {
                  ::djogi::expr::subquery::__macro_support::__make_outer_ref::<
                   #name,
                   #ty,
                  >(#column)
                 }
                })
            })
            .collect();

        quote! {
         impl #outer_name {
          #(#accessors)*
         }
        }
    };

    quote! {
     /// Typed outer-scope column references for correlated subqueries.
     /// Each inherent associated function returns an
     /// [`OuterRef`](::djogi::expr::OuterRef) for one column; call
     /// `.as_expr()` on the handle to produce an
     /// [`Expr<V>`](::djogi::expr::Expr) you can slot into a nested
     /// `filter_expr` closure. Matches the `{Model}Fields` pattern
     /// for inner-scope column references but emits an outer-scope
     /// reference (unqualified column name — Postgres resolves against
     /// the enclosing query scope).
     /// `Default` is derived so downstream code can trivially
     /// construct the ZST if some future API wants an instance-style
     /// handle; today every method is an associated function with no
     /// receiver.
     #[derive(Debug, Clone, Copy, Default)]
     pub struct #outer_name;

     #accessor_impl
    }
}
