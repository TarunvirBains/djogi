//! Emit a per-visage queryset entry point and narrow `FromPgRow` impl.
//!
//! For each visage `V` produced by [`super::visages::expand`], this
//! emitter generates:
//!
//! 1. An `impl V { pub fn filter(...) -> VisageQuerySet<V>; ... }`
//!    block whose entry methods build a [`VisageQuerySet<V>`] over the
//!    source model's table with the visage's *narrowed* column list.
//! 2. An `impl FromPgRow for V` block that decodes a row positionally
//!    from the same narrowed column list, in the same order — so the
//!    SELECT projection and the row decoder agree by construction.
//!
//! # Why the entry methods live on the visage type, not on `QuerySet`
//!
//! `QuerySet<T: Model>` carries a `T: Model` bound that visages cannot
//! satisfy (visages are projections, not tables). Re-using the model
//! queryset would force every helper on `QuerySet<T>` (`prefetch`,
//! `select_related`, JSONB / spatial / FTS emitters, …) to dispatch on
//! "is `T` a model or a visage" — an open-ended split that grows every
//! time a new visage-only feature lands. Sibling type
//! [`VisageQuerySet<V>`] keeps the read-only path narrow and the model
//! path unchanged.
//!
//! # Read-only surface — no `bulk_create` / `save` / `delete`
//!
//! The compile-time enforcement that visages reject writes falls out of
//! method absence: this emitter does not emit `bulk_create` / `save` /
//! `delete` on `V`, and `VisageQuerySet<V>` only exposes read terminals
//! (`fetch_all`, `fetch_one`, `first`, `count`, `exists`).
//!
//! # Relation-embed visages are skipped
//!
//! A visage that embeds a peer projection (`expose(scope -> Peer)` on a
//! relation field) does not map to a flat column list on the source
//! table — the SELECT would need a JOIN or a follow-up query. The
//! emitter falls back to a no-op for any visage whose field set contains
//! a relation entry; the visage struct still exists and the conversion
//! impls still work, but `V::filter(...)` is not emitted.
//!
//! [`VisageQuerySet<V>`]: ::djogi::query::VisageQuerySet

use crate::model::attrs::PkStrategy;
use crate::model::visage_ctx::{ScopeMembership, VisageEmitContext, classify_field_for_scope};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Ident;

