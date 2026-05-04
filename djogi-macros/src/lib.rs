//! Proc macros for the Djogi framework.
//!
//! Provides:
//!
//! - `#[model(table = "...")]` — the attribute macro that does field
//!   injection and derives all `Model` impls.
//! - `reverse_one_to_many!` / `reverse_one_to_one!` — function-like
//!   macros emitting reverse-relation accessor methods on the target
//!   model plus an `inventory::submit!` registration record.
//! - `many_to_many!` — function-like macro emitting one direction of
//!   a many-to-many relation: the `ManyToMany<Target>` trait impl,
//!   a named inherent accessor on the source type, and an
//!   `inventory::submit!` registration record.
//!
//! `#[derive(Model)]` is a no-op stub kept for potential future use.

mod apps;
mod case;
mod compose;
mod djogi_enum;
mod ident;
mod jsonb_schema;
mod many_to_many;
mod model;
mod primary_key_macro;
mod reverse_relation;
mod syn_util;
mod testing;

use proc_macro::TokenStream;

/// The primary Djogi macro. Annotate any struct with `#[model(table = "...")]`
/// to inject framework fields (`id`, `created_at`, `updated_at`) and derive
/// CRUD, `FromRow`, and the `ModelDescriptor` the migration differ consumes.
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[model(table = "posts")]
/// #[derive(Debug, Clone)]
/// pub struct Post {
///     pub title: String,
///     pub published: bool,
/// }
/// ```
///
/// # `#[model(...)]` attribute grammar
///
/// | Key | Shape | Meaning |
/// |-----|-------|---------|
/// | `table` | `= "snake_case"` | Physical table name. Required. |
/// | `pk` | `= "heerid" \| "ranjid" \| "heerid_desc" \| "ranjid_desc" \| "serial" \| "none"` | Primary-key strategy. Default: `heerid`. The `_desc` variants (Phase 7-Zero v3) store the XOR-flipped bit layout so BTree scans run newest-first without a secondary descending index — see the [indexing spec] §4.1 for the one-question decision rule. |
/// | `no_default` | flag | Suppress the `impl Default` emitted for the model. Use when a user field lacks `Default`. |
/// | `through` | flag | Marks a through-table (M2M join) — relaxes the "M2M needs explicit through model" check. |
/// | `events` | flag | Opt into outbox `ModelEvent` emission for create/update/delete. |
/// | `idempotency_key` | `= "field"` | Name of a field whose value is the upsert idempotency key. |
/// | `tenant_key` | `= "field"` | Name of a field that carries the tenant id; enables `auto-set_tenant` and RLS sealing. |
/// | `fts(config = "english", fields = [...])` | list | Register a full-text-search vector over the listed fields. |
/// | `indexes(...)` | list | Declare model-level indexes — see below. |
///
/// # `indexes(...)` sub-grammar (Phase 7-Zero v3 §5)
///
/// Each entry is either `index(...)` or `unique(...)`. The body keys are:
///
/// | Key | Shape | Meaning |
/// |-----|-------|---------|
/// | `fields` | `= [ident, ...]` or `= [(col = ident, opclass = "...", order = asc\|desc, nulls = first\|last\|default), ...]` | Column list. Order is semantic — `[last, first]` and `[first, last]` are different indexes with different names. |
/// | `expr` | `= "lower(email)"` | Expression-target index (mutually exclusive with `fields`). |
/// | `using` | `= "btree" \| "gin" \| "gist" \| "brin" \| "hash"` | Access method. Default: `btree`. |
/// | `opclass` | `= "text_pattern_ops"` | Single-column opclass (declaration shortcut; the per-column record form is preferred for multi-column indexes). |
/// | `include` | `= [ident, ...]` | `INCLUDE(...)` payload columns for covering indexes. |
/// | `where` | `= "deleted_at IS NULL"` | Partial-index predicate. Raw SQL — Djogi does not parse it; Postgres validates at migration time. |
/// | `nulls_not_distinct` | `= true` | Unique indexes only — treat two `NULL`s as equal. Forces the `UniqueIndex` kind. |
/// | `concurrently` | `= true` | Emit `CREATE INDEX CONCURRENTLY`. On a `unique(...)` declaration this escalates the kind to `UniqueIndex` (`ALTER TABLE ADD CONSTRAINT` has no concurrent form). **Foot-gun:** omitting this on an index added to a large production table blocks every writer — `SHARE` on the `CREATE INDEX` path, `ACCESS EXCLUSIVE` on the `ADD CONSTRAINT` path. The framework does not auto-detect — operator responsibility. See the [indexing spec] "concurrently contract" section for the full eight-item doc promise. |
/// | `name` | `= "custom_idx"` | Override the deterministic index name. Must not collide with a name the emitter would generate for another declared index. |
///
/// `unique(...)` differs from `index(...)` only in kind — by default it lowers
/// to a `UNIQUE` constraint (`..._key` name), but the emitter escalates to a
/// `UNIQUE INDEX` (`..._uidx` name) when the declaration uses `where`,
/// `include`, `nulls_not_distinct`, or an `expr` target (Postgres constraints
/// do not support those features).
///
/// Example:
///
/// ```rust,ignore
/// #[model(table = "orders", indexes(
///     index(fields = [created_at, id]),
///     unique(fields = [tenant_id, external_id]),
///     index(fields = [tenant_id], where = "deleted_at IS NULL"),
///     index(expr = "lower(email)"),
///     index(fields = [(col = body, opclass = "jsonb_path_ops")], using = "gin"),
/// ))]
/// pub struct Order { /* ... */ }
/// ```
///
/// [indexing spec]: ../docs/spec/indexing.md
#[proc_macro_attribute]
pub fn model(attr: TokenStream, item: TokenStream) -> TokenStream {
    model::expand(attr.into(), item.into()).into()
}

