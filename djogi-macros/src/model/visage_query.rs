//! Emit a per-visage queryset entry point and narrow `FromPgRow` impl.
//! For each visage `V` produced by [`super::visages::expand`], this
//! emitter generates:
//! 1. An `impl V { pub fn filter(...) -> VisageQuerySet<V>; ... }`
//!    block whose entry methods build a [`VisageQuerySet<V>`] over the
//!    source model's table with the visage's *narrowed* column list.
//! 2. An `impl FromPgRow for V` block that decodes a row positionally
//!    from the same narrowed column list, in the same order — so the
//!    SELECT projection and the row decoder agree by construction.
//! # Why the entry methods live on the visage type, not on `QuerySet`
//! `QuerySet<T: Model>` carries a `T: Model` bound that visages cannot
//! satisfy (visages are projections, not tables). Re-using the model
//! queryset would force every helper on `QuerySet<T>` (`prefetch`,
//! `select_related`, JSONB / spatial / FTS emitters, …) to dispatch on
//! "is `T` a model or a visage" — an open-ended split that grows every
//! time a new visage-only feature lands. Sibling type
//! [`VisageQuerySet<V>`] keeps the read-only path narrow and the model
//! path unchanged.
//! # Read-only surface — no `bulk_create` / `save` / `delete`
//! The compile-time enforcement that visages reject writes falls out of
//! method absence: this emitter does not emit `bulk_create` / `save` /
//! `delete` on `V`, and `VisageQuerySet<V>` only exposes read terminals
//! (`fetch_all`, `fetch_one`, `first`, `count`, `exists`).
//! # Relation-embed visages are skipped
//! A visage that embeds a peer projection (`expose(scope -> Peer)` on a
//! relation field) does not map to a flat column list on the source
//! table — the SELECT would need a JOIN or a follow-up query. The
//! emitter falls back to a no-op for any visage whose field set contains
//! a relation entry; the visage struct still exists and the conversion
//! impls still work, but `V::filter(...)` is not emitted.
//! [`VisageQuerySet<V>`]: ::djogi::query::VisageQuerySet

use crate::model::attrs::{FieldAttrs, PkStrategy};
use crate::model::protected::PerScopeCodecEntry;
use crate::model::visage_ctx::{ScopeMembership, VisageEmitContext, classify_field_for_scope};
use crate::model::visages::projection_entries;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::Ident;

