//! Phase 7-Zero-2 T10 — emit a per-visage queryset entry point.
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
//! # Relation-embed visages are skipped today
//!
//! A visage that embeds a peer projection (`expose(scope -> Peer)` on a
//! relation field) does not map to a flat column list on the source
//! table — the SELECT would need a JOIN (full-peer or narrow visage) or
//! a follow-up query (prefetch). Routing that lift through the queryset
//! emitter is significantly larger work than this task and the user-
//! visible test surface (`UserPublic::filter(...)`) is a scalar-only
//! visage. The emitter therefore falls back to a no-op for any visage
//! whose field set contains a relation entry; the visage struct still
//! exists, the conversion impls still work, but `V::filter(...)` is not
//! emitted. A future task can lift this restriction.
//!
//! [`VisageQuerySet<V>`]: ::djogi::query::VisageQuerySet

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Ident, ItemStruct};

/// Emit the per-visage queryset entry block + the visage's narrow
/// `FromPgRow` impl.
///
/// The caller (`visages.rs::emit_projection_for_scope`) passes the
/// already-classified scope plus the source struct's user-field
/// attribute list. This emitter mirrors the same scope gate the visage
/// struct emitter uses so the column list it bakes matches the visage
/// struct's field order exactly.
pub fn expand(
    source: &Ident,
    visage_ident: &Ident,
    scope: &str,
    struct_item: &ItemStruct,
    field_attrs: &[FieldAttrs],
    model_attrs: &ModelAttrs,
    n_framework: usize,
) -> TokenStream {
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

        if attrs.expose.suppressed {
            continue;
        }

        let scalar_hit = attrs.expose.scalar_scopes.contains(scope);
        let relation_hit = attrs.expose.relation_scopes.get(scope);
        let rel_info = detect_relation(&field.ty);
        let is_relation = rel_info.is_some();

        match (scalar_hit, relation_hit, is_relation) {
            (false, None, _) => continue,

            // Scalar form on scalar field — flatten into the column list.
            (true, None, false) => {
                let raw = fname.to_string();
                let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
                columns.push(column);
            }

            // Relation entry on a visage — this emitter does not yet
            // handle peer-embedding projections. Bail out for the whole
            // visage; the struct + conversion impls (emitted elsewhere)
            // still work, the visage just lacks a `::filter` entry.
            (false, Some(_), true) => {
                return TokenStream::new();
            }

            // Other shape-mismatched cases (scalar on relation, relation
            // on scalar, mixed) are rejected with a span-precise error
            // by `visages.rs::emit_projection_for_scope` before we run.
            // Treat as no-op here so we don't double-emit a diagnostic.
            _ => return TokenStream::new(),
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
    decode_assignments.push(framework_decode("id", idx));
    idx += 1;
    decode_assignments.push(framework_decode("created_at", idx));
    idx += 1;
    decode_assignments.push(framework_decode("updated_at", idx));
    idx += 1;

    for (field, attrs) in &user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        if attrs.expose.suppressed {
            continue;
        }
        let scalar_hit = attrs.expose.scalar_scopes.contains(scope);
        if !scalar_hit {
            continue;
        }
        // Relation/scalar mismatches were filtered above; only scalar
        // hits on scalar fields reach here.
        let raw = fname.to_string();
        let column = raw.strip_prefix("r#").unwrap_or(&raw).to_string();
        decode_assignments.push(scalar_decode(fname, &column, idx));
        idx += 1;
    }

    let _ = source;

    // Stash one column-list slice expression and reuse it across every
    // emitted entry method via interpolation. Re-using `columns_lit`
    // directly inside multiple `#(...)*` blocks would consume the
    // iterator on the first use — bake the slice into a single
    // expression and clone the wrapping `TokenStream` instead.
    let columns_slice = quote! { &[ #(#columns_lit),* ] };
    let cs_filter = columns_slice.clone();
    let cs_order = columns_slice.clone();
    let cs_limit = columns_slice.clone();
    let cs_offset = columns_slice.clone();
    let cs_const = columns_slice.clone();

    quote! {
        impl #visage_ident {
            /// Build a [`VisageQuerySet`] over the source model's table
            /// with this visage's narrowed column projection, AND-ing
            /// the closure's returned condition onto the queryset's
            /// (vacuously-true) root.
            ///
            /// The closure receives the visage's path-aware
            /// `Fields` handle (a stateless ZST at the root) so column
            /// references compose with the visage's exposed-only
            /// surface.
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
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    #cs_filter,
                    __cond,
                )
            }

            /// Append an ordering expression to a fresh visage queryset.
            ///
            /// Equivalent to `V::filter(|_| Condition::True).order_by(...)`,
            /// shaped as a top-level entry so visage call sites that
            /// only care about ordering (e.g. listing pages) can skip
            /// the dummy filter closure.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn order_by<__DjogiF, __DjogiO>(
                f: __DjogiF,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident>
            where
                __DjogiF: ::core::ops::FnOnce(#fields_ident) -> __DjogiO,
                __DjogiO: ::core::convert::Into<::std::vec::Vec<::djogi::OrderExpr>>,
            {
                let __exprs = f(<#fields_ident as ::core::default::Default>::default()).into();
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    #cs_order,
                    ::djogi::Condition::True,
                )
                .order_by(__exprs)
            }

            /// Apply `LIMIT n` to a fresh visage queryset.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn limit(n: u64) -> ::djogi::query::VisageQuerySet<#visage_ident> {
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    #cs_limit,
                    ::djogi::Condition::True,
                )
                .limit(n)
            }

            /// Apply `OFFSET n` to a fresh visage queryset.
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn offset(n: u64) -> ::djogi::query::VisageQuerySet<#visage_ident> {
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    #cs_offset,
                    ::djogi::Condition::True,
                )
                .offset(n)
            }
        }

        impl ::djogi::__private::pg::FromPgRow for #visage_ident {
            const COLUMNS: &'static [&'static str] = #cs_const;

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

