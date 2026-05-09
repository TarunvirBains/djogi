//! Generates `{Model}Fields` and `{Model}SqlFields` — the typed column-handle bags.
//!
//! # `{Model}Fields`
//!
//! A ZST whose inherent methods return [`FieldRef<Self, V>`] for every column,
//! framework and user alike. The emission order mirrors `descriptor::expand`:
//!
//! 1. `id` — present for `pk = HeerId | RanjId | HeerIdDesc | RanjIdDesc |
//!    Serial`; omitted for `pk = None` (matches the descriptor's
//!    framework-prefix gating, keeping the single schema contract consistent).
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
//! # `{Model}SqlFields` (Phase 8eta PR2d)
//!
//! Sibling ZST emitted alongside `{Model}Fields` for every PK-backed model.
//! Has the same per-column accessor surface but is intentionally
//! **SQL-only / path-aware**: the accessors return `FieldRef<Self, V>` and
//! carry the optional `__djogi_path` prefix used by relation/visage
//! traversal sites. The split exists so PR3 (the next slice in the 8eta
//! train) can flip `{Model}Fields` to return the portable `DjogiField<Self,
//! V>` wrapper without disturbing the SQL-only routing surface that
//! traversal code depends on.
//!
//! Both views use the same per-field metadata vector — the shared
//! [`crate::model::portable_field_emit::PortableFieldEmitInfo`] — so the
//! column names and Rust types they emit always agree with `crud::expand`'s
//! `Model::__djogi_emit_field_predicate` arm dispatch.
//!
//! # `pk = None`
//!
//! `FieldRef<M, V>` has `M: Model` as a trait bound, and `crud::expand` does
//! **not** emit `impl Model` for `pk = None` models (the `Pk: Encode` bound
//! can't be honestly satisfied without a real PK — see `crud.rs` for the
//! rationale). So emitting accessors here for those models would fail at
//! E0277 the moment the user's struct is parsed. This module mirrors
//! `crud.rs`'s gate: `pk = None` keeps the Phase-1 empty-stub behaviour
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
use crate::model::portable_field_emit::PortableFieldEmitInfo;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

