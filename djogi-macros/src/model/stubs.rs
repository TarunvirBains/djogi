//! Generates `{Model}Fields` and the path-aware sibling `{Model}SqlFields`.
//! # `{Model}Fields` (PR3)
//! After, `{Model}Fields` is a **zero-sized** type whose
//! inherent methods return [`DjogiField<Self, V>`](::djogi::query::DjogiField)
//! for every column — framework and user alike. The wrapper bundles a
//! portable [`sassi::Field<M, V>`] (consumed by Punnu in-memory evaluation)
//! and a SQL-only [`FieldRef<M, V>`](::djogi::query::FieldRef) so a single
//! closure can compose portable predicates **and** PostgreSQL-specific
//! predicates without naming two types.
//! Emission order mirrors `descriptor::expand`:
//! 1. `id` — present for `pk = HeerId | RanjId | HeerIdDesc | RanjIdDesc |
//! Serial`; omitted for `pk = None` (matches the descriptor's
//!    framework-prefix gating, keeping the single schema contract consistent).
//! 2. `created_at`, `updated_at` — always emitted, typed as
//!    `::djogi::types::DateTime`.
//! 3. User-declared columns in struct source order.
//!    Root `{Model}Fields` carries **no** `__djogi_path` slot. Path-aware
//!    traversal lives on the SQL-only sibling `{Model}SqlFields` so portable
//!    predicates target physical root columns only — relation traversal
//!    columns (e.g. `"department.name"`) are not portable across cache and
//!    refresh boundaries.
//! # `{Model}SqlFields`
//! Path-aware companion view. Same accessor surface as `{Model}Fields`
//! but every accessor returns [`FieldRef<Self, V>`](::djogi::query::FieldRef).
//! Used by macro-emitted relation/visage SQL paths and by internal helpers
//! that already operate in SQL space. Carries the optional `__djogi_path`
//! slot and a `with_path(path: &'static str)` constructor; the root
//! `{Model}Fields` ZST does not.
//! # `pk = None`
//! `DjogiField<M, V>` and `FieldRef<M, V>` both bound `M: Model`, and
//! `crud::expand` does **not** emit `impl Model` for `pk = None` models
//! (the `Pk: Encode` bound can't be honestly satisfied without a real PK
//! see `crud.rs` for the rationale). This module mirrors the gate:
//! `pk = None` keeps the Phase-1 empty-stub `{Model}Fields` ZST and
//! suppresses `{Model}SqlFields` entirely. A future phase introducing a
//! composite/user-managed PK trait will unlock accessors for them.
//! # `{Model}Filter`
//! Lives in its own module: [`crate::model::filter`]. `{Model}Filter` is an
//! **erased** counterpart to `{Model}Fields` — a runtime `Vec<FilterClause>`
//! with one setter per user field, used with `QuerySet::filter_struct` for
//! closure-free filtering (shell, admin, dynamic UI). Keeping the two in
//! separate modules keeps this file focused on the typed-accessor surface.
//! # Path routing
//! All emitted type references go through `::djogi::types::*` and
//! `::djogi::query::*` rather than reaching into `heeranjid` / `time` /
//! `uuid` directly. Macro output compiles in the user's crate, which depends
//! only on `djogi`; the re-exports mean a single dep is sufficient.