/// No-op stub — field injection requires `#[model]` (attribute macro).
/// Kept as a placeholder for future derive-based extensions.
///
/// NOTE: Only `field` is listed as a helper attribute here, not `model`.
/// Listing `model` as a helper would shadow the `#[model]` proc_macro_attribute
/// and cause ambiguous resolution (Post-Review Fix #4).
#[proc_macro_derive(Model, attributes(field))]
pub fn derive_model(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// Emit a reverse one-to-many accessor on a model.
///
/// Invocation form:
///
/// ```ignore
/// djogi::reverse_one_to_many!(Owner, cars -> Vehicle by owner_id);
/// // expands to (roughly):
/// //
/// // impl Owner {
/// //     pub fn cars<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Vec<Vehicle>, DjogiError>> + Send + 'ctx
/// //     { ... filters Vehicle by owner_id ... }
/// // }
/// ```
///
/// The macro also emits an `inventory::submit!` registration carrying a
/// `ReverseRelationMarker` record — Phase 4.5's projection generator
/// walks those markers to discover every registered reverse accessor.
///
/// See [`djogi_macros::reverse_relation`] module docs for the full
/// expansion shape, the terminology note on "source" vs "target", and
/// the rationale for function-like (not derive) form.
#[proc_macro]
pub fn reverse_one_to_many(input: TokenStream) -> TokenStream {
    reverse_relation::expand(
        input.into(),
        reverse_relation::AccessorKindOpaque::ONE_TO_MANY,
    )
    .into()
}

/// Emit a reverse one-to-one accessor on a model.
///
/// Invocation form:
///
/// ```ignore
/// djogi::reverse_one_to_one!(User, profile -> Profile by user_id);
/// // expands to (roughly):
/// //
/// // impl User {
/// //     pub fn profile<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Option<Profile>, DjogiError>> + Send + 'ctx
/// //     { ... returns .first() match on Profile.user_id ... }
/// // }
/// ```
///
/// Intended for reverses of `OneToOneField<Receiver>` (or a
/// `ForeignKey<Receiver>` + `UNIQUE` pair on the foreign side) — the
/// `.first()` terminal is correct when the schema guarantees at most
/// one matching row. If the schema does not enforce uniqueness, prefer
/// `reverse_one_to_many!` to surface the fact that multiple rows are
/// possible.
///
/// Also emits an `inventory::submit!` marker with
/// `RelationKind::O2O`.
#[proc_macro]
pub fn reverse_one_to_one(input: TokenStream) -> TokenStream {
    reverse_relation::expand(
        input.into(),
        reverse_relation::AccessorKindOpaque::ONE_TO_ONE,
    )
    .into()
}

/// Emit one direction of a many-to-many relation — the
/// `ManyToMany<Target>` trait impl, the named inherent accessor on the
/// source type, and an inventory marker for Phase 4.5.
///
/// Invocation form:
///
/// ```ignore
/// djogi::many_to_many!(
///     Person, Group,
///     through  = PersonGroup,
///     this_fk  = person_id,
///     that_fk  = group_id,
///     relation = "groups"
/// );
/// // expands to (roughly):
/// //
/// // impl djogi::relation::ManyToMany<Group> for Person {
/// //     type Through = PersonGroup;
/// //     const RELATION: &'static str = "groups";
/// //     fn this_fk() -> &'static str { "person_id" }
/// //     fn that_fk() -> &'static str { "group_id" }
/// //     async fn related(...) { ... }
/// //     async fn add_related(...) { ... }
/// //     async fn remove_related(...) { ... }
/// // }
/// //
/// // impl Person {
/// //     pub fn groups<'ctx>(&'ctx self, ctx: &'ctx mut DjogiContext)
/// //         -> impl Future<Output = Result<Vec<Group>, DjogiError>> + Send + 'ctx
/// //     { <Self as ManyToMany<Group>>::related(self, ctx) }
/// // }
/// //
/// // inventory::submit! { ReverseRelationMarker { kind: M2M, ... } }
/// ```
///
/// See [`djogi_macros::many_to_many`] module docs (crate-internal) for
/// the full expansion shape, the rationale for emitting one direction
/// per call, and the seal story for the identifier arguments.
#[proc_macro]
pub fn many_to_many(input: TokenStream) -> TokenStream {
    many_to_many::expand(input.into()).into()
}

/// Derive typed Postgres enum support.
///
/// Emits `postgres_types::ToSql` + `FromSql` impls that encode/decode the enum
/// as its mapped Postgres string label, plus an `inventory::submit!` of an
/// `EnumDescriptor` for the Phase 7 migration differ.
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[derive(DjogiEnum, Clone, Copy, PartialEq, Eq, Debug)]
/// #[djogi_enum(name = "vehicle_status", rename_all = "snake_case")]
/// pub enum VehicleStatus {
///     Active,
///     InMaintenance,
///     #[djogi_enum_variant(name = "decommissioned")]
///     Retired,
/// }
/// ```
///
/// See the `djogi_enum` module for the full expansion contract.
#[proc_macro_derive(DjogiEnum, attributes(djogi_enum, djogi_enum_variant))]
pub fn derive_djogi_enum(input: TokenStream) -> TokenStream {
    djogi_enum::expand(input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Derive the typed JSONB deep-path API for a schema struct.
///
/// Applying this derive to a named struct causes the macro to emit a
/// `{T}Path<M>` struct with one method per field. Scalar fields (from the
/// cast-matrix allowlist: `i16`, `i32`, `i64`, `f32`, `f64`, `bool`,
/// `String`, `time::OffsetDateTime`, `time::Date`, `uuid::Uuid`,
/// `rust_decimal::Decimal`, `serde_json::Value`, `HeerId`, `RanjId`) return
/// a [`JsonbPathRef<M, FieldType>`](djogi::jsonb::JsonbPathRef) ready for
/// comparison. All other field types are assumed to implement `JsonbSchema`
/// and their method returns the nested type's `Path<M>` with the path
/// accumulator extended.
///
/// # Example
///
/// ```rust,ignore
/// use djogi::JsonbSchema;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(JsonbSchema, Serialize, Deserialize, Default)]
/// pub struct EngineSpecs {
///     pub cylinders: i32,
///     pub displacement_cc: f32,
/// }
///
/// #[derive(JsonbSchema, Serialize, Deserialize, Default)]
/// pub struct VehicleSpecs {
///     pub engine: EngineSpecs,
///     pub weight_kg: f32,
/// }
/// ```
///
/// Then in a filter closure:
///
/// ```rust,ignore
/// Vehicle::objects()
///     .filter(|f| f.specs().typed().engine.cylinders.gt(4))
///     .fetch_all(&mut ctx).await?
/// ```
///
/// # Compile errors
///
/// - Non-struct (enum, union) → "can only be applied to named structs".
/// - Tuple struct → "requires a named struct — tuple structs are not supported".
#[proc_macro_derive(JsonbSchema)]
pub fn derive_jsonb_schema(input: TokenStream) -> TokenStream {
    jsonb_schema::expand(input.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Derive [`djogi::Auditable`] — emit the trait impl exposing
/// `created_by(&self) -> Option<&str>`.
///
/// Phase 8 §T2.2.
///
/// # Adopter contract
///
/// The adopter declares `pub created_by: Option<String>` on the struct
/// **before** stacking the derive. The macro emits getter only — it
/// does **not** inject the field. Standard Rust derives cannot mutate
/// the input AST; the v3 spec settled this on Path B (line 866).
///
/// Population of `created_by` from the request-side
/// [`djogi::auth::AuthContext`] is wired separately via
/// `#[model(hooks)]` + an adopter-written `before_create` body. T2.4
/// will introduce a macro-emitted helper that synthesises the body;
/// T2.2 ships getter only.
///
/// # Macro ordering
///
/// Stack `#[derive(Auditable)]` **above** `#[model(...)]`:
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[derive(Auditable)]
/// #[model(table = "posts", hooks)]
/// #[derive(Debug, Clone)]
/// pub struct Post {
///     pub title: String,
///     pub created_by: Option<String>,
/// }
/// ```
///
/// # Compile errors
///
/// - Field `created_by` missing → rustc emits an `E0609 no field
///   "created_by" on type ...` at the macro-generated impl. The
///   diagnostic is implementer-actionable and points at the adopter's
///   struct declaration.
/// - Unsupported input shape (enum, union) is silently accepted at
///   parse time but produces an `E0609`-equivalent failure when the
///   compiler reaches `self.created_by` resolution; the failure is
///   still actionable. T2.5 may add a tighter compile_fail fixture.
///
/// See `compose::auditable` module docs for the Path B rationale and
/// the seal/path-routing decision.
#[proc_macro_derive(Auditable)]
pub fn derive_auditable(input: TokenStream) -> TokenStream {
    compose::auditable::expand(input.into()).into()
}

/// Derive [`djogi::SoftDeletable`] — emit the trait impl exposing
/// `deleted_at(&self) -> Option<DateTime>`.
///
/// Phase 8 §T2.3.
///
/// # Adopter contract
///
/// The adopter declares `pub deleted_at: Option<djogi::DateTime>` on
/// the struct **before** stacking the derive. The macro emits getter
/// only — it does **not** inject the field. Standard Rust derives
/// cannot mutate the input AST; the v3 spec settled this on Path B
/// (line 866).
///
/// Automatic exclusion of soft-deleted rows from default queries is
/// **deferred to Phase 8γ T6** — see spec line 971
/// (RESOLVED 2026-05-03, lens, locked). T2.3 ships the trait impl
/// plus a manual `QuerySet::not_deleted()` helper that adopters call
/// explicitly on each `objects()` chain. 8γ will replace
/// `.not_deleted()` with auto-composition once the `Q<T>` substrate
/// lands.
///
/// # Macro ordering
///
/// Stack `#[derive(SoftDeletable)]` **above** `#[model(...)]`:
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// #[derive(SoftDeletable)]
/// #[model(table = "posts")]
/// #[derive(Debug, Clone)]
/// pub struct Post {
///     pub title: String,
///     pub deleted_at: Option<djogi::DateTime>,
/// }
/// ```
///
/// At call sites that should exclude soft-deleted rows, invoke the
/// manual helper:
///
/// ```rust,ignore
/// let live = Post::objects().not_deleted().fetch_all(&mut ctx).await?;
/// ```
///
/// # Compile errors
///
/// - Field `deleted_at` missing → rustc emits an `E0609 no field
///   "deleted_at" on type ...` at the macro-generated impl. The
///   diagnostic is implementer-actionable and points at the adopter's
///   struct declaration.
/// - Unsupported input shape (enum, union) is silently accepted at
///   parse time but produces an `E0609`-equivalent failure when the
///   compiler reaches `self.deleted_at` resolution; the failure is
///   still actionable. T2.5 may add a tighter compile_fail fixture.
///
/// See `compose::soft_deletable` module docs for the Path B rationale,
/// the seal/path-routing decision, and the deferred-to-8γ note.
#[proc_macro_derive(SoftDeletable)]
pub fn derive_soft_deletable(input: TokenStream) -> TokenStream {
    compose::soft_deletable::expand(input.into()).into()
}

/// Per-test database lifecycle harness.
///
/// Transforms an `async fn my_test(ctx: DjogiContext)` into a
/// `#[tokio::test]`-runnable wrapper that:
///
/// 1. Creates a fresh `djogi_test_<uuid>` Postgres database.
/// 2. Installs the HeeRanjID schema and seeds the default node.
/// 3. Sets `heer.node_id = '1'` at the database level so all connections
///    inherit the node ID without per-connection setup.
/// 4. Constructs a `DjogiContext` from a deadpool-postgres pool.
/// 5. Passes the context to the test body.
/// 6. Drops the database when the body returns — whether normally or via panic.
///
/// The runtime machinery uses `tokio_postgres` directly (no sqlx) and calls
/// `heeranjid::postgres_schema::install_schema` and `seed_default_node` from
/// heeranjid 0.2.1.
///
/// # Usage
///
/// ```rust,ignore
/// use djogi::DjogiContext;
///
/// #[djogi_macros::djogi_test]
/// async fn my_test(ctx: DjogiContext) {
///     // ctx is a DjogiContext backed by a fresh, isolated per-test DB.
///     // HeeRanjID is installed and the default node is seeded.
///     // The database is dropped automatically when this function returns.
/// }
/// ```
///
/// # Attribute arguments
///
/// - `extensions = [ "postgis", "pg_trgm", ... ]` — optional array of
///   Postgres extension names to provision on the per-test database via
///   `CREATE EXTENSION IF NOT EXISTS` before the test body runs. Each
///   name is validated against a strict ASCII-identifier rule at runtime
///   (letters / digits / underscores, 1..=63 bytes) before being
///   interpolated into SQL.
///
/// Future versions may accept additional options such as
/// `migrations = "path/to/sql"` to apply fixtures before the test body.
///
/// # Requirements
///
/// - `DATABASE_URL` must be set to a Postgres connection URL pointing at a
///   cluster where the test runner has `CREATE DATABASE` / `DROP DATABASE`
///   privileges.
/// - The annotated function must be `async` and have exactly one parameter
///   of type `DjogiContext` (or any name — the type check happens at
///   compile time of the test crate, not in the macro).
#[proc_macro_attribute]
pub fn djogi_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    testing::expand(attr.into(), item.into()).into()
}

/// Declare the crate's compile-time schema ownership domains.
///
/// `djogi::apps!` takes a block of unit-struct declarations, each
/// carrying an `#[app(...)]` attribute describing the database target
/// and (optionally) an explicit label:
///
/// ```rust,ignore
/// use djogi::prelude::*;
///
/// djogi::apps! {
///     #[app(database = "main")]
///     pub struct Vehicles;
///
///     #[app(database = "main")]
///     pub struct Users;
///
///     #[app(database = "crud_log", label = "fleet_audit")]
///     pub struct Audit;
/// }
/// ```
///
/// For each entry the macro emits:
///
/// - the unit struct itself (visibility preserved),
/// - `impl djogi::apps::App` with const `LABEL`, `DATABASE`, and
///   `DESCRIPTOR` associated constants,
/// - a sealed-trait impl that enforces "only this macro creates apps",
/// - an `inventory::submit!` registering the struct's
///   [`djogi::AppDescriptor`] for Phase 7's migration differ.
///
/// # `#[app(...)]` grammar
///
/// | Key | Shape | Meaning |
/// |-----|-------|---------|
/// | `database` | `= "main"` (required) | Database-target name this app belongs to. |
/// | `label` | `= "fleet_vehicles"` (optional) | Override the default label (struct name lowercased). |
///
/// # Constraints
///
/// - At most one `djogi::apps!` invocation per crate. A second
///   invocation produces a duplicate-definition error on the hidden
///   sentinel module the macro emits.
/// - Every label (whether default-derived or explicit) must satisfy
///   the Postgres identifier grammar: non-empty, first byte an ASCII
///   letter or `_`, remaining bytes ASCII alphanumerics or `_`, total
///   length ≤ 63 bytes. No regex engine — validation uses byte-level
///   primitives per `CLAUDE.md` + `feedback_no_regex_in_djogi.md`.
/// - Structs must be unit form (`pub struct Foo;`). Tuple or named
///   structs are rejected with a span-precise diagnostic.
///
/// Phase 7-Zero v3 T7 lands this core infrastructure; T8 extends the
/// `#[app(...)]` grammar with the lifecycle markers (`renamed_from`,
/// `tombstone`) and wires `#[model(app = …)]` into
/// `ModelDescriptor`.
#[proc_macro]
pub fn apps(input: TokenStream) -> TokenStream {
    apps::expand(input.into()).into()
}

/// Declarative-style macro for declaring custom primary-key types.
///
/// Emits a `pub struct <Name>(<Inner>);` newtype plus the trait impls the
/// `#[model(pk = <Name>)]` attribute relies on — `PrimaryKey` (with its
/// `KIND` / `SQL_TYPE` / `DEFAULT_SQL` associated consts), `ToSql` /
/// `FromSql` delegation to the inner type, and optionally
/// `PrimaryKeyDbGen` (when `bulk_sql = "..."` is set) or
/// `PrimaryKeyClientGen` (when `generate = |...| expr` is set).
///
/// ```ignore
/// djogi::primary_key! {
///     pub struct MyAppId(i64);
///     sql_type = "BIGINT";
///     default_sql = "my_app_id_next()";
///     bulk_sql = "SELECT id FROM my_app_id_next_many($1)";
/// }
///
/// #[model(table = "orders", pk = MyAppId)]
/// pub struct Order { /* ... */ }
/// ```
///
/// See `djogi_macros::primary_key_macro` for the full grammar and
/// `docs/guide/primary-keys.md#custom-pk-types` for the user-facing
/// narrative.
#[proc_macro]
pub fn primary_key(input: TokenStream) -> TokenStream {
    primary_key_macro::expand(input.into()).into()
}