/// Emit `{Model}Fields` (and the SQL-only sibling `{Model}SqlFields`)
/// with one inherent accessor per column.
///
/// `struct_item` is the post-injection struct: its `fields` list already has
/// the framework-injected columns (`id` / `created_at` / `updated_at`) at the
/// front in the same order `descriptor::expand` relies on. The `model_attrs`
/// are consulted solely to type the `id` accessor — the per-field methods
/// otherwise read the Rust type verbatim from the struct.
///
/// `portable_field_info` is the shared metadata vector built by
/// [`crate::model::portable_field_emit::build`] in `mod.rs`. PR2d threads it
/// through here so the per-column accessor emission shares column-name and
/// Rust-type computation with `crud::expand`'s portable predicate-arm
/// emission. Re-deriving column names locally would let the two diverge on
/// raw-identifier handling, JSONB wrapper detection, and `Option<U>` inner
/// recovery — the metadata is the single source of truth.
///
/// `{Model}Filter` is emitted separately by [`crate::model::filter::expand`]
/// — keeping the two in different modules isolates their codegen surfaces.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    portable_field_info: &[PortableFieldEmitInfo],
) -> TokenStream {
    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);
    let sql_fields_name = format_ident!("{}SqlFields", name);

    // ── `{Model}Fields` accessor emission ────────────────────────────────────
    //
    // `FieldRef<M, V>` is bounded `M: Model`. `crud::expand` does NOT emit
    // `impl Model` for `pk = None` models (the trait's `Pk: Encode` bound
    // can't be honestly satisfied without a real PK), so emitting accessor
    // methods here for those models would fail to compile with E0277 the
    // moment the user's struct is parsed — which breaks the contract
    // that pk=none models still get struct injection, `FromRow`, and
    // descriptor registration.
    //
    // Resolution: mirror `crud::expand`'s gate exactly. `pk = None` keeps
    // the Phase-1 empty-stub behavior; when the future phase introduces a
    // composite/user-managed PK trait, this branch can emit accessors keyed
    // on that trait instead.
    let accessor_impl: TokenStream = if matches!(model_attrs.pk, PkStrategy::None) {
        TokenStream::new()
    } else {
        // Walk the shared portable field metadata vector. It iterates in
        // the same order as `struct_item.fields` (framework fields first,
        // then user fields) so emission order matches the descriptor /
        // FromPgRow / column_list contract by construction. Reading the
        // ident, column name, and Rust type from the metadata avoids
        // re-deriving column-name conventions (raw-identifier stripping,
        // JSONB wrapper detection) locally — the same vector drives
        // `crud::expand`'s portable predicate arms.
        let accessors: Vec<TokenStream> = portable_field_info
            .iter()
            .map(|info| {
                let ident = &info.rust_ident;
                let column = info.column_name.as_str();
                let ty = &info.rust_type;
                quote! {
                    /// Typed handle for this column.
                    ///
                    /// Returns a [`FieldRef`] carrying the column name plus
                    /// phantom markers that bind it to this model and the
                    /// column's Rust type. Consume it via lookup methods
                    /// (`.eq`, `.gte`, `.contains`, …) to build a
                    /// [`Condition`] leaf.
                    ///
                    /// [`FieldRef`]: ::djogi::query::FieldRef
                    /// [`Condition`]: ::djogi::query::internal::Condition
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
            impl #fields_name {
                /// Construct a root-scope `Fields` handle with no SQL-alias
                /// path. Equivalent to the `Default` impl.
                #[doc(hidden)]
                #[inline]
                pub const fn new() -> Self {
                    Self { __djogi_path: ::core::option::Option::None }
                }

                /// Construct a traversal-scope `Fields` handle threaded
                /// with the given SQL-alias path. Used by visage traversal
                /// accessors when the relation form names the full peer
                /// model (e.g. `expose(public -> Department)`) — the
                /// peer's scalar accessors then produce `FieldRef`s whose
                /// column path is `"{prefix}.{col}"`.
                #[doc(hidden)]
                #[inline]
                pub const fn with_path(path: &'static str) -> Self {
                    Self { __djogi_path: ::core::option::Option::Some(path) }
                }

                #(#accessors)*
            }
        }
    };

    // ── `{Model}SqlFields` accessor emission (Phase 8eta PR2d) ───────────────
    //
    // The SQL-only sibling view — same accessor surface as `{Model}Fields`
    // but explicitly named to communicate "use this when you need a
    // path-aware SQL handle, not a portable predicate boundary." 8eta
    // designates `{Model}SqlFields` as the route for relation/visage
    // traversal because cached root rows don't carry joined relation
    // values — traversal predicates are SQL-only by construction.
    //
    // The two views share the per-field accessor list because they emit
    // the same column metadata; only the type/struct name differs. PR3
    // will diverge them when `{Model}Fields` flips to return
    // `DjogiField<Self, V>` (the portable-predicate wrapper);
    // `{Model}SqlFields` continues returning `FieldRef<Self, V>`.
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
                    ///
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
                /// with the given SQL-alias path. Used by visage traversal
                /// accessors that compose dotted column paths through
                /// relation chains.
                #[doc(hidden)]
                #[inline]
                pub const fn with_path(path: &'static str) -> Self {
                    Self { __djogi_path: ::core::option::Option::Some(path) }
                }

                #(#accessors)*
            }
        }
    };

    // Emit `search()` accessor when the model has an FTS spec.
    // The accessor returns `::djogi::fts_query::FtsFieldRef<#name>` with the
    // tsvector column name ("search") and dictionary baked in as `&'static str`s.
    let fts_accessor_impl: TokenStream = if let Some(fts) = &model_attrs.fts {
        let dictionary = &fts.dictionary;
        quote! {
            impl #fields_name {
                /// Typed handle for the full-text search column.
                ///
                /// Returns an [`FtsFieldRef`](::djogi::fts_query::FtsFieldRef) that
                /// exposes `.matches(q)` and `.rank(q)` for building `@@` and
                /// `ts_rank` predicates in filter closures and order expressions.
                ///
                /// The tsvector column is a `GENERATED ALWAYS AS` computed column —
                /// Postgres maintains it automatically on every INSERT / UPDATE, so
                /// application code never writes to it directly.
                ///
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

    // The `{Model}Fields` and `{Model}SqlFields` structs both carry an
    // optional SQL-alias path prefix so visage-scoped traversal chains
    // (`.department().name()`) compose into dot-qualified column names at
    // emission time. Default (`None`) means the handle works as a
    // plain-column accessor in `QuerySet::filter(|f| …)`.
    //
    // For `pk = None` models the structs are empty (no path slot); the
    // trait surface is suppressed anyway by the gate above.
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
                #[derive(Debug, Clone, Copy, Default)]
                pub struct #fields_name {
                    /// SQL-alias path prefix threaded through traversal
                    /// chains. `None` for the root-scope handle used by
                    /// `QuerySet::filter(|f| …)`; `Some("parent_fk_col")` on
                    /// handles produced by visage-scoped traversal accessors.
                    #[doc(hidden)]
                    pub __djogi_path: ::core::option::Option<&'static str>,
                }
            },
            quote! {
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
        /// Typed field accessors for QuerySet filter closures.
        ///
        /// Each inherent method returns a [`FieldRef`](::djogi::query::FieldRef)
        /// for one column; chain a lookup method to produce a
        /// [`Condition`](::djogi::query::internal::Condition).
        ///
        /// `Default` is required by the `Model::Fields` associated type
        /// (`Copy + Default + Send + Sync + 'static`) so that
        /// `QuerySet::filter(|f| …)` can construct the handle from
        /// inside the closure without the caller naming the type.
        ///
        /// The optional `__djogi_path` slot lets relation-traversal
        /// accessors embed this handle as a peer in a visage-scoped chain.
        #struct_decls

        /// SQL-only typed field accessors — the path-aware sibling of
        /// `{Model}Fields`.
        ///
        /// Phase 8eta PR2d emits this view alongside the existing
        /// `{Model}Fields` so traversal sites that want a SQL-only
        /// handle (relation walks, visage path composition) reach for
        /// `{Model}SqlFields` rather than the portable-predicate-bound
        /// surface. PR3 flips `{Model}Fields` accessors to the
        /// portable `DjogiField<Self, V>` wrapper; the SQL-only view
        /// preserved here will keep returning `FieldRef<Self, V>`.
        #sql_struct_decl

        #accessor_impl

        #sql_accessor_impl

        #fts_accessor_impl
    }
}