use crate::model::attrs::{ModelAttrs, PkStrategy};
use crate::model::portable_field_emit::PortableFieldEmitInfo;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}Fields` (and the SQL-only sibling `{Model}SqlFields`)
/// with one inherent accessor per column.
/// `struct_item` is the post-injection struct: its `fields` list already has
/// the framework-injected columns (`id` / `created_at` / `updated_at`) at the
/// front in the same order `descriptor::expand` relies on. The `model_attrs`
/// are consulted solely to gate `pk = None` (which keeps the empty-stub
/// behaviour). Per-field emission reads ident, column name, and Rust type
/// from `portable_field_info` so root accessors stay in lock-step with
/// `crud::expand`'s `Model::__djogi_emit_field_predicate` arms.
/// `portable_field_info` is the shared metadata vector built by
/// [`crate::model::portable_field_emit::build`] in `mod.rs`. Both `stubs.rs`
/// (root + SQL accessors) and `crud.rs` (portable predicate dispatch arms)
/// walk the same vector so column-name and Rust-type computation never drift.
/// `{Model}Filter` is emitted separately by [`crate::model::filter::expand`]
/// keeping the two in different modules isolates their codegen surfaces.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    portable_field_info: &[PortableFieldEmitInfo],
) -> TokenStream {
    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);
    let sql_fields_name = format_ident!("{}SqlFields", name);

    // ── `{Model}Fields` accessor emission (PR3 — flipped to DjogiField) ──────
    // Every accessor returns `DjogiField<Self, V>`. The wrapper's inherent
    // methods cover the portable predicate surface (eq/neq/in/not_in/null
    // tests, ordering for `V: DjogiPortableOrd`, ASCII-stable string
    // patterns) and route PostgreSQL-specific predicates through
    // `.explicit_pg_predicate`.
    // `crud::expand` does NOT emit `impl Model` for `pk = None` models (the
    // trait's `Pk: Encode` bound can't be honestly satisfied without a real
    // PK), so emitting accessor methods here for those models would fail
    // to compile with E0277 the moment the user's struct is parsed
    // breaking the contract that pk=none models still get struct
    // injection, `FromRow`, and descriptor registration.
    // Resolution: mirror `crud::expand`'s gate exactly. `pk = None` keeps
    // the Phase-1 empty-stub behavior; a future phase introducing a
    // composite/user-managed PK trait can emit accessors keyed on that
    // trait instead.
    let accessor_impl: TokenStream = if matches!(model_attrs.pk, PkStrategy::None) {
        TokenStream::new()
    } else {
        // Walk the shared portable field metadata vector. It iterates in
        // the same order as `struct_item.fields` (framework fields first,
        // then user fields) so emission order matches the descriptor /
        // FromPgRow / column_list contract by construction. Reading the
        // ident, column name, and Rust type from the metadata keeps this
        // accessor surface in lock-step with `crud::expand`'s portable
        // predicate arms — the same vector drives both consumers.
        // Each accessor calls
        // `::djogi::query::field::__macro_support::__make_djogi_field`,
        // which is a re-export of
        // `crate::query::field::djogi_field_macro_support::__make_djogi_field`
        // routed through `::djogi::__private::query` for cross-crate
        // expansion. The closure stamped into each accessor is a function
        // pointer (`fn(&Self) -> &V`), which keeps the resulting
        // `DjogiField` `Copy + Clone` without captured state — the
        // closure-API filter path needs to call `f.col` multiple times
        // inside one closure invocation, so non-`Copy` would cripple
        // composition.
        let accessors: Vec<TokenStream> = portable_field_info
            .iter()
            .map(|info| {
                let ident = &info.rust_ident;
                let column = info.column_name.as_str();
                let ty = &info.rust_type;
                quote! {
                    /// Typed handle for this column.
                    /// Returns a [`DjogiField`] that bundles a portable
                    /// `sassi::Field<Self, V>` (used by Punnu in-memory
                    /// evaluation) with a SQL-only `FieldRef<Self, V>`
                    /// (used by SQL emission). Chain a portable predicate
                    /// method (`.eq`, `.gte`, `.contains`, …) to build a
                    /// `PortablePredicate<Self>` that flows through both
                    /// the database and the cache. Reach PostgreSQL-
                    /// specific predicates (regex, JSONB path, spatial,
                    /// array operators) through
                    /// [`DjogiField::explicit_pg_predicate`].
                    /// [`DjogiField`]: ::djogi::query::DjogiField
                    /// [`DjogiField::explicit_pg_predicate`]: ::djogi::query::DjogiField::explicit_pg_predicate
                    #[inline]
                    pub fn #ident(&self) -> ::djogi::query::DjogiField<#name, #ty> {
                        ::djogi::__private::query::__make_djogi_field::<#name, #ty>(
                            #column,
                            |__djogi_value: &#name| &__djogi_value.#ident,
                        )
                    }
                }
            })
            .collect();

        quote! {
            impl #fields_name {
                /// Construct a root-scope `Fields` handle.
                /// Equivalent to the `Default` impl. Root `{Model}Fields` is
                /// a ZST after, so this constructor produces
                /// the empty struct directly without copying any state.
                #[doc(hidden)]
                #[inline]
                pub const fn new() -> Self {
                    Self {}
                }

                #(#accessors)*
            }
        }
    };

    // ── `{Model}SqlFields` accessor emission (PR2d retained, PR3 sole route
    // for relation/visage traversal) ──────────────────────────────────────
    // The SQL-only sibling view — same accessor surface as `{Model}Fields`
    // but every accessor returns `FieldRef<Self, V>` and the struct carries
    // an optional SQL-alias path prefix (`__djogi_path`). Visage relation
    // traversal sites and other internal helpers that already compose
    // dotted column paths reach for this view rather than the portable
    // root surface. Cached root rows do not carry joined relation values,
    // so traversal predicates are SQL-only by construction; relegating
    // them to `{Model}SqlFields` keeps cache and refresh boundaries free
    // of relation paths that would silently misclassify as portable.
    let sql_accessor_impl: TokenStream = if matches!(model_attrs.pk, PkStrategy::None) {
        TokenStream::new()
    } else {
        let accessors: Vec<TokenStream> = portable_field_info
            .iter()
            .map(|info| {
                let ident = &info.rust_ident;
                let column = info.column_name.as_str();
                let ty = &info.rust_type;
                quote! {
                    /// SQL-only typed handle for this column.
                    /// Returns a [`FieldRef`](::djogi::query::FieldRef)
                    /// carrying the column name plus phantom markers that
                    /// bind it to this model and the column's Rust type.
                    /// Mirrors the accessor on `Self`'s `Fields` ZST but
                    /// stays explicitly SQL-only so traversal/relation
                    /// chains do not enter the portable predicate
                    /// boundary.
                    #[inline]
                    pub fn #ident(&self) -> ::djogi::query::FieldRef<#name, #ty> {
                        ::djogi::query::field::__macro_support::__make_field_ref::<#name, #ty>(
                            self.__djogi_path,
                            #column,
                        )
                    }
                }
            })
            .collect();

        quote! {
            impl #sql_fields_name {
                /// Construct a root-scope SQL fields handle with no
                /// SQL-alias path. Equivalent to the `Default` impl.
                #[doc(hidden)]
                #[inline]
                pub const fn new() -> Self {
                    Self { __djogi_path: ::core::option::Option::None }
                }

                /// Construct a traversal-scope SQL fields handle threaded
                /// with the given SQL-alias path. Used by macro-emitted
                /// relation/visage accessors that compose dotted column
                /// paths through relation chains.
                #[doc(hidden)]
                #[inline]
                pub const fn with_path(path: &'static str) -> Self {
                    Self { __djogi_path: ::core::option::Option::Some(path) }
                }

                #(#accessors)*
            }
        }
    };

    // Emit `search` accessor when the model has an FTS spec.
    // The accessor returns `::djogi::fts_query::FtsFieldRef<#name>` with the
    // tsvector column name ("search") and dictionary baked in as `&'static str`s.
    // FTS is SQL-only by construction (the tsvector column is a `GENERATED
    // ALWAYS AS` projection and `@@` / `ts_rank` have no portable Punnu
    // evaluator in 8eta); the FTS accessor stays on `{Model}Fields` directly
    // because adopters reach it through `f.search` and never need to thread
    // a `with_path` prefix through this column.
    let fts_accessor_impl: TokenStream = if let Some(fts) = &model_attrs.fts {
        let dictionary = &fts.dictionary;
        quote! {
            impl #fields_name {
                /// Typed handle for the full-text search column.
                /// Returns an [`FtsFieldRef`](::djogi::fts_query::FtsFieldRef) that
                /// exposes `.matches(q)` and `.rank(q)` for building `@@` and
                /// `ts_rank` predicates in filter closures and order expressions.
                /// The tsvector column is a `GENERATED ALWAYS AS` computed column
                /// Postgres maintains it automatically on every INSERT / UPDATE, so
                /// application code never writes to it directly.
                /// [`FtsFieldRef`]: ::djogi::fts_query::FtsFieldRef
                #[inline]
                pub fn search(&self) -> ::djogi::fts_query::FtsFieldRef<#name> {
                    ::djogi::fts_query::__macro_support::__make_fts_ref::<#name>(
                        "search",
                        #dictionary,
                    )
                }
            }
        }
    } else {
        TokenStream::new()
    };

    // The two structs differ in shape after PR3:
    // - `{Model}Fields` — ZST. No `__djogi_path` slot; root portable fields
    // target physical root columns only. Adopters compose root closures
    // against this handle via `QuerySet::filter(|f| f.col.eq(...))`.
    // - `{Model}SqlFields` — path-aware. Carries `__djogi_path:
    // Option<&'static str>` so visage/relation accessors can compose
    // dotted column paths.
    // For `pk = None` models both structs are suppressed (no `Model` impl,
    // no `FieldRef`/`DjogiField` accessors).
    let (struct_decls, sql_struct_decl) = if matches!(model_attrs.pk, PkStrategy::None) {
        (
            quote! {
                #[derive(Debug, Clone, Copy, Default)]
                pub struct #fields_name;
            },
            // `{Model}SqlFields` is suppressed for pk = None too — same
            // E0277 reasoning: `FieldRef` requires `M: Model`, which
            // pk=none models don't impl.
            TokenStream::new(),
        )
    } else {
        (
            quote! {
                /// Typed root field accessors for QuerySet filter closures.
                /// Zero-sized after every accessor reaches
                /// the column metadata through a baked-in `&'static str`
                /// (the column name) and a baked-in `fn(&Self) -> &V`
                /// extractor. `Default` is required by the
                /// `Model::Fields` associated type
                /// (`Copy + Default + Send + Sync + 'static`) so that
                /// `QuerySet::filter(|f| …)` can construct the handle from
                /// inside the closure without the caller naming the type.
                #[derive(Debug, Clone, Copy, Default)]
                pub struct #fields_name;
            },
            quote! {
                /// SQL-only typed field accessors — the path-aware sibling
                /// of `{Model}Fields`.
                /// Carries an optional SQL-alias path so visage-scoped
                /// traversal chains (`a.department.name`) compose into
                /// dot-qualified column names at emission time. Default
                /// (`None`) means the handle works as a plain-column
                /// accessor; the root portable surface lives on
                /// `{Model}Fields` and does not enter the path-threading
                /// surface.
                #[derive(Debug, Clone, Copy, Default)]
                pub struct #sql_fields_name {
                    /// SQL-alias path prefix threaded through traversal
                    /// chains. `None` for root-scope handles; `Some(...)` for
                    /// visage-scoped traversal accessors.
                    #[doc(hidden)]
                    pub __djogi_path: ::core::option::Option<&'static str>,
                }
            },
        )
    };

    quote! {
        #struct_decls

        #sql_struct_decl

        #accessor_impl

        #sql_accessor_impl

        #fts_accessor_impl
    }
}