/// Emit the decode token for a framework field at ordinal `idx`. The
/// `field_name` is the Rust field name on the visage struct (matches
/// the SQL column name 1:1 — `id` / `created_at` / `updated_at`).
fn framework_decode(field_name: &'static str, idx: usize) -> TokenStream {
    let fname = format_ident!("{}", field_name);
    quote! {
        #fname: {
            ::std::debug_assert_eq!(
                ::djogi::__private::tokio_postgres::Row::columns(row)[#idx].name(),
                #field_name,
                "FromPgRow column-order drift on visage: position {} expected {:?}, got {:?}",
                #idx,
                #field_name,
                ::djogi::__private::tokio_postgres::Row::columns(row)[#idx].name(),
            );
            ::djogi::__private::tokio_postgres::Row::try_get::<_, _>(row, #idx)
                .map_err(|e| ::djogi::DjogiError::Decode(
                    ::std::format!("column `{}`: {}", #field_name, e)
                ))?
        }
    }
}

/// Emit the decode token for a scalar user field. `fname` is the Rust
/// ident (may be a raw identifier `r#type`); `column_name` is the SQL
/// column literal (raw-prefix stripped) that the debug guard checks
/// against the wire column header.
fn scalar_decode(fname: &Ident, column_name: &str, idx: usize) -> TokenStream {
    quote! {
        #fname: {
            ::std::debug_assert_eq!(
                ::djogi::__private::tokio_postgres::Row::columns(row)[#idx].name(),
                #column_name,
                "FromPgRow column-order drift on visage: position {} expected {:?}, got {:?}",
                #idx,
                #column_name,
                ::djogi::__private::tokio_postgres::Row::columns(row)[#idx].name(),
            );
            ::djogi::__private::tokio_postgres::Row::try_get::<_, _>(row, #idx)
                .map_err(|e| ::djogi::DjogiError::Decode(
                    ::std::format!("column `{}`: {}", #column_name, e)
                ))?
        }
    }
}
