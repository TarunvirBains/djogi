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
//! # Why `&mut DjogiContext` (not `impl sqlx::Executor`)
//!
//! Phase 4 v3's Q1 resolution flipped the API from "each method is generic over
//! `sqlx::Executor`" to "each method takes `&mut DjogiContext`". The context
//! carries either a pool or an active transaction and pattern-matches on the
//! variant at each sqlx boundary in the generated body (via
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
//! direct dependency. Paths like `::sqlx::query_as` or `::inventory::iter`
//! would fail with E0433 unless the user explicitly added those crates. To
//! avoid that, all external crate references are routed through
//! `::djogi::__private::sqlx`, `::djogi::__private::inventory`, and
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
//!   columns — the Postgres defaults (`generate_id()`, `now()`) populate them.
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
//! # `pk = "none"` special case
//!
//! Models with `#[model(pk = "none")]` have no framework-injected `id` field
//! and declare their own primary key (possibly composite). Phase 1 does NOT
//! emit `impl Model for T` for these — the `Model` trait's `type Pk` requires
//! `sqlx::Encode<'q, Postgres>`, which `()` does not implement, and choosing
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

/// Generate the full `impl Model for T` block.
///
/// Called from `mod.rs` after `inject::expand` has mutated `struct_item`, so
/// the field list already includes `id`, `created_at`, and `updated_at` at the
/// front.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    _field_attrs: &[FieldAttrs],
) -> TokenStream {
    // pk = "none" skips Model impl in Phase 1 — Task 8 adds a composite-PK-
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

    let name = &struct_item.ident;
    let fields_name = format_ident!("{}Fields", name);
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();
    let table = &model_attrs.table;

    // -------------------------------------------------------------------------
    // Associated Pk type and pk_value() body — vary by PK strategy.
    // (pk = "none" is handled by the early return above.)
    // -------------------------------------------------------------------------
    let (pk_type_tokens, pk_value_body) = match model_attrs.pk {
        PkStrategy::HeerId => (quote! { ::djogi::types::HeerId }, quote! { &self.id }),
        PkStrategy::RanjId => (quote! { ::djogi::types::RanjId }, quote! { &self.id }),
        PkStrategy::Serial => (quote! { i32 }, quote! { &self.id }),
        PkStrategy::None => unreachable!("handled by early return"),
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

    let n_user = user_fields.len();

    // -------------------------------------------------------------------------
    // `create` SQL: INSERT with user fields only; DB handles the framework
    // columns via column defaults. RETURNING * brings back the full row.
    //
    // For zero-user-field models we must use `DEFAULT VALUES` — empty parens
    // `()` are invalid SQL and `INSERT ... () VALUES ()` is rejected by
    // Postgres. `DEFAULT VALUES` is standard SQL and Postgres-supported.
    // -------------------------------------------------------------------------
    let insert_sql = if n_user == 0 {
        format!("INSERT INTO {table} DEFAULT VALUES RETURNING *")
    } else {
        let insert_columns: Vec<String> = user_fields.iter().map(|i| i.to_string()).collect();
        let insert_col_list = insert_columns.join(", ");
        let insert_placeholders: Vec<String> = (1..=n_user).map(|i| format!("${i}")).collect();
        let insert_placeholder_list = insert_placeholders.join(", ");
        format!(
            "INSERT INTO {table} ({insert_col_list}) VALUES ({insert_placeholder_list}) RETURNING *"
        )
    };
    let create_binds: Vec<TokenStream> = user_fields
        .iter()
        .map(|f| quote! { .bind(&value.#f) })
        .collect();

    // -------------------------------------------------------------------------
    // `save` SQL: UPDATE with user fields + SQL-side `updated_at = now()`,
    // WHERE id = $M.
    //
    // Parameters: $1..$n_user = user fields, $n_user+1 = id.
    //
    // Using Postgres `now()` (not a client-side `OffsetDateTime::now_utc()`
    // bound as a parameter) keeps the timestamp source consistent with the
    // column's `DEFAULT now()` on INSERT: all writes use the same server
    // clock, so `created_at <= updated_at` always holds even across clients
    // with drifted clocks.
    //
    // Zero-user-field edge case: when n_user == 0 the UPDATE has no user
    // SET clauses — emit only `SET updated_at = now()` to avoid the invalid
    // `SET , updated_at = now()` (leading comma) SQL.
    // -------------------------------------------------------------------------
    let set_clauses: Vec<String> = user_fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{} = ${}", f, i + 1))
        .collect();
    let id_param = n_user + 1;
    let save_sql = if n_user == 0 {
        format!("UPDATE {table} SET updated_at = now() WHERE id = ${id_param} RETURNING *")
    } else {
        format!(
            "UPDATE {table} SET {set_list}, updated_at = now() WHERE id = ${id_param} RETURNING *",
            set_list = set_clauses.join(", "),
        )
    };

    // save field binds — bind by reference to user fields on `&self`. No
    // clone is required: the returned `impl Future + Send` captures `&self`'s
    // lifetime via RPITIT elision, so the async block can borrow `self.#f`
    // across `.await`. This means user field types don't have to implement
    // `Clone` — only `sqlx::Encode`, which the trait's contract already implies.
    let save_field_binds: Vec<TokenStream> = user_fields
        .iter()
        .map(|f| quote! { .bind(&self.#f) })
        .collect();

    // id bind for the save WHERE clause. Captured inline; HeerId needs
    // `.as_i64()` to bind as `BIGINT`, others bind directly.
    let save_id_bind = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(self.id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(self.id) },
        PkStrategy::Serial => quote! { .bind(self.id) },
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `get` SQL: SELECT * WHERE id = $1. `id` comes in as an owned Self::Pk.
    // -------------------------------------------------------------------------
    let get_sql = format!("SELECT * FROM {table} WHERE id = $1");

    let id_bind_for_get = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(id) },
        PkStrategy::Serial => quote! { .bind(id) },
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `refresh_from_db` — same query as get, but binds `&self.id` directly.
    // Like save, RPITIT captures `&self` so no pre-capture clone is needed.
    // -------------------------------------------------------------------------
    let refresh_id_bind = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(self.id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(self.id) },
        PkStrategy::Serial => quote! { .bind(self.id) },
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `delete` SQL: DELETE WHERE id = $1. `self` is consumed (moved in).
    // -------------------------------------------------------------------------
    let delete_sql = format!("DELETE FROM {table} WHERE id = $1");

    let owned_pk_bind = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(self.id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(self.id) },
        PkStrategy::Serial => quote! { .bind(self.id) },
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
    // Per-method async bodies.
    // -------------------------------------------------------------------------
    // Every body pattern-matches on the context's inner variant at the sqlx
    // boundary. The match arms feed the same `sqlx::query_as` / `sqlx::query`
    // chain into either `fetch_*`(&*pool) or `fetch_*`(&mut **tx) — inline
    // match is explicit, free of GAT gymnastics, and costs one pattern match
    // per call. See djogi/src/context.rs for the full rationale.
    let get_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query_as::<
                ::djogi::__private::sqlx::Postgres,
                Self,
            >(#get_sql)
            #id_bind_for_get;
            let result = match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.fetch_optional(&*__pool).await?
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.fetch_optional(&mut **__tx).await?
                }
            };
            result.ok_or_else(|| ::djogi::DjogiError::not_found(#table))
        }
    };

    let create_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query_as::<
                ::djogi::__private::sqlx::Postgres,
                Self,
            >(#insert_sql)
            #(#create_binds)*;
            let row = match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.fetch_one(&*__pool).await?
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.fetch_one(&mut **__tx).await?
                }
            };
            // Phase 4 Task 6 — outbox emission (no-op for non-events models).
            // Runs in the same ctx so a transactional caller gets the
            // outbox row committed/rolled back atomically with `row`.
            #emit_outbox_create
            ::std::result::Result::Ok(row)
        }
    };

    let save_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query_as::<
                ::djogi::__private::sqlx::Postgres,
                Self,
            >(#save_sql)
            #(#save_field_binds)*
            #save_id_bind;
            let row: Self = match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.fetch_one(&*__pool).await?
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.fetch_one(&mut **__tx).await?
                }
            };
            *self = row;
            // Phase 4 Task 6 — outbox payload must reflect the DB-refreshed
            // values (triggers, column defaults), so emission runs AFTER the
            // `*self = row` rehydration. No-op for non-events models.
            #emit_outbox_save
            ::std::result::Result::Ok(())
        }
    };

    let delete_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query(#delete_sql)
            #owned_pk_bind;
            match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.execute(&*__pool).await?;
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.execute(&mut **__tx).await?;
                }
            }
            // Phase 4 Task 6 — outbox carries the pre-delete snapshot
            // (reads `self` before it drops at function scope end).
            // No-op for non-events models.
            #emit_outbox_delete
            ::std::result::Result::Ok(())
        }
    };

    let refresh_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query_as::<
                ::djogi::__private::sqlx::Postgres,
                Self,
            >(#get_sql)
            #refresh_id_bind;
            let result = match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.fetch_optional(&*__pool).await?
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.fetch_optional(&mut **__tx).await?
                }
            };
            result.ok_or_else(|| ::djogi::DjogiError::not_found(#table))
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
            format!("INSERT INTO {table} (id) VALUES ($1) ON CONFLICT (id) DO NOTHING RETURNING *")
        } else {
            let cols: Vec<String> = user_fields.iter().map(|i| i.to_string()).collect();
            let col_list = cols.join(", ");
            // id binds to $1; user fields shift by 1 → $2..$n_user+1
            let vals: Vec<String> = (2..=n_user + 1).map(|n| format!("${n}")).collect();
            let val_list = vals.join(", ");
            format!(
                "INSERT INTO {table} (id, {col_list}) VALUES ($1, {val_list}) ON CONFLICT (id) DO NOTHING RETURNING *"
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
                    let q = ::djogi::__private::sqlx::query_as::<
                        ::djogi::__private::sqlx::Postgres,
                        Self,
                    >(#insert_with_id_sql)
                        .bind(id.as_i64())
                        #(#create_binds)*;
                    let maybe_row = match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                        ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                            q.fetch_optional(&*__pool).await?
                        }
                        ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                            q.fetch_optional(&mut **__tx).await?
                        }
                    };

                    ::std::result::Result::Ok(maybe_row.unwrap_or_else(|| {
                        let mut v = value;
                        v.id = id;
                        v
                    }))
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
    // Emits a leading "(" then push_bind per user field with ", "
    // separators, then a trailing ")". Uses a `__first` flag on the
    // outer row loop for row-separator handling.
    //
    // Zero-user-field branch never reaches this — n_user >= 1 gated by
    // the per-method body.
    let per_row_binds: TokenStream = if n_user == 0 {
        quote! {}
    } else {
        let first_field = &user_fields[0];
        let rest_fields = &user_fields[1..];
        quote! {
            __qb.push("(");
            __qb.push_bind(&row.#first_field);
            #(
                __qb.push(", ");
                __qb.push_bind(&row.#rest_fields);
            )*
            __qb.push(")");
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
    // RanjId / Serial bind directly. We route through FieldRef::in_list
    // which takes `IntoIterator<Item = V>` where `V: IntoFilterValue`.
    // HeerId / RanjId / i32 (Serial) all `impl IntoFilterValue`, so the
    // same expression compiles for every pk_type.
    //
    // `Self::Pk` is already the right type at macro expansion time
    // (see `pk_type_tokens` above), so we can forward `ids` verbatim
    // into `.in_list(ids)`.
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
            /// `Self::objects().filter(|f| f.id().in_list(ids)).update(closure).execute(ctx)`.
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
                    .filter(|f| f.id().in_list(ids))
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
            /// `Self::objects().filter(|f| f.id().in_list(ids)).update(closure).execute(ctx)`;
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
                    .filter(|f| f.id().in_list(ids))
                    .update(closure)
                    .execute(ctx)
                    .await
            }
        }
    };

    // bulk_create / bulk_upsert are elided for zero-user-field models.
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
    } else {
        let insert_prefix = format!("INSERT INTO {table} ({bulk_insert_col_list}) VALUES ");
        quote! {
            /// Bulk-insert every row in `rows` and return the rehydrated
            /// results.
            ///
            /// One `INSERT` round trip emitting
            /// `INSERT INTO <table> (<user-cols>) VALUES (...), (...) RETURNING *`.
            /// Framework columns (`id`, `created_at`, `updated_at`) are
            /// populated by their column defaults and surface in the
            /// returned rows.
            ///
            /// Empty `rows` short-circuits to `Ok(Vec::new())` without
            /// SQL — an empty `VALUES ()` clause is invalid Postgres.
            ///
            /// Postgres caps bound parameters at 65_535. With `N` user
            /// columns per model, the effective cap is `65_535 / N`
            /// rows per call. Chunk larger batches at the call site.
            ///
            /// When the model has `#[model(events)]`, outbox rows are
            /// written per inserted row **after** rehydration (so the
            /// outbox payload reflects DB-truth column defaults and
            /// trigger mutations). Runs inside the caller's
            /// transaction / atomic scope when `ctx` holds one.
            pub async fn bulk_create(
                ctx: &mut ::djogi::context::DjogiContext,
                rows: ::std::vec::Vec<Self>,
            ) -> ::std::result::Result<::std::vec::Vec<Self>, ::djogi::DjogiError> {
                if rows.is_empty() {
                    return ::std::result::Result::Ok(::std::vec::Vec::new());
                }
                let mut __qb: ::djogi::__private::sqlx::QueryBuilder<
                    '_,
                    ::djogi::__private::sqlx::Postgres,
                > = ::djogi::__private::sqlx::QueryBuilder::new(#insert_prefix);
                {
                    let mut __first = true;
                    for row in rows.iter() {
                        if __first { __first = false; } else { __qb.push(", "); }
                        #per_row_binds
                    }
                }
                __qb.push(" RETURNING *");
                let __q = __qb.build_query_as::<Self>();
                let created: ::std::vec::Vec<Self> =
                    match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                        ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                            __q.fetch_all(&*__pool).await?
                        }
                        ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                            __q.fetch_all(&mut **__tx).await?
                        }
                    };
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
        let do_update_set_clause = format!(" DO UPDATE SET {bulk_upsert_set_list} RETURNING *");

        // For bulk_upsert the id column is included up front so
        // callers can upsert with pre-allocated ids. Per-row bind tail
        // needs the id bind before the user-field binds.
        let pk_bind_for_upsert = match model_attrs.pk {
            PkStrategy::HeerId => quote! { __qb.push_bind(row.id.as_i64()); },
            PkStrategy::RanjId => quote! { __qb.push_bind(row.id); },
            PkStrategy::Serial => quote! { __qb.push_bind(row.id); },
            PkStrategy::None => unreachable!("handled by early return"),
        };
        let upsert_per_row_binds: TokenStream = {
            let all_fields_iter = user_fields.iter();
            quote! {
                __qb.push("(");
                #pk_bind_for_upsert
                #(
                    __qb.push(", ");
                    __qb.push_bind(&row.#all_fields_iter);
                )*
                __qb.push(")");
            }
        };

        quote! {
            /// Bulk-upsert — `INSERT ... ON CONFLICT (<cols>) DO UPDATE SET ...`.
            ///
            /// Inserts every row in `rows`; on conflict against the
            /// `conflict_cols` key, updates every user field plus
            /// `updated_at = now()` with the incoming values
            /// (`EXCLUDED.*`). Returns the rehydrated rows —
            /// `RETURNING *` emits one row per input regardless of
            /// whether it was inserted or updated.
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
            /// [`HeerId::generate_many(ctx, n)`](::djogi::types::HeerId::generate_many)
            /// the ids up front — row.id is inserted verbatim, no
            /// column default fires.
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

                let mut __qb: ::djogi::__private::sqlx::QueryBuilder<
                    '_,
                    ::djogi::__private::sqlx::Postgres,
                > = ::djogi::__private::sqlx::QueryBuilder::new(#insert_prefix);
                {
                    let mut __first = true;
                    for row in rows.iter() {
                        if __first { __first = false; } else { __qb.push(", "); }
                        #upsert_per_row_binds
                    }
                }
                __qb.push(" ON CONFLICT (");
                {
                    let mut __first = true;
                    for col in conflict_cols {
                        if __first { __first = false; } else { __qb.push(", "); }
                        __qb.push(*col);
                    }
                }
                __qb.push(")");
                __qb.push(#do_update_set_clause);
                let __q = __qb.build_query_as::<Self>();
                let created: ::std::vec::Vec<Self> =
                    match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                        ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                            __q.fetch_all(&*__pool).await?
                        }
                        ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                            __q.fetch_all(&mut **__tx).await?
                        }
                    };
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
                     ON CONFLICT ({key_str}) DO NOTHING RETURNING *"
                )
            };
            let select_by_key_sql = format!("SELECT * FROM {table} WHERE {key_str} = $1 LIMIT 1");

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

            quote! {
                /// Idempotent create — insert a row keyed off the
                /// descriptor's `idempotency_key` attribute, or
                /// return the existing row when the key conflicts.
                ///
                /// Shape:
                /// `INSERT INTO <table> (<user-cols>) VALUES ($1,...)
                ///  ON CONFLICT (<key>) DO NOTHING RETURNING *`.
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
                    let q = ::djogi::__private::sqlx::query_as::<
                        ::djogi::__private::sqlx::Postgres,
                        Self,
                    >(#insert_or_nothing_sql)
                    #(.bind(&row.#user_fields))*;
                    let maybe_inserted: ::std::option::Option<Self> =
                        match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                            ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                                q.fetch_optional(&*__pool).await?
                            }
                            ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                                q.fetch_optional(&mut **__tx).await?
                            }
                        };
                    match maybe_inserted {
                        ::std::option::Option::Some(__row) => {
                            #create_or_find_outbox
                            ::std::result::Result::Ok((__row, true))
                        }
                        ::std::option::Option::None => {
                            // Conflict fired — re-SELECT the
                            // existing row by the idempotency key.
                            // The key-field value comes from the
                            // caller's `row` input (unchanged across
                            // the insert attempt).
                            let q = ::djogi::__private::sqlx::query_as::<
                                ::djogi::__private::sqlx::Postgres,
                                Self,
                            >(#select_by_key_sql)
                            .bind(&row.#key_ident);
                            let existing: Self =
                                match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                                    ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                                        q.fetch_one(&*__pool).await?
                                    }
                                    ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                                        q.fetch_one(&mut **__tx).await?
                                    }
                                };
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
    // Assemble the full impl block.
    // -------------------------------------------------------------------------
    quote! {
        #create_with_id_impl
        #bulk_methods_impl

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
                self,
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
        }
    }
}
