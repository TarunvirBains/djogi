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
//!     fn get<'a>(executor, id) -> impl Future<Output = Result<Self, DjogiError>> + Send { ... }
//!     fn create<'a>(executor, value) -> impl Future<Output = Result<Self, DjogiError>> + Send { ... }
//!     fn save<'a>(&self, executor) -> impl Future<Output = Result<(), DjogiError>> + Send { ... }
//!     fn delete<'a>(self, executor) -> impl Future<Output = Result<(), DjogiError>> + Send { ... }
//!     fn refresh_from_db<'a>(&self, executor) -> impl Future<Output = Result<Self, DjogiError>> + Send { ... }
//! }
//! ```
//!
//! # Why the `'a` lifetime on every method
//!
//! `impl Trait` in return position (RPITIT) on stable Rust requires named
//! lifetimes — using `'_` in this position is only allowed on nightly. Every
//! CRUD method therefore introduces an explicit `'a` that scopes the
//! `sqlx::Executor` borrow. The `async move` body captures the executor by
//! value, which is sound because `sqlx::Executor: Send` (a supertrait) so the
//! captured type propagates `Send` to the returned `Future`.
//!
//! # Why `+ Send` on the Future but NOT on the executor
//!
//! `sqlx::Executor` already declares `Send` as a supertrait. Adding `+ Send`
//! explicitly on the executor parameter would trip
//! `clippy::implied_bounds_in_impls`. The returned `Future` carries `+ Send`
//! because tokio's multi-threaded runtime requires futures to be `Send` across
//! `.await` points.
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
//! - `save` and `refresh_from_db` take `&self` but use `async move`, so they
//!   clone all required field values before entering the async block.

