//! Generates `impl djogi::model::Model for T` — the five CRUD methods.
//!
//! # What
//!
//! For every `#[model]`-annotated struct this module emits:
//!
//! ```ignore
//! impl ::djogi::model::Model for Post {
//!     type Pk = ::djogi::types::HeerId;
//!
//!     fn table_name() -> &'static str { "posts" }
//!     fn pk_value(&self) -> &Self::Pk { &self.id }
//!     fn descriptor() -> &'static ::djogi::ModelDescriptor { ... }
//!
//!     fn get(ctx, id) -> impl Future<Output = Result<Self, DjogiError>> + Send { ... }
//!     fn create(ctx, value) -> impl Future<Output = Result<Self, DjogiError>> + Send { ... }
//!     fn save<'ctx>(&'ctx mut self, ctx: &'ctx mut DjogiContext)
//!         -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx { ... }
//!     fn delete(self, ctx) -> impl Future<Output = Result<(), DjogiError>> + Send { ... }
//!     fn refresh_from_db<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
//!         -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx { ... }
//! }
//! ```
//!
//! # Why `&mut DjogiContext`
//!
//! Phase 4 v3's Q1 resolution settled on "each method takes `&mut DjogiContext`".
//! The context carries either a pool or an active transaction and pattern-matches
//! on the variant at each query dispatch boundary in the generated body (via
//! `::djogi::context::DjogiContext::inner_mut`). This unifies the call site —
//! the same `Post::create(&mut ctx, post)` works whether `ctx` is pool-backed
//! or inside an `atomic()` transaction scope.
//!
//! # Why the `'ctx` lifetime on `save` / `refresh_from_db`
//!
//! `save` and `refresh_from_db` take `&self` and `&mut DjogiContext`. Both
//! borrows must outlive the returned future (RPITIT elision), so the method
//! introduces an explicit `'ctx` and ties both receivers plus the returned
//! future to it. `get`, `create`, and `delete` consume their `Self` / receive
//! by value, so they don't need the lifetime annotation — the returned future
//! only borrows from `ctx`, whose lifetime is inferred.
//!
//! # Why `+ Send` on the Future
//!
//! The returned `Future` carries `+ Send` explicitly because tokio's
//! multi-threaded runtime requires futures to be `Send` across `.await` points.
//! `&mut DjogiContext` is `Send` because the context only holds `Send` data.
//!
//! # Path routing through `::djogi::__private`
//!
//! Macro-generated code runs in the user's crate, which only has `djogi` as a
//! direct dependency. Direct paths like `::tokio_postgres::...` or
//! `::inventory::iter` would fail with E0433 unless the user explicitly added
//! those crates. To avoid that, all external crate references are routed through
//! `::djogi::__private::pg`, `::djogi::__private::inventory`, and
//! `::djogi::types`. This is the same convention established in Task 5
//! (`from_row.rs`) and Task 6 (`descriptor.rs`).
//!
//! # `inventory::iter` — no parentheses
//!
//! `::djogi::__private::inventory::iter::<T>` is a zero-sized type that
//! implements `IntoIterator`. It is NOT a function — calling it with `()` is a
//! type error. Use `.into_iter()` on the ZST directly, which Task 6 and the
//! Task 6 integration test already validate.
//!
//! # SQL conventions
//!
//! - Column name == Rust field name (snake_case). This matches the injection
//!   convention in `inject.rs` and the `FromRow` impl in `from_row.rs`.
//! - `create` omits `id`, `created_at`, and `updated_at` from the `INSERT`
//!   columns — the Postgres defaults (`heerid_next()`, `now()`) populate them.
//!   `RETURNING *` sends the full row back so the returned `Self` has all
//!   fields populated from the database.
//! - `save` sets all user fields plus `updated_at = now()`. Only user fields
//!   are written — `id` and `created_at` are immutable after creation.
//! - `delete` consumes `self` to prevent accidental use of a stale handle.
//! - `save` and `refresh_from_db` take `&self` and borrow `self` directly
//!   across the async block — Rust 2024 RPITIT captures `&self`'s lifetime
//!   into the returned future, so no clone-capture is needed. `Model: Send
//!   + Sync` → `&Self: Send`, which keeps the returned future Send-bound.
//!
//! # `pk = None` special case
//!
//! Models with `#[model(pk = None)]` have no framework-injected `id` field
//! and declare their own primary key (possibly composite). Phase 1 does NOT
//! emit `impl Model for T` for these — the `Model` trait's `type Pk` requires
//! `postgres_types::ToSql`, which `()` does not implement, and choosing
//! any other dummy type would misrepresent the model's actual key shape.
//!
//! Everything else (struct injection, `Default` impl, `FromRow`, descriptor
//! registration, Fields/Filter stubs) is still emitted for pk=none models,
//! so users can serialize, use struct-update syntax, and iterate descriptors.
//! They just can't call `::create`, `::get`, `.save()`, `.delete()`, or
//! `.refresh_from_db()`.
//!
//! A future phase will introduce a separate trait or code path for
//! composite/user-managed PK models. Phase 1 deliberately excludes them
//! from `impl Model` rather than shipping a shim that lies about the key.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ItemStruct;

use super::attrs::{FieldAttrs, ModelAttrs, PkStrategy};
use super::portable_field_emit::{PortableFieldEmitInfo, PortableFieldKind};
use super::sql_bind::{
    bind_kind, create_param_tokens, is_nullable, is_tracked_inner, push_bind_tokens,
};

fn old_new_returning_alias(prefix: &str, idx: usize, col: &str) -> String {
    match prefix {
        "__djogi_old__" => format!("o{idx}"),
        "__djogi_new__" => format!("n{idx}"),
        _ => format!("{prefix}{col}"),
    }
}

fn build_old_new_returning_suffix(all_cols: &[&str], include_new: bool) -> String {
    let mut s = if include_new {
        String::from(" RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)")
    } else {
        String::from(" RETURNING WITH (OLD AS __djogi_old)")
    };
    let mut first = true;
    for (idx, col) in all_cols.iter().enumerate() {
        if first {
            s.push(' ');
            first = false;
        } else {
            s.push_str(", ");
        }
        s.push_str("__djogi_old.");
        s.push_str(col);
        s.push_str(" AS \"");
        s.push_str(&old_new_returning_alias("__djogi_old__", idx, col));
        s.push('"');
    }
    if include_new {
        for (idx, col) in all_cols.iter().enumerate() {
            s.push_str(", __djogi_new.");
            s.push_str(col);
            s.push_str(" AS \"");
            s.push_str(&old_new_returning_alias("__djogi_new__", idx, col));
            s.push('"');
        }
    }
    s
}