/// Emit the per-visage queryset entry block + the visage's narrow
/// `FromPgRow` impl.
///
/// The caller (`visages.rs::emit_projection_for_scope`) passes the
/// already-classified scope plus the source struct's user-field
/// attribute list. This emitter mirrors the same scope gate the visage
/// struct emitter uses so the column list it bakes matches the visage
/// struct's field order exactly.
pub fn expand(ctx: &VisageEmitContext<'_>) -> TokenStream {
    let source = ctx.source;
    let visage_ident = &ctx.visage_ident;
    let scope = ctx.scope;
    let struct_item = ctx.struct_item;
    let field_attrs = ctx.field_attrs;
    let model_attrs = ctx.model_attrs;
    let n_framework = ctx.n_framework;

    // `pk = None` models do not impl `Model`; their visages have no
    // queryset entry to wire because `Model::table_name()` is the
    // source of truth for the SQL table and is not available. The
    // visage struct + conversion impls still emit through the other
    // pipeline; this emitter just elides its block.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return TokenStream::new();
    }

    let user_field_pairs: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    // Build the narrowed column list. Order matches the visage struct's
    // field order (framework first, then scope-included user fields in
    // declaration order). Skip relation-entry visages — the SELECT
    // projection cannot represent an embedded peer as a flat column,
    // see module docs.
    let mut columns: Vec<String> = Vec::new();
    columns.push("id".to_string());
    columns.push("created_at".to_string());
    columns.push("updated_at".to_string());

    for (field, attrs) in &user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };

        match classify_field_for_scope(field, attrs, scope) {
            ScopeMembership::Absent => continue,

            // Scalar form on scalar field — flatten into the column list.
            ScopeMembership::Scalar => {
                let raw = fname.to_string();
                let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
                columns.push(column);
            }

            // Relation entry on a visage — this emitter does not yet
            // handle peer-embedding projections. Bail out for the whole
            // visage; the struct + conversion impls (emitted elsewhere)
            // still work, the visage just lacks a `::filter` entry.
            ScopeMembership::RelationEmbed { .. } => {
                return TokenStream::new();
            }

            // Shape-mismatched cases are rejected with a span-precise error
            // by `visages.rs::emit_projection_for_scope` before we run.
            // Treat as no-op here so we don't double-emit a diagnostic.
            ScopeMembership::Reject { .. } => return TokenStream::new(),
        }
    }

    let columns_lit: Vec<TokenStream> = columns.iter().map(|c| quote! { #c }).collect();
    let column_list_str: String = columns.join(", ");
    let n_cols = columns.len();
    let fields_ident = format_ident!("{visage_ident}Fields");

    // Per-column decode token: positional `try_get(i)`, with the same
    // debug-build name guard the model-side `FromPgRow` emitter uses.
    // Visages keep the framework columns at fixed ordinals (0/1/2) and
    // append the scope-included scalar user fields in declaration order
    // — this list must match the visage struct's field order, which the
    // sibling `visages::emit_projection_for_scope` builds via the same
    // scope gate.
    let mut decode_assignments: Vec<TokenStream> = Vec::new();
    let mut idx: usize = 0;

    // Framework decode — `id` / `created_at` / `updated_at`.
    decode_assignments.push(emit_decode_assignment(&format_ident!("id"), "id", idx));
    idx += 1;
    decode_assignments.push(emit_decode_assignment(
        &format_ident!("created_at"),
        "created_at",
        idx,
    ));
    idx += 1;
    decode_assignments.push(emit_decode_assignment(
        &format_ident!("updated_at"),
        "updated_at",
        idx,
    ));
    idx += 1;

    for (field, attrs) in &user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        // Only scalar hits reach here — relation/mismatched cases were
        // filtered above in the column-building pass.
        if !matches!(
            classify_field_for_scope(field, attrs, scope),
            ScopeMembership::Scalar
        ) {
            continue;
        }
        let raw = fname.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
        decode_assignments.push(emit_decode_assignment(fname, &column, idx));
        idx += 1;
    }

    quote! {
        impl #visage_ident {
            // Internal ctor — builds a fresh `VisageQuerySet` with the
            // visage's baked column list and a vacuous root condition.
            // All public entry methods delegate here so the construction
            // path is written exactly once.
            #[inline]
            fn __new() -> ::djogi::query::VisageQuerySet<#visage_ident> {
                const COLS: &[&'static str] = &[ #(#columns_lit),* ];
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    COLS,
                    ::djogi::Condition::True,
                )
            }

            /// Build a [`VisageQuerySet`] over the source model's table
            /// with this visage's narrowed column projection, AND-ing
            /// the closure's returned condition onto the queryset's root.
            ///
            /// See also: [`QuerySet::filter`](::djogi::query::QuerySet::filter)
            ///
            /// [`VisageQuerySet`]: ::djogi::query::VisageQuerySet
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn filter<__DjogiF>(
                predicate: __DjogiF,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident>
            where
                __DjogiF: ::core::ops::FnOnce(#fields_ident) -> ::djogi::Condition,
            {
                let __cond = predicate(<#fields_ident as ::core::default::Default>::default());
                Self::__new().filter(__cond)
            }

            /// Append an ordering expression to a fresh visage queryset.
            ///
            /// Equivalent to `V::filter(|_| Condition::True).order_by(...)`.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn order_by<__DjogiF, __DjogiO>(
                f: __DjogiF,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident>
            where
                __DjogiF: ::core::ops::FnOnce(#fields_ident) -> __DjogiO,
                __DjogiO: ::core::convert::Into<::std::vec::Vec<::djogi::OrderExpr>>,
            {
                let __exprs = f(<#fields_ident as ::core::default::Default>::default()).into();
                Self::__new().order_by(__exprs)
            }

            /// Apply `LIMIT n` to a fresh visage queryset.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn limit(n: u64) -> ::djogi::query::VisageQuerySet<#visage_ident> {
                Self::__new().limit(n)
            }

            /// Apply `OFFSET n` to a fresh visage queryset.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn offset(n: u64) -> ::djogi::query::VisageQuerySet<#visage_ident> {
                Self::__new().offset(n)
            }

            /// Internal seam — build a [`VisageQuerySet`] over the source
            /// model's table with this visage's narrowed column projection
            /// and the supplied `Condition` as the queryset's root.
            ///
            /// Used by macro-emitted reverse-FK and M2M visage accessors.
            /// The visage's baked-in `columns` slice ensures the emitted
            /// SELECT stays narrowed to the visage's exposed columns.
            ///
            /// `#[doc(hidden)]` — adopter code should reach the visage
            /// query surface through [`Self::filter`], [`Self::order_by`],
            /// [`Self::limit`], and [`Self::offset`].
            ///
            /// [`VisageQuerySet`]: ::djogi::query::VisageQuerySet
            #[doc(hidden)]
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn __filter_with_initial_condition(
                cond: ::djogi::Condition,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident> {
                Self::__new().filter(cond)
            }
        }

        impl ::djogi::__private::pg::FromPgRow for #visage_ident {
            const COLUMNS: &'static [&'static str] = &[ #(#columns_lit),* ];

            const COLUMN_LIST: &'static str = #column_list_str;

            fn from_pg_row(
                row: &::djogi::__private::tokio_postgres::Row,
            ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                ::std::debug_assert!(
                    ::djogi::__private::tokio_postgres::Row::columns(row).len() >= #n_cols,
                    "FromPgRow column-count mismatch on visage: expected at least {}, got {}",
                    #n_cols,
                    ::djogi::__private::tokio_postgres::Row::columns(row).len(),
                );
                ::std::result::Result::Ok(Self {
                    #(#decode_assignments,)*
                })
            }
        }
    }
}

/// Emit the struct-init decode token for one column at ordinal `idx`.
/// The `fname` is the Rust field ident (may be a raw identifier
/// `r#foo`); `column_name` is the SQL column literal (raw-prefix stripped).
/// Delegates to the shared `decode_at` runtime helper that holds the
/// debug-build name guard and the error mapping.
fn emit_decode_assignment(fname: &Ident, column_name: &str, idx: usize) -> TokenStream {
    quote! {
        #fname: ::djogi::__private::pg::decode_at::<_>(row, #idx, #column_name)?
    }
}