use proc_macro2::TokenStream;
use quote::quote;
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
    let name = &struct_item.ident;
    let (impl_generics, ty_generics, where_clause) = struct_item.generics.split_for_impl();
    let table = &model_attrs.table;

    // -------------------------------------------------------------------------
    // Associated Pk type and pk_value() body — vary by PK strategy.
    // -------------------------------------------------------------------------
    let (pk_type_tokens, pk_value_body) = match model_attrs.pk {
        PkStrategy::HeerId => (quote! { ::djogi::types::HeerId }, quote! { &self.id }),
        PkStrategy::RanjId => (quote! { ::uuid::Uuid }, quote! { &self.id }),
        PkStrategy::Serial => (quote! { i32 }, quote! { &self.id }),
        PkStrategy::None => (
            quote! { () },
            quote! {
                static UNIT: () = ();
                &UNIT
            },
        ),
    };

    // -------------------------------------------------------------------------
    // User fields: all fields AFTER the framework-injected ones.
    // For HeerId/RanjId/Serial: id + created_at + updated_at = 3 to skip.
    // For None: created_at + updated_at = 2 to skip.
    // -------------------------------------------------------------------------
    let n_framework = match model_attrs.pk {
        PkStrategy::None => 2,
        _ => 3,
    };
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
        PkStrategy::None => quote! { .bind(()) },
    };

    // -------------------------------------------------------------------------
    // `get` SQL: SELECT * WHERE id = $1. `id` comes in as an owned Self::Pk.
    // -------------------------------------------------------------------------
    let get_sql = format!("SELECT * FROM {table} WHERE id = $1");

    let id_bind_for_get = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(id) },
        PkStrategy::Serial => quote! { .bind(id) },
        PkStrategy::None => quote! { .bind(()) },
    };

    // -------------------------------------------------------------------------
    // `refresh_from_db` — same query as get, but binds `&self.id` directly.
    // Like save, RPITIT captures `&self` so no pre-capture clone is needed.
    // -------------------------------------------------------------------------
    let refresh_id_bind = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(self.id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(self.id) },
        PkStrategy::Serial => quote! { .bind(self.id) },
        PkStrategy::None => quote! { .bind(()) },
    };

    // -------------------------------------------------------------------------
    // `delete` SQL: DELETE WHERE id = $1. `self` is consumed (moved in).
    // -------------------------------------------------------------------------
    let delete_sql = format!("DELETE FROM {table} WHERE id = $1");

    let owned_pk_bind = match model_attrs.pk {
        PkStrategy::HeerId => quote! { .bind(self.id.as_i64()) },
        PkStrategy::RanjId => quote! { .bind(self.id) },
        PkStrategy::Serial => quote! { .bind(self.id) },
        PkStrategy::None => quote! { .bind(()) },
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
    // Assemble the full impl block.
    // -------------------------------------------------------------------------
    quote! {
        impl #impl_generics ::djogi::model::Model for #name #ty_generics #where_clause {
            type Pk = #pk_type_tokens;

            fn table_name() -> &'static str {
                #table
            }

            fn pk_value(&self) -> &Self::Pk {
                #pk_value_body
            }

            #descriptor_impl

            fn get<'a>(
                executor: impl ::djogi::__private::sqlx::Executor<
                    'a,
                    Database = ::djogi::__private::sqlx::Postgres,
                >,
                id: Self::Pk,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + Send {
                async move {
                    let result = ::djogi::__private::sqlx::query_as::<
                        ::djogi::__private::sqlx::Postgres,
                        Self,
                    >(#get_sql)
                    #id_bind_for_get
                    .fetch_optional(executor)
                    .await?;

                    result.ok_or(::djogi::DjogiError::NotFound)
                }
            }

            fn create<'a>(
                executor: impl ::djogi::__private::sqlx::Executor<
                    'a,
                    Database = ::djogi::__private::sqlx::Postgres,
                >,
                value: Self,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + Send {
                async move {
                    let row = ::djogi::__private::sqlx::query_as::<
                        ::djogi::__private::sqlx::Postgres,
                        Self,
                    >(#insert_sql)
                    #(#create_binds)*
                    .fetch_one(executor)
                    .await?;

                    ::std::result::Result::Ok(row)
                }
            }

            fn save<'a>(
                &self,
                executor: impl ::djogi::__private::sqlx::Executor<
                    'a,
                    Database = ::djogi::__private::sqlx::Postgres,
                >,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + Send {
                // RPITIT captures `&self`'s lifetime into the returned future's
                // hidden lifetime, so the async block can borrow `&self.#f` across
                // `.await` without cloning. `Model: Send + Sync` makes `&Self:
                // Send` so the returned future satisfies `+ Send`.
                async move {
                    ::djogi::__private::sqlx::query(#save_sql)
                    #(#save_field_binds)*
                    #save_id_bind
                    .execute(executor)
                    .await?;

                    ::std::result::Result::Ok(())
                }
            }

            fn delete<'a>(
                self,
                executor: impl ::djogi::__private::sqlx::Executor<
                    'a,
                    Database = ::djogi::__private::sqlx::Postgres,
                >,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<(), ::djogi::DjogiError>,
            > + Send {
                async move {
                    ::djogi::__private::sqlx::query(#delete_sql)
                    #owned_pk_bind
                    .execute(executor)
                    .await?;

                    ::std::result::Result::Ok(())
                }
            }

            fn refresh_from_db<'a>(
                &self,
                executor: impl ::djogi::__private::sqlx::Executor<
                    'a,
                    Database = ::djogi::__private::sqlx::Postgres,
                >,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self, ::djogi::DjogiError>,
            > + Send {
                // RPITIT captures `&self`'s lifetime into the returned future,
                // so binding `self.id` directly in the async block works
                // without a pre-capture clone.
                async move {
                    let result = ::djogi::__private::sqlx::query_as::<
                        ::djogi::__private::sqlx::Postgres,
                        Self,
                    >(#get_sql)
                    #refresh_id_bind
                    .fetch_optional(executor)
                    .await?;

                    result.ok_or(::djogi::DjogiError::NotFound)
                }
            }
        }
    }
}