/// Generate the full `impl Model for T` block.
///
/// Called from `mod.rs` after `inject::expand` has mutated `struct_item`, so
/// the field list already includes `id`, `created_at`, and `updated_at` at the
/// front.
///
/// `portable_field_info` is the shared per-field metadata vector built by
/// [`super::portable_field_emit::build`] in `mod.rs`. It is threaded
/// through here so the emitted `Model::__djogi_emit_field_predicate`
/// override agrees with `stubs::expand`'s `{Model}Fields` /
/// `{Model}SqlFields` accessor emission on column names, declared Rust
/// types, and the portable-kind classification. Re-deriving any of those
/// facts in `crud.rs` would let the override and the accessors drift —
/// the metadata is the single source of truth.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
    portable_field_info: &[PortableFieldEmitInfo],
) -> TokenStream {
    // pk = None skips Model impl in Phase 1 — Task 8 adds a composite-PK-
    // aware version. The other macro outputs (struct, Default, FromRow,
    // descriptor, Fields/Filter stubs) are still emitted by other modules.
    if matches!(model_attrs.pk, PkStrategy::None) {
        return TokenStream::new();
    }

    // Phase 4 Task 6 — `#[model(table = "...", events)]` opts the model
    // into transactional outbox emission. We inline the outbox call at
    // macro-expansion time (not via a runtime `if has_outbox` branch)
    // so non-events models do not carry the `serde::Serialize` bound
    // implied by `emit_event`. Non-events emits nothing here — the
    // `if` gate on `events` compiles away entirely for those models.
    let emit_outbox_create = if model_attrs.events {
        quote! {
            ::djogi::outbox::emit_event(
                ctx,
                &row,
                ::djogi::outbox::OutboxAction::Create,
            ).await?;
        }
    } else {
        quote! {}
    };
    let emit_outbox_save = if model_attrs.events {
        quote! {
            ::djogi::outbox::emit_event(
                ctx,
                &*self,
                ::djogi::outbox::OutboxAction::Save,
            ).await?;
        }
    } else {
        quote! {}
    };
    let emit_outbox_delete = if model_attrs.events {
        quote! {
            ::djogi::outbox::emit_event(
                ctx,
                &self,
                ::djogi::outbox::OutboxAction::Delete,
            ).await?;
        }
    } else {
        quote! {}
    };

    let emit_outbox_returning_save = quote! {
        <Self as ::djogi::model::Model>::__djogi_emit_save_outbox(
            ctx,
            &__pair.new,
        )
        .await
    };

    let emit_outbox_returning_save_override = if model_attrs.events {
        quote! {
            fn __djogi_emit_save_outbox<'ctx>(
                ctx: &'ctx mut ::djogi::context::DjogiContext,
                row: &'ctx Self,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + ::std::marker::Send {
                async move {
                    ::djogi::outbox::emit_event(
                        ctx,
                        row,
                        ::djogi::outbox::OutboxAction::Save,
                    )
                    .await
                }
            }

            fn __djogi_emit_save_outbox_batch<'ctx>(
                ctx: &'ctx mut ::djogi::context::DjogiContext,
                rows: &'ctx [&'ctx Self],
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + ::std::marker::Send {
                async move { ::djogi::outbox::emit_save_events_batch(ctx, rows).await }
            }
        }
    } else {
        quote! {}
    };

    let emit_on_save_cache_invalidation_override = quote! {
        fn __djogi_enqueue_on_save_cache_invalidation<'ctx>(
            ctx: &'ctx mut ::djogi::context::DjogiContext,
            row: &'ctx Self,
        ) -> ::std::result::Result<(), ::djogi::DjogiError> {
            if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                let __id_for_cache = ::core::clone::Clone::clone(&row.id);
                ctx.on_commit(move || async move {
                    if let ::std::result::Result::Err(__e) = __punnu
                        .invalidate(
                            &__id_for_cache,
                            ::djogi::cache::InvalidationReason::OnSave,
                        )
                        .await
                    {
                        ::djogi::__private::tracing::warn!(
                            target: "djogi::cache",
                            error = ?__e,
                            model = ::std::any::type_name::<Self>(),
                            "Punnu::invalidate L2 backend failed during on_commit drain",
                        );
                    }
                    ::std::result::Result::Ok(())
                });
            }
            ::std::result::Result::Ok(())
        }
    };

    let emit_bulk_on_save_cache_invalidation_override = quote! {
        fn __djogi_should_collect_bulk_update_ids(
            ctx: &::djogi::context::DjogiContext,
        ) -> bool {
            ctx.__djogi_is_transaction_backed_for_macros()
                && ctx.punnu::<Self>().is_some()
        }

        fn __djogi_enqueue_bulk_on_save_cache_invalidation(
            ctx: &mut ::djogi::context::DjogiContext,
            ids: ::std::vec::Vec<Self::Pk>,
        ) -> ::std::result::Result<(), ::djogi::DjogiError> {
            if ids.is_empty() {
                return ::std::result::Result::Ok(());
            }

            if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                ctx.on_commit(move || async move {
                    for __id_for_cache in ids {
                        if let ::std::result::Result::Err(__e) = __punnu
                            .invalidate(
                                &__id_for_cache,
                                ::djogi::cache::InvalidationReason::OnSave,
                            )
                            .await
                        {
                            ::djogi::__private::tracing::warn!(
                                target: "djogi::cache",
                                error = ?__e,
                                model = ::std::any::type_name::<Self>(),
                                "Punnu::invalidate L2 backend failed during on_commit drain",
                            );
                        }
                    }
                    ::std::result::Result::Ok(())
                });
            }

            ::std::result::Result::Ok(())
        }
    };

    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();
    let table = &model_attrs.table;

    // -------------------------------------------------------------------------
    // Associated Pk type and pk_value() body — vary by PK strategy.
    // (pk = None is handled by the early return above.)
    // -------------------------------------------------------------------------
    let (pk_type_tokens, pk_value_body) = match &model_attrs.pk {
        PkStrategy::HeerId => (quote! { ::djogi::types::HeerId }, quote! { &self.id }),
        PkStrategy::RanjId => (quote! { ::djogi::types::RanjId }, quote! { &self.id }),
        PkStrategy::HeerIdDesc => (quote! { ::djogi::types::HeerIdDesc }, quote! { &self.id }),
        PkStrategy::RanjIdDesc => (quote! { ::djogi::types::RanjIdDesc }, quote! { &self.id }),
        PkStrategy::Serial => (quote! { i32 }, quote! { &self.id }),
        PkStrategy::None => unreachable!("handled by early return"),
        PkStrategy::Custom(path) => (quote! { #path }, quote! { &self.id }),
    };

    // -------------------------------------------------------------------------
    // User fields: all fields AFTER the framework-injected ones.
    // For HeerId/RanjId/Serial: id + created_at + updated_at = 3 to skip.
    // For None: created_at + updated_at = 2 to skip.
    // -------------------------------------------------------------------------
    // pk != "none" always injects 3 framework fields (id, created_at, updated_at).
    let n_framework = 3;
    let user_fields: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .filter_map(|f| f.ident.as_ref())
        .cloned()
        .collect();

    // Collect user field types in parallel with user_fields so we can identify
    // which fields are Tracked<T>. The i-th entry in user_field_types corresponds
    // to the i-th entry in user_fields.
    let user_field_types: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .map(|f| f.ty.clone())
        .collect();

    // Returns true when a `syn::Type` is `Tracked<…>` (or any fully-qualified
    // spelling: `djogi::Tracked<…>`, `::djogi::Tracked<…>`,
    // `djogi::prelude::Tracked<…>`). Detection is by last-segment ident only —
    // the same approach used for `ForeignKey` detection in `attrs.rs`.
    // A user-defined type literally named `Tracked` would shadow this; that
    // trade-off matches the existing ForeignKey convention and is acceptable.
    let is_tracked = |ty: &syn::Type| -> bool {
        let syn::Type::Path(syn::TypePath { path, .. }) = ty else {
            return false;
        };
        path.segments
            .last()
            .map(|seg| seg.ident == "Tracked")
            .unwrap_or(false)
    };

    let n_user = user_fields.len();

    // -------------------------------------------------------------------------
    // Canonical column list — the comma-joined sequence of column names in
    // struct-field order after injection. `id, created_at, updated_at,
    // <user_fields>`. Matches `FromPgRow::COLUMN_LIST` emitted by
    // `from_row.rs` byte-for-byte so the `SELECT {column_list}` and
    // `RETURNING {column_list}` SQL below is decoded positionally by
    // `FromPgRow::from_pg_row`.
    //
    // Replaces the historical `SELECT *` / `RETURNING *` spelling — that
    // shape leaked DDL column order into the decode path and would
    // mis-decode against migrations that happen to list user columns
    // before framework columns (Phase 4's `accounts` fixture is the
    // canonical example).
    //
    // Strips raw-identifier prefixes (`r#type` -> `type`) to match the
    // convention already used in `stubs.rs` / `descriptor.rs` /
    // `from_row.rs`.
    // -------------------------------------------------------------------------
    let framework_cols: [&str; 3] = ["id", "created_at", "updated_at"];
    let user_col_names: Vec<String> = user_fields
        .iter()
        .map(crate::syn_util::column_name_from_ident)
        .collect();
    let column_list: String = framework_cols
        .iter()
        .map(|s| (*s).to_string())
        .chain(user_col_names.iter().cloned())
        .collect::<Vec<_>>()
        .join(", ");

    // -------------------------------------------------------------------------
    // `create` SQL: INSERT with user fields only; DB handles the framework
    // columns via column defaults. `RETURNING {column_list}` brings back the
    // full row in canonical order for ordinal decode.
    //
    // For zero-user-field models we must use `DEFAULT VALUES` — empty parens
    // `()` are invalid SQL and `INSERT ... () VALUES ()` is rejected by
    // Postgres. `DEFAULT VALUES` is standard SQL and Postgres-supported.
    // -------------------------------------------------------------------------
    let insert_sql = if n_user == 0 {
        format!("INSERT INTO {table} DEFAULT VALUES RETURNING {column_list}")
    } else {
        let insert_columns: Vec<String> = user_fields.iter().map(|i| i.to_string()).collect();
        let insert_col_list = insert_columns.join(", ");
        let insert_placeholders: Vec<String> = (1..=n_user).map(|i| format!("${i}")).collect();
        let insert_placeholder_list = insert_placeholders.join(", ");
        format!(
            "INSERT INTO {table} ({insert_col_list}) VALUES ({insert_placeholder_list}) RETURNING {column_list}"
        )
    };
    // Create params: one &(dyn ToSql + Sync) per user field, in order.
    // Used to build the __params vec in the create body.
    //
    // For widened types (i8/u8 → i16, u16 → i32, u32 → i64, u64 → Decimal),
    // `create_param_tokens` returns a pre-declaration (`let __bind_N: WideType
    // = widen(value.field)`) and a slice entry (`&__bind_N as …`). The
    // declarations must appear before the slice literal so the borrows are
    // valid. For direct types, the pre-declaration is empty and the entry is
    // `&value.field as …`.
    let (create_param_pre_decls, create_param_entries): (Vec<TokenStream>, Vec<TokenStream>) =
        user_fields
            .iter()
            .zip(user_field_types.iter())
            .enumerate()
            .map(|(slot, (f, ty))| {
                let kind = bind_kind(ty);
                let nullable = is_nullable(ty);
                let tracked = is_tracked_inner(ty);
                let val_expr = quote! { value.#f };
                create_param_tokens(&kind, nullable, tracked, val_expr, slot)
            })
            .unzip();

    // -------------------------------------------------------------------------
    // `save` — dirty-aware SqlAccumulator-based SET emission (Task 2).
    //
    // The save() body now builds the SET list at runtime using SqlAccumulator
    // so it can conditionally include or skip Tracked<T> fields depending on
    // their `is_dirty()` flag. Non-Tracked fields are unconditional (always
    // emitted).
    //
    // Two shapes are emitted per user field at macro-expansion time:
    //
    // 1. Tracked field — runtime-conditional:
    //      if self.<field>.is_dirty() {
    //          if __first { __first = false; } else { __acc.push_sql(", "); }
    //          __acc.push_sql("<col> = ");
    //          __acc.push_bind((*self.<field>).clone());
    //      }
    //
    // 2. Non-Tracked field — always emitted (behavioral regression guard):
    //      if __first { __first = false; } else { __acc.push_sql(", "); }
    //      __acc.push_sql("<col> = ");
    //      __acc.push_bind(self.<field>.clone());
    //
    // After the user-field loop:
    //      if !__first { __acc.push_sql(", "); }   // comma if any user col emitted
    //      __acc.push_sql("updated_at = now()");   // always present
    //
    // If ALL Tracked fields are clean AND there are no non-Tracked fields,
    // the SQL is `UPDATE t SET updated_at = now() WHERE id = $1 RETURNING …`
    // — no leading comma, valid Postgres.
    //
    // Using Postgres `now()` (not a client-side `OffsetDateTime::now_utc()`
    // bound) keeps the timestamp source consistent with the column's
    // `DEFAULT now()` on INSERT: all writes use the same server clock, so
    // `created_at <= updated_at` always holds across clients with drifted clocks.
    // -------------------------------------------------------------------------

    // -------------------------------------------------------------------------
    // Task 3 — version field detection for optimistic locking.
    //
    // Find the single `#[field(version)]`-annotated user field (validated
    // to exist at most once, with type i32 or i64, by `mod.rs`'s
    // `validate_version_fields`). When present:
    //
    // - SET: append `{col} = {col} + 1` (unconditional, after user cols).
    //   The version field is EXCLUDED from the dirty-aware user-field loop
    //   below — it always gets its special `col + 1` fragment regardless of
    //   dirty state.
    // - WHERE: append `AND {col} = $n` binding the current in-memory value.
    //   This is the optimistic-lock predicate: if another writer bumped the
    //   version, the WHERE won't match and Postgres returns 0 rows.
    // - Zero rows → `DjogiError::LockConflict`.
    //
    // When absent (no version field): current behavior (no change).
    // -------------------------------------------------------------------------
    let version_field_info: Option<(syn::Ident, String)> =
        field_attrs.iter().enumerate().find_map(|(i, fa)| {
            if fa.version {
                let f = &user_fields[i];
                let col_str = crate::syn_util::column_name_from_ident(f);
                Some((f.clone(), col_str))
            } else {
                None
            }
        });

    // Build per-field token fragments for the runtime accumulator loop.
    // Version field is excluded here — it gets its own special SET fragment.
    let save_set_fragments: Vec<TokenStream> = user_fields
        .iter()
        .zip(user_field_types.iter())
        .zip(field_attrs.iter())
        .filter_map(|((f, ty), fa)| {
            // Version field has its own SET emission: `col = col + 1`.
            // Exclude it from the dirty-aware user-field loop.
            if fa.version {
                return None;
            }
            let col_str = crate::syn_util::column_name_from_ident(f);
            let col_eq = format!("{col_str} = ");
            let kind = bind_kind(ty);
            let nullable = is_nullable(ty);
            if is_tracked(ty) {
                // Tracked<T>: emit only when dirty.
                // `(*self.<f>).clone()` gives the inner `T` via Deref.
                // Pass tracked=false because we've already extracted T;
                // `push_bind_tokens` sees T, not Tracked<T>.
                let inner_expr = quote! { (*self.#f).clone() };
                let push_stmt = push_bind_tokens(&kind, nullable, false, inner_expr);
                Some(quote! {
                    if self.#f.is_dirty() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(#col_eq);
                        #push_stmt;
                    }
                })
            } else if is_tracked_inner(ty) {
                // Option<Tracked<T>>: emitted unconditionally on every save.
                //
                // `is_tracked(ty)` is false (the outermost type is `Option`,
                // not `Tracked`), but `is_tracked_inner(ty)` is true because
                // the inner type after Option-stripping is `Tracked<T>`.
                //
                // **Why unconditional**: checking `is_dirty()` on the inner
                // Tracked<T> misses two transitions that change the field value:
                //   1. None  → Some(clean_value)  — inner is not dirty, but the
                //      column must change from NULL to the new value.
                //   2. Some(_) → None             — the Option evaluates to None
                //      so `as_ref().map(|t| t.is_dirty()).unwrap_or(false)`
                //      returns false, but the column must be NULLed.
                // Emitting unconditionally is always correct at the cost of one
                // extra bind slot when neither transition has occurred. For full
                // dirty-tracking of optional fields, prefer `Tracked<Option<T>>`
                // (Tracked is the outer wrapper; it detects any assignment to the
                // field, including None ↔ Some transitions).
                //
                // Inner expr: `as_ref().map(|__t| (**__t).clone())` → `Option<T>`.
                // Two dereferences in `(**__t).clone()`:
                //   - First `*`: `&Tracked<T>` → `Tracked<T>` (reference deref)
                //   - Second `*`: `Tracked<T>` → `T` (via `Tracked: Deref<Target=T>`)
                // `push_bind_tokens` receives `Option<T>` (nullable=true,
                // tracked=false), correctly applying widening (if any).
                let inner_expr = quote! { self.#f.as_ref().map(|__t| (**__t).clone()) };
                let push_stmt = push_bind_tokens(&kind, nullable, false, inner_expr);
                Some(quote! {
                    {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(#col_eq);
                        #push_stmt;
                    }
                })
            } else {
                // Non-Tracked: unconditional — behavioral regression guard for
                // models that do not opt into dirty tracking.
                // `self.#f.clone()` may be T or Option<T>; tracked=false.
                let field_expr = quote! { self.#f.clone() };
                let push_stmt = push_bind_tokens(&kind, nullable, false, field_expr);
                Some(quote! {
                    {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(#col_eq);
                        #push_stmt;
                    }
                })
            }
        })
        .collect();

    // After save() rehydrates self via RETURNING, walk every Tracked field
    // and call mark_clean(). `Tracked::new(T)` already constructs with dirty=false
    // so this is defensive — but required by the Task 2 contract so that future
    // in-place rehydration changes cannot silently break the invariant.
    //
    // Two shapes:
    // - `Tracked<T>`: `self.#f.mark_clean()`
    // - `Option<Tracked<T>>`: `if let Some(ref mut __t) = self.#f { __t.mark_clean(); }`
    let mark_clean_fragments: Vec<TokenStream> = user_fields
        .iter()
        .zip(user_field_types.iter())
        .filter_map(|(f, ty)| {
            if is_tracked(ty) {
                Some(quote! { self.#f.mark_clean(); })
            } else if is_tracked_inner(ty) {
                // Option<Tracked<T>>: mark clean if Some.
                // `ref mut __t` borrows the inner Tracked<T> in place;
                // `mark_clean()` takes `&mut self`.
                Some(quote! {
                    if let ::std::option::Option::Some(ref mut __t) = self.#f {
                        __t.mark_clean();
                    }
                })
            } else {
                None
            }
        })
        .collect();

    // Static prefix for the save accumulator. We begin the SET clause body here.
    // The save accumulator prefix. The WHERE + RETURNING suffix is appended
    // dynamically in the save body once we know how many bind slots the SET
    // list consumed (i.e., `__acc.bind_count() + 1` gives the id's `$n`).
    let save_acc_prefix = format!("UPDATE {table} SET ");

    // -------------------------------------------------------------------------
    // `get` SQL: SELECT * WHERE id = $1. `id` comes in as an owned Self::Pk.
    // -------------------------------------------------------------------------
    let get_sql = format!("SELECT {column_list} FROM {table} WHERE id = $1");

    let id_param_for_get = match &model_attrs.pk {
        PkStrategy::HeerId
        | PkStrategy::RanjId
        | PkStrategy::HeerIdDesc
        | PkStrategy::RanjIdDesc
        | PkStrategy::Serial
        | PkStrategy::Custom(_) => {
            quote! { &id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `refresh_from_db` — same query as get, but binds `&self.id` directly.
    // Like save, RPITIT captures `&self` so no pre-capture clone is needed.
    // -------------------------------------------------------------------------
    let refresh_id_param = match &model_attrs.pk {
        PkStrategy::HeerId
        | PkStrategy::RanjId
        | PkStrategy::HeerIdDesc
        | PkStrategy::RanjIdDesc
        | PkStrategy::Serial
        | PkStrategy::Custom(_) => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `delete` SQL: DELETE WHERE id = $1. `self` is consumed (moved in).
    // -------------------------------------------------------------------------
    let delete_sql = format!("DELETE FROM {table} WHERE id = $1");

    let owned_pk_param = match &model_attrs.pk {
        PkStrategy::HeerId
        | PkStrategy::RanjId
        | PkStrategy::HeerIdDesc
        | PkStrategy::RanjIdDesc
        | PkStrategy::Serial
        | PkStrategy::Custom(_) => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `descriptor()` — looks up the ModelDescriptor emitted by descriptor.rs.
    // `inventory::iter::<T>` is a ZST implementing IntoIterator — no parens.
    // -------------------------------------------------------------------------
    let descriptor_impl = quote! {
        fn descriptor() -> &'static ::djogi::ModelDescriptor {
            ::djogi::__private::inventory::iter::<::djogi::ModelDescriptor>
                .into_iter()
                .find(|d| d.table_name == #table)
                .expect("ModelDescriptor not registered — did #[model] run?")
        }
    };

    // -------------------------------------------------------------------------
    // Proxy default-filter / default-order overrides.
    //
    // For proxy models (`#[model(proxy_for = Parent, default_filter = |f| ...,
    // default_order = [(field, Asc|Desc), ...])]`), emit overrides for
    // `Model::default_filter_condition` and `Model::default_order_by` so
    // every freshly constructed `QuerySet<Self>` starts with the proxy's
    // state already AND-composed / appended.
    //
    // Non-proxy models emit nothing here — the trait's default impls
    // (`None` / empty `Vec`) inline to a no-op at every `QuerySet::new()`
    // call site. Zero-cost for the common case per the lens (`feedback_
    // decision_priorities.md`).
    //
    // The default-filter override threads the lowered SQL fragment from
    // T3.3 through `Condition::__from_raw_sql_fragment` — the
    // `#[doc(hidden)]` constructor that wraps the `pub(crate)`
    // `Condition::RawSql` variant. The fragment is `&'static str`,
    // baked at expand time, so no allocation runs at queryset
    // construction.
    //
    // The default-order override emits a `Vec::with_capacity(N)` followed
    // by `.push(OrderExpr::Column { ... })` per parsed `(field, Asc|Desc)`
    // tuple. NULL position defaults to `NullsOrder::Default` (matching
    // the queryset convention from `query/order.rs`).
    let proxy_default_filter_override = match &model_attrs.proxy_default_filter {
        Some(closure) => {
            let sql = match crate::model::proxy::lower_default_filter_to_sql(closure) {
                Ok(s) => s,
                Err(err) => return err.to_compile_error(),
            };
            quote! {
                fn default_filter_condition() -> ::std::option::Option<
                    ::djogi::query::internal::Condition,
                > {
                    ::std::option::Option::Some(
                        ::djogi::query::internal::Condition::__from_raw_sql_fragment(#sql),
                    )
                }
            }
        }
        None => quote! {},
    };
    let proxy_default_order_override = if model_attrs.proxy_default_order.is_empty() {
        quote! {}
    } else {
        let n = model_attrs.proxy_default_order.len();
        let pushes: Vec<TokenStream> = model_attrs
            .proxy_default_order
            .iter()
            .map(|(field_ident, dir)| {
                let column_lit = field_ident.to_string();
                let dir_tokens = match dir {
                    crate::model::proxy::OrderDir::Asc => {
                        quote! { ::djogi::query::Direction::Asc }
                    }
                    crate::model::proxy::OrderDir::Desc => {
                        quote! { ::djogi::query::Direction::Desc }
                    }
                };
                quote! {
                    // Use the `#[doc(hidden)]` constructor — the variant
                    // is `#[non_exhaustive]`, so downstream-crate literal
                    // construction is rejected. The constructor lives in
                    // the djogi crate where the variant is defined.
                    __out.push(::djogi::query::OrderExpr::__from_macro_column(
                        #column_lit,
                        #dir_tokens,
                        ::djogi::query::NullsOrder::Default,
                    ));
                }
            })
            .collect();
        quote! {
            fn default_order_by() -> ::std::vec::Vec<::djogi::query::OrderExpr> {
                let mut __out = ::std::vec::Vec::with_capacity(#n);
                #(#pushes)*
                __out
            }
        }
    };

    // -------------------------------------------------------------------------
    // Cluster 8δ T8.6 — `__delta_should_tombstone` override for soft-deletable
    // models.
    //
    // The delta-sync fetcher in `djogi::query::refresh` calls
    // `item.__delta_should_tombstone()` to decide whether to route a fetched
    // row to the tombstones set (evict from Punnu) or to the live-items list
    // (upsert into Punnu). The `Model` trait carries a default impl that
    // always returns `false`, so non-soft-deletable models pay zero overhead
    // (the vtable slot folds to a constant in practice).
    //
    // For `#[model(soft_deletable)]` models we emit an override that forwards
    // to `<Self as ::djogi::SoftDeletable>::deleted_at(self).is_some()`. The
    // override lives in the same `impl Model for Self` block emitted here (in
    // `crud.rs`) because:
    //
    // 1. This is the only place the full `impl Model for T` block is assembled
    //    — adding it in `soft_deletable.rs` would require a second `impl Model`
    //    block on the same type, which Rust rejects.
    // 2. `model_attrs.soft_deletable` is already read by T2.6 path-routing (see
    //    `soft_deletable::expand`) so reading it here adds zero new state.
    //
    // Path routing: `::djogi::SoftDeletable` follows the macro-path-routing
    // convention (`feedback_macro_path_routing.md`) — public re-export path,
    // no `__private` indirection needed because the trait is not sealed via a
    // private token.
    // -------------------------------------------------------------------------
    let delta_should_tombstone_override = if model_attrs.soft_deletable {
        quote! {
            #[doc(hidden)]
            fn __delta_should_tombstone(&self) -> bool {
                <Self as ::djogi::SoftDeletable>::deleted_at(self).is_some()
            }
        }
    } else {
        quote! {}
    };

    // -------------------------------------------------------------------------
    // `Model::__djogi_emit_field_predicate` override.
    //
    // Generates the `(field_name, LookupOp)` -> SQL emission dispatch
    // for every PK-backed model. The emitted body matches on
    // `(field.field_name(), field.op())`, dispatches to the hidden
    // `::djogi::__private::query::portable_emit::*` helpers for portable
    // field kinds, and falls through to typed `PortablePredicateError`
    // variants for anything else.
    //
    // # Why match `(field_name, op)` rather than `field_name` alone
    //
    // Each portable field has a small set of supported operators
    // (Eq/Neq/In/NotIn for every kind, plus ordering for scalars,
    // string-pattern arms for `String`, null-test arms for `Option<U>`).
    // Matching on the pair lets each arm bind the correct concrete
    // payload type — Sassi's `FieldPredicate::value` is type-erased as
    // `Arc<dyn Any + Send + Sync>` and the macro is the only place
    // that knows the user's declared Rust `V` type.
    //
    // # Why per-field wildcard arms are mandatory
    //
    // `LookupOp` is `#[non_exhaustive]` and the macro output expands
    // in adopter crates. An exhaustive match would force every
    // adopter to recompile when sassi adds a new `LookupOp` variant.
    // Each known portable field gets a `(field_name, op) =>
    // UnsupportedLookup { field, op }` catch-all so future operators
    // surface as typed errors rather than as downstream compilation
    // failures.
    //
    // # Final unknown-field arm
    //
    // After every known portable arm, a single `(name, _) =>
    // UnsupportedField { field: name }` arm catches anything else —
    // SQL-only fields, relation/visage paths, computed-FTS columns,
    // `Jsonb`/`Vec`/spatial wrappers, and user types whose portable
    // parity has not been validated. The portable cache/refresh
    // boundary already rejects upstream queries that would touch
    // these fields; the typed runtime error here is belt-and-braces
    // for any future macro path that ends up wrapping one in a
    // `DjogiField`.
    //
    // # Path routing
    //
    // The override spells Sassi inspection types through
    // `::djogi::types::*` (per `feedback_macro_path_routing.md`),
    // emit helpers through `::djogi::__private::query::portable_emit::*`,
    // and all error / context types through
    // `::djogi::__private::query::*` so the impl block compiles in
    // adopter crates that depend only on `djogi`.
    let emit_field_predicate_override = emit_djogi_emit_field_predicate(name, portable_field_info);

    // -------------------------------------------------------------------------
    // Auto-tenant wiring (Phase 5.5 Task 10 + Task 11).
    //
    // Emitted only for tenant-keyed models. When `ctx.auth()` carries a
    // `tenant_id`, this snippet calls `ctx.__ensure_tenant_set_for_macros`
    // (the public shim over `ensure_tenant_set`) before any SQL runs.
    //
    // Task 11 extends this: when `auth` is present but `tenant_id` is `None`,
    // a `tracing::warn!` fires (the "silent cross-tenant leak" footgun) unless
    // `ctx.__tenant_scope_suppressed_for_macros()` is `true`. Callers that
    // deliberately want cross-tenant queries call `ctx.with_no_tenant_scope()`
    // or `ctx.set_no_tenant_scope()` to suppress the warn.
    //
    // Borrow split: `ctx.auth()` borrows `ctx` immutably; we clone the
    // `String` before the immutable borrow drops so `ctx.ensure_tenant_set`
    // can take `&mut ctx` without a simultaneous immutable borrow. The
    // `__djogi_auth_present` bool also captures the `is_some()` result
    // before the clone so no second borrow is needed in the `None` arm.
    //
    // Path routing: bare `Option` paths are spelled `::std::option::Option::*`
    // per the `feedback_macro_path_routing` convention; temp bindings are
    // prefixed `__djogi_` to avoid colliding with user-chosen field names.
    //
    // Tracing path: `::tracing::warn!` is used bare here. The `tracing` crate
    // is a workspace dependency of `djogi` and is re-exported via
    // `::djogi::__private::tracing`. Macro-emitted code in user crates cannot
    // reach `::tracing` directly unless the user added it, but
    // `::djogi::__private::tracing` is always present. Match the existing
    // `_insecurely` warn pattern in `context_ext.rs` which uses bare
    // `tracing::warn!` inside the `djogi` crate. The macro emits
    // `::djogi::__private::tracing::warn!` to be safe across user crates.
    // -------------------------------------------------------------------------
    let auto_set_tenant = if model_attrs.tenant_key.is_some() {
        let model_name_str = table.as_str();
        quote! {
            // No auth attached → hands-off. The explicit-`set_tenant`
            // flow (Phase 5 tenancy pattern, admin tooling, test
            // harnesses) must not have its GUC clobbered by the auto-
            // wiring. Only auth-bound contexts participate.
            if ctx.auth().is_some() {
                let __djogi_tid: ::std::option::Option<::std::string::String> =
                    ctx.auth().and_then(|__djogi_auth| __djogi_auth.tenant_id.clone());
                match __djogi_tid {
                    ::std::option::Option::Some(__djogi_tid_str) => {
                        ctx.__ensure_tenant_set_for_macros(&__djogi_tid_str).await?;
                    }
                    ::std::option::Option::None => {
                        // Auth present but carries no tenant_id. Clear any
                        // previously auth-applied tenant scope so
                        // subsequent queries don't leak under the stale
                        // GUC (Phase 5.5 phase-boundary fixup — see
                        // djogi/src/query/terminal.rs auto_set_tenant for
                        // the full rationale).
                        if ctx.applied_tenant_id().is_some() {
                            ctx.clear_tenant().await?;
                        }
                        if !ctx.__tenant_scope_suppressed_for_macros() {
                            ::djogi::__private::tracing::warn!(
                                model = #model_name_str,
                                "auth attached but tenant_id is None on a tenant-keyed model; \
                                 queries will span tenants — call ctx.with_no_tenant_scope() to suppress",
                            );
                        }
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // Tenant-keyed + hooks paths need a rollback point immediately
    // before auto tenant application so pre-hook failures can restore
    // the original tenant/auth scope.
    let snapshot_auth_state_before_auto_set =
        if model_attrs.hooks && model_attrs.tenant_key.is_some() {
            quote! {
                let __djogi_auth_snapshot = ctx.__snapshot_auth_state_for_macros();
            }
        } else {
            TokenStream::new()
        };

    // -------------------------------------------------------------------------
    // Per-method async bodies.
    // -------------------------------------------------------------------------
    // Every body calls the public-but-hidden execution helpers on `ctx`
    // (`ctx.__query_opt_for_macros`, `ctx.__query_one_for_macros`, etc.) and
    // decodes rows via `FromPgRow::from_pg_row`. These helpers are
    // accessible from user crates (macro-generated code runs outside `djogi`)
    // even though the underlying `pub(crate)` methods are not. See
    // djogi/src/context.rs for the execution helper rationale.
    let get_body = quote! {
        async move {
            #auto_set_tenant
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #id_param_for_get,
            ];
            match ctx.__query_opt_for_macros(#get_sql, __params).await? {
                ::std::option::Option::Some(__row) => {
                    <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__row)
                }
                ::std::option::Option::None => {
                    ::std::result::Result::Err(::djogi::DjogiError::not_found(#table))
                }
            }
        }
    };

    // Phase 4 Task 7.6 — detect `#[field(sequence_within = "parent_col")]`.
    //
    // `field_attrs[i]` maps 1:1 to `user_fields[i]` because both skip
    // the framework columns at the front. At most one field may
    // declare the attribute today (multi-scope sequencing is future
    // work); a `compile_error!` token fires when users violate that.
    //
    // When set, the generated `create` body preempts the main INSERT
    // with a counter upsert against the companion table
    // `<table>_seq_<parent_col>` (hand-written by the caller today;
    // the DDL side-channel emission is DEFERRED per the Phase 4 Task
    // 6 outbox deferral pattern). The upsert returns the next seq
    // value which is then assigned to the sequence field on `value`
    // before the row INSERT emits. Rollback of the caller's
    // `atomic()` scope cleans both the counter increment and the
    // main row.
    //
    // The parent field must be shaped `ForeignKey<T>` where
    // `T::Pk = HeerId` — the macro binds
    // `value.<parent>.key().as_i64()` to a `BIGINT parent_id`
    // column. RanjId and Serial parents are a future extension
    // (companion-table shape + bind path must change in lockstep).
    let seq_within_fields: Vec<(usize, &str)> = field_attrs
        .iter()
        .enumerate()
        .filter_map(|(i, fa)| fa.sequence_within.as_deref().map(|s| (i, s)))
        .collect();
    // `value` must be mutable when any of the following participates:
    //   - `sequence_within` (assigns the counter back into the seq field)
    //   - `#[model(hooks)]` (`before_create(&mut value, ctx)`)
    //   - `#[model(auditable)]` (T2.4 — `value.__djogi_auditable_populate(ctx)`
    //     mutates `created_by` in place when auth is present and the field
    //     is currently None)
    // A single shared flag keeps the binding choice explicit.
    let value_must_be_mut =
        !seq_within_fields.is_empty() || model_attrs.hooks || model_attrs.auditable;
    let create_value_binding_default = if value_must_be_mut {
        quote! { let mut value = value; }
    } else {
        quote! { let value = value; }
    };
    let (sequence_compile_err, sequence_upsert_preamble, create_value_binding) =
        if seq_within_fields.len() > 1 {
            let msg = "models may declare #[field(sequence_within = ...)] on at most one field; \
                       multi-scope sequencing is a future extension";
            (
                quote! { ::std::compile_error!(#msg); },
                quote! {},
                create_value_binding_default.clone(),
            )
        } else if let Some(&(seq_idx, parent_col)) = seq_within_fields.first() {
            let seq_field_ident = &user_fields[seq_idx];
            let parent_col_ident = format_ident!("{}", parent_col);
            let seq_table = format!("{table}_seq_{parent_col}");
            let upsert_sql = format!(
                "INSERT INTO {seq_table} (parent_id, last_seq) VALUES ($1, 1) \
                 ON CONFLICT (parent_id) DO UPDATE SET last_seq = {seq_table}.last_seq + 1 \
                 RETURNING last_seq"
            );
            let preamble = quote! {
                // Counter upsert — same ctx as the main INSERT so a
                // rollback cleans the increment alongside the row.
                // `ForeignKey::key()` returns the parent's Pk
                // (HeerId for the supported case); `.as_i64()` binds
                // to the companion table's `parent_id BIGINT`
                // column. Uses the public-but-hidden ctx helper so
                // macro-emitted code can dispatch through either Pool
                // or Transaction variants without reaching into the
                // crate-private ContextInner.
                let __seq_parent_id: i64 = value.#parent_col_ident.key().as_i64();
                let __seq_params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] =
                    &[&__seq_parent_id];
                let __seq_row = ctx.__query_one_for_macros(#upsert_sql, __seq_params).await?;
                let __seq_val: i64 = ::djogi::__private::tokio_postgres::Row::try_get(
                    &__seq_row,
                    "last_seq",
                ).map_err(|e| ::djogi::DjogiError::Decode(
                    ::std::format!("sequence_within: failed to decode last_seq: {}", e)
                ))?;
                value.#seq_field_ident = __seq_val;
            };
            // `value` is already mutable via `create_value_binding_default`
            // (sequence_within forces it on through `value_must_be_mut`).
            (quote! {}, preamble, create_value_binding_default.clone())
        } else {
            (quote! {}, quote! {}, create_value_binding_default.clone())
        };

    // Hook dispatch wrapping the INSERT.
    //
    // When `#[model(hooks)]` is set, `before_create(&mut value, ctx)` runs
    // before the INSERT (may mutate the in-memory value or abort the whole
    // operation by returning Err) and `after_create(&row, ctx)` runs after
    // the outbox write (hook sequence:
    // before_create -> INSERT -> outbox -> after_create -> on_commit drain).
    //
    // For non-hooks models both branches collapse to empty `TokenStream`
    // (no `quote!` invocation) so opt-out paths emit zero codegen — T1.8
    // verifies this with `cargo asm`. The dispatch itself routes through
    // `::djogi::__private::hooks::ModelHooks` per the macro-path-routing
    // convention; the `HasHooks` impl emitted in T1.3 satisfies the bound
    // at the use site without any runtime branch.
    let (before_create_call, after_create_call) = if model_attrs.hooks {
        let before_create_call = if model_attrs.tenant_key.is_some() {
            quote! {
                if let ::std::result::Result::Err(__djogi_hook_err) =
                    <Self as ::djogi::__private::hooks::ModelHooks>::before_create(
                        &mut value,
                        ctx,
                    )
                    .await
                {
                    if let ::std::result::Result::Err(__djogi_restore_err) =
                        ctx.__restore_auth_state_for_macros(__djogi_auth_snapshot).await
                    {
                        return ::std::result::Result::Err(__djogi_restore_err);
                    }
                    return ::std::result::Result::Err(__djogi_hook_err);
                }
            }
        } else {
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::before_create(
                    &mut value,
                    ctx,
                ).await?;
            }
        };
        (
            before_create_call,
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::after_create(
                    &row,
                    ctx,
                ).await?;
            },
        )
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    // Composition populator for `#[model(auditable)]`.
    //
    // When the flag is set, the model emitter also produced an inherent
    // `__djogi_auditable_populate(&mut self, ctx: &mut DjogiContext)`
    // method (see `compose::auditable::expand`). Call it BEFORE the user
    // `before_create` hook so user code can inspect or override the
    // populated `created_by` value (spec line 990). The populator
    // contains an `if self.created_by.is_none()` guard so a user-set
    // value at construction time is never clobbered (spec line 1062).
    //
    // Models without `auditable` emit empty TokenStream — zero
    // dispatch overhead at the create path and the inherent method is
    // never emitted.
    let auditable_populate = if model_attrs.auditable {
        quote! {
            value.__djogi_auditable_populate(ctx);
        }
    } else {
        TokenStream::new()
    };

    let create_body = quote! {
        async move {
            #snapshot_auth_state_before_auto_set
            #auto_set_tenant
            #create_value_binding
            // Composition populator runs BEFORE the user
            // `before_create` hook so user hooks can inspect/override
            // the populated `created_by` value. Per spec line 1032 the
            // canonical sequence is:
            //   #auto_set_tenant
            //   #create_value_binding
            //   #auditable_populate     ← here (T2.4)
            //   #before_create_call     ← T1.4
            //   #sequence_upsert_preamble  ← T1 BLOCK-1 fix (982bee2)
            //   ... INSERT, outbox, after_create ...
            // Empty TokenStream when `#[model(auditable)]` is absent —
            // zero codegen for opt-out models.
            #auditable_populate
            // before_create fires before ANY DB write on
            // the create path (including the sequence_within counter upsert
            // below). Per the hook ordering contract "before -> DB -> outbox -> after":
            // a hook returning Err must leave the database untouched, so
            // the counter upsert MUST run after this point — otherwise
            // an aborted create would still increment the per-parent
            // counter, leaking sequence numbers on validation failure.
            // Returning Err short-circuits via `?` — no upsert, no
            // INSERT, no outbox row, surrounding atomic() rolls back
            // through standard error propagation.
            #before_create_call
            // Counter upsert (if `#[field(sequence_within = ...)]` is
            // declared) runs AFTER before_create so the hook may mutate
            // `value.<parent>` and have the upsert key off the updated
            // parent_id. Aborted hooks never reach this point.
            #sequence_upsert_preamble
            // Widened-type temporaries (empty for direct-mapped types).
            // Must be declared before the slice literal so the borrows live
            // long enough.
            #(#create_param_pre_decls)*
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #(#create_param_entries,)*
            ];
            let __raw_row = ctx.__query_one_for_macros(#insert_sql, __params).await?;
            let row = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw_row)?;
            // Phase 4 Task 6 — outbox emission (no-op for non-events models).
            // Runs in the same ctx so a transactional caller gets the
            // outbox row committed/rolled back atomically with `row`.
            #emit_outbox_create
            // after_create runs AFTER the outbox emission
            // per the hook sequence: "after_create … can read the just-inserted
            // row, can read the just-written outbox row." Order is load-
            // bearing.
            #after_create_call
            ::std::result::Result::Ok(row)
        }
    };

    // -------------------------------------------------------------------------
    // Task 3 — version-aware save body fragments.
    //
    // Two shapes depending on whether a version field exists:
    //
    // A. No version field (current behavior, preserved): after user-field
    //    fragments, append `updated_at = now()` and WHERE `id = $n`.
    //    Use `__query_one_for_macros` (errors on zero rows).
    //
    // B. Version field present: append `updated_at = now(), {ver_col} = {ver_col} + 1`
    //    and WHERE `id = $n AND {ver_col} = $m` binding the current in-memory
    //    version. Use `__query_opt_for_macros` and map `None` →
    //    `DjogiError::LockConflict`. `DjogiError::LockConflict` wraps a
    //    `DbError::other(...)` message-only error — no Postgres SQLSTATE
    //    comes from a zero-row UPDATE (Postgres returns 0 rows without an
    //    error code when the WHERE clause matches nothing).
    // -------------------------------------------------------------------------

    // Hook dispatch wrapping the UPDATE.
    //
    // When `#[model(hooks)]` is set, `before_save(self, ctx)` runs before
    // the UPDATE composes (may mutate `*self` or abort the whole operation
    // by returning Err) and `after_save(&*self, ctx)` runs after the
    // outbox emission AND after the `*self = row` rehydration so the hook
    // observes server-side defaults, triggers, and any DB-bumped column
    // values (hook sequence:
    // before_save -> UPDATE -> outbox -> after_save -> on_commit drain).
    //
    // Critical placement notes:
    // - `save()` works directly on `&mut self`; unlike `create()` there is
    //   no local `value` binding. Pass `self` (re-borrow `&mut self`) into
    //   `before_save` and `&*self` (immutable re-borrow after rehydration)
    //   into `after_save`.
    // - In Shape B (version-aware), `after_save` MUST be placed inside the
    //   `Some(__raw_row)` success branch — the `None` arm returns
    //   `LockConflict` early and `after_save` would observe stale state
    //   that was never written to the DB.
    //
    // For non-hooks models both branches collapse to empty `TokenStream`
    // (no `quote!` invocation) so opt-out paths emit zero codegen — T1.8
    // verifies this with `cargo asm`.
    let (before_save_call, after_save_call) = if model_attrs.hooks {
        let before_save_call = if model_attrs.tenant_key.is_some() {
            quote! {
                if let ::std::result::Result::Err(__djogi_hook_err) =
                    <Self as ::djogi::__private::hooks::ModelHooks>::before_save(
                        self,
                        ctx,
                    )
                    .await
                {
                    if let ::std::result::Result::Err(__djogi_restore_err) =
                        ctx.__restore_auth_state_for_macros(__djogi_auth_snapshot).await
                    {
                        return ::std::result::Result::Err(__djogi_restore_err);
                    }
                    return ::std::result::Result::Err(__djogi_hook_err);
                }
            }
        } else {
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::before_save(
                    self,
                    ctx,
                ).await?;
            }
        };
        (
            before_save_call,
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::after_save(
                    &*self,
                    ctx,
                ).await?;
            },
        )
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    let save_body = if let Some((ver_ident, ver_col)) = &version_field_info {
        // Shape B — version-aware save.
        let ver_set = format!(", {ver_col} = {ver_col} + 1");
        let ver_where = format!(" AND {ver_col} = ");
        let ver_conflict_msg =
            format!("optimistic lock conflict: {ver_col} mismatch in table {table}");
        quote! {
            async move {
                #snapshot_auth_state_before_auto_set
                #auto_set_tenant
                // before_save fires after auto_set_tenant
                // is in scope (so the hook can read tenant context) but
                // before the UPDATE composes its SET clause. Returning Err
                // short-circuits via `?` — no UPDATE, no outbox row,
                // surrounding atomic() rolls back via standard error
                // propagation.
                #before_save_call
                // Build the SET clause dynamically. Tracked<T> fields are only
                // included when dirty; non-Tracked fields are always included.
                // The version field is excluded from the dirty loop — it always
                // gets `{ver_col} = {ver_col} + 1` appended after updated_at.
                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#save_acc_prefix);
                {
                    let mut __first = true;
                    #(#save_set_fragments)*
                    // `updated_at = now()` always present. Comma if any user col fired.
                    if !__first { __acc.push_sql(", "); }
                    __acc.push_sql("updated_at = now()");
                    // Version counter — always incremented, not dirty-gated.
                    __acc.push_sql(#ver_set);
                }
                // WHERE id = $n AND {ver_col} = $m
                __acc.push_sql(" WHERE id = ");
                let __id_val = self.id.clone();
                __acc.push_bind(__id_val);
                // Bind current in-memory version for the optimistic-lock predicate.
                // If DB version != in-memory version, Postgres returns 0 rows.
                __acc.push_sql(#ver_where);
                let __ver_val = self.#ver_ident.clone();
                __acc.push_bind(__ver_val);
                __acc.push_sql(::std::concat!(" RETURNING ", #column_list));
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                // Use query_opt: a zero-row UPDATE is not a driver error — Postgres
                // returns no rows silently when the WHERE predicate matches nothing.
                // We map None → LockConflict so the caller can branch on it.
                match ctx.__query_opt_for_macros(&__sql, &__params).await? {
                    ::std::option::Option::Some(__raw_row) => {
                        let row: Self = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw_row)?;
                        *self = row;
                        // After `*self = row`, mark every Tracked field clean.
                        // from_pg_row already constructs Tracked::new (dirty=false),
                        // but the explicit walk is required by the Task 2 contract.
                        #(#mark_clean_fragments)*
                        // Phase 4 Task 6 — outbox after DB-refreshed rehydration.
                        #emit_outbox_save
                        // after_save runs AFTER the outbox
                        // emission AND after `*self = row` rehydration so
                        // the hook observes server-side defaults, triggers,
                        // and the bumped version counter (hook sequence:
                        // "after_save … reads DB truth, not pre-call
                        // value"). MUST stay inside the success arm — the
                        // None branch (LockConflict) skips after_save.
                        #after_save_call
                        // Cluster 8δ T7.5 — enqueue on_commit cache
                        // invalidation. The callback is captured by value
                        // so it does not need to borrow ctx after the SQL
                        // completes. The `if let Some(...)` gate skips
                        // models without a registered Punnu (pk=None or
                        // no-cache context). Pool-backed contexts log a
                        // warn and drop the callback — no special-case
                        // needed here.
                        //
                        // L2 backend errors are logged explicitly at
                        // `warn!` level (not `error!`): L1 is still
                        // correctly invalidated; only L2 distribution
                        // failed. Returning Ok(()) keeps the substrate
                        // from treating this as a transaction-level
                        // failure.
                        if let ::std::option::Option::Some(__punnu) =
                            ctx.punnu::<Self>()
                        {
                            let __id_for_cache =
                                ::core::clone::Clone::clone(&self.id);
                            ctx.on_commit(move || async move {
                                if let ::std::result::Result::Err(__e) = __punnu
                                    .invalidate(
                                        &__id_for_cache,
                                        ::djogi::cache::InvalidationReason::OnSave,
                                    )
                                    .await
                                {
                                    ::djogi::__private::tracing::warn!(
                                        target: "djogi::cache",
                                        error = ?__e,
                                        model = ::std::any::type_name::<Self>(),
                                        "Punnu::invalidate L2 backend failed during on_commit drain",
                                    );
                                }
                                ::std::result::Result::Ok(())
                            });
                        }
                        ::std::result::Result::Ok(())
                    }
                    ::std::option::Option::None => {
                        // Zero rows updated — DB version has moved ahead of our
                        // in-memory version. Signal optimistic lock conflict.
                        // after_save deliberately NOT
                        // dispatched here: the UPDATE didn't actually
                        // mutate the row, so observing stale in-memory
                        // state would violate the hook sequence guarantee
                        // that after_save sees DB truth.
                        ::std::result::Result::Err(
                            ::djogi::DjogiError::LockConflict(
                                ::djogi::DbError::other(#ver_conflict_msg)
                            )
                        )
                    }
                }
            }
        }
    } else {
        // Shape A — no version field: existing behavior.
        quote! {
            async move {
                #snapshot_auth_state_before_auto_set
                #auto_set_tenant
                // before_save fires after auto_set_tenant
                // is in scope (so the hook can read tenant context) but
                // before the UPDATE composes its SET clause. Returning Err
                // short-circuits via `?` — no UPDATE, no outbox row,
                // surrounding atomic() rolls back via standard error
                // propagation.
                #before_save_call
                // Build the SET clause dynamically. Tracked<T> fields are only
                // included when dirty; non-Tracked fields are always included.
                // `__first` tracks whether we have emitted any SET assignment yet
                // so comma insertion is correct regardless of which fields fire.
                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#save_acc_prefix);
                {
                    let mut __first = true;
                    #(#save_set_fragments)*
                    // `updated_at = now()` is always appended. If any user column
                    // was emitted above, we need a leading comma; otherwise it is
                    // the first (and only) assignment.
                    if !__first { __acc.push_sql(", "); }
                    __acc.push_sql("updated_at = now()");
                }
                // Append WHERE id = $<next> RETURNING <column_list>.
                // Emit the WHERE prefix as raw SQL, then let push_bind append the
                // `$n` placeholder and store the id value. RETURNING is appended
                // as raw SQL AFTER the bind so it follows the positional slot.
                __acc.push_sql(" WHERE id = ");
                // Clone id so push_bind (requires 'static) can take ownership.
                // HeerId, RanjId, and i32 (Serial) are all Clone + ToSql + Send + Sync + 'static.
                let __id_val = self.id.clone();
                __acc.push_bind(__id_val);
                __acc.push_sql(::std::concat!(" RETURNING ", #column_list));
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                let __raw_row = ctx.__query_one_for_macros(&__sql, &__params).await?;
                let row: Self = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw_row)?;
                *self = row;
                // After `*self = row`, walk every Tracked field and call mark_clean().
                // `from_pg_row` uses `Tracked::new(T)` which already starts clean,
                // so this is defensive — but required by the Task 2 contract.
                #(#mark_clean_fragments)*
                // Phase 4 Task 6 — outbox payload must reflect the DB-refreshed
                // values (triggers, column defaults), so emission runs AFTER the
                // `*self = row` rehydration. No-op for non-events models.
                #emit_outbox_save
                // after_save runs AFTER the outbox emission
                // AND after `*self = row` rehydration so the hook observes
                // server-side defaults, triggers, and any DB-bumped column
                // values (hook sequence).
                #after_save_call
                // Cluster 8δ T7.5 — enqueue on_commit cache invalidation.
                // Captured by value; pool-backed contexts warn + drop.
                // L2 backend errors are logged at `warn!` level — L1 is
                // still correctly invalidated; only L2 distribution
                // failed.
                if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                    let __id_for_cache = ::core::clone::Clone::clone(&self.id);
                    ctx.on_commit(move || async move {
                        if let ::std::result::Result::Err(__e) = __punnu
                            .invalidate(
                                &__id_for_cache,
                                ::djogi::cache::InvalidationReason::OnSave,
                            )
                            .await
                        {
                            ::djogi::__private::tracing::warn!(
                                target: "djogi::cache",
                                error = ?__e,
                                model = ::std::any::type_name::<Self>(),
                                "Punnu::invalidate L2 backend failed during on_commit drain",
                            );
                        }
                        ::std::result::Result::Ok(())
                    });
                }
                ::std::result::Result::Ok(())
            }
        }
    };

    // Hook dispatch wrapping the DELETE.
    //
    // When `#[model(hooks)]` is set, `before_delete(&mut self, ctx)` runs
    // before the DELETE composes (may mutate the in-memory snapshot or
    // abort the whole operation by returning Err) and
    // `after_delete(&self, ctx)` runs after the outbox emission so the
    // hook can observe both the pre-delete snapshot AND the just-written
    // outbox row (hook sequence:
    // before_delete -> DELETE -> outbox -> after_delete -> on_commit drain).
    //
    // Critical placement notes:
    // - `delete(self, ctx)` consumes `self`. To pass `&mut self` to
    //   `before_delete`, the impl signature emits `mut self` instead of
    //   `self` when hooks are enabled (Rust forbids shadowing the `self`
    //   keyword with `let mut self = self;`, but `mut self` as a function
    //   binding pattern is permitted and matches the trait declaration).
    //   The `mut` binding is emitted ONLY when `hooks` is true so non-hook
    //   models keep the original `self` binding and do not trip clippy's
    //   `unused_mut` lint.
    // - `after_delete` takes `&self` (immutable re-borrow of the same
    //   consumed-but-still-in-scope value). The DB row is gone; the
    //   outbox row carries the canonical snapshot. v3 §D1 fixes the
    //   after-hook receiver as `&self`, not `&mut self`.
    //
    // For non-hooks models both branches collapse to empty `TokenStream`
    // (no `quote!` invocation) so opt-out paths emit zero codegen — T1.8
    // verifies this with `cargo asm`.
    let (before_delete_call, after_delete_call) = if model_attrs.hooks {
        let before_delete_call = if model_attrs.tenant_key.is_some() {
            quote! {
                if let ::std::result::Result::Err(__djogi_hook_err) =
                    <Self as ::djogi::__private::hooks::ModelHooks>::before_delete(
                        &mut self,
                        ctx,
                    )
                    .await
                {
                    if let ::std::result::Result::Err(__djogi_restore_err) =
                        ctx.__restore_auth_state_for_macros(__djogi_auth_snapshot).await
                    {
                        return ::std::result::Result::Err(__djogi_restore_err);
                    }
                    return ::std::result::Result::Err(__djogi_hook_err);
                }
            }
        } else {
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::before_delete(
                    &mut self,
                    ctx,
                ).await?;
            }
        };
        (
            before_delete_call,
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::after_delete(
                    &self,
                    ctx,
                ).await?;
            },
        )
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    // The `self` binding pattern in the `delete(...)` impl signature.
    // `mut self` only when hooks are enabled — otherwise `self` so non-
    // hook models do not trip clippy's `unused_mut` lint.
    let delete_self_pat = if model_attrs.hooks {
        quote! { mut self }
    } else {
        quote! { self }
    };

    let delete_body = quote! {
        async move {
            #snapshot_auth_state_before_auto_set
            #auto_set_tenant
            // D3 step 1: before_delete fires before the
            // DELETE composes its parameter slice. Returning Err short-
            // circuits via `?` — no DELETE, no outbox row, surrounding
            // atomic() rolls back through standard error propagation.
            #before_delete_call
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #owned_pk_param,
            ];
            // D3 step 2 — DELETE.
            ctx.__execute_for_macros(#delete_sql, __params).await?;
            // D3 step 3 — outbox carries the pre-delete snapshot
            // (reads `self` before it drops at function scope end).
            // No-op for non-events models.
            #emit_outbox_delete
            // D3 step 4: after_delete observes the
            // consumed-but-still-in-scope `self` (last valid read of the
            // pre-delete snapshot — the DB row is gone, but the outbox
            // row carries the canonical payload by the time after_delete
            // fires). Order is load-bearing: an audit sink consuming
            // outbox sees the row before after_delete's body runs.
            #after_delete_call
            // Cluster 8δ T7.5 — enqueue on_commit cache invalidation.
            // Captured by value; pool-backed contexts warn + drop.
            // L2 backend errors are logged at `warn!` level — L1 is
            // still correctly invalidated; only L2 distribution
            // failed.
            if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                let __id_for_cache = ::core::clone::Clone::clone(&self.id);
                ctx.on_commit(move || async move {
                    if let ::std::result::Result::Err(__e) = __punnu
                        .invalidate(
                            &__id_for_cache,
                            ::djogi::cache::InvalidationReason::OnDelete,
                        )
                        .await
                    {
                        ::djogi::__private::tracing::warn!(
                            target: "djogi::cache",
                            error = ?__e,
                            model = ::std::any::type_name::<Self>(),
                            "Punnu::invalidate L2 backend failed during on_commit drain",
                        );
                    }
                    ::std::result::Result::Ok(())
                });
            }
            ::std::result::Result::Ok(())
        }
    };

    // ── update_returning_pair / delete_returning bodies ──
    //
    // Both methods are emitted for every pk-backed `#[model]` struct alongside
    // the existing five CRUD methods. The trait provides default
    // `unreachable!()` bodies so hand-rolled test Fake models need not change.
    //
    // SQL construction:
    // - The RETURNING suffix is baked at macro-expansion time using the
    //   already-built `column_list` slice (`framework_cols + user_col_names`),
    //   mirroring `save()` / `delete()` which also bake their SQL at expansion.
    // - Projection aliases use short ordinal aliases (`o{idx}` / `n{idx}`) to
    //   avoid PostgreSQL identifier-limit truncation, matching runtime builders.
    // - `FromJoinedPgRow` with prefix `"__djogi_old__"` / `"__djogi_new__"`
    //   decodes the two sides.
    //
    // Hook / outbox / cache sequencing:
    // - `update_returning_pair`: before_save → UPDATE RETURNING → decode pair
    //   → outbox(pair.new) → after_save(pair.new) → on_commit cache.
    // - `delete_returning`:      before_delete → DELETE RETURNING → decode deleted
    //   → outbox(self) → after_delete(self) → on_commit cache.
    // Old snapshot comes from DB `OLD`; hook/outbox keep any
    // `before_delete` mutations on the in-memory instance.

    // Static RETURNING suffixes — built once at expansion time.
    let all_cols: Vec<&str> = framework_cols
        .iter()
        .copied()
        .chain(user_col_names.iter().map(|s| s.as_str()))
        .collect();
    let update_returning_suffix: String = build_old_new_returning_suffix(&all_cols, true);
    let delete_returning_suffix: String = build_old_new_returning_suffix(&all_cols, false);

    // `delete_returning` uses a single WHERE id = $1 SQL plus the OLD suffix.
    let delete_returning_sql =
        format!("DELETE FROM {table} WHERE id = $1{delete_returning_suffix}");

    // Hook aliases for update_returning_pair.
    // `before_save` is reused (same pre-write hook). `after_save` receives
    // `&pair.new` — the DB-returned post-image — not a stale `&*self`.
    let (before_urp_call, after_urp_call) = if model_attrs.hooks {
        let before_urp_call = if model_attrs.tenant_key.is_some() {
            quote! {
                if let ::std::result::Result::Err(__djogi_hook_err) =
                    <Self as ::djogi::__private::hooks::ModelHooks>::before_save(
                        &mut self,
                        ctx,
                    )
                    .await
                {
                    if let ::std::result::Result::Err(__djogi_restore_err) =
                        ctx.__restore_auth_state_for_macros(__djogi_auth_snapshot).await
                    {
                        return ::std::result::Result::Err(__djogi_restore_err);
                    }
                    return ::std::result::Result::Err(__djogi_hook_err);
                }
            }
        } else {
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::before_save(
                    &mut self,
                    ctx,
                ).await?;
            }
        };
        (
            before_urp_call,
            // `after_save` takes `&pair.new`, not `&*self`.
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::after_save(
                    &__pair.new,
                    ctx,
                ).await?;
            },
        )
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    // Hook aliases for delete_returning.
    let (before_drp_call, after_drp_call) = if model_attrs.hooks {
        let before_drp_call = if model_attrs.tenant_key.is_some() {
            quote! {
                if let ::std::result::Result::Err(__djogi_hook_err) =
                    <Self as ::djogi::__private::hooks::ModelHooks>::before_delete(
                        &mut self,
                        ctx,
                    )
                    .await
                {
                    if let ::std::result::Result::Err(__djogi_restore_err) =
                        ctx.__restore_auth_state_for_macros(__djogi_auth_snapshot).await
                    {
                        return ::std::result::Result::Err(__djogi_restore_err);
                    }
                    return ::std::result::Result::Err(__djogi_hook_err);
                }
            }
        } else {
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::before_delete(
                    &mut self,
                    ctx,
                ).await?;
            }
        };
        (
            before_drp_call,
            // `after_delete` takes `&self` — consumed instance after
            // `before_delete` hooks.
            quote! {
                <Self as ::djogi::__private::hooks::ModelHooks>::after_delete(
                    &self,
                    ctx,
                ).await?;
            },
        )
    } else {
        (TokenStream::new(), TokenStream::new())
    };

    // Outbox for delete_returning: emit Delete from `self` so hook-time
    // mutations are preserved.
    let emit_outbox_drp_delete = if model_attrs.events {
        quote! {
            ::djogi::outbox::emit_event(
                ctx,
                &self,
                ::djogi::outbox::OutboxAction::Delete,
            ).await?;
        }
    } else {
        quote! {}
    };

    // self-binding pattern for delete_returning (mut self if hooks; self otherwise).
    let drp_self_pat = if model_attrs.hooks {
        quote! { mut self }
    } else {
        quote! { self }
    };

    // update_returning_pair body — non-versioned shape (Shape A).
    // Mirrors save() Shape A but decodes pair instead of rehydrating self.
    let update_returning_pair_body_shape_a = quote! {
        async move {
            // `mut self` so before_save can take `&mut self`.
            #snapshot_auth_state_before_auto_set
            #auto_set_tenant
            #before_urp_call
            // Build the SET clause — same logic as save() Shape A.
            let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#save_acc_prefix);
            {
                let mut __first = true;
                #(#save_set_fragments)*
                if !__first { __acc.push_sql(", "); }
                __acc.push_sql("updated_at = now()");
            }
            __acc.push_sql(" WHERE id = ");
            let __id_val = self.id.clone();
            __acc.push_bind(__id_val);
            __acc.push_sql(#update_returning_suffix);
            let (__sql, __binds) = __acc.into_parts();
            let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
            match ctx.__query_opt_for_macros(&__sql, &__params).await? {
                ::std::option::Option::Some(__raw_row) => {
                    let __old = <Self as ::djogi::__private::pg::FromJoinedPgRow>::from_joined_pg_row(
                        &__raw_row, "__djogi_old__",
                    )?;
                    let __new = <Self as ::djogi::__private::pg::FromJoinedPgRow>::from_joined_pg_row(
                        &__raw_row, "__djogi_new__",
                    )?;
                    let __pair = ::djogi::query::ReturningPair { old: __old, new: __new };
                    // Outbox: post-image only (pair.new), same outbox schema as save().
                    #emit_outbox_returning_save?;
                    // Hooks: after_save receives pair.new (DB truth, not stale self).
                    #after_urp_call
                    // Cache invalidation: invalidate pair.new.id (same as save()).
                    if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                        let __id_for_cache = ::core::clone::Clone::clone(&__pair.new.id);
                        ctx.on_commit(move || async move {
                            if let ::std::result::Result::Err(__e) = __punnu
                                .invalidate(
                                    &__id_for_cache,
                                    ::djogi::cache::InvalidationReason::OnSave,
                                )
                                .await
                            {
                                ::djogi::__private::tracing::warn!(
                                    target: "djogi::cache",
                                    error = ?__e,
                                    model = ::std::any::type_name::<Self>(),
                                    "Punnu::invalidate L2 backend failed during on_commit drain (update_returning_pair)",
                                );
                            }
                            ::std::result::Result::Ok(())
                        });
                    }
                    ::std::result::Result::Ok(__pair)
                }
                ::std::option::Option::None => {
                    ::std::result::Result::Err(::djogi::DjogiError::not_found(#table))
                }
            }
        }
    };

    // update_returning_pair body — versioned shape (Shape B).
    // Mirrors save() Shape B: version predicate in WHERE, query_opt, LockConflict.
    let update_returning_pair_body = if let Some((ver_ident, ver_col)) = &version_field_info {
        let ver_set = format!(", {ver_col} = {ver_col} + 1");
        let ver_where = format!(" AND {ver_col} = ");
        let ver_conflict_msg =
            format!("optimistic lock conflict: {ver_col} mismatch in table {table}");
        quote! {
            async move {
                #snapshot_auth_state_before_auto_set
                #auto_set_tenant
                #before_urp_call
                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#save_acc_prefix);
                {
                    let mut __first = true;
                    #(#save_set_fragments)*
                    if !__first { __acc.push_sql(", "); }
                    __acc.push_sql("updated_at = now()");
                    __acc.push_sql(#ver_set);
                }
                __acc.push_sql(" WHERE id = ");
                let __id_val = self.id.clone();
                __acc.push_bind(__id_val);
                __acc.push_sql(#ver_where);
                let __ver_val = self.#ver_ident.clone();
                __acc.push_bind(__ver_val);
                __acc.push_sql(#update_returning_suffix);
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                match ctx.__query_opt_for_macros(&__sql, &__params).await? {
                    ::std::option::Option::Some(__raw_row) => {
                        let __old = <Self as ::djogi::__private::pg::FromJoinedPgRow>::from_joined_pg_row(
                            &__raw_row, "__djogi_old__",
                        )?;
                        let __new = <Self as ::djogi::__private::pg::FromJoinedPgRow>::from_joined_pg_row(
                            &__raw_row, "__djogi_new__",
                        )?;
                        let __pair = ::djogi::query::ReturningPair { old: __old, new: __new };
                        #emit_outbox_returning_save?;
                        #after_urp_call
                        if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                            let __id_for_cache = ::core::clone::Clone::clone(&__pair.new.id);
                            ctx.on_commit(move || async move {
                                if let ::std::result::Result::Err(__e) = __punnu
                                    .invalidate(
                                        &__id_for_cache,
                                        ::djogi::cache::InvalidationReason::OnSave,
                                    )
                                    .await
                                {
                                    ::djogi::__private::tracing::warn!(
                                        target: "djogi::cache",
                                        error = ?__e,
                                        model = ::std::any::type_name::<Self>(),
                                        "Punnu::invalidate L2 backend failed during on_commit drain (update_returning_pair versioned)",
                                    );
                                }
                                ::std::result::Result::Ok(())
                            });
                        }
                        ::std::result::Result::Ok(__pair)
                    }
                    ::std::option::Option::None => {
                        match ctx.__query_opt_for_macros(
                            #get_sql,
                            &[#owned_pk_param],
                        )
                        .await?
                        {
                            ::std::option::Option::Some(_) => {
                                ::std::result::Result::Err(
                                    ::djogi::DjogiError::LockConflict(
                                        ::djogi::DbError::other(#ver_conflict_msg)
                                    )
                                )
                            }
                            ::std::option::Option::None => {
                                ::std::result::Result::Err(
                                    ::djogi::DjogiError::not_found(#table)
                                )
                            }
                        }
                    }
                }
            }
        }
    } else {
        update_returning_pair_body_shape_a
    };

    // delete_returning body.
    // Mirrors delete() but adds RETURNING WITH (OLD AS __djogi_old) and
    // returns the DB-returned snapshot instead of ().
    let delete_returning_body = quote! {
        async move {
            #snapshot_auth_state_before_auto_set
            #auto_set_tenant
            #before_drp_call
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #owned_pk_param,
            ];
            match ctx.__query_opt_for_macros(#delete_returning_sql, __params).await? {
                ::std::option::Option::Some(__raw_row) => {
                    let __deleted = <Self as ::djogi::__private::pg::FromJoinedPgRow>::from_joined_pg_row(
                        &__raw_row, "__djogi_old__",
                    )?;
                    // Outbox: emit from `self` so `before_delete` mutations are included.
                    #emit_outbox_drp_delete
                    // Hooks: after_delete receives `self` (after outbox emission).
                    #after_drp_call
                    // Cache invalidation: invalidate the deleted row's id.
                    if let ::std::option::Option::Some(__punnu) = ctx.punnu::<Self>() {
                        let __id_for_cache = ::core::clone::Clone::clone(&__deleted.id);
                        ctx.on_commit(move || async move {
                            if let ::std::result::Result::Err(__e) = __punnu
                                .invalidate(
                                    &__id_for_cache,
                                    ::djogi::cache::InvalidationReason::OnDelete,
                                )
                                .await
                            {
                                ::djogi::__private::tracing::warn!(
                                    target: "djogi::cache",
                                    error = ?__e,
                                    model = ::std::any::type_name::<Self>(),
                                    "Punnu::invalidate L2 backend failed during on_commit drain (delete_returning)",
                                );
                            }
                            ::std::result::Result::Ok(())
                        });
                    }
                    ::std::result::Result::Ok(__deleted)
                }
                ::std::option::Option::None => {
                    ::std::result::Result::Err(::djogi::DjogiError::not_found(#table))
                }
            }
        }
    };

    let refresh_body = quote! {
        async move {
            #auto_set_tenant
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #refresh_id_param,
            ];
            match ctx.__query_opt_for_macros(#get_sql, __params).await? {
                ::std::option::Option::Some(__row) => {
                    <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__row)
                }
                ::std::option::Option::None => {
                    ::std::result::Result::Err(::djogi::DjogiError::not_found(#table))
                }
            }
        }
    };

    // -------------------------------------------------------------------------
    // `create_with_id` — HeerId-only inherent method (form pre-generation).
    //
    // Emitted as a separate `impl` block only for HeerId models.  It is an
    // *inherent* method (not a trait method), so `pub async fn` is correct —
    // no `impl Future + Send` RPITIT wrapper is needed; the compiler infers
    // `+ Send` from the executor's bound.
    //
    // SQL: INSERT INTO {table} (id, {user_cols}) VALUES ($1, $2..$n+1)
    //      ON CONFLICT (id) DO NOTHING RETURNING *
    //
    // DONE_WITH_CONCERNS (Phase 1 limitation): when the conflict fires,
    // `ON CONFLICT DO NOTHING` suppresses the INSERT and RETURNING * returns
    // no rows.  Rather than issue a follow-up SELECT (which would consume the
    // executor, making transaction callers awkward), the method returns the
    // caller-supplied `value` with `id` overwritten to the pre-generated id.
    // The id is guaranteed correct; other fields reflect the *second* caller's
    // input rather than the first-inserted row.  A later phase will add a
    // `get_or_create` helper that does a proper fetch-on-conflict under an
    // explicit transaction boundary.
    // -------------------------------------------------------------------------
    let create_with_id_impl = if matches!(model_attrs.pk, PkStrategy::HeerId) {
        let insert_with_id_sql = if n_user == 0 {
            format!(
                "INSERT INTO {table} (id) VALUES ($1) ON CONFLICT (id) DO NOTHING RETURNING {column_list}"
            )
        } else {
            let cols: Vec<String> = user_fields.iter().map(|i| i.to_string()).collect();
            let col_list = cols.join(", ");
            // id binds to $1; user fields shift by 1 → $2..$n_user+1
            let vals: Vec<String> = (2..=n_user + 1).map(|n| format!("${n}")).collect();
            let val_list = vals.join(", ");
            format!(
                "INSERT INTO {table} (id, {col_list}) VALUES ($1, {val_list}) ON CONFLICT (id) DO NOTHING RETURNING {column_list}"
            )
        };

        quote! {
            impl #impl_generics #name #ty_generics #where_clause {
                /// Insert a row using a pre-generated `HeerId`.
                ///
                /// Intended for the **form pre-generation pattern**: the application
                /// allocates an ID before the user submits a form (e.g. to embed it in
                /// a URL), then passes that same ID on submit.  The underlying SQL uses
                /// `ON CONFLICT (id) DO NOTHING` so that duplicate submits do not
                /// produce a constraint-violation error.
                ///
                /// **Phase 1 limitation** — on conflict (the row already exists) the
                /// `RETURNING *` clause returns no rows and this method falls back to
                /// returning the caller-supplied `value` with its `id` field set to the
                /// pre-generated id.  The id is correct; other fields reflect the
                /// second caller's input rather than the originally-inserted data.  A
                /// later phase will add a proper `get_or_create` helper that fetches the
                /// existing row when a conflict fires.
                pub async fn create_with_id(
                    ctx: &mut ::djogi::context::DjogiContext,
                    id: ::djogi::types::HeerId,
                    value: Self,
                ) -> ::std::result::Result<Self, ::djogi::DjogiError> {
                    #auto_set_tenant
                    let __id_i64: i64 = id.as_i64();
                    // Widened-type temporaries (empty for direct-mapped types).
                    #(#create_param_pre_decls)*
                    let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                        &__id_i64,
                        #(#create_param_entries,)*
                    ];
                    let __maybe_row = ctx.__query_opt_for_macros(#insert_with_id_sql, __params).await?;
                    match __maybe_row {
                        ::std::option::Option::Some(__raw) => {
                            <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw)
                        }
                        ::std::option::Option::None => {
                            let mut v = value;
                            v.id = id;
                            ::std::result::Result::Ok(v)
                        }
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // -------------------------------------------------------------------------
    // Phase 4 Task 7d — bulk methods (bulk_create / bulk_update / bulk_upsert).
    //
    // Emitted as a separate inherent `impl` block. Scope: the three
    // Contract Decision #7 methods that operate on `Vec<Self>` or a
    // list of primary keys. `in_bulk` (PK-keyed fetch) stays on
    // `QuerySet<T>` — it is read-only and needs no per-model field
    // knowledge.
    //
    // Zero-user-field edge case: `bulk_create` / `bulk_upsert` require
    // at least one user column. Emitting the placeholder "(.push_bind
    // nothing)" tuple produces `()` in SQL which Postgres rejects. We
    // emit a compile-time `unimplemented!()` body for those models —
    // callers can still use `create()` row-by-row.
    // -------------------------------------------------------------------------
    let bulk_insert_col_list = if n_user == 0 {
        String::new()
    } else {
        user_fields
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    // For `bulk_upsert`'s ON CONFLICT DO UPDATE SET — every user col
    // plus `updated_at = now()`. If the conflict key is itself a user
    // col, EXCLUDED.col is that same incoming value, so leaving it in
    // the SET list is semantically a no-op. Postgres accepts it.
    let bulk_upsert_set_list = if n_user == 0 {
        "updated_at = now()".to_string()
    } else {
        let mut parts: Vec<String> = user_fields
            .iter()
            .map(|f| format!("{f} = EXCLUDED.{f}"))
            .collect();
        parts.push("updated_at = now()".to_string());
        parts.join(", ")
    };

    // Valid-column set for runtime validation of bulk_upsert's
    // `conflict_cols` argument. Include the three framework columns
    // (id / created_at / updated_at) so upserts on `id` (the common
    // case) validate, as well as every user field. This closes the
    // "user passes arbitrary string — gets SQL-injected" vector.
    let bulk_valid_columns: Vec<String> = {
        let mut v: Vec<String> = vec![
            "id".to_string(),
            "created_at".to_string(),
            "updated_at".to_string(),
        ];
        v.extend(user_fields.iter().map(|f| f.to_string()));
        v
    };
    let bulk_valid_columns_lit = bulk_valid_columns.iter().map(|s| quote! { #s });

    // Per-row bind tokens for bulk_create / bulk_upsert's VALUES tail.
    // Uses SqlAccumulator::push_bind for each field — emits a positional
    // `$n` placeholder and stores the bind value by move (fields come from
    // `rows.into_iter()` so each row is owned). Rows are comma-separated
    // via push_sql(", ") between iterations.
    //
    // Zero-user-field branch never reaches this — n_user >= 1 gated by
    // the per-method body.
    // Per-row bind tokens for bulk_create / bulk_upsert's VALUES tail.
    // Uses per-field `push_bind_tokens` so widened types (u8/u16/u64/…)
    // emit the appropriate widening call before `__acc.push_bind`. Rows
    // come from `rows.into_iter()` so each `row` is an owned value;
    // `row.#field` moves the field out of the row, which is fine since
    // each row is consumed once per iteration.
    let per_row_binds: TokenStream = if n_user == 0 {
        quote! {}
    } else {
        let field_bind_stmts: Vec<TokenStream> = user_fields
            .iter()
            .zip(user_field_types.iter())
            .enumerate()
            .map(|(i, (f, ty))| {
                let kind = bind_kind(ty);
                let nullable = is_nullable(ty);
                let tracked = is_tracked_inner(ty);
                let field_expr = quote! { row.#f };
                let push_stmt = push_bind_tokens(&kind, nullable, tracked, field_expr);
                if i == 0 {
                    quote! {
                        __acc.push_sql("(");
                        #push_stmt;
                    }
                } else {
                    quote! {
                        __acc.push_sql(", ");
                        #push_stmt;
                    }
                }
            })
            .collect();
        quote! {
            #(#field_bind_stmts)*
            __acc.push_sql(")");
        }
    };

    // Outbox-per-row emission for bulk_create / bulk_upsert's
    // rehydrated result rows. Iterate `&created` so outbox sees the
    // DB-truth snapshot (post-RETURNING). No-op for non-events models.
    let emit_outbox_bulk_create = if model_attrs.events {
        quote! {
            for __row in &created {
                ::djogi::outbox::emit_event(
                    ctx,
                    __row,
                    ::djogi::outbox::OutboxAction::Create,
                ).await?;
            }
        }
    } else {
        quote! {}
    };
    let emit_outbox_bulk_save = if model_attrs.events {
        quote! {
            for __row in &created {
                ::djogi::outbox::emit_event(
                    ctx,
                    __row,
                    ::djogi::outbox::OutboxAction::Save,
                ).await?;
            }
        }
    } else {
        quote! {}
    };

    // bulk_update's id bind — HeerId needs `.as_i64()` on each element,
    // RanjId / Serial bind directly. We route through `DjogiField::in_`
    // which takes `IntoIterator<Item = V>` where
    // `V: PartialEq + ToSql + Clone + Send + Sync + 'static`. HeerId /
    // RanjId / i32 (Serial) all satisfy the bind/clone surface, so the
    // same expression compiles for every pk_type. The root
    // `{Model}Fields` accessors return `DjogiField`; the
    // portable `.in_(ids)` produces a `PortablePredicate<Self>` that
    // `QuerySet::filter` accepts via `IntoQ<Self>` like every other
    // root predicate.
    //
    // `Self::Pk` is already the right type at macro expansion time
    // (see `pk_type_tokens` above), so we can forward `ids` verbatim
    // into `.in_(ids)`.
    let bulk_update_impl = if n_user == 0 {
        // Pathological case: no user fields to update + `updated_at`
        // bumped via the existing `.update` emitter's implicit
        // handling. We still emit a usable `bulk_update` — it just
        // compiles down to an `UPDATE ... SET updated_at = now()`
        // across the id list.
        quote! {
            /// Bulk-update every row whose primary key is in `ids`.
            ///
            /// Equivalent to
            /// `Self::objects().filter(|f| f.id().in_(ids)).update(closure).execute(ctx)`.
            /// This method is sugar for the common "update these
            /// specific rows" pattern without the caller spelling out
            /// the filter chain.
            pub async fn bulk_update<F, A>(
                ctx: &mut ::djogi::context::DjogiContext,
                ids: ::std::vec::Vec<<Self as ::djogi::model::Model>::Pk>,
                closure: F,
            ) -> ::std::result::Result<u64, ::djogi::DjogiError>
            where
                F: ::std::ops::FnOnce(<Self as ::djogi::model::Model>::Fields) -> A,
                A: ::djogi::query::IntoAssignments,
            {
                if ids.is_empty() { return ::std::result::Result::Ok(0); }
                <Self as ::djogi::model::Model>::objects()
                    .filter(|f| f.id().in_(ids))
                    .update(closure)
                    .execute(ctx)
                    .await
            }
        }
    } else {
        quote! {
            /// Bulk-update every row whose primary key is in `ids`.
            ///
            /// One `UPDATE` round trip emitting
            /// `UPDATE <table> SET <assignments>, updated_at = now() WHERE id IN (...)`.
            /// Empty `ids` short-circuits to `Ok(0)` without SQL.
            ///
            /// Equivalent to the explicit chain
            /// `Self::objects().filter(|f| f.id().in_(ids)).update(closure).execute(ctx)`;
            /// this method is sugar for the common "update these
            /// specific rows" pattern.
            pub async fn bulk_update<F, A>(
                ctx: &mut ::djogi::context::DjogiContext,
                ids: ::std::vec::Vec<<Self as ::djogi::model::Model>::Pk>,
                closure: F,
            ) -> ::std::result::Result<u64, ::djogi::DjogiError>
            where
                F: ::std::ops::FnOnce(<Self as ::djogi::model::Model>::Fields) -> A,
                A: ::djogi::query::IntoAssignments,
            {
                if ids.is_empty() { return ::std::result::Result::Ok(0); }
                <Self as ::djogi::model::Model>::objects()
                    .filter(|f| f.id().in_(ids))
                    .update(closure)
                    .execute(ctx)
                    .await
            }
        }
    };

    // Per-row bind emission that prepends an explicit id bind ahead of
    // the user-column binds. Shared by the post-T5 `bulk_create` dispatch
    // (for every PK kind except `Serial`) and by `bulk_upsert` (which
    // has always taken caller-supplied ids). Hoisted above both
    // emitters so the bulk_create path can reference it — TokenStream
    // interpolation is single-use but re-cloning the same source
    // definition keeps the emitted shapes byte-identical.
    //
    // HeerId needs `.as_i64()` to encode as BIGINT; RanjId and i32 bind
    // as-is. Custom PKs delegate ToSql to the inner type, so `row.id`
    // binds directly — the macro-emitted impl handles the wire
    // encoding.
    let pk_bind_for_id_first = match &model_attrs.pk {
        PkStrategy::HeerId => quote! { __acc.push_bind(row.id.as_i64()); },
        PkStrategy::HeerIdDesc => quote! { __acc.push_bind(row.id.as_i64()); },
        PkStrategy::RanjId => quote! { __acc.push_bind(row.id); },
        PkStrategy::RanjIdDesc => quote! { __acc.push_bind(row.id); },
        PkStrategy::Serial => quote! { __acc.push_bind(row.id); },
        PkStrategy::None => unreachable!("handled by early return"),
        PkStrategy::Custom(_) => quote! { __acc.push_bind(row.id); },
    };
    let id_first_per_row_binds: TokenStream = if n_user == 0 {
        // Zero-user-field models never reach either bulk_create /
        // bulk_upsert via this path (they early-return with a
        // Validation error), but we still emit a stub expression so
        // quoting the tree is valid.
        quote! {}
    } else {
        // Like `per_row_binds` but with the PK bound first (`id` column).
        // Uses `push_bind_tokens` for each user field so widened types get
        // the appropriate shim.
        let user_field_bind_stmts: Vec<TokenStream> = user_fields
            .iter()
            .zip(user_field_types.iter())
            .map(|(f, ty)| {
                let kind = bind_kind(ty);
                let nullable = is_nullable(ty);
                let tracked = is_tracked_inner(ty);
                let field_expr = quote! { row.#f };
                let push_stmt = push_bind_tokens(&kind, nullable, tracked, field_expr);
                quote! {
                    __acc.push_sql(", ");
                    #push_stmt;
                }
            })
            .collect();
        quote! {
            __acc.push_sql("(");
            #pk_bind_for_id_first
            #(#user_field_bind_stmts)*
            __acc.push_sql(")");
        }
    };

    // Phase 7-Zero-2 T5 — `bulk_create` dispatches on `pk_kind`.
    //
    // Pre-T5 emission inserted rows with per-row `DEFAULT` for `id`,
    // which forced Postgres to invoke `heerid_next()` / `ranjid_next()`
    // / custom `default_sql` once per row — N separate allocations per
    // batch. Post-T5, every PK kind that implements `PrimaryKeyDbGen`
    // pre-allocates the full batch of ids in **one** round-trip through
    // `<T as PrimaryKeyDbGen>::generate_many(ctx, n)` and then issues
    // the INSERT with explicit `id` values.
    //
    // `Serial` (`i32`) deliberately does not implement
    // `PrimaryKeyDbGen` — its sequence is per-row by construction, and
    // there is no bulk allocator to call. Serial models keep the
    // pre-T5 per-row-`DEFAULT` path; the generic dispatch arm is
    // unreachable for them.
    //
    // `None` is handled by the early return at the top of `expand`, so
    // we never reach this block.
    //
    // Every PK except Serial takes the pre-allocation path. The
    // upsert-shaped SQL (explicit `id` column + id-first per-row binds)
    // was already emitted for `bulk_upsert`; we reuse the same
    // `id_first_per_row_binds` / `insert_prefix_with_id` tokens here to
    // keep the two emitters structurally aligned.
    let pk_is_serial = matches!(model_attrs.pk, PkStrategy::Serial);
    let bulk_create_impl = if n_user == 0 {
        quote! {
            /// Not supported for zero-user-field models.
            ///
            /// A table with no non-framework columns cannot be bulk-
            /// inserted — the emitted SQL would be `INSERT INTO t ()
            /// VALUES ()` which Postgres rejects. Row-by-row
            /// [`create`](::djogi::model::Model::create) still works
            /// via the column's `DEFAULT`s.
            #[doc(hidden)]
            pub async fn bulk_create(
                _ctx: &mut ::djogi::context::DjogiContext,
                _rows: ::std::vec::Vec<Self>,
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                ::std::result::Result::Err(::djogi::DjogiError::Validation(
                    "bulk_create requires at least one non-framework column".to_string()
                ))
            }
        }
    } else if pk_is_serial {
        // `pk = Serial` keeps the per-row `DEFAULT` path — the
        // underlying `INTEGER` sequence is not bulk-allocatable and
        // `i32` deliberately does not implement `PrimaryKeyDbGen`.
        let insert_prefix = format!("INSERT INTO {table} ({bulk_insert_col_list}) VALUES ");
        let bulk_returning_suffix = format!(" RETURNING {column_list}");
        quote! {
            /// Bulk-insert every row in `rows` and return the rehydrated
            /// results.
            ///
            /// `pk = Serial` models insert with per-row `DEFAULT` on
            /// `id` — Postgres advances the backing sequence once per
            /// row, exactly as row-by-row [`create`] does. There is no
            /// bulk-allocation path for `Serial` because the sequence
            /// is owned by the database and has no `generate_many`
            /// primitive.
            ///
            /// Empty `rows` short-circuits to `Ok(Vec::new())` without
            /// SQL — an empty `VALUES ()` clause is invalid Postgres.
            pub async fn bulk_create(
                ctx: &mut ::djogi::context::DjogiContext,
                rows: ::std::vec::Vec<Self>,
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                if rows.is_empty() {
                    return ::std::result::Result::Ok(::std::vec::Vec::new());
                }
                #auto_set_tenant
                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insert_prefix);
                {
                    let mut __first = true;
                    for row in rows.into_iter() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        #per_row_binds
                    }
                }
                __acc.push_sql(#bulk_returning_suffix);
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                let __raw_rows = ctx.__query_all_for_macros(&__sql, &__params).await?;
                let created: ::std::vec::Vec<Self> = __raw_rows
                    .iter()
                    .map(|r| <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(r))
                    .collect::<::std::result::Result<::std::vec::Vec<Self>, _>>()?;
                #emit_outbox_bulk_create
                ::std::result::Result::Ok(created)
            }
        }
    } else {
        // Every other PK kind — HeerId / HeerIdDesc / RanjId /
        // RanjIdDesc / custom DB-gen — takes the pre-allocation path.
        // The insert prefix lists `id` first so the per-row tuple
        // provides an explicit id bind ahead of the user-column binds.
        let insert_prefix_with_id =
            format!("INSERT INTO {table} (id, {bulk_insert_col_list}) VALUES ");
        let bulk_returning_suffix = format!(" RETURNING {column_list}");
        quote! {
            /// Bulk-insert every row in `rows` and return the rehydrated
            /// results.
            ///
            /// Two round trips per call: one
            /// `<Self::Pk as PrimaryKeyDbGen>::generate_many(ctx, n)`
            /// to pre-allocate every row's primary key, followed by the
            /// main
            /// `INSERT INTO <table> (id, <user-cols>) VALUES (...) RETURNING <column_list>`.
            /// The pre-allocation round trip replaces N separate
            /// per-row `DEFAULT` calls with a single batched
            /// `generate_ids` / `generate_ranjids` / custom `bulk_sql`
            /// invocation — a hard scalability win on tables larger
            /// than a few hundred rows.
            ///
            /// Caller-supplied `row.id` values are overwritten by the
            /// pre-allocated ids. Row-by-row [`create`] is the
            /// escape hatch when a specific id must be preserved;
            /// [`bulk_upsert`] also preserves caller-supplied ids by
            /// construction.
            ///
            /// Empty `rows` short-circuits to `Ok(Vec::new())` without
            /// SQL — an empty `VALUES ()` clause is invalid Postgres.
            ///
            /// Postgres caps bound parameters at 65_535. With `N` user
            /// columns per model plus the `id` column, the effective
            /// cap is `65_535 / (N + 1)` rows per call. Chunk larger
            /// batches at the call site.
            ///
            /// When the model has `#[model(events)]`, outbox rows are
            /// written per inserted row **after** rehydration (so the
            /// outbox payload reflects DB-truth column defaults and
            /// trigger mutations). Runs inside the caller's
            /// transaction / atomic scope when `ctx` holds one.
            pub async fn bulk_create(
                ctx: &mut ::djogi::context::DjogiContext,
                mut rows: ::std::vec::Vec<Self>,
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                if rows.is_empty() {
                    return ::std::result::Result::Ok(::std::vec::Vec::new());
                }
                #auto_set_tenant
                // Pre-allocate N ids in one round trip. The allocated
                // ids are written onto each row's `id` field in order,
                // overwriting whatever sentinel / caller-supplied value
                // was there.
                let __n = rows.len();
                let __ids = <#pk_type_tokens as ::djogi::primary_key::PrimaryKeyDbGen>::generate_many(
                    ctx,
                    __n,
                ).await?;
                // Length-check before the zip. Built-ins uphold the
                // `len() == n` contract by construction, but custom PKs
                // drive the batch via user-supplied SQL (or a synthesised
                // `SELECT … FROM generate_series(1, $1)` when only
                // `default_sql` is set), and either can legally return
                // fewer rows. Zipping silently would leave trailing rows
                // pointing at stale sentinel ids and the INSERT would
                // commit duplicates or nulls. Fail loudly instead.
                if __ids.len() != __n {
                    return ::std::result::Result::Err(::djogi::DjogiError::Db(
                        ::djogi::DbError::other(::std::format!(
                            "bulk_create: PrimaryKeyDbGen::generate_many returned {} ids for n={}",
                            __ids.len(),
                            __n
                        )),
                    ));
                }
                for (row, id) in rows.iter_mut().zip(__ids.into_iter()) {
                    row.id = id;
                }
                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insert_prefix_with_id);
                {
                    let mut __first = true;
                    for row in rows.into_iter() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        #id_first_per_row_binds
                    }
                }
                __acc.push_sql(#bulk_returning_suffix);
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                let __raw_rows = ctx.__query_all_for_macros(&__sql, &__params).await?;
                let created: ::std::vec::Vec<Self> = __raw_rows
                    .iter()
                    .map(|r| <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(r))
                    .collect::<::std::result::Result<::std::vec::Vec<Self>, _>>()?;
                #emit_outbox_bulk_create
                ::std::result::Result::Ok(created)
            }
        }
    };

    let bulk_upsert_impl = if n_user == 0 {
        quote! {
            /// Not supported for zero-user-field models.
            ///
            /// See [`bulk_create`] for the rationale — upsert emits
            /// the same `INSERT INTO t (...) VALUES (...)` prefix
            /// which is empty-clause-invalid for zero-user-field
            /// tables.
            #[doc(hidden)]
            pub async fn bulk_upsert(
                _ctx: &mut ::djogi::context::DjogiContext,
                _rows: ::std::vec::Vec<Self>,
                _conflict_cols: &[&'static str],
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                ::std::result::Result::Err(::djogi::DjogiError::Validation(
                    "bulk_upsert requires at least one non-framework column".to_string()
                ))
            }
        }
    } else {
        let insert_prefix = format!("INSERT INTO {table} (id, {bulk_insert_col_list}) VALUES ");
        let do_update_set_clause =
            format!(" DO UPDATE SET {bulk_upsert_set_list} RETURNING {column_list}");

        // `id_first_per_row_binds` (hoisted above `bulk_create_impl`)
        // emits the id-first per-row tuple bindings — same shape the
        // post-T5 `bulk_create` uses after `PrimaryKeyDbGen::generate_many`.

        quote! {
            /// Bulk-upsert — `INSERT ... ON CONFLICT (<cols>) DO UPDATE SET ...`.
            ///
            /// Inserts every row in `rows`; on conflict against the
            /// `conflict_cols` key, updates every user field plus
            /// `updated_at = now()` with the incoming values
            /// (`EXCLUDED.*`). Returns the rehydrated rows —
            /// `RETURNING <column_list>` emits one row per input
            /// regardless of whether it was inserted or updated.
            ///
            /// `conflict_cols` must reference real columns of this
            /// model (framework or user). Unknown names return
            /// [`DjogiError::Validation`] without a round trip — this
            /// closes the SQL-injection vector from arbitrary
            /// `&'static str` input.
            ///
            /// Empty `rows` short-circuits to `Ok(Vec::new())` without
            /// SQL. Empty `conflict_cols` returns
            /// [`DjogiError::Validation`] — `ON CONFLICT ()` is
            /// invalid SQL.
            ///
            /// Callers upserting with pre-allocated primary keys must
            /// call `<HeerId as djogi::primary_key::PrimaryKeyDbGen>::generate_many(&mut ctx, n)`
            /// up front — row.id is inserted verbatim, no column default fires.
            pub async fn bulk_upsert(
                ctx: &mut ::djogi::context::DjogiContext,
                rows: ::std::vec::Vec<Self>,
                conflict_cols: &[&'static str],
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                if rows.is_empty() {
                    return ::std::result::Result::Ok(::std::vec::Vec::new());
                }
                if conflict_cols.is_empty() {
                    return ::std::result::Result::Err(::djogi::DjogiError::Validation(
                        "bulk_upsert requires at least one conflict column".to_string()
                    ));
                }
                #auto_set_tenant
                // Validate every conflict column against the macro-
                // emitted allow-list of real columns. Rejects typos
                // and closes the arbitrary-string SQL-injection path.
                const __VALID_COLS: &[&str] = &[ #(#bulk_valid_columns_lit),* ];
                for col in conflict_cols {
                    if !__VALID_COLS.contains(col) {
                        return ::std::result::Result::Err(::djogi::DjogiError::Validation(
                            ::std::format!(
                                "unknown conflict column '{}' for table {}",
                                col,
                                #table,
                            )
                        ));
                    }
                }

                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insert_prefix);
                {
                    let mut __first = true;
                    for row in rows.into_iter() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        #id_first_per_row_binds
                    }
                }
                __acc.push_sql(" ON CONFLICT (");
                {
                    let mut __first = true;
                    for col in conflict_cols {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(*col);
                    }
                }
                __acc.push_sql(")");
                __acc.push_sql(#do_update_set_clause);
                let (__sql, __binds) = __acc.into_parts();
                let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                    __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                let __raw_rows = ctx.__query_all_for_macros(&__sql, &__params).await?;
                let created: ::std::vec::Vec<Self> = __raw_rows
                    .iter()
                    .map(|r| <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(r))
                    .collect::<::std::result::Result<::std::vec::Vec<Self>, _>>()?;
                // Upsert outbox policy: emit a Save event per returned
                // row — the caller does not tell us whether each row
                // was inserted or updated, and "Save" is the
                // action-agnostic variant. Consumers that need
                // fine-grained distinction can read the row's
                // `created_at == updated_at` tautology from the payload.
                #emit_outbox_bulk_save
                ::std::result::Result::Ok(created)
            }
        }
    };

    // -------------------------------------------------------------------------
    // Phase 4 Task 7.5 — idempotency_key consumer wiring.
    //
    // `create_or_find` + `bulk_upsert_by_descriptor` are emitted for
    // every model. When `#[model(idempotency_key = "col")]` is set
    // they produce real upsert-on-idempotency-key semantics; when the
    // attribute is absent, they produce stub bodies that return
    // `DjogiError::MissingIdempotencyKey` at runtime — pointing
    // callers at the attribute they need to add.
    //
    // The attribute value is validated in `attrs.rs` to be a plain
    // ASCII identifier, so embedding `#idempotency_key` into the SQL
    // is safe (no quote injection surface).
    //
    // Zero-user-field models emit the stub even when the attribute is
    // set — the underlying bulk_upsert / create SQL is not emittable
    // for zero-user-field models, so consumer wiring cannot deliver
    // the advertised semantics. Same policy as bulk_create /
    // bulk_upsert above.
    // -------------------------------------------------------------------------
    let idempotency_methods_impl = match (&model_attrs.idempotency_key, n_user) {
        // Attribute set + at least one user column: real methods.
        (Some(key_str), n) if n > 0 => {
            let key_ident = format_ident!("{}", key_str);
            let insert_or_nothing_sql = {
                let cols: Vec<String> = user_fields.iter().map(|i| i.to_string()).collect();
                let col_list = cols.join(", ");
                let placeholders: Vec<String> = (1..=n_user).map(|i| format!("${i}")).collect();
                let ph_list = placeholders.join(", ");
                format!(
                    "INSERT INTO {table} ({col_list}) VALUES ({ph_list}) \
                     ON CONFLICT ({key_str}) DO NOTHING RETURNING {column_list}"
                )
            };
            let select_by_key_sql =
                format!("SELECT {column_list} FROM {table} WHERE {key_str} = $1 LIMIT 1");

            // The `create_or_find` outbox emission policy — fire the
            // Create event only when the insert actually inserted a
            // row. Skipped on the "found existing row" branch so the
            // outbox reflects the DB-truth "what changed". Non-events
            // models emit nothing.
            let create_or_find_outbox = if model_attrs.events {
                quote! {
                    ::djogi::outbox::emit_event(
                        ctx,
                        &__row,
                        ::djogi::outbox::OutboxAction::Create,
                    ).await?;
                }
            } else {
                quote! {}
            };

            // ── Bind shims for the INSERT params ────────────────────────────
            //
            // Route every user field through `create_param_tokens` so widened
            // types (i8/u8 → i16, u16 → i32, u32 → i64, u64 → Decimal) get
            // the correct SQL wire type. Previously the insert params were
            // built as a direct `&[&(dyn ToSql + Sync)]` slice, bypassing the
            // shims that `create` / `create_with_id` / bulk paths use — this
            // caused silent type-mismatch failures for models with narrow or
            // unsigned integer fields (djogi#GPT-5.5 BLOCK 1).
            //
            // Mirrors the pattern in the `create_body` section above: each
            // widened field gets a named `let __bind_N: WideType = …` pre-
            // declaration, and the slice entry references that binding. Direct
            // types emit an empty pre-declaration and bind the field value
            // inline.
            let (cof_insert_pre_decls, cof_insert_entries): (Vec<TokenStream>, Vec<TokenStream>) =
                user_fields
                    .iter()
                    .zip(user_field_types.iter())
                    .enumerate()
                    .map(|(slot, (f, ty))| {
                        let kind = bind_kind(ty);
                        let nullable = is_nullable(ty);
                        let tracked = is_tracked_inner(ty);
                        let val_expr = quote! { row.#f };
                        create_param_tokens(&kind, nullable, tracked, val_expr, slot)
                    })
                    .unzip();

            // ── Bind shim for the fallback SELECT key param ──────────────────
            //
            // The fallback SELECT after an ON CONFLICT hit binds the idempotency
            // key value as `$1`. If the key field is a widened type, the old
            // direct `&row.#key_ident as &(dyn ToSql + Sync)` would fail or
            // send the wrong wire type. Look up the key field's type in the
            // user-field list and apply `create_param_tokens` on slot 0.
            let (cof_key_pre_decl, cof_key_entry) =
                if let Some(idx) = user_fields.iter().position(|f| f == &key_ident) {
                    let key_ty = &user_field_types[idx];
                    create_param_tokens(
                        &bind_kind(key_ty),
                        is_nullable(key_ty),
                        is_tracked_inner(key_ty),
                        quote! { row.#key_ident },
                        0,
                    )
                } else {
                    // Key field not found — this is unreachable when the macro
                    // attribute is validated correctly (the key must be an
                    // existing field). Fallback to direct bind so downstream
                    // compilation surfaces the name-resolution error naturally.
                    (
                        TokenStream::new(),
                        quote! {
                            &row.#key_ident
                                as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)
                        },
                    )
                };

            quote! {
                /// Idempotent create — insert a row keyed off the
                /// descriptor's `idempotency_key` attribute, or
                /// return the existing row when the key conflicts.
                ///
                /// Shape:
                /// `INSERT INTO <table> (<user-cols>) VALUES ($1,...)
                ///  ON CONFLICT (<key>) DO NOTHING RETURNING <column_list>`.
                /// On empty RETURNING (the "key already existed"
                /// branch) the method re-SELECTs the existing row by
                /// `<key> = row.<key>` and returns `(existing, false)`.
                /// New rows return `(inserted, true)`.
                ///
                /// When the model does **not** declare
                /// `#[model(idempotency_key = "...")]`, this method
                /// emits [`DjogiError::MissingIdempotencyKey`] at
                /// runtime pointing at the attribute that must be
                /// added. Shipping the stub (rather than hiding the
                /// method behind a cfg flag) keeps the API shape
                /// uniform across models and surfaces the missing
                /// attribute eagerly when a consumer expects it.
                ///
                /// When `#[model(events)]` is also set, only the
                /// **newly-inserted** branch emits an outbox row —
                /// the "found existing" branch reflects no state
                /// change.
                pub async fn create_or_find(
                    ctx: &mut ::djogi::context::DjogiContext,
                    row: Self,
                ) -> ::std::result::Result<(Self, bool), ::djogi::DjogiError> {
                    #auto_set_tenant
                    // Widened-type temporaries (empty for direct-mapped types).
                    // Must be declared before the slice literal so the borrows
                    // live long enough.
                    #(#cof_insert_pre_decls)*
                    let __insert_params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                        #(#cof_insert_entries,)*
                    ];
                    let __maybe_inserted = ctx.__query_opt_for_macros(
                        #insert_or_nothing_sql,
                        __insert_params,
                    ).await?;
                    match __maybe_inserted {
                        ::std::option::Option::Some(__raw) => {
                            let __row = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw)?;
                            #create_or_find_outbox
                            ::std::result::Result::Ok((__row, true))
                        }
                        ::std::option::Option::None => {
                            // Conflict fired — re-SELECT the
                            // existing row by the idempotency key.
                            // The key-field value comes from the
                            // caller's `row` input (unchanged across
                            // the insert attempt). Widened-type
                            // temporary for the key field (empty for
                            // direct-mapped key types).
                            #cof_key_pre_decl
                            let __select_params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                                #cof_key_entry,
                            ];
                            let __raw = ctx.__query_one_for_macros(
                                #select_by_key_sql,
                                __select_params,
                            ).await?;
                            let existing = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw)?;
                            ::std::result::Result::Ok((existing, false))
                        }
                    }
                }

                /// Bulk-upsert keyed off the descriptor's
                /// `idempotency_key` attribute.
                ///
                /// Thin wrapper over [`bulk_upsert`] that passes the
                /// declared idempotency-key column as the sole ON
                /// CONFLICT target. Returns
                /// [`DjogiError::MissingIdempotencyKey`] at runtime
                /// if the attribute is not set.
                pub async fn bulk_upsert_by_descriptor(
                    ctx: &mut ::djogi::context::DjogiContext,
                    rows: ::std::vec::Vec<Self>,
                ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                    Self::bulk_upsert(ctx, rows, &[#key_str]).await
                }
            }
        }
        // Attribute NOT set, or zero-user-field model: stub bodies
        // that return the missing-attribute error.
        _ => {
            let type_name_str = name.to_string();
            quote! {
                /// Idempotent create — requires
                /// `#[model(idempotency_key = "...")]` on the model.
                ///
                /// Emits [`DjogiError::MissingIdempotencyKey`] at
                /// runtime for models that haven't declared the
                /// attribute. See the variant's rustdoc for the
                /// remediation pointer.
                pub async fn create_or_find(
                    _ctx: &mut ::djogi::context::DjogiContext,
                    _row: Self,
                ) -> ::std::result::Result<(Self, bool), ::djogi::DjogiError> {
                    ::std::result::Result::Err(
                        ::djogi::DjogiError::missing_idempotency_key(#type_name_str),
                    )
                }

                /// Bulk-upsert by descriptor — requires
                /// `#[model(idempotency_key = "...")]`.
                ///
                /// Same stub semantics as
                /// [`create_or_find`]: runtime error pointing at
                /// the missing attribute.
                pub async fn bulk_upsert_by_descriptor(
                    _ctx: &mut ::djogi::context::DjogiContext,
                    _rows: ::std::vec::Vec<Self>,
                ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                    ::std::result::Result::Err(
                        ::djogi::DjogiError::missing_idempotency_key(#type_name_str),
                    )
                }
            }
        }
    };

    let bulk_methods_impl = quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            #bulk_create_impl
            #bulk_update_impl
            #bulk_upsert_impl
            #idempotency_methods_impl
        }
    };

    // -------------------------------------------------------------------------
    // Phase 5 Task 10 — `_insecurely()` suffix methods.
    //
    // Emitted ONLY when `#[model(tenant_key = "...")]` is declared. For every
    // other model this block is empty — no inherent methods, no associated
    // items, no compile-time surface at all.
    //
    // ## Bypass mechanism
    //
    // RLS is enforced at the Postgres level via a `CREATE POLICY ... USING`
    // expression that checks `col = current_setting('app.tenant_id', true)::T`.
    // Issuing `SET LOCAL row_security = off` inside a transaction disables that
    // check for the duration of the transaction. Outside `atomic()` `SET LOCAL`
    // silently no-ops (Postgres emits a WARNING but accepts the statement), so
    // the bypass only takes effect when the caller wraps the call in `atomic()`.
    //
    // ## `#[track_caller]` + caller capture
    //
    // For async methods we use the "sync wrapper returns impl Future" pattern:
    //
    //   #[track_caller]
    //   pub fn method_insecurely(...) -> impl Future<...> {
    //       let __caller = ::std::panic::Location::caller();
    //       async move {
    //           ::djogi::__private::tracing::warn!(..., caller = %__caller, ...);
    //           ...
    //       }
    //   }
    //
    // `Location::caller()` is resolved in the sync preamble so it reflects the
    // user's call site, not an internal async-block location. The outer `fn`
    // carries `#[track_caller]` so Rust traces through it during resolution.
    //
    // ## `objects_insecurely` (sync)
    //
    // Returns `Self::objects()` — an unexecuted lazy QuerySet. The bypass
    // (`SET LOCAL row_security = off`) cannot be issued here because there is no
    // context at queryset-construction time; it fires later when a terminal
    // method (`.fetch_all(ctx)`, etc.) runs. The caller must issue
    // `ctx.raw_execute("SET LOCAL row_security = off", &[]).await?` inside an
    // `atomic()` scope before calling the terminal method. This limitation is
    // documented in the method's rustdoc.
    //
    // ## Path routing
    //
    // `tracing::warn!` routes through `::djogi::__private::tracing::warn!` —
    // same convention as `inventory`, `postgres_types`, and `futures` in this
    // file.
    //
    // ## `bulk_update_insecurely` bound divergence vs `bulk_update`
    //
    // `bulk_update` is a plain `async fn` where the compiler infers Send / 'ctx
    // on the captured closure. `bulk_update_insecurely` cannot use `async fn`
    // because `#[track_caller]` does not reflect the user's call site across
    // an `async fn` boundary — we must use the sync-wrapper + `impl Future +
    // Send + 'ctx` shape. That return type forces `F: Send + 'ctx` on the
    // captured closure. `A` is only the return type of `F` and never lives
    // inside the captured future, so it stays unbounded — matching
    // `bulk_update`'s shape for that parameter. The emitted rustdoc spells
    // out this rationale so users who hit an unexpected bound error at the
    // call site can see why the two methods diverge.
    // -------------------------------------------------------------------------
    let model_name_str = name.to_string();
    let insecurely_impl = if model_attrs.tenant_key.is_some() {
        // Bulk insert prefix / returning suffix reuse the values already computed.
        let insecure_bulk_insert_prefix =
            format!("INSERT INTO {table} ({bulk_insert_col_list}) VALUES ");
        let insecure_bulk_returning_suffix = format!(" RETURNING {column_list}");

        // --- bulk_create_insecurely (n_user == 0: error; n_user > 0: real body) ---
        let bulk_create_insecurely_body = if n_user == 0 {
            quote! {
                /// Not supported for zero-user-field models.
                ///
                /// A zero-user-field table has no non-framework columns — emitting
                /// `INSERT INTO t () VALUES ()` is invalid SQL. Use
                /// [`create_insecurely`] row-by-row instead.
                ///
                /// The `_insecurely` suffix means RLS tenant isolation is bypassed.
                /// Bypass only takes effect inside [`atomic()`](::djogi::transaction::atomic);
                /// on a pool-backed context the call still executes but RLS remains active.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn bulk_create_insecurely(
                    _ctx: &mut ::djogi::context::DjogiContext,
                    _rows: ::std::vec::Vec<Self>,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError>,
                > + ::std::marker::Send {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "bulk_create_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::std::result::Result::Err(::djogi::DjogiError::Validation(
                            "bulk_create_insecurely requires at least one non-framework column".to_string()
                        ))
                    }
                }
            }
        } else {
            quote! {
                /// Bulk-insert rows, bypassing the RLS tenant predicate.
                ///
                /// One `INSERT` round trip; framework columns are populated
                /// by column defaults. The RLS bypass is issued via
                /// `SET LOCAL row_security = off` before the insert.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the statement still executes but RLS remains active
                /// because `SET LOCAL` is a no-op outside a transaction.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn bulk_create_insecurely<'ctx>(
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                    rows: ::std::vec::Vec<Self>,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "bulk_create_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        if rows.is_empty() {
                            return ::std::result::Result::Ok(::std::vec::Vec::new());
                        }
                        let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insecure_bulk_insert_prefix);
                        {
                            let mut __first = true;
                            for row in rows.into_iter() {
                                if __first { __first = false; } else { __acc.push_sql(", "); }
                                #per_row_binds
                            }
                        }
                        __acc.push_sql(#insecure_bulk_returning_suffix);
                        let (__sql, __binds) = __acc.into_parts();
                        let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                            __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                        let __raw_rows = ctx.__query_all_for_macros(&__sql, &__params).await?;
                        let created: ::std::vec::Vec<Self> = __raw_rows
                            .iter()
                            .map(|r| <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(r))
                            .collect::<::std::result::Result<::std::vec::Vec<Self>, _>>()?;
                        ::std::result::Result::Ok(created)
                    }
                }
            }
        };

        // --- bulk_upsert_insecurely (n_user == 0: error; n_user > 0: real body) ---
        let bulk_upsert_insecurely_body = if n_user == 0 {
            quote! {
                /// Not supported for zero-user-field models.
                ///
                /// See [`bulk_create_insecurely`] for the rationale.
                ///
                /// The `_insecurely` suffix means RLS tenant isolation is bypassed.
                /// Bypass only takes effect inside [`atomic()`](::djogi::transaction::atomic);
                /// on a pool-backed context the call still executes but RLS remains active.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[doc(hidden)]
                #[track_caller]
                pub fn bulk_upsert_insecurely(
                    _ctx: &mut ::djogi::context::DjogiContext,
                    _rows: ::std::vec::Vec<Self>,
                    _conflict_cols: &[&'static str],
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError>,
                > + ::std::marker::Send {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "bulk_upsert_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::std::result::Result::Err(::djogi::DjogiError::Validation(
                            "bulk_upsert_insecurely requires at least one non-framework column".to_string()
                        ))
                    }
                }
            }
        } else {
            let insecure_upsert_prefix =
                format!("INSERT INTO {table} (id, {bulk_insert_col_list}) VALUES ");
            let insecure_do_update =
                format!(" DO UPDATE SET {bulk_upsert_set_list} RETURNING {column_list}");
            let insecure_valid_cols_lit = bulk_valid_columns.iter().map(|s| quote! { #s });

            // Per-row binds for the upsert path (same shape as
            // `id_first_per_row_binds`). Uses `push_bind_tokens` for
            // user fields so widened types get the appropriate shim.
            let insecure_upsert_per_row_binds: TokenStream = {
                let pk_bind = match &model_attrs.pk {
                    PkStrategy::HeerId => quote! { __acc.push_bind(row.id.as_i64()); },
                    PkStrategy::HeerIdDesc => quote! { __acc.push_bind(row.id.as_i64()); },
                    PkStrategy::RanjId => quote! { __acc.push_bind(row.id); },
                    PkStrategy::RanjIdDesc => quote! { __acc.push_bind(row.id); },
                    PkStrategy::Serial => quote! { __acc.push_bind(row.id); },
                    PkStrategy::None => unreachable!("handled by early return"),
                    PkStrategy::Custom(_) => quote! { __acc.push_bind(row.id); },
                };
                let uf_bind_stmts: Vec<TokenStream> = user_fields
                    .iter()
                    .zip(user_field_types.iter())
                    .map(|(f, ty)| {
                        let kind = bind_kind(ty);
                        let nullable = is_nullable(ty);
                        let tracked = is_tracked_inner(ty);
                        let field_expr = quote! { row.#f };
                        let push_stmt = push_bind_tokens(&kind, nullable, tracked, field_expr);
                        quote! {
                            __acc.push_sql(", ");
                            #push_stmt;
                        }
                    })
                    .collect();
                quote! {
                    __acc.push_sql("(");
                    #pk_bind
                    #(#uf_bind_stmts)*
                    __acc.push_sql(")");
                }
            };

            quote! {
                /// Bulk-upsert rows, bypassing the RLS tenant predicate.
                ///
                /// Identical to [`bulk_upsert`] but issues
                /// `SET LOCAL row_security = off` before the statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the statement still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn bulk_upsert_insecurely<'ctx>(
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                    rows: ::std::vec::Vec<Self>,
                    conflict_cols: &'ctx [&'static str],
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "bulk_upsert_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        if rows.is_empty() {
                            return ::std::result::Result::Ok(::std::vec::Vec::new());
                        }
                        if conflict_cols.is_empty() {
                            return ::std::result::Result::Err(::djogi::DjogiError::Validation(
                                "bulk_upsert_insecurely requires at least one conflict column".to_string()
                            ));
                        }
                        const __VALID_COLS: &[&str] = &[ #(#insecure_valid_cols_lit),* ];
                        for col in conflict_cols {
                            if !__VALID_COLS.contains(col) {
                                return ::std::result::Result::Err(::djogi::DjogiError::Validation(
                                    ::std::format!(
                                        "unknown conflict column '{}' for table {}",
                                        col,
                                        #table,
                                    )
                                ));
                            }
                        }
                        let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insecure_upsert_prefix);
                        {
                            let mut __first = true;
                            for row in rows.into_iter() {
                                if __first { __first = false; } else { __acc.push_sql(", "); }
                                #insecure_upsert_per_row_binds
                            }
                        }
                        __acc.push_sql(" ON CONFLICT (");
                        {
                            let mut __first = true;
                            for col in conflict_cols {
                                if __first { __first = false; } else { __acc.push_sql(", "); }
                                __acc.push_sql(*col);
                            }
                        }
                        __acc.push_sql(")");
                        __acc.push_sql(#insecure_do_update);
                        let (__sql, __binds) = __acc.into_parts();
                        let __params: ::std::vec::Vec<&(dyn ::djogi::__private::postgres_types::ToSql + Sync)> =
                            __binds.iter().map(|b| b.as_ref() as &(dyn ::djogi::__private::postgres_types::ToSql + Sync)).collect();
                        let __raw_rows = ctx.__query_all_for_macros(&__sql, &__params).await?;
                        let created: ::std::vec::Vec<Self> = __raw_rows
                            .iter()
                            .map(|r| <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(r))
                            .collect::<::std::result::Result<::std::vec::Vec<Self>, _>>()?;
                        ::std::result::Result::Ok(created)
                    }
                }
            }
        };

        quote! {
            impl #impl_generics #name #ty_generics #where_clause {
                /// Fetch a single row by primary key, bypassing the RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the SELECT to
                /// lift the per-row policy for this statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active because
                /// `SET LOCAL` is a no-op outside a transaction.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call to aid audit trails.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn get_insecurely<'ctx>(
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                    id: <Self as ::djogi::model::Model>::Pk,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<Self, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "get_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        <Self as ::djogi::model::Model>::get(ctx, id).await
                    }
                }

                /// Insert a new row, bypassing the RLS `WITH CHECK` tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the INSERT so the
                /// `WITH CHECK` clause on the policy is not evaluated — the row is
                /// written regardless of whether its tenant-key field matches
                /// `app.tenant_id`.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn create_insecurely<'ctx>(
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                    value: Self,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<Self, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "create_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        <Self as ::djogi::model::Model>::create(ctx, value).await
                    }
                }

                /// Save (UPDATE) this row, bypassing the RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the UPDATE so both
                /// the `USING` (row visibility) and `WITH CHECK` (write restriction)
                /// clauses are lifted for this statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn save_insecurely<'ctx>(
                    &'ctx mut self,
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<(), ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "save_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        <Self as ::djogi::model::Model>::save(self, ctx).await
                    }
                }

                /// Delete this row, bypassing the RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the DELETE so the
                /// `USING` clause on the policy is lifted for this statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn delete_insecurely<'ctx>(
                    self,
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<(), ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "delete_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        <Self as ::djogi::model::Model>::delete(self, ctx).await
                    }
                }

                /// Delete this row and return the pre-delete snapshot,
                /// bypassing the RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the DELETE so the
                /// policy's `USING` clause is lifted for this statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn delete_returning_insecurely<'ctx>(
                    self,
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<Self, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "delete_returning_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(
                            ctx,
                            "SET LOCAL row_security = off",
                            &[],
                        )
                        .await?;
                        <Self as ::djogi::model::Model>::delete_returning(self, ctx).await
                    }
                }

                /// Update this row and return old/new snapshots, bypassing the
                /// RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the UPDATE so both
                /// the `USING` (row visibility) and `WITH CHECK` (write restriction)
                /// clauses are lifted for this statement.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn update_returning_pair_insecurely<'ctx>(
                    self,
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<
                        ::djogi::query::ReturningPair<Self>,
                        ::djogi::DjogiError,
                    >,
                > + ::std::marker::Send + 'ctx {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "update_returning_pair_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(
                            ctx,
                            "SET LOCAL row_security = off",
                            &[],
                        )
                        .await?;
                        <Self as ::djogi::model::Model>::update_returning_pair(self, ctx).await
                    }
                }

                /// Return a lazy `QuerySet<Self>` without any tenant predicate.
                ///
                /// This method itself is synchronous — it just constructs the
                /// queryset; no SQL is issued until a terminal method (`.fetch_all`,
                /// `.fetch_one`, etc.) is called.
                ///
                /// **The `SET LOCAL row_security = off` bypass cannot be issued
                /// here** because there is no `DjogiContext` at queryset-construction
                /// time. To bypass RLS on the fetch, the caller must issue
                /// `ctx.raw_execute("SET LOCAL row_security = off", &[]).await?`
                /// inside an `atomic()` scope _before_ calling the terminal method.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted synchronously when the queryset is constructed.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                #[track_caller]
                pub fn objects_insecurely() -> ::djogi::query::QuerySet<Self> {
                    ::djogi::__private::tracing::warn!(
                        model = #model_name_str,
                        method = "objects_insecurely",
                        caller = %::std::panic::Location::caller(),
                        "insecure method bypasses tenant scope",
                    );
                    <Self as ::djogi::model::Model>::objects()
                }

                /// Bulk-update rows by primary key, bypassing the RLS tenant predicate.
                ///
                /// Issues `SET LOCAL row_security = off` before the UPDATE so the
                /// policy's `USING` clause does not filter the target rows.
                ///
                /// **Bypass only takes effect inside
                /// [`atomic()`](::djogi::transaction::atomic).** On a pool-backed
                /// context the call still executes but RLS remains active.
                ///
                /// A `tracing::warn!` with `model`, `method`, and `caller` fields
                /// is emitted on every call.
                ///
                /// **Audit**: every bypass call site is grep-able via `_insecurely`.
                ///
                /// # Type-parameter bounds
                ///
                /// The `F: Send + 'ctx` bound is tighter than [`bulk_update`]'s
                /// because the sync-wrapper + `#[track_caller]` pattern requires
                /// `impl Future + Send + 'ctx` as the return type, which in turn
                /// requires every value captured into the future to satisfy it.
                /// `A` is only the return type of `F` — it never lives inside the
                /// future — so it keeps its unbounded shape.
                ///
                /// `bulk_update`'s `async fn` surface infers bounds implicitly — we
                /// cannot use `async fn` here because `#[track_caller]` would not
                /// reflect the user's call site.
                #[track_caller]
                pub fn bulk_update_insecurely<'ctx, F, A>(
                    ctx: &'ctx mut ::djogi::context::DjogiContext,
                    ids: ::std::vec::Vec<<Self as ::djogi::model::Model>::Pk>,
                    closure: F,
                ) -> impl ::std::future::Future<
                    Output = ::std::result::Result<u64, ::djogi::DjogiError>,
                > + ::std::marker::Send + 'ctx
                where
                    F: ::std::ops::FnOnce(<Self as ::djogi::model::Model>::Fields) -> A
                        + ::std::marker::Send + 'ctx,
                    A: ::djogi::query::IntoAssignments,
                {
                    let __caller = ::std::panic::Location::caller();
                    async move {
                        ::djogi::__private::tracing::warn!(
                            model = #model_name_str,
                            method = "bulk_update_insecurely",
                            caller = %__caller,
                            "insecure method bypasses tenant scope",
                        );
                        ::djogi::__bypass::RawAccessExt::raw_execute(ctx, "SET LOCAL row_security = off", &[]).await?;
                        Self::bulk_update(ctx, ids, closure).await
                    }
                }

                #bulk_create_insecurely_body

                #bulk_upsert_insecurely_body
            }
        }
    } else {
        quote! {}
    };

    // -------------------------------------------------------------------------
    // Assemble the full impl block.
    // -------------------------------------------------------------------------
    quote! {
        #sequence_compile_err
        #create_with_id_impl
        #bulk_methods_impl
        #insecurely_impl

        // Satisfy the `Model: __sealed::Sealed` supertrait. The sealed
        // module is `#[doc(hidden)] pub`, so downstream hand-rolled
        // `impl Model for T` without an accompanying `impl Sealed`
        // fails to compile — closing the hostile-Model vector Codex
        // flagged on de42874 (malicious `table_name()` /
        // `descriptor().fields[].name` strings flowing into the SQL
        // emitter). The macro is the only supported emitter of both
        // impls.
        impl #impl_generics ::djogi::model::__sealed::Sealed for #name #ty_generics #where_clause {}

        impl #impl_generics ::djogi::model::Model for #name #ty_generics #where_clause {
            type Pk = #pk_type_tokens;

            // Typed field handles — the ZST generated alongside this impl by
            // `stubs::expand` (Phase 1) / `fields::expand` (Phase 2 Task 4).
            // Its `Default` impl lets `QuerySet::filter` construct the handle
            // inside the closure without the caller naming the type.
            type Fields = #fields_name;

            fn table_name() -> &'static str {
                #table
            }

            fn pk_value(&self) -> &Self::Pk {
                #pk_value_body
            }

            #descriptor_impl

            #proxy_default_filter_override
            #proxy_default_order_override

            fn get(
                ctx: &mut ::djogi::context::DjogiContext,
                id: Self::Pk,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + ::std::marker::Send {
                #get_body
            }

            fn create(
                ctx: &mut ::djogi::context::DjogiContext,
                value: Self,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + ::std::marker::Send {
                #create_body
            }

            fn save<'ctx>(
                &'ctx mut self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + ::std::marker::Send + 'ctx {
                // RPITIT captures `&self`'s lifetime into the returned future's
                // hidden lifetime, so the async block can borrow `&self.#f`
                // across `.await` without cloning. `Model: Send + Sync` makes
                // `&Self: Send` so the returned future satisfies `+ Send`.
                #save_body
            }

            fn delete(
                #delete_self_pat,
                ctx: &mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + ::std::marker::Send {
                #delete_body
            }

            fn refresh_from_db<'ctx>(
                &'ctx self,
                ctx: &'ctx mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + ::std::marker::Send + 'ctx {
                #refresh_body
            }

            // PG18 OLD/NEW RETURNING.
            fn update_returning_pair(
                mut self,
                ctx: &mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<
                    ::djogi::query::ReturningPair<Self>,
                    ::djogi::DjogiError,
                >,
            > + ::std::marker::Send {
                // `mut self` lets before_save take `&mut self` even though
                // the consuming path has already moved `self` in. Rust
                // permits `mut self` as a binding pattern in `fn` signatures.
                #update_returning_pair_body
            }

            fn delete_returning(
                #drp_self_pat,
                ctx: &mut ::djogi::context::DjogiContext,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + ::std::marker::Send {
                #delete_returning_body
            }

            // Bulk RETURNING save outbox hook.
            // Non-events models inherit `Model` default no-op.
            #emit_outbox_returning_save_override
            // Shared save-style on_commit cache invalidation hook.
            #emit_on_save_cache_invalidation_override
            // Bulk save-style on_commit cache invalidation hook.
            #emit_bulk_on_save_cache_invalidation_override

            // Cluster 8δ T8.6 — soft-deletable tombstone signal.
            // Emitted only for `#[model(soft_deletable)]` models; non-soft-deletable
            // models inherit the `Model` trait's default `false` impl.
            #delta_should_tombstone_override

            // Portable-field SQL emission override.
            // Replaces the default `Model::__djogi_emit_field_predicate`
            // hook (which returns `UnsupportedModel`) with a generated
            // `(field_name, LookupOp)` dispatch keyed off the shared
            // portable field metadata. Hand-written `Model` impls (test
            // fixtures, internal stubs) keep the trait default and
            // surface a typed error if a portable predicate against
            // them ever reaches SQL emission.
            #emit_field_predicate_override
        }
    }
}

// ── `Model::__djogi_emit_field_predicate` emission ──────────────────────
//
// The helper is split out of `expand` so `expand`'s body stays focused on
// CRUD emission. Inputs are the model's struct ident + the shared
// portable-field metadata vector built by
// `super::portable_field_emit::build`.

/// Build the `Model::__djogi_emit_field_predicate` override block.
///
/// Returns an empty token stream when `portable_field_info` is empty
/// (e.g., `pk = None` models — the early return in [`expand`] handles
/// those, but the helper stays defensive in case the caller threads
/// an empty slice for any reason). The empty-vec branch falls through
/// to the trait's default impl in `crate::model`, which returns
/// `PortablePredicateError::UnsupportedModel`.
///
/// The emitted match has one or more arms per known portable field
/// (Eq / Neq / Gt / Gte / Lt / Lte / Between / In / NotIn / IsNull /
/// IsNotNull / pattern family, depending on `field_kind`), one
/// catch-all arm per known field for non-portable operators, then a
/// single final unknown-field arm. Every helper call goes through
/// `::djogi::__private::query::portable_emit::*` so the macro emission
/// compiles in adopter crates without an explicit dep on
/// `crate::query::portable::emit`.
fn emit_djogi_emit_field_predicate(
    model_name: &syn::Ident,
    portable_field_info: &[PortableFieldEmitInfo],
) -> TokenStream {
    if portable_field_info.is_empty() {
        // Defensive: the trait default handles this case, so the
        // override emit is purely additive and we can skip it
        // entirely when there's nothing to dispatch on.
        return TokenStream::new();
    }

    let mut arms: Vec<TokenStream> = Vec::with_capacity(portable_field_info.len() * 6);

    for info in portable_field_info {
        let column = info.column_name.as_str();
        match info.field_kind {
            PortableFieldKind::Scalar => {
                let ty = &info.rust_type;
                arms.extend(scalar_arms(model_name, ty, column, /*ordering=*/ true));
                // Final per-field wildcard for unknown operators.
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::Bool => {
                let ty = &info.rust_type;
                // bool has no ordering / pattern surface; only equality
                // and list arms make sense.
                arms.extend(scalar_arms(
                    model_name, ty, column, /*ordering=*/ false,
                ));
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::String => {
                let ty = &info.rust_type;
                arms.extend(scalar_arms(
                    model_name, ty, column, /*ordering=*/ false,
                ));
                arms.extend(string_pattern_arms(model_name, column));
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::OptionScalar
            | PortableFieldKind::OptionBool
            | PortableFieldKind::OptionString
            | PortableFieldKind::OptionArray
            | PortableFieldKind::OptionRelationOrVisage => {
                // `option_inner_type` MUST be Some for portable Option*
                // kinds — `classify` populates it for OptionScalar /
                // OptionString / OptionBool / OptionRelationOrVisage. Defensive `expect` so a
                // future bug in `portable_field_emit::classify` fails
                // loudly during macro expansion rather than emitting
                // wrong arms.
                let inner = info
                    .option_inner_type
                    .as_ref()
                    .expect("Option* portable kind must carry an inner type");
                let supports_ordering = matches!(info.field_kind, PortableFieldKind::OptionScalar);
                arms.extend(option_arms(
                    model_name,
                    inner,
                    column,
                    supports_ordering,
                    info.tracked_wrapped,
                ));
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::RelationOrVisage => {
                let ty = &info.rust_type;
                // Root FK/O2O wrapper columns support equality and
                // membership because their cached value is the same key
                // wrapper SQL binds through. Traversal remains on the
                // SQL-only field view, and ordering/pattern operators are
                // intentionally unsupported.
                arms.extend(scalar_arms(
                    model_name, ty, column, /*ordering=*/ false,
                ));
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::Array => {
                let ty = &info.rust_type;
                // One-dimensional Postgres arrays support equality and list
                // membership with the same length/order/element semantics as
                // Rust `Vec<T>` equality. Array-specific operators remain
                // SQL-only on the explicit-PG surface.
                arms.extend(scalar_arms(
                    model_name, ty, column, /*ordering=*/ false,
                ));
                arms.push(quote! {
                    (#column, op) => ::std::result::Result::Err(
                        ::djogi::__private::query::PortablePredicateError::UnsupportedLookup {
                            field: #column,
                            op,
                        },
                    ),
                });
            }
            PortableFieldKind::Jsonb
            | PortableFieldKind::Spatial
            | PortableFieldKind::FtsComputed
            | PortableFieldKind::Unsupported => {
                let ty = &info.rust_type;
                // Non-portable kinds get a single catch-all arm
                // returning the typed `UnsupportedFieldType` error,
                // except for runtime-registered DjogiEnum codecs. The
                // model macro cannot observe sibling derives, so
                // DjogiEnum fields still classify as `Unsupported`
                // here and opt into SQL lowering through the registry
                // emitted by `#[derive(DjogiEnum)]`.
                // `LookupOp` is `#[non_exhaustive]`; this single arm
                // covers every current and future variant for the
                // field.
                arms.push(quote! {
                    (#column, _) => ::djogi::__private::query::portable_emit::emit_registered_custom::<#model_name, #ty>(
                        acc, ctx, #column, field,
                    ),
                });
            }
        }
    }

    // Final unknown-field arm — any (field_name, _) pair that did not
    // match a known field above. Returns `UnsupportedField` with the
    // observed `field_name`. This handles future field additions (a
    // user adds a new column but somehow constructs a portable
    // predicate against it before recompiling) and macros that
    // forward through the override (visage paths, dynamic fixtures)
    // without an exact match.
    arms.push(quote! {
        (field_name, _) => ::std::result::Result::Err(
            ::djogi::__private::query::PortablePredicateError::UnsupportedField {
                field: field_name,
            },
        ),
    });

    quote! {
        #[doc(hidden)]
        fn __djogi_emit_field_predicate(
            acc: &mut ::djogi::__private::pg::SqlAccumulator,
            field: &::djogi::types::FieldPredicate<Self>,
            ctx: ::djogi::__private::query::SqlEmitContext,
        ) -> ::std::result::Result<
            (),
            ::djogi::__private::query::PortablePredicateError,
        > {
            match (field.field_name(), field.op()) {
                #(#arms)*
            }
        }
    }
}

/// Emit the equality / list arms for a portable scalar-shaped field
/// (used by `Scalar`, `Bool`, `String`, and as the inner-payload
/// fallback for `Option*` kinds via [`option_arms`]).
///
/// `ordering = true` adds `Gt` / `Gte` / `Lt` / `Lte` / `Between`
/// arms; `false` skips them (used for `Bool` and `String` whose
/// portable surfaces don't expose ordering — `String` only gets
/// ordering through `explicit_pg_predicate()` because Postgres text
/// collation differs from Rust byte ordering, and `bool` has no
/// natural ordering at all).
fn scalar_arms(
    model_name: &syn::Ident,
    ty: &syn::Type,
    column: &str,
    ordering: bool,
) -> Vec<TokenStream> {
    let mut out = vec![
        quote! {
            (#column, ::djogi::types::LookupOp::Eq) =>
                ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                    acc, ctx, #column, " = ", field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::Neq) =>
                ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                    acc, ctx, #column, " <> ", field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::In) =>
                ::djogi::__private::query::portable_emit::emit_list::<#model_name, #ty>(
                    acc, ctx, #column, field, false,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::NotIn) =>
                ::djogi::__private::query::portable_emit::emit_list::<#model_name, #ty>(
                    acc, ctx, #column, field, true,
                ),
        },
    ];

    if ordering {
        out.extend([
            quote! {
                (#column, ::djogi::types::LookupOp::Gt) =>
                    ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                        acc, ctx, #column, " > ", field,
                    ),
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Gte) =>
                    ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                        acc, ctx, #column, " >= ", field,
                    ),
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Lt) =>
                    ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                        acc, ctx, #column, " < ", field,
                    ),
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Lte) =>
                    ::djogi::__private::query::portable_emit::emit_value::<#model_name, #ty>(
                        acc, ctx, #column, " <= ", field,
                    ),
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Between) =>
                    ::djogi::__private::query::portable_emit::emit_pair::<#model_name, #ty>(
                        acc, ctx, #column, field,
                    ),
            },
        ]);
    }
    out
}

/// Emit the LIKE/ILIKE family arms for a non-Option `String` field.
///
/// Sassi's `Field<T, String>` exposes the case-sensitive `Contains` /
/// `StartsWith` / `EndsWith` ops AND the ASCII-stable case-insensitive
/// `IContains` / `IStartsWith` / `IEndsWith` / `IExact` ops. Each
/// arm dispatches to the hidden
/// `portable_emit::emit_string_pattern` helper with the matching
/// `PatternOp` variant; the helper escapes user-supplied `%` / `_` /
/// `\\` and wraps the pattern with the substring / prefix / suffix
/// shape per op. `IExact` emits a no-wildcard `COLLATE "C" ILIKE`
/// comparison so exact user input cannot become a wildcard match.
fn string_pattern_arms(_model_name: &syn::Ident, column: &str) -> Vec<TokenStream> {
    vec![
        quote! {
            (#column, ::djogi::types::LookupOp::Contains) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::Contains,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::IContains) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::IContains,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::StartsWith) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::StartsWith,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::IStartsWith) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::IStartsWith,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::EndsWith) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::EndsWith,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::IEndsWith) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::IEndsWith,
                    field,
                ),
        },
        quote! {
            (#column, ::djogi::types::LookupOp::IExact) =>
                ::djogi::__private::query::portable_emit::emit_string_pattern(
                    acc, ctx, #column,
                    ::djogi::__private::query::portable_emit::PatternOp::IExact,
                    field,
                ),
        },
    ]
}

/// Emit arms for a portable `Option<U>` field.
///
/// Each arm tries the `Option<U>` payload shape first (matching
/// `DjogiField::eq(Some|None)` / direct `.in_([Some, None])` calls),
/// then falls back to the inner `U` (matching `.some().eq(_)` /
/// `.some().in_([_])` calls via `DjogiPresentField`). If neither shape
/// matches, the arm returns `ValueTypeMismatch` rather than panicking.
///
/// `IsNull` / `IsNotNull` arms use the helper-side `emit_null` shape
/// directly because Sassi carries an inert `Arc<()>` payload for them
/// and `emit_null` does not consume `field`.
///
/// Direct `Option<U>` ordering returns `UnsupportedLookup` because
/// Rust's `Option` ordering (`None < Some(_)`) does not match SQL
/// three-valued NULL semantics. Inner-`U` ordering (`PresentField`
/// payloads) is supported when `supports_ordering = true` (i.e. the
/// kind is `OptionScalar`).
///
/// # Tracked-wrapped fields
///
/// `tracked_wrapped = true` indicates the original Rust column type was
/// `Tracked<Option<U>>` (or `Tracked<Option<String>>` /
/// `Tracked<Option<bool>>`). The `IntoPortableFieldValue<Tracked<V>> for V`
/// blanket on the field-side surface wraps caller arguments into
/// `Tracked::new(_)` before storing them in the type-erased
/// `FieldPredicate::value` payload, so the predicate value reaching
/// this dispatch is `Tracked<Option<U>>` rather than the bare
/// `Option<U>` the original arm chain expected. We prepend a
/// Tracked-aware fallback that downcasts to `Tracked<Option<U>>`
/// (or `Tracked<U>` for the `.some()`-style payload, which today is
/// unreachable from the public API but kept symmetric for future
/// surface additions) and forwards through the same `emit_option_*`
/// helpers using `Deref::deref` to project the inner reference.
/// Without this fallback every `f.tracked_optional().eq(Some(v))` /
/// `.neq(_)` / `.in_([_])` / `.not_in([_])` call against a
/// `Tracked<Option<U>>` column lands on `ValueTypeMismatch` at runtime.
fn option_arms(
    model_name: &syn::Ident,
    inner: &syn::Type,
    column: &str,
    supports_ordering: bool,
    tracked_wrapped: bool,
) -> Vec<TokenStream> {
    let mut out = Vec::with_capacity(16);

    // Pre-built Tracked-aware fallback fragments. Each yields zero-or-more
    // `if let Some(value) = value_as::<Tracked<…>>(field) { return …; }`
    // statements that run before the bare-`Option<U>` / `U` chain. Empty
    // when the field is not Tracked-wrapped — the existing chain stays
    // identical to the non-Tracked emission so call sites do not pay any
    // extra type-tag downcast for plain `Option<U>` columns.
    let tracked_eq_prelude = if tracked_wrapped {
        quote! {
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::djogi::Tracked<::std::option::Option<#inner>>,
                >(field)
            {
                return ::djogi::__private::query::portable_emit::emit_option_eq::<#inner>(
                    acc, ctx, #column, ::std::ops::Deref::deref(value),
                );
            }
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::djogi::Tracked<#inner>,
                >(field)
            {
                return ::djogi::__private::query::portable_emit::emit_value_ref::<#inner>(
                    acc, ctx, #column, " = ", ::std::ops::Deref::deref(value),
                );
            }
        }
    } else {
        TokenStream::new()
    };
    let tracked_neq_prelude = if tracked_wrapped {
        quote! {
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::djogi::Tracked<::std::option::Option<#inner>>,
                >(field)
            {
                return ::djogi::__private::query::portable_emit::emit_option_neq::<#inner>(
                    acc, ctx, #column, ::std::ops::Deref::deref(value),
                );
            }
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::djogi::Tracked<#inner>,
                >(field)
            {
                return ::djogi::__private::query::portable_emit::emit_value_ref::<#inner>(
                    acc, ctx, #column, " <> ", ::std::ops::Deref::deref(value),
                );
            }
        }
    } else {
        TokenStream::new()
    };
    // For `in_` / `not_in` the wrapped shape is
    // `Vec<Tracked<Option<U>>>` (not `Tracked<Vec<Option<U>>>`):
    // `IntoPortableFieldValue` runs per-element on the user's
    // `Vec<Option<U>>` argument, wrapping each `Option<U>` into a
    // `Tracked<Option<U>>` before sassi collects them. The fallback
    // therefore downcasts the stored payload as `Vec<Tracked<Option<U>>>`,
    // projects each entry through `Deref` into a fresh `Vec<Option<U>>`
    // (Option<U> is `Clone` whenever U is — required by the bind path
    // already), then forwards to the existing scalar-shape helpers so
    // the SQL emission stays identical to the non-Tracked path.
    let tracked_in_prelude = if tracked_wrapped {
        quote! {
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::djogi::Tracked<::std::option::Option<#inner>>>,
                >(field)
            {
                let projected: ::std::vec::Vec<::std::option::Option<#inner>> = values
                    .iter()
                    .map(|tracked| <
                        ::std::option::Option<#inner> as ::std::clone::Clone
                    >::clone(::std::ops::Deref::deref(tracked)))
                    .collect();
                return ::djogi::__private::query::portable_emit::emit_option_in::<#inner>(
                    acc, ctx, #column, &projected, false,
                );
            }
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::djogi::Tracked<#inner>>,
                >(field)
            {
                let projected: ::std::vec::Vec<#inner> = values
                    .iter()
                    .map(|tracked| <#inner as ::std::clone::Clone>::clone(
                        ::std::ops::Deref::deref(tracked),
                    ))
                    .collect();
                return ::djogi::__private::query::portable_emit::emit_present_list::<#inner>(
                    acc, ctx, #column, &projected, false,
                );
            }
        }
    } else {
        TokenStream::new()
    };
    let tracked_not_in_prelude = if tracked_wrapped {
        quote! {
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::djogi::Tracked<::std::option::Option<#inner>>>,
                >(field)
            {
                let projected: ::std::vec::Vec<::std::option::Option<#inner>> = values
                    .iter()
                    .map(|tracked| <
                        ::std::option::Option<#inner> as ::std::clone::Clone
                    >::clone(::std::ops::Deref::deref(tracked)))
                    .collect();
                return ::djogi::__private::query::portable_emit::emit_option_in::<#inner>(
                    acc, ctx, #column, &projected, true,
                );
            }
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::djogi::Tracked<#inner>>,
                >(field)
            {
                let projected: ::std::vec::Vec<#inner> = values
                    .iter()
                    .map(|tracked| <#inner as ::std::clone::Clone>::clone(
                        ::std::ops::Deref::deref(tracked),
                    ))
                    .collect();
                return ::djogi::__private::query::portable_emit::emit_present_list::<#inner>(
                    acc, ctx, #column, &projected, true,
                );
            }
        }
    } else {
        TokenStream::new()
    };

    // Eq — Tracked<Option<U>> / Tracked<U> first (when Tracked-wrapped),
    // then direct Option<U>, then inner U via `.some()`.
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::Eq) => {
            #tracked_eq_prelude
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::option::Option<#inner>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_option_eq::<#inner>(
                    acc, ctx, #column, value,
                )
            } else if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<#inner>(field)
            {
                ::djogi::__private::query::portable_emit::emit_value_ref::<#inner>(
                    acc, ctx, #column, " = ", value,
                )
            } else {
                ::std::result::Result::Err(
                    ::djogi::__private::query::PortablePredicateError::ValueTypeMismatch {
                        field: #column,
                        op: field.op(),
                    },
                )
            }
        },
    });

    // Neq — Tracked-aware fallback, then direct Option<U>, then inner U.
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::Neq) => {
            #tracked_neq_prelude
            if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::option::Option<#inner>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_option_neq::<#inner>(
                    acc, ctx, #column, value,
                )
            } else if let ::std::option::Option::Some(value) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<#inner>(field)
            {
                ::djogi::__private::query::portable_emit::emit_value_ref::<#inner>(
                    acc, ctx, #column, " <> ", value,
                )
            } else {
                ::std::result::Result::Err(
                    ::djogi::__private::query::PortablePredicateError::ValueTypeMismatch {
                        field: #column,
                        op: field.op(),
                    },
                )
            }
        },
    });

    // In — Tracked-aware fallback, then direct Vec<Option<U>>, then inner Vec<U>.
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::In) => {
            #tracked_in_prelude
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::std::option::Option<#inner>>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_option_in::<#inner>(
                    acc, ctx, #column, values, false,
                )
            } else if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<#inner>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_present_list::<#inner>(
                    acc, ctx, #column, values, false,
                )
            } else {
                ::std::result::Result::Err(
                    ::djogi::__private::query::PortablePredicateError::ValueTypeMismatch {
                        field: #column,
                        op: field.op(),
                    },
                )
            }
        },
    });

    // NotIn — Tracked-aware fallback, then direct Vec<Option<U>>, then inner Vec<U>.
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::NotIn) => {
            #tracked_not_in_prelude
            if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<::std::option::Option<#inner>>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_option_in::<#inner>(
                    acc, ctx, #column, values, true,
                )
            } else if let ::std::option::Option::Some(values) =
                <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                    ::std::vec::Vec<#inner>,
                >(field)
            {
                ::djogi::__private::query::portable_emit::emit_present_list::<#inner>(
                    acc, ctx, #column, values, true,
                )
            } else {
                ::std::result::Result::Err(
                    ::djogi::__private::query::PortablePredicateError::ValueTypeMismatch {
                        field: #column,
                        op: field.op(),
                    },
                )
            }
        },
    });

    // IsNull / IsNotNull — inert `()` payload.
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::IsNull) =>
            ::djogi::__private::query::portable_emit::emit_null(acc, ctx, #column, true),
    });
    out.push(quote! {
        (#column, ::djogi::types::LookupOp::IsNotNull) =>
            ::djogi::__private::query::portable_emit::emit_null(acc, ctx, #column, false),
    });

    if supports_ordering {
        // Ordering arms — only meaningful for the inner `U` payload
        // (i.e. predicates built via `.some().gt(_)` / `.gte(_)` /
        // `.lt(_)` / `.lte(_)` / `.between(_, _)`). Direct
        // `Option<U>` ordering is rejected at the `DjogiField` level
        // (no method exposed) and surfaces here as
        // `UnsupportedLookup` if a caller somehow constructs it.
        //
        // For Tracked-wrapped fields the same `.some()` surface is
        // unreachable today (`Tracked<Option<U>>` does not implement
        // the `DjogiField<M, Option<U>>::some` extension), but a
        // Tracked-aware preamble keeps the dispatch symmetric: a
        // future API addition that exposes `.some()` for Tracked
        // fields will route through the same arms without a second
        // macro change.
        // Tracked-aware ordering preludes capture the downcast result and
        // forward to `emit_value_ref::<Tracked<U>>` / inline the
        // `BETWEEN` emission for the pair shape. `Tracked<T>: ToSql`
        // forwards binds to the inner value, so the bound SQL bytes
        // match what a bare-`U` bind would produce — column comparison
        // stays exact across the Tracked wrapper.
        let tracked_ord_prelude = |op_token: &str| {
            if tracked_wrapped {
                quote! {
                    if let ::std::option::Option::Some(value) =
                        <::djogi::types::FieldPredicate<#model_name>>::value_as::<
                            ::djogi::Tracked<#inner>,
                        >(field)
                    {
                        return ::djogi::__private::query::portable_emit::emit_value_ref::<
                            ::djogi::Tracked<#inner>,
                        >(acc, ctx, #column, #op_token, value);
                    }
                }
            } else {
                TokenStream::new()
            }
        };
        let tracked_between_prelude = if tracked_wrapped {
            quote! {
                if <::djogi::types::FieldPredicate<#model_name>>::value_as::<(
                    ::djogi::Tracked<#inner>,
                    ::djogi::Tracked<#inner>,
                )>(field).is_some()
                {
                    return ::djogi::__private::query::portable_emit::emit_pair::<
                        #model_name, ::djogi::Tracked<#inner>,
                    >(acc, ctx, #column, field);
                }
            }
        } else {
            TokenStream::new()
        };
        let gt_prelude = tracked_ord_prelude(" > ");
        let gte_prelude = tracked_ord_prelude(" >= ");
        let lt_prelude = tracked_ord_prelude(" < ");
        let lte_prelude = tracked_ord_prelude(" <= ");
        out.extend([
            quote! {
                (#column, ::djogi::types::LookupOp::Gt) => {
                    #gt_prelude
                    ::djogi::__private::query::portable_emit::emit_value::<
                        #model_name, #inner,
                    >(acc, ctx, #column, " > ", field)
                },
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Gte) => {
                    #gte_prelude
                    ::djogi::__private::query::portable_emit::emit_value::<
                        #model_name, #inner,
                    >(acc, ctx, #column, " >= ", field)
                },
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Lt) => {
                    #lt_prelude
                    ::djogi::__private::query::portable_emit::emit_value::<
                        #model_name, #inner,
                    >(acc, ctx, #column, " < ", field)
                },
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Lte) => {
                    #lte_prelude
                    ::djogi::__private::query::portable_emit::emit_value::<
                        #model_name, #inner,
                    >(acc, ctx, #column, " <= ", field)
                },
            },
            quote! {
                (#column, ::djogi::types::LookupOp::Between) => {
                    #tracked_between_prelude
                    ::djogi::__private::query::portable_emit::emit_pair::<
                        #model_name, #inner,
                    >(acc, ctx, #column, field)
                },
            },
        ]);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{build_old_new_returning_suffix, old_new_returning_alias};

    #[test]
    fn old_new_returning_alias_matches_runtime_prefix_contract() {
        assert_eq!(old_new_returning_alias("__djogi_old__", 3, "title"), "o3");
        assert_eq!(old_new_returning_alias("__djogi_new__", 3, "title"), "n3");
        assert_eq!(
            old_new_returning_alias("rel_owner_id.", 3, "title"),
            "rel_owner_id.title"
        );
    }

    #[test]
    fn build_old_new_returning_suffix_uses_compact_old_and_new_aliases() {
        let cols = ["id", "title"];
        let sql = build_old_new_returning_suffix(&cols, true);

        assert!(
            sql.contains("RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)"),
            "missing RETURNING WITH clause: {sql}"
        );
        assert!(sql.contains("__djogi_old.id AS \"o0\""), "{sql}");
        assert!(sql.contains("__djogi_old.title AS \"o1\""), "{sql}");
        assert!(sql.contains("__djogi_new.id AS \"n0\""), "{sql}");
        assert!(sql.contains("__djogi_new.title AS \"n1\""), "{sql}");
    }

    #[test]
    fn build_old_new_returning_suffix_uses_old_only_shape_for_delete() {
        let cols = ["id", "title"];
        let sql = build_old_new_returning_suffix(&cols, false);

        assert!(
            sql.contains("RETURNING WITH (OLD AS __djogi_old)"),
            "missing delete RETURNING WITH clause: {sql}"
        );
        assert!(sql.contains("__djogi_old.id AS \"o0\""), "{sql}");
        assert!(sql.contains("__djogi_old.title AS \"o1\""), "{sql}");
        assert!(
            !sql.contains("__djogi_new."),
            "delete suffix must not include NEW projection: {sql}"
        );
    }
}
