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

/// Generate the full `impl Model for T` block.
///
/// Called from `mod.rs` after `inject::expand` has mutated `struct_item`, so
/// the field list already includes `id`, `created_at`, and `updated_at` at the
/// front.
pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
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
        .map(|i| {
            let raw = i.to_string();
            raw.strip_prefix("r#").unwrap_or(&raw).to_string()
        })
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
    let create_param_entries: Vec<TokenStream> = user_fields
        .iter()
        .map(|f| {
            quote! { &value.#f as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        })
        .collect();

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
                let col_name = f.to_string();
                let col_str = col_name.strip_prefix("r#").unwrap_or(&col_name).to_string();
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
            let col_name = f.to_string();
            let col_str = col_name.strip_prefix("r#").unwrap_or(&col_name).to_string();
            let col_eq = format!("{col_str} = ");
            if is_tracked(ty) {
                // Tracked<T>: emit only when dirty.
                // Bind the inner T via `(*self.<f>).clone()` — Deref<Target=T>
                // so the dereference gives `&T` and clone() gives `T`.
                Some(quote! {
                    if self.#f.is_dirty() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(#col_eq);
                        __acc.push_bind((*self.#f).clone());
                    }
                })
            } else {
                // Non-Tracked: unconditional — behavioral regression guard for
                // models that do not opt into dirty tracking.
                Some(quote! {
                    {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        __acc.push_sql(#col_eq);
                        __acc.push_bind(self.#f.clone());
                    }
                })
            }
        })
        .collect();

    // After save() rehydrates self via RETURNING, walk every Tracked field
    // and call mark_clean(). `Tracked::new(T)` already constructs with dirty=false
    // so this is defensive — but required by the Task 2 contract so that future
    // in-place rehydration changes cannot silently break the invariant.
    let mark_clean_fragments: Vec<TokenStream> = user_fields
        .iter()
        .zip(user_field_types.iter())
        .filter_map(|(f, ty)| {
            if is_tracked(ty) {
                Some(quote! { self.#f.mark_clean(); })
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

    let id_param_for_get = match model_attrs.pk {
        PkStrategy::HeerId => {
            quote! { &id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::RanjId => {
            quote! { &id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::Serial => {
            quote! { &id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `refresh_from_db` — same query as get, but binds `&self.id` directly.
    // Like save, RPITIT captures `&self` so no pre-capture clone is needed.
    // -------------------------------------------------------------------------
    let refresh_id_param = match model_attrs.pk {
        PkStrategy::HeerId => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::RanjId => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::Serial => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::None => unreachable!("handled by early return"),
    };

    // -------------------------------------------------------------------------
    // `delete` SQL: DELETE WHERE id = $1. `self` is consumed (moved in).
    // -------------------------------------------------------------------------
    let delete_sql = format!("DELETE FROM {table} WHERE id = $1");

    let owned_pk_param = match model_attrs.pk {
        PkStrategy::HeerId => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::RanjId => {
            quote! { &self.id as &(dyn ::djogi::__private::postgres_types::ToSql + Sync) }
        }
        PkStrategy::Serial => {
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
    let (sequence_compile_err, sequence_upsert_preamble, create_value_binding) =
        if seq_within_fields.len() > 1 {
            let msg = "models may declare #[field(sequence_within = ...)] on at most one field; \
                       multi-scope sequencing is a future extension";
            (
                quote! { ::std::compile_error!(#msg); },
                quote! {},
                quote! { let value = value; },
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
            // `value` needs to be mutable so we can assign the
            // seq field.
            (quote! {}, preamble, quote! { let mut value = value; })
        } else {
            (quote! {}, quote! {}, quote! { let value = value; })
        };

    let create_body = quote! {
        async move {
            #create_value_binding
            #sequence_upsert_preamble
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #(#create_param_entries,)*
            ];
            let __raw_row = ctx.__query_one_for_macros(#insert_sql, __params).await?;
            let row = <Self as ::djogi::__private::pg::FromPgRow>::from_pg_row(&__raw_row)?;
            // Phase 4 Task 6 — outbox emission (no-op for non-events models).
            // Runs in the same ctx so a transactional caller gets the
            // outbox row committed/rolled back atomically with `row`.
            #emit_outbox_create
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
    let save_body = if let Some((ver_ident, ver_col)) = &version_field_info {
        // Shape B — version-aware save.
        let ver_set = format!(", {ver_col} = {ver_col} + 1");
        let ver_where = format!(" AND {ver_col} = ");
        let ver_conflict_msg =
            format!("optimistic lock conflict: {ver_col} mismatch in table {table}");
        quote! {
            async move {
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
                        ::std::result::Result::Ok(())
                    }
                    ::std::option::Option::None => {
                        // Zero rows updated — DB version has moved ahead of our
                        // in-memory version. Signal optimistic lock conflict.
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
                ::std::result::Result::Ok(())
            }
        }
    };

    let delete_body = quote! {
        async move {
            let __params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                #owned_pk_param,
            ];
            ctx.__execute_for_macros(#delete_sql, __params).await?;
            // Phase 4 Task 6 — outbox carries the pre-delete snapshot
            // (reads `self` before it drops at function scope end).
            // No-op for non-events models.
            #emit_outbox_delete
            ::std::result::Result::Ok(())
        }
    };

    let refresh_body = quote! {
        async move {
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
                    let __id_i64: i64 = id.as_i64();
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
    let per_row_binds: TokenStream = if n_user == 0 {
        quote! {}
    } else {
        let first_field = &user_fields[0];
        let rest_fields = &user_fields[1..];
        quote! {
            __acc.push_sql("(");
            __acc.push_bind(row.#first_field);
            #(
                __acc.push_sql(", ");
                __acc.push_bind(row.#rest_fields);
            )*
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
        let bulk_returning_suffix = format!(" RETURNING {column_list}");
        quote! {
            /// Bulk-insert every row in `rows` and return the rehydrated
            /// results.
            ///
            /// One `INSERT` round trip emitting
            /// `INSERT INTO <table> (<user-cols>) VALUES (...), (...) RETURNING <column_list>`.
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

        // For bulk_upsert the id column is included up front so
        // callers can upsert with pre-allocated ids. Per-row bind tail
        // needs the id bind before the user-field binds.
        // Uses SqlAccumulator::push_bind — emits `$n` placeholders and
        // stores bind values by move (rows consumed via into_iter).
        // HeerId needs `.as_i64()` to encode as BIGINT; RanjId and i32 bind as-is.
        let pk_bind_for_upsert = match model_attrs.pk {
            PkStrategy::HeerId => quote! { __acc.push_bind(row.id.as_i64()); },
            PkStrategy::RanjId => quote! { __acc.push_bind(row.id); },
            PkStrategy::Serial => quote! { __acc.push_bind(row.id); },
            PkStrategy::None => unreachable!("handled by early return"),
        };
        let upsert_per_row_binds: TokenStream = {
            let all_fields_iter = user_fields.iter();
            quote! {
                __acc.push_sql("(");
                #pk_bind_for_upsert
                #(
                    __acc.push_sql(", ");
                    __acc.push_bind(row.#all_fields_iter);
                )*
                __acc.push_sql(")");
            }
        };

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

                let mut __acc = ::djogi::__private::pg::SqlAccumulator::new(#insert_prefix);
                {
                    let mut __first = true;
                    for row in rows.into_iter() {
                        if __first { __first = false; } else { __acc.push_sql(", "); }
                        #upsert_per_row_binds
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
                    let __insert_params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                        #(&row.#user_fields as &(dyn ::djogi::__private::postgres_types::ToSql + Sync),)*
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
                            // the insert attempt).
                            let __select_params: &[&(dyn ::djogi::__private::postgres_types::ToSql + Sync)] = &[
                                &row.#key_ident as &(dyn ::djogi::__private::postgres_types::ToSql + Sync),
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
    // Assemble the full impl block.
    // -------------------------------------------------------------------------
    quote! {
        #sequence_compile_err
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
