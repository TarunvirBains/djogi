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
//!     fn save<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
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
        format!("UPDATE {table} SET updated_at = now() WHERE id = ${id_param}")
    } else {
        format!(
            "UPDATE {table} SET {set_list}, updated_at = now() WHERE id = ${id_param}",
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
            ::std::result::Result::Ok(row)
        }
    };

    let save_body = quote! {
        async move {
            let q = ::djogi::__private::sqlx::query(#save_sql)
            #(#save_field_binds)*
            #save_id_bind;
            match ::djogi::context::DjogiContext::__inner_mut_for_macros(ctx) {
                ::djogi::context::__ContextInnerForMacros::Pool(__pool) => {
                    q.execute(&*__pool).await?;
                }
                ::djogi::context::__ContextInnerForMacros::Transaction(__tx) => {
                    q.execute(&mut **__tx).await?;
                }
            }
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
    // Assemble the full impl block.
    // -------------------------------------------------------------------------
    quote! {
        #create_with_id_impl

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
                &'ctx self,
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
