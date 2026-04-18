//! Generates `{Model}Fields` — the typed column-handle bag.
//!
//! # `{Model}Fields`
//!
//! A ZST whose inherent methods return [`FieldRef<Self, V>`] for every column,
//! framework and user alike. The emission order mirrors `descriptor::expand`:
//!
//! 1. `id` — present for `pk = heerid | ranjid | serial`; omitted for `pk =
//!    "none"` (matches the descriptor's framework-prefix gating, keeping the
//!    single schema contract consistent).
//! 2. `created_at`, `updated_at` — always emitted, typed as
//!    `::djogi::types::DateTime`.
//! 3. User-declared columns in struct source order.
//!
//! The `FieldRef`'s `V` generic is the user's declared Rust type verbatim —
//! `String`, `i32`, `Option<i64>`, `Jsonb<Foo>`, etc. — so lookup methods like
//! `.eq(value)` type-check against the column's actual binding type. Methods
//! gated on `V = String` (`.contains`, `.starts_with`, …) resolve only on the
//! string columns; any other column gets a compile error citing the method's
//! absence. That's the feature.
//!
//! # `pk = "none"`
//!
//! `FieldRef<M, V>` has `M: Model` as a trait bound, and `crud::expand` does
//! **not** emit `impl Model` for `pk = "none"` models (the `Pk: Encode` bound
//! can't be honestly satisfied without a real PK — see `crud.rs` for the
//! rationale). So emitting accessors here for those models would fail at
//! E0277 the moment the user's struct is parsed. This module mirrors
//! `crud.rs`'s gate: `pk = "none"` keeps the Phase-1 empty-stub behaviour
//! unchanged, so everything else (struct injection, `FromRow`, descriptor
//! registration) still compiles. A future phase introducing a
//! composite/user-managed PK trait will unlock accessors for them.
//!
//! # `{Model}Filter`
//!
//! Lives in its own module: [`crate::model::filter`]. `{Model}Filter` is an
//! **erased** counterpart to `{Model}Fields` — a runtime `Vec<FilterClause>`
//! with one setter per user field, used with `QuerySet::filter_struct` for
//! closure-free filtering (shell, admin, dynamic UI). Keeping the two in
//! separate modules keeps this file focused on the typed-accessor surface.
//!
//! # Path routing
//!
//! All emitted type references go through `::djogi::types::*` and
//! `::djogi::query::*` rather than reaching into `heeranjid` / `time` /
//! `uuid` directly. Macro output compiles in the user's crate, which depends
//! only on `djogi`; the re-exports mean a single dep is sufficient.

use crate::model::attrs::{ModelAttrs, PkStrategy};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}Fields` with one inherent method per column.
///
/// `struct_item` is the post-injection struct: its `fields` list already has
/// the framework-injected columns (`id` / `created_at` / `updated_at`) at the
/// front in the same order `descriptor::expand` relies on. The `model_attrs`
/// are consulted solely to type the `id` accessor — the per-field methods
/// otherwise read the Rust type verbatim from the struct.
///
/// `{Model}Filter` is emitted separately by [`crate::model::filter::expand`]
/// — keeping the two in different modules isolates their codegen surfaces.
pub fn expand(struct_item: &ItemStruct, model_attrs: &ModelAttrs) -> TokenStream {
    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);

    // ── Per-column accessor emission ─────────────────────────────────────────
    //
    // `FieldRef<M, V>` is bounded `M: Model`. `crud::expand` does NOT emit
    // `impl Model` for `pk = "none"` models (the trait's `Pk: Encode` bound
    // can't be honestly satisfied without a real PK), so emitting accessor
    // methods here for those models would fail to compile with E0277 the
    // moment the user's struct is parsed — which breaks Phase 1's contract
    // that pk=none models still get struct injection, `FromRow`, and
    // descriptor registration.
    //
    // Resolution: mirror `crud::expand`'s gate exactly. `pk = "none"` keeps
    // the Phase-1 empty-stub behavior; when the future phase introduces a
    // composite/user-managed PK trait, this branch can emit accessors keyed
    // on that trait instead.
    let accessor_impl: TokenStream = if matches!(model_attrs.pk, PkStrategy::None) {
        TokenStream::new()
    } else {
        // The struct's field list (after inject::expand) already holds
        // framework columns at the front; user fields follow in source order.
        // Iterate the list directly — no separate framework/user branching —
        // and let the field ident (`id`, `created_at`, `updated_at`, …) drive
        // the method name and the column literal. The field's Rust type is
        // the `FieldRef`'s `V` generic verbatim, so `Option<T>` / `Jsonb<T>`
        // / user-defined wrapper types propagate through without a
        // translation table.
        let accessors: Vec<TokenStream> = struct_item
            .fields
            .iter()
            .filter_map(|field| {
                let ident = field.ident.as_ref()?;
                // `Ident::to_string()` on a raw identifier returns the `r#`
                // prefix verbatim (e.g. `r#type` → `"r#type"`). SQL column
                // names must never carry that prefix — strip it so a field
                // named `r#type` maps to column `type`, matching the
                // behaviour users expect from any raw-ident-aware ORM.
                let raw = ident.to_string();
                let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
                let ty = &field.ty;
                Some(quote! {
                    /// Typed handle for this column.
                    ///
                    /// Returns a [`FieldRef`] carrying the column name plus
                    /// phantom markers that bind it to this model and the
                    /// column's Rust type. Consume it via lookup methods
                    /// (`.eq`, `.gte`, `.contains`, …) to build a
                    /// [`Condition`] leaf.
                    ///
                    /// [`FieldRef`]: ::djogi::query::FieldRef
                    /// [`Condition`]: ::djogi::query::Condition
                    #[inline]
                    pub fn #ident(&self) -> ::djogi::query::FieldRef<#name, #ty> {
                        ::djogi::query::FieldRef::new(#column)
                    }
                })
            })
            .collect();

        quote! {
            impl #fields_name {
                #(#accessors)*
            }
        }
    };

    quote! {
        /// Typed field accessors for QuerySet filter closures.
        ///
        /// Each inherent method returns a [`FieldRef`](::djogi::query::FieldRef)
        /// for one column; chain a lookup method to produce a
        /// [`Condition`](::djogi::query::Condition).
        ///
        /// `Default` is required by the `Model::Fields` associated type
        /// (`Copy + Default + Send + Sync + 'static`) so that
        /// `QuerySet::filter(|f| …)` can construct the ZST handle from
        /// inside the closure without the caller naming the type.
        #[derive(Debug, Clone, Copy, Default)]
        pub struct #fields_name;

        #accessor_impl
    }
}