/// Emit the per-visage queryset entry block + the visage's narrow
/// `FromPgRow` impl.
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
    let source_name_str = source.to_string();

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

    // #231 — projection entries are the single source of
    // truth for the visage's SELECT column shape and FromPgRow ordinal
    // decode. The helper returns one tuple per ordinal position:
    // `(name_or_alias, is_derived, sql_fragment_or_column_name)`. The
    // helper returns an empty vec for relation-embed visages — bail
    // out so the visage just lacks a `::filter` entry (the visage
    // struct + From/TryFrom conversion still work elsewhere).
    let entries = projection_entries(ctx);
    if entries.is_empty() {
        // Either a relation-embed visage or nothing in scope. Look at
        // the user-field pairs to distinguish: relation-embed should
        // bail; an empty list of in-scope user fields is fine (the
        // framework columns alone still need a queryset).
        let any_relation_embed = user_field_pairs.iter().any(|(field, attrs)| {
            matches!(
                classify_field_for_scope(field, attrs, scope),
                ScopeMembership::RelationEmbed { .. }
            )
        });
        if any_relation_embed {
            return TokenStream::new();
        }
        // Otherwise: fall through with no entries; the queryset will
        // still emit `SELECT FROM table` which Postgres rejects but
        // also will never be reached since the empty case is rare and
        // primarily a `pk = None` shape (already gated above). Defer
        // for now and keep code simple — emit empty-entries no-op.
        return TokenStream::new();
    }

    // Render the column-only slice for FromPgRow positional decode.
    // For derived entries the position's column NAME on the wire is
    // the alias (rendered by `(<sql>) AS <alias>`); the decoder's
    // debug-build name guard compares `COLUMNS[i]` to the wire's
    // column name at position `i`, so the alias is what to emit.
    let columns: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();

    // PROJECTION_LIST — the full comma-joined SELECT-list string with
    // derived entries rendered as `(<sql>) AS <alias>`. This becomes
    // the queryset's `projection_list: &'static str`, spliced into
    // the SELECT slot at query time without a runtime walk.
    let mut projection_list = String::new();
    for (i, (name, is_derived, payload)) in entries.iter().enumerate() {
        if i > 0 {
            projection_list.push_str(", ");
        }
        if *is_derived {
            projection_list.push('(');
            projection_list.push_str(payload);
            projection_list.push_str(") AS ");
            projection_list.push_str(name);
        } else {
            projection_list.push_str(name);
        }
    }

    let columns_lit: Vec<TokenStream> = columns.iter().map(|c| quote! { #c }).collect();
    let projection_list_lit = &projection_list;
    let n_cols = columns.len();
    let fields_ident = format_ident!("{visage_ident}Fields");
    let scoped_derived: Vec<&crate::model::derived::DerivedAttr> = ctx.scope_derived().collect();
    let mut column_accessors: Vec<TokenStream> = Vec::new();

    for field in struct_item.fields.iter().take(n_framework) {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        let column = crate::syn_util::column_name_from_ident(fname);
        let ty = &field.ty;
        column_accessors.push(quote! {
            #[must_use]
            pub fn #fname() -> ::djogi::query::VisageColumn<#visage_ident, #ty> {
                ::djogi::query::VisageColumn::<#visage_ident, #ty>::__new_for_visage_column(
                    #column,
                    ::djogi::__private::visage_column_seal::TOKEN,
                )
            }
        });
    }

    for (field, attrs) in &user_field_pairs {
        let Some(fname) = field.ident.as_ref() else {
            continue;
        };
        if !matches!(
            classify_field_for_scope(field, attrs, scope),
            ScopeMembership::Scalar
        ) {
            continue;
        }
        if scoped_derived.iter().any(|d| d.name == *fname) {
            continue;
        }
        if lookup_per_scope_codec(attrs, scope).is_some() {
            continue;
        }
        if attrs
            .protected
            .as_ref()
            .is_some_and(|spec| spec.codec.is_some())
        {
            continue;
        }
        let column = crate::syn_util::column_name_from_ident(fname);
        let ty = &field.ty;
        column_accessors.push(quote! {
            #[must_use]
            pub fn #fname() -> ::djogi::query::VisageColumn<#visage_ident, #ty> {
                ::djogi::query::VisageColumn::<#visage_ident, #ty>::__new_for_visage_column(
                    #column,
                    ::djogi::__private::visage_column_seal::TOKEN,
                )
            }
        });
    }

    // #231 — derived entries decode via the same `decode_at`
    // helper as columns. The position carries the alias name on the
    // wire; the decoder's debug-build name guard compares
    // `COLUMNS[i]` against the wire column name, both of which equal
    // the alias. The Rust type comes from the derived entry's `ty`.

    // Per-column decode token: positional `try_get(i)`, with the same
    // debug-build name guard the model-side `FromPgRow` emitter uses.
    // Visages keep the framework columns at fixed ordinals (0/1/2) and
    // append the scope-included scalar user fields in declaration order
    // this list must match the visage struct's field order, which the
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
        let column = crate::syn_util::column_name_from_ident(fname);
        decode_assignments.push(emit_scalar_decode_assignment(
            fname,
            &field.ty,
            attrs,
            &source_name_str,
            scope,
            &column,
            idx,
        ));
        idx += 1;
    }

    // #231 — derived entries follow the scalar user columns
    // in the ordinal order. Each entry's `name` becomes the wire
    // column name (the SELECT alias) and the position decoder reads
    // it positionally as the entry's `ty`. The decode-name guard runs
    // against the alias, matching `(<sql>) AS <alias>` on the wire.
    for d in &scoped_derived {
        let raw_name = d.name.to_string();
        let alias = raw_name
            .strip_prefix("r#")
            .unwrap_or(raw_name.as_str())
            .to_string();
        let alias_lit = &alias;
        let fname = &d.name;
        let visage_name = visage_ident.to_string();
        decode_assignments.push(quote! {
            #fname: ::djogi::__private::pg::decode_derived_at::<_>(
                row,
                #idx,
                #visage_name,
                #alias_lit,
            )?
        });
        idx += 1;
    }

    quote! {
        impl #visage_ident {
            #(#column_accessors)*

            // Internal ctor — builds a fresh `VisageQuerySet` with the
            // visage's baked projection list and a vacuous root condition.
            // All public entry methods delegate here so the construction
            // path is written exactly once.
            // #231 — the queryset carries a rendered
            // `projection_list: &'static str` so derived entries'
            // `(<sql>) AS <alias>` fragments splice into the SELECT
            // slot without any runtime walk over `PROJECTIONS`. The
            // text-rendering happens once at macro time.
            #[inline]
            fn __new() -> ::djogi::query::VisageQuerySet<#visage_ident> {
                // Seed the source model's default filter so proxy visage querysets
                // respect the proxy's default_filter_condition, exactly as QuerySet::new()
                // does on the model side. Non-proxy models return None → always_true.
                let __djogi_default_condition =
                    <#source as ::djogi::prelude::Model>::default_filter_condition()
                        .map_or_else(
                            ::djogi::query::Q::<#source>::always_true,
                            ::djogi::query::Q::<#source>::Condition,
                        );
                ::djogi::query::VisageQuerySet::<#visage_ident>::new_for_visage(
                    <#source as ::djogi::prelude::Model>::table_name(),
                    #projection_list_lit,
                )
                .filter(__djogi_default_condition)
            }

            /// Build a [`VisageQuerySet`] over the source model's table
            /// with this visage's narrowed column projection, AND-ing
            /// the closure's returned predicate onto the queryset's root.
            /// The closure may return any
            /// [`IntoQ<Source>`](::djogi::query::IntoQ) payload: a legacy
            /// `Condition`, a portable or mixed predicate wrapper, or a
            /// pre-built `Q<Source>`.
            /// See also: [`QuerySet::filter`](::djogi::query::QuerySet::filter)
            /// [`VisageQuerySet`]: ::djogi::query::VisageQuerySet
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn filter<__DjogiF, __DjogiP>(
                predicate: __DjogiF,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident>
            where
                __DjogiF: ::core::ops::FnOnce(#fields_ident) -> __DjogiP,
                __DjogiP: ::djogi::query::IntoQ<#source>,
            {
                let __cond = predicate(<#fields_ident as ::core::default::Default>::default());
                Self::__new().filter(__cond)
            }

            /// Append an ordering expression to a fresh visage queryset.
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
            /// and the supplied predicate as the queryset's root.
            /// Used by macro-emitted reverse-FK and M2M visage accessors.
            /// The visage's baked-in `columns` slice ensures the emitted
            /// SELECT stays narrowed to the visage's exposed columns.
            /// `#[doc(hidden)]` — adopter code should reach the visage
            /// query surface through [`Self::filter`], [`Self::order_by`],
            /// [`Self::limit`], and [`Self::offset`].
            /// [`VisageQuerySet`]: ::djogi::query::VisageQuerySet
            #[doc(hidden)]
            #[must_use = "querysets are lazy — dropping one silently omits the query"]
            pub fn __filter_with_initial_condition<__DjogiP>(
                cond: __DjogiP,
            ) -> ::djogi::query::VisageQuerySet<#visage_ident>
            where
                __DjogiP: ::djogi::query::IntoQ<#source>,
            {
                Self::__new().filter(cond)
            }
        }

        impl ::djogi::__private::pg::FromPgRow for #visage_ident {
            // #231 — `COLUMNS` carries the alias at every
            // ordinal position (column name for column entries, derived
            // `name` for derived entries). The visage's
            // `FromPgRow::COLUMN_LIST` is the rendered `PROJECTION_LIST`,
            // which differs from `COLUMNS.join(", ")` once any derived
            // entry is present (the alias position renders as
            // `(<sql>) AS <alias>` in COLUMN_LIST, just `<alias>` in
            // COLUMNS). See `docs/spec/visage-derived-fields.md`
            // §"Column-list constants" for the rationale.
            const COLUMNS: &'static [&'static str] = &[ #(#columns_lit),* ];

            const COLUMN_LIST: &'static str = #projection_list_lit;

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

/// Emit the struct-init decode token for one scalar user column at ordinal
/// `idx`, applying any per-scope presentation codec after the storage value is
/// decoded.
fn emit_scalar_decode_assignment(
    fname: &Ident,
    fty: &syn::Type,
    attrs: &FieldAttrs,
    source_name: &str,
    scope: &str,
    column_name: &str,
    idx: usize,
) -> TokenStream {
    let Some(entry) = lookup_per_scope_codec(attrs, scope) else {
        return emit_decode_assignment(fname, column_name, idx);
    };

    let codec_ty = &entry.codec_type;
    let fname_str = fname.to_string();
    let scope_str = scope.to_string();
    let codec_type_name_for_err = codec_runtime_type_name_tokens(codec_ty);

    if entry.fallible {
        quote! {
            #fname: {
                let __djogi_storage_value: #fty =
                    ::djogi::__private::pg::decode_at::<#fty>(row, #idx, #column_name)?;
                <#codec_ty as ::djogi::presentation::TryPresentationCodec<#fty>>::try_present(
                    &__djogi_storage_value,
                )
                .map_err(|__djogi_codec_err| ::djogi::VisageError::PresentationCodec {
                    model: #source_name,
                    field: #fname_str,
                    scope: #scope_str,
                    codec: #codec_type_name_for_err,
                    source: ::std::boxed::Box::new(__djogi_codec_err),
                })?
            }
        }
    } else {
        quote! {
            #fname: {
                let __djogi_storage_value: #fty =
                    ::djogi::__private::pg::decode_at::<#fty>(row, #idx, #column_name)?;
                <#codec_ty as ::djogi::presentation::PresentationCodec<#fty>>::present(
                    &__djogi_storage_value,
                )
            }
        }
    }
}

/// Look up the per-scope presentation codec entry (if any) for the current
/// scope on a protected scalar field.
fn lookup_per_scope_codec<'a>(
    attrs: &'a FieldAttrs,
    scope: &str,
) -> Option<&'a PerScopeCodecEntry> {
    attrs
        .protected
        .as_ref()
        .and_then(|spec| spec.per_scope.iter().find(|entry| entry.scope == scope))
}

/// Emit `::std::any::type_name::<CodecTy>()` for a codec type path.
fn codec_runtime_type_name_tokens(path: &syn::Path) -> TokenStream {
    quote! { ::std::any::type_name::<#path>() }
}
