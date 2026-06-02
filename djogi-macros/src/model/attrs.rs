//! Attribute parsing for `#[model(table = "...", pk = X)]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
//! `pk` takes a bare identifier — `HeerId`,
//! `HeerIdRecencyBiased`, `RanjId`, etc. The old string-literal form
//! (`pk = "heerid"`) is rejected with a span-carrying diagnostic.
//! `ModelAttrs` keeps a hand-rolled parser: the surface is three keys, the
//! error messages from `syn::Error::new_spanned` already carry precise
//! source spans, and there is no incentive to grow it.
//! `FieldAttrs` parses via `darling::FromField`. Per-field attrs grow over
//! time (later phases add `db_column`, `choices`, `validators`, etc.), and
//! darling's declarative derive gives us span-aware errors for unknown
//! keys, type mismatches, and duplicate keys for free — matching the prior
//! hand-rolled behaviour without each new key duplicating the same
//! `Meta::NameValue` match arm.

use std::collections::BTreeSet;

use darling::{FromField, FromMeta};
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token};

/// Parsed `#[model(fts(source = "...", dictionary = "..."))]` sub-attribute.
/// Both fields are required. `source` is a comma-separated list of column
/// names (e.g. `"title, body"`); `dictionary` is a Postgres text-search
/// configuration name (e.g. `"english"`). Both are validated byte-level at
/// parse time per `feedback_no_regex_in_djogi`.
#[derive(Debug, Clone)]
pub struct FtsSpec {
    /// Comma-separated source column names, e.g. `"title, body"`.
    pub source: String,
    /// Postgres text-search configuration name, e.g. `"english"`.
    pub dictionary: String,
}

/// Options extracted from `#[model(table = "...", pk = X)]`.
// Fields are read by Tasks 4–9 (inject, crud, descriptor, stubs). The
// dead-code lint fires now because those callers are stubs — suppress it.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ModelAttrs {
    /// SQL table name, e.g. `"posts"`.
    pub table: String,
    /// Primary key strategy.
    pub pk: PkStrategy,
    /// When `true`, the macro skips generating the `Default` impl.
    /// Use `#[model(table = "...", no_default)]` for models that contain
    /// field types that do not implement `Default` (e.g. `time::Date`).
    /// Without this flag the generated `Default` impl would fail to compile.
    /// Users must then initialise all fields explicitly instead of relying
    /// on struct-update syntax (`..Model::default`).
    pub no_default: bool,
    /// When `true`, this model is a many-to-many through / junction model.
    /// Set via `#[model(table = "...", through)]`. The flag flows through
    /// to [`ModelDescriptor::is_through`](djogi::descriptor::ModelDescriptor::is_through)
    /// at codegen time, where it acts as a marker that:
    /// - This table is the junction for a specific `impl ManyToMany<Target> for Source`.
    /// - the migration differ may later suppress standalone-model
    ///   admin/routing affordances for through tables (deferred).
    ///   Through models remain ordinary queryable `Model`s — the flag is
    ///   documentation and future differentiation, not a structural
    ///   constraint. Users still `#[derive(Model)]` them as normal and
    ///   query them with the standard `QuerySet` API.
    pub through: bool,
    /// When `true`, this model emits a transactional outbox row on every
    /// successful `create` / `save` / `delete` performed through a
    /// `DjogiContext`.
    /// Set via `#[model(table = "...", events)]`. The flag flows through
    /// to [`ModelDescriptor::has_outbox`](djogi::descriptor::ModelDescriptor::has_outbox)
    /// at codegen time, where `djogi::outbox::emit_event` keys off it to
    /// decide whether to write to `{table}_outbox` inside the active
    /// transaction. lands the CRUD-side emission; macro-
    /// side DDL emission to `target/djogi_outbox/{table}_outbox.sql` (so
    /// the migration differ can consume it) is **deferred**. For
    /// now, downstream crates hand-write the `{table}_outbox` DDL
    /// alongside their own migrations.
    /// Models without `events` skip the outbox call entirely at
    /// macro-expansion time — there is no runtime cost for opt-out.
    pub events: bool,
    /// The user-field name that serves as the idempotency key
    /// Consumer wiring.
    /// Set via `#[model(table = "...", idempotency_key = "request_id")]`.
    /// When present, the macro emits two inherent methods:
    /// - `create_or_find(ctx, row)` — attempts an
    ///   `INSERT ... ON CONFLICT (<this col>) DO NOTHING RETURNING *`
    ///   and, on conflict, re-SELECTs the existing row. Returns
    ///   `(Self, bool /* created */)`.
    /// - `bulk_upsert_by_descriptor(ctx, rows)` — thin wrapper over
    ///   [`bulk_upsert`] that reads this column as the sole ON
    ///   CONFLICT target.
    ///   When not set, both methods are still emitted as thin stubs
    ///   that return [`DjogiError::MissingIdempotencyKey`] at runtime
    ///   simplest-possible pointer at the attribute the caller needs
    ///   to add. This mirrors the approach of populating the
    ///   descriptor slot even for models that don't consume it yet.
    ///   The inner string must be a plain ASCII-identifier column name
    ///   (letter/underscore start, alphanumerics and underscores after).
    ///   The parser rejects anything else so the value can be safely
    ///   embedded into the emitted SQL.
    pub idempotency_key: Option<String>,

    /// The user-field name that serves as the tenant discriminator for
    /// Row Level Security — .
    /// Set via `#[model(table = "...", tenant_key = "org_id")]`. When present,
    /// the macro emits a side-channel `target/djogi_rls/{table}_rls.sql` file
    /// containing `ALTER TABLE … ENABLE ROW LEVEL SECURITY` and a
    /// `CREATE POLICY … USING (col = current_setting('app.tenant_id')::type)`
    /// statement. the migration differ will consume this file; for now
    /// it is hand-applied in integration tests.
    /// The column referenced by `tenant_key` must be one of: `BigInt`
    /// (HeerId), `Uuid` (RanjId), `Text`, or `Citext`. Any other SQL type
    /// triggers a span-precise compile error at the `tenant_key` attribute.
    /// At runtime, call [`DjogiContext::set_tenant`] inside an `atomic`
    /// block to activate the RLS policy for a request.
    pub tenant_key: Option<String>,

    /// Full-text search specification — .
    /// Set via `#[model(fts(source = "col1, col2", dictionary = "english"))]`.
    /// When present, the macro emits an `FtsDescriptor` into the
    /// `ModelDescriptor` and the `{Model}Fields` struct gains a `search`
    /// accessor that returns a typed `FtsFieldRef` for building
    /// `@@` / `ts_rank` predicates.
    /// Both `source` and `dictionary` are required; omitting either is a
    /// compile error. Both values are validated byte-level at parse time
    /// per `feedback_no_regex_in_djogi`.
    pub fts: Option<FtsSpec>,

    /// Model-level index declarations parsed from `#[model(indexes(...))]`
    /// Each entry lowers to one `IndexSpec` struct literal in the
    /// descriptor's `indexes` slice. Empty when no `indexes(...)` group
    /// is present; otherwise the order follows the user's source-order
    /// declarations, and the descriptor emitter alphabetises by
    /// generated name before producing the final slice literal.
    pub indexes: Vec<crate::model::indexes::ModelIndexDecl>,

    /// Model-level `EXCLUDE` constraint declarations parsed from
    /// `#[model(exclusion(...))]` — PR 7.
    /// Multiple `exclusion(...)` entries on the same model are valid
    /// and accumulate here in source order. Names must be unique within
    /// a model; duplicate names are rejected at parse time. Each entry
    /// lowers to one `ExclusionConstraintSpec` struct literal in the
    /// descriptor's `exclusion_constraints` slice.
    pub exclusions: Vec<crate::model::exclusion::ExclusionDecl>,

    /// Compile-time schema ownership domain — the type path of the app
    /// this model belongs to. Set via `#[model(app = Vehicles)]`.
    /// `None` places the model in the synthetic global bucket, which
    /// The differ files under `<default-database>/<empty-label>/`.
    /// Resolved at emission time via `<Path as ::djogi::App>::LABEL` so
    /// the descriptor carries the stable string identifier, not the
    /// Rust type path.
    pub app: Option<syn::Path>,

    /// Historical-metadata pointer to the model's prior app. Set via
    /// `#[model(moved_from_app = OldBilling)]`. Enables the
    /// differ to track model-across-app moves without forcing the old
    /// app to stay declared. The pointed-at app may be tombstoned;
    /// that's the intended lifecycle shape for retirements. Resolved
    /// via `<Path as ::djogi::App>::LABEL` same as `app`.
    pub moved_from_app: Option<syn::Path>,

    /// Prior table name when the model has been renamed via
    /// `#[model(table = "...", renamed_from = "old_table")]`
    /// String literal value. The differ uses this to emit
    /// `ALTER TABLE old_table RENAME TO new_table` rather than the
    /// destructive DROP+CREATE pair. The macro validates the string
    /// against the same Postgres identifier grammar that `table = "..."`
    /// uses (ASCII letter/underscore start, ASCII alphanumerics/
    /// underscores after, ≤63 bytes).
    pub renamed_from: Option<String>,

    /// Default self-FK column for tree-recursive queries.
    /// Set via `#[model(tree_edge = "parent_id")]`. The named column
    /// must exist on the user's struct AND must resolve to a self-FK
    /// (a `ForeignKey<Self>` / `OneToOneField<Self>` field, optionally
    /// `Option<…>`-wrapped). Cross-checks against the user-field list
    /// and the self-FK detector run at descriptor-emit time;
    /// failures surface as span-precise compile errors pointing at the
    /// `"…"` literal.
    /// Stored as `Option<syn::LitStr>` rather than `Option<String>`
    /// so the descriptor-side validator can attach the original literal's
    /// span to its diagnostic — same `.value -> String` accessor as
    /// every other span-bearing string attr in this file (e.g.
    /// `FieldAttrs::generated`). The grammar enforced at parse time is
    /// the standard Djogi identifier rule (ASCII letter or underscore
    /// first byte; ASCII alphanumerics or underscores thereafter;
    /// ≤ 63 bytes — the Postgres unquoted-identifier cap).
    /// `None` means the model declares no default tree edge. Models with
    /// 2+ self-FK edges must set this *or* every caller passes
    /// `RelationPath` explicitly to the recursive-query builder.
    pub tree_edge: Option<syn::LitStr>,

    /// When `true`, this model opts into lifecycle-hook dispatch.
    /// Set via `#[model(hooks)]`. The macro emits
    /// `impl ::djogi::__private::hooks::Sealed for #ident {}` and
    /// `impl ::djogi::__private::hooks::HasHooks for #ident {}` so the
    /// CRUD terminals can branch monomorphically between the
    /// no-op fast path and the hook-dispatch path. The adopter must also
    /// `impl ModelHooks for MyModel` — the `HasHooks` supertrait bound
    /// (`HasHooks: ModelHooks + Sealed`) makes that requirement a compile
    /// error at the use site if it is missing.
    /// Models without the flag pay zero hook-dispatch overhead — no
    /// `HasHooks` impl is emitted, so the dispatch helpers fold to no-ops
    /// via marker-trait monomorphisation (§D2).
    /// Standalone keyword only — `hooks = true` / `hooks = false` are
    /// rejected, mirroring the convention `events`, `through`, and
    /// `no_default` already follow.
    pub hooks: bool,

    /// When `true`, this model opts into the [`Auditable`] composition
    /// 4.
    /// Set via `#[model(auditable)]`. The macro emits both:
    /// 1. `impl ::djogi::Auditable for #ident { ... }` — the trait impl
    ///    exposing `created_by(&self) -> Option<&str>`, borrowing from
    ///    the adopter-declared `pub created_by: Option<String>` field.
    /// 2. `impl #ident { #[doc(hidden)] pub(crate) fn
    /// __djogi_auditable_populate(&mut self, ctx: &mut DjogiContext)
    /// { ... } }` — the populator helper invoked from
    ///    `Model::create` between `auto_set_tenant` and the user
    ///    `before_create` hook (§D6).
    ///    Models without the flag pay zero auditable-dispatch overhead
    ///    no impl is emitted, so the populator call collapses to a
    ///    no-op and the `Auditable` bound is unsatisfied at the use site
    ///    (compile-time error if a generic asks for it).
    ///    Standalone keyword only — `auditable = true` / `auditable = false`
    ///    are rejected, mirroring the convention `hooks`, `events`,
    ///    `through`, and `no_default` already follow.
    /// # 2026-05-03 design pivot
    /// Commit 939b9ab shipped `#[derive(Auditable)]` as the
    /// opt-in surface; the current design supersedes it with this attribute and the
    /// derive is removed from the current surface. Per spec line 1037
    /// (locked 2026-05-03): proc macros cannot observe sibling derives,
    /// so the derive could not deterministically signal to
    /// `#[derive(Model)]` / `#[model(...)]`. Single
    /// `#[model(auditable)]` solves this cleanly — model macro emits
    /// the trait impl AND the populator hook wiring in one expansion.
    /// [`Auditable`]: ::djogi::Auditable
    pub auditable: bool,

    /// When `true`, this model opts into the [`SoftDeletable`] composition
    /// 6.
    /// Set via `#[model(soft_deletable)]`. The macro emits:
    /// `impl ::djogi::SoftDeletable for #ident { ... }` — the trait impl
    /// exposing `deleted_at(&self) -> Option<DateTime>`, copying from the
    /// adopter-declared `pub deleted_at: Option<DateTime>` field. The
    /// trait-level `const COLUMN: &'static str = "deleted_at"` provides
    /// the canonical column name without re-emitting it per model; the
    /// emitted `impl` inherits the default and `QuerySet::not_deleted`
    /// reads it through `<M as SoftDeletable>::COLUMN`.
    /// Models without the flag pay zero soft-deletable-dispatch overhead
    /// no impl is emitted, so the `SoftDeletable` bound is unsatisfied at
    /// the use site (compile-time error if a generic asks for it).
    /// Standalone keyword only — `soft_deletable = true` /
    /// `soft_deletable = false` are rejected, mirroring the convention
    /// `auditable`, `hooks`, `events`, `through`, and `no_default` already
    /// follow.
    /// # 2026-05-03 design pivot
    /// Commit 863c4cb shipped `#[derive(SoftDeletable)]` as the
    /// opt-in surface; the current design supersedes it with this attribute and the
    /// derive is removed from the current surface — same constraint that drove
    /// the Auditable pivot. Proc macros cannot observe sibling
    /// derives, so a derive could not deterministically signal to
    /// `#[model(...)]` that automatic default-filter composition
    /// should be wired. Doing the migration NOW is cheaper than later
    /// (otherwise composition wiring would need unwinding).
    /// [`SoftDeletable`]: ::djogi::SoftDeletable
    pub soft_deletable: bool,

    /// Parent type identifier when this model is a proxy — 2.
    /// Set via `#[model(proxy_for = ParentType)]`. The bare identifier
    /// names the parent model whose table this proxy shares; the
    /// migration differ treats proxies as schema-passthrough (no
    /// `CREATE TABLE` emitted) and uses the parent's table for SQL
    /// emission. The descriptor emitter lowers this into
    /// `ModelDescriptor.proxy_for` as a `&'static str` of the parent's
    /// name, and the trait-impl emitter wires the override methods on
    /// `Model` so the proxy queryset honours its own default filter /
    /// default ordering without leaking storage concerns into the
    /// proxy's emission path.
    /// `None` for ordinary (non-proxy) models — the common case.
    pub proxy_for: Option<syn::Ident>,

    /// Default ordering applied to every `QuerySet<Self>` on
    /// construction — 2.
    /// Set via `#[model(default_order = [(field, Asc|Desc), ...])]`.
    /// Empty when no `default_order = [...]` clause is present;
    /// otherwise the entries follow the user's source order, which
    /// becomes the SQL `ORDER BY` prefix at queryset construction.
    /// Explicit `.order_by(...)` calls APPEND to this default per
    /// Django-style semantics — see `queryset.rs:25-28` for the
    /// canonical append rule.
    /// Only meaningful when `proxy_for` is also set; the parser
    /// surfaces a span-precise error if `default_order` is set on a
    /// non-proxy model.
    pub proxy_default_order: Vec<(syn::Ident, crate::model::proxy::OrderDir)>,

    /// Default filter AND-composed into every `QuerySet<Self>` on
    /// construction — 2.
    /// Set via `#[model(default_filter = |f| f.active.eq(true))]`.
    /// The closure body is captured verbatim at parse time; the
    /// descriptor emitter walks it via recursive descent and lowers
    /// the recognised patterns (eq / gte / and_with / etc.) to a SQL
    /// fragment string, then emits that fragment as the body of the
    /// proxy's `Model::default_filter_condition` override.
    /// Only meaningful when `proxy_for` is also set; the parser
    /// surfaces a span-precise error if `default_filter` is set on a
    /// non-proxy model.
    pub proxy_default_filter: Option<syn::ExprClosure>,

    /// Watermark field for the auto-emitted [`DeltaSyncCacheable`] impl
    /// 2.
    /// Set via `#[model(watermark_field = "expires_at")]` to override
    /// the default `updated_at` watermark. The named field MUST exist
    /// on the post-injection struct; the field-existence check runs in
    /// `model::cacheable::expand` (where the user field list is in
    /// scope) and surfaces a span-precise compile error pointing at
    /// the `"…"` literal when missing.
    /// `None` means the macro pipes `updated_at` through to
    /// `sassi_codegen::WatermarkField` — every model emits a
    /// `DeltaSyncCacheable` impl by default because framework-
    /// field injection guarantees `updated_at: ::djogi::types::DateTime`
    /// exists on every model that carries a PK (i.e. every variant
    /// except `pk = None`).
    /// Stored as `Option<syn::LitStr>` so the cacheable-emit pass can
    /// attach the original literal's span to its diagnostic — same
    /// pattern as `tree_edge`.
    /// Byte-level grammar enforced at parse time: ASCII letter or
    /// underscore first byte, ASCII alphanumerics or underscores
    /// thereafter, ≤ 63 bytes (the standard Djogi identifier rule per
    /// `feedback_no_regex_in_djogi`).
    pub watermark_field: Option<syn::LitStr>,

    /// `#[model(table_comment = "<text>")]` — .
    /// Free-text table-level comment lowered by the migration composer
    /// to `COMMENT ON TABLE <t> IS '<text>'` immediately after the
    /// `CREATE TABLE` statement. The composer doubles single quotes
    /// inside the value at SQL-emission time (per the standard Postgres
    /// lexer rule under `standard_conforming_strings = on`, the PG 18
    /// default that djogi requires), so adopters can write apostrophes
    /// verbatim.
    /// Validated as non-empty / non-whitespace-only at parse time in
    /// `ModelAttrs::parse`; the descriptor stores the adopter's
    /// original text. `None` for models that declare no table-level
    /// comment — the common case.
    pub table_comment: Option<String>,

    /// `#[model(storage_params = "key=val, ...")]` — .
    /// Comma-separated Postgres storage-parameter fragment lowered to
    /// `ALTER TABLE <t> SET (key=val, ...)`. Parsed and validated at
    /// macro time; the descriptor stores a canonical safe fragment
    /// rendered from structured entries rather than the raw adopter
    /// string.
    pub storage_params: Option<String>,

    /// `#[model(tablespace = "<name>")]` — .
    /// Explicit table tablespace lowered to `ALTER TABLE <t> SET
    /// TABLESPACE <name>`. Validated with the same plain-identifier
    /// grammar used for table names so SQL emission can quote it
    /// safely and deterministically.
    pub tablespace: Option<String>,

    /// Custom visage scopes from `#[model(visage_scopes(name = Suffix, ...))]`
    /// GH #227.
    /// Each entry is `(scope_key, struct_suffix)` — e.g. `("support",
    /// "Support")` generates `{Model}Support` alongside the four built-in
    /// scope visages (`Public` / `SelfView` / `Admin` / `Export`). The
    /// scope key is the lowercase identifier the adopter uses inside
    /// `#[field(expose(...))]`; the struct suffix is the PascalCase suffix
    /// appended to the model ident to form the generated visage struct
    /// name.
    /// Validated at parse time:
    /// - Scope keys must NOT collide with [`ExposeSpec::BUILTIN_SCOPES`]
    ///   (`public` / `self_view` / `admin` / `export`) — shadowing a
    ///   built-in scope would produce two visage structs with the same
    ///   name.
    /// - Scope keys must satisfy the standard Djogi identifier grammar
    ///   (ASCII letter or underscore start, alphanumerics / underscores
    ///   after, ≤ 63 bytes) — per [`feedback_no_regex_in_djogi`], spelled
    ///   out byte-level.
    /// - Struct suffix idents must start with an uppercase ASCII letter
    ///   (matching the built-in `Public` / `SelfView` casing convention).
    /// - Scope keys must be unique within the same `visage_scopes(...)`
    ///   block.
    ///   Empty when no `visage_scopes(...)` block is present. The visage
    ///   emitter chains this Vec onto its built-in `SCOPES` table and
    ///   emits one generated visage struct per resulting `(key, suffix)`
    ///   pair.
    pub visage_scopes: Vec<(String, String)>,

    /// `#[model(strict_ids)]` — .
    /// When `true`, the macro propagates the opt-in strict structural
    /// CHECK to every column on this model whose declared shape *could*
    /// be a HeerId / RanjId carrier: the framework-injected `id` field
    /// (only when `pk` is one of `HeerId` / `HeerIdRecencyBiased` /
    /// `RanjId` / `RanjIdRecencyBiased` — Serial / Custom / None PKs
    /// receive no propagation here, because the framework `id` column
    /// is not a HeerRanjID identifier in those cases); every bare HeerId
    /// / RanjId user field; and every `ForeignKey<T>` / `OneToOneField<T>`
    /// user field. The projection layer reads the resulting descriptor
    /// flag plus the column's **HeerRanjID semantic family** — `HeerId`
    /// family projects `<col> >= 0`, `RanjId` family projects the
    /// UUIDv8 + RFC 4122 variant CHECK, and any other family (Serial,
    /// Custom, Composite, None) silently skips the CHECK.
    /// **Family vs SQL-type dispatch.** The projection layer dispatches
    /// on the descriptor's [`PkType`] semantic family
    /// ([`StrictIdFamily`] in `djogi/src/migrate/projection.rs`), not on
    /// the resolved SQL type string. A `PkType::Custom { sql_type: "BIGINT"
    /// / "UUID", .. }` PK / FK carrier shares the SQL type with HeerId /
    /// RanjId but carries no HeerRanjID bit-layout invariant; the
    /// family-based dispatch correctly maps it to `StrictIdFamily::None`
    /// (no CHECK) rather than coercing a custom adopter ID into the
    /// HeerId / RanjId structural CHECK.
    /// Models whose `pk` is not a HeerId / RanjId variant still receive
    /// the propagation on their FK columns — those FKs may target HeerId
    /// or RanjId tables, and the strict CHECK applies there. The macro
    /// cannot inspect FK target PK families at parse time, so it relies on
    /// the projection layer's per-family dispatch to filter.
    /// Standalone keyword only — `strict_ids = true` / `strict_ids = false`
    /// are rejected, mirroring the convention `hooks`, `auditable`,
    /// `soft_deletable`, `events`, `through`, and `no_default` already
    /// follow.
    pub strict_ids: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageParamEntry {
    key: String,
    value: String,
}

fn parse_storage_params_literal(lit: &syn::LitStr) -> syn::Result<String> {
    let value = lit.value();
    let entries =
        parse_storage_params(&value).map_err(|reason| syn::Error::new_spanned(lit, reason))?;
    Ok(render_storage_param_entries(&entries))
}

fn parse_storage_params(params: &str) -> Result<Vec<StorageParamEntry>, String> {
    if params.trim().is_empty() {
        return Err(
            "`storage_params = \"\"` is not allowed — value must be a non-empty \
             comma-separated storage-parameter fragment such as `fillfactor=70`."
                .to_string(),
        );
    }

    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for part in params.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("`storage_params` entries must not be empty.".to_string());
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(
                "`storage_params` entries must use `key=value` form separated by commas, \
                 for example `fillfactor=70, autovacuum_enabled=false`."
                    .to_string(),
            );
        };
        if value.contains('=') {
            return Err(
                "`storage_params` entries must contain exactly one `=` around key and value."
                    .to_string(),
            );
        }

        let key = key.trim();
        let value = value.trim();
        validate_storage_param_key(key)?;
        validate_storage_param_value(value)?;

        let key = key.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            return Err(format!("duplicate `storage_params` key `{key}`"));
        }
        entries.push(StorageParamEntry {
            key,
            value: value.to_string(),
        });
    }

    Ok(entries)
}

fn validate_storage_param_key(key: &str) -> Result<(), String> {
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return Err("`storage_params` entries must have non-empty keys.".to_string());
    }
    if bytes.len() > 63 {
        return Err("`storage_params` keys must be at most 63 bytes.".to_string());
    }
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return Err(
            "`storage_params` keys must start with an ASCII letter or underscore.".to_string(),
        );
    }
    if !bytes
        .iter()
        .skip(1)
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(
            "`storage_params` keys must be plain ASCII reloption names; dotted keys are not \
             supported."
                .to_string(),
        );
    }
    Ok(())
}

fn validate_storage_param_value(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err("`storage_params` entries must have non-empty values.".to_string());
    }
    if is_storage_param_word(bytes) {
        if is_storage_param_sql_control_word(bytes) {
            return Err(
                "`storage_params` values must not be SQL statement/control words.".to_string(),
            );
        }
        return Ok(());
    }
    if is_storage_param_number(bytes) {
        return Ok(());
    }
    Err(
        "`storage_params` values must be bare words or decimal numbers; quotes, comments, \
         commas, parentheses, semicolons, and SQL expressions are not supported."
            .to_string(),
    )
}

fn is_storage_param_word(bytes: &[u8]) -> bool {
    (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn is_storage_param_sql_control_word(bytes: &[u8]) -> bool {
    let word = bytes
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    matches!(
        word.as_slice(),
        b"alter"
            | b"begin"
            | b"call"
            | b"comment"
            | b"commit"
            | b"copy"
            | b"create"
            | b"delete"
            | b"do"
            | b"drop"
            | b"execute"
            | b"from"
            | b"grant"
            | b"insert"
            | b"reset"
            | b"revoke"
            | b"rollback"
            | b"select"
            | b"set"
            | b"table"
            | b"truncate"
            | b"union"
            | b"update"
            | b"where"
    )
}

fn is_storage_param_number(bytes: &[u8]) -> bool {
    let mut seen_dot = false;
    let mut digits_before_dot = 0usize;
    let mut digits_after_dot = 0usize;

    for byte in bytes {
        if byte.is_ascii_digit() {
            if seen_dot {
                digits_after_dot += 1;
            } else {
                digits_before_dot += 1;
            }
        } else if *byte == b'.' && !seen_dot {
            seen_dot = true;
        } else {
            return false;
        }
    }

    digits_before_dot > 0 && (!seen_dot || digits_after_dot > 0)
}

fn render_storage_param_entries(entries: &[StorageParamEntry]) -> String {
    let mut out = String::new();
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&entry.key);
        out.push('=');
        out.push_str(&entry.value);
    }
    out
}

/// Parsed `pk = X` value.
/// Grammar is a bare identifier. The old
/// string-literal grammar (`pk = "heerid"`) is rejected with a
/// span-carrying diagnostic directing callers at the new form. The
/// accepted identifier set:
/// - `HeerId` — ascending 64-bit HeerId (historical default).
/// - `RanjId` — ascending UUIDv8 RanjId.
/// - `HeerIdRecencyBiased` / `HeerIdDesc` — reverse-chronological
///   HeerId; both identifiers lower to the same
///   [`PkStrategy::HeerIdDesc`] internal variant. `HeerIdRecencyBiased`
///   is the adopter-facing name per `docs/spec/primary-keys.md` §3.5a;
///   `HeerIdDesc` is kept as a secondary alias for callers who read
///   migration internals.
/// - `RanjIdRecencyBiased` / `RanjIdDesc` — reverse-chronological
///   RanjId; same dual-name treatment.
/// - `Serial` — `SERIAL` / `INTEGER` PK for lookup tables.
/// - `None` — no framework-injected `id`; adopter manages the PK.
///   Flipped the attribute's default: omitted `pk` now
///   resolves to [`PkStrategy::HeerIdDesc`] (recency-biased), not
///   [`PkStrategy::HeerId`].
///   Adds [`PkStrategy::Custom`] — the attribute parser's
///   fall-through bucket for any identifier that is not one of the built-in
///   aliases. Carries the full `syn::Path` so the descriptor emitter can
///   reference the user's newtype via `<Path as ::djogi::primary_key::PrimaryKey>::KIND`,
///   which lowers to `PkType::Custom(CustomPrimaryKeyKind { .. })` at
///   `inventory::submit!` registration time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkStrategy {
    HeerId,
    RanjId,
    /// `pk = HeerIdRecencyBiased` (canonical) or `pk = HeerIdDesc`
    /// (internal-name alias) — reverse-chronological HeerId variant
    /// added in v3. Lowers to `PkType::HeerIdDesc`;
    /// injects `id: HeerIdDesc` into the struct.
    HeerIdDesc,
    /// `pk = RanjIdRecencyBiased` (canonical) or `pk = RanjIdDesc`
    /// (internal-name alias) — reverse-chronological RanjId variant
    /// added in v3. Lowers to `PkType::RanjIdDesc`;
    /// injects `id: RanjIdDesc` into the struct.
    RanjIdDesc,
    Serial,
    None,
    /// `pk = MyAppId` — adopter-declared custom PK, typically emitted by
    /// the `djogi::primary_key!` helper macro. The inner path is the
    /// user's type; injection and descriptor emission route trait impls
    /// through it.
    Custom(syn::Path),
}

impl ModelAttrs {
    /// Number of framework-injected fields prepended by `inject::expand`:
    /// `created_at` + `updated_at`, plus `id` when the PK strategy
    /// requires one. Use this to skip past framework fields when iterating
    /// `struct_item.fields` aligned with user-side `field_attrs`.
    pub fn framework_field_count(&self) -> usize {
        match self.pk {
            PkStrategy::None => 2,
            _ => 3,
        }
    }

    /// Parse `#[model(table = "posts", pk = HeerId)]` from the attribute token stream.
    /// Duplicate keys are rejected with a span-carrying error pointing at the
    /// second occurrence — last-write-wins silently is a footgun in proc-macro
    /// UX (users can't see which key won without expanding the macro).
    pub fn parse(attr_tokens: proc_macro2::TokenStream) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr_tokens)?;

        let mut table: Option<String> = Option::None;
        let mut pk: Option<PkStrategy> = Option::None;
        let mut no_default = false;
        let mut seen_no_default = false;
        let mut through = false;
        let mut seen_through = false;
        let mut events = false;
        let mut seen_events = false;
        let mut hooks = false;
        let mut seen_hooks = false;
        let mut auditable = false;
        let mut seen_auditable = false;
        let mut soft_deletable = false;
        let mut seen_soft_deletable = false;
        // `#[model(strict_ids)]` flag accumulator.
        let mut strict_ids = false;
        let mut seen_strict_ids = false;
        let mut idempotency_key: Option<String> = Option::None;
        let mut tenant_key: Option<String> = Option::None;
        let mut fts: Option<FtsSpec> = Option::None;
        let mut indexes: Vec<crate::model::indexes::ModelIndexDecl> = Vec::new();
        let mut seen_indexes = false;
        let mut exclusions: Vec<crate::model::exclusion::ExclusionDecl> = Vec::new();
        let mut app: Option<syn::Path> = Option::None;
        let mut moved_from_app: Option<syn::Path> = Option::None;
        let mut renamed_from: Option<String> = Option::None;
        let mut tree_edge: Option<syn::LitStr> = Option::None;
        let mut watermark_field: Option<syn::LitStr> = Option::None;
        // Djogi#217) — `#[model(table_comment = "<text>")]`
        // free-text table comment accumulator. Filled by the matching arm
        // inside the meta dispatch loop; duplicate detection happens there.
        let mut table_comment: Option<String> = Option::None;
        let mut storage_params: Option<String> = Option::None;
        let mut tablespace: Option<String> = Option::None;
        // GH #227 — `#[model(visage_scopes(name = Suffix, ...))]`.
        // Accumulator + duplicate-block detection. Each entry is
        // `(scope_key, struct_suffix)`; the visage emitter chains this
        // Vec onto its built-in `SCOPES` table.
        let mut visage_scopes: Vec<(String, String)> = Vec::new();
        let mut seen_visage_scopes = false;
        let mut proxy_for: Option<syn::Ident> = Option::None;
        let mut proxy_default_order: Vec<(syn::Ident, crate::model::proxy::OrderDir)> = Vec::new();
        let mut seen_proxy_default_order = false;
        let mut proxy_default_filter: Option<syn::ExprClosure> = Option::None;
        // Capture the span of the `default_order`
        // / `default_filter` key idents at parse time so the post-loop
        // orphan-attribute guards can surface span-precise diagnostics
        // pointing at the offending key rather than at the model's
        // `#[model(...)]` blob via `Span::call_site`.
        let mut proxy_default_order_span: Option<proc_macro2::Span> = Option::None;
        let mut proxy_default_filter_span: Option<proc_macro2::Span> = Option::None;

        for meta in &metas {
            match meta {
                // Flag-only attribute: `no_default`
                Meta::Path(path) if path.is_ident("no_default") => {
                    if seen_no_default {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `no_default` flag in #[model(...)]",
                        ));
                    }
                    seen_no_default = true;
                    no_default = true;
                }
                // Flag-only attribute: `through`
                Meta::Path(path) if path.is_ident("through") => {
                    if seen_through {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `through` flag in #[model(...)]",
                        ));
                    }
                    seen_through = true;
                    through = true;
                }
                // Flag-only attribute: `events`
                Meta::Path(path) if path.is_ident("events") => {
                    if seen_events {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `events` flag in #[model(...)]",
                        ));
                    }
                    seen_events = true;
                    events = true;
                }
                // Flag-only attribute: `hooks` — 3.
                // Standalone keyword only; the `hooks::expand` emitter
                // reads `model_attrs.hooks` to decide whether to emit
                // the `Sealed` + `HasHooks` impl pair for this model.
                Meta::Path(path) if path.is_ident("hooks") => {
                    if seen_hooks {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `hooks` flag in #[model(...)]",
                        ));
                    }
                    seen_hooks = true;
                    hooks = true;
                }
                // Flag-only attribute: `auditable`.
                // Standalone keyword only; supersedes the
                // `#[derive(Auditable)]` derive (removed 2026-05-03 per
                // spec line 1037). The model emitter reads
                // `model_attrs.auditable` to decide whether to emit:
                // 1. `impl ::djogi::Auditable for #ident { ... }`
                // 2. `impl #ident { fn __djogi_auditable_populate(...) }`
                // 3. The populator call inside `create_body` between
                // `#auto_set_tenant` and `#before_create_call`.
                Meta::Path(path) if path.is_ident("auditable") => {
                    if seen_auditable {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `auditable` flag in #[model(...)]",
                        ));
                    }
                    seen_auditable = true;
                    auditable = true;
                }
                // Flag-only attribute: `soft_deletable`.
                // Standalone keyword only; supersedes the
                // `#[derive(SoftDeletable)]` derive. The model emitter
                // reads `model_attrs.soft_deletable` to decide whether
                // to emit `impl ::djogi::SoftDeletable for #ident { ... }`
                // and to gate the `composed_via: Some("SoftDeletable")`
                // tag on the `deleted_at` column (descriptor).
                Meta::Path(path) if path.is_ident("soft_deletable") => {
                    if seen_soft_deletable {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `soft_deletable` flag in #[model(...)]",
                        ));
                    }
                    seen_soft_deletable = true;
                    soft_deletable = true;
                }
                // Flag-only attribute: `strict_ids` — .
                // Standalone keyword only; the descriptor emitter
                // propagates `model_attrs.strict_ids` to every applicable
                // column (id field on HeerId / RanjId PKs, bare HeerId /
                // RanjId user fields, every FK / O2O user field). The
                // projection layer reads the per-field flag plus the
                // column's HeerRanjID semantic family (derived from the
                // parent `PkType` for the framework `id` column and from
                // the FK target's `PkType` for relation columns) to
                // decide whether to emit the structural CHECK.
                Meta::Path(path) if path.is_ident("strict_ids") => {
                    if seen_strict_ids {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `strict_ids` flag in #[model(...)]",
                        ));
                    }
                    seen_strict_ids = true;
                    strict_ids = true;
                }
                // `pk = X` bare-identifier form. Accepts
                // only single-segment paths matching the alias set in
                // `PkStrategy::from_path`. Multi-segment paths and unknown
                // identifiers are rejected so that custom PK types
                // can't sneak through the parser.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: Expr::Path(expr_path),
                    ..
                }) if path.is_ident("pk") => {
                    if pk.is_some() {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `pk` key in #[model(...)]",
                        ));
                    }
                    pk = Some(PkStrategy::from_path(&expr_path.path)?);
                }
                // `pk = "…"` — the old string-literal form. Dedicated
                // diagnostic so callers get a clear migration message; the
                // span points at the `pk` key so the underline isolates the
                // offender rather than the whole attribute.
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(_), ..
                        }),
                    ..
                }) if path.is_ident("pk") => {
                    return Err(syn::Error::new_spanned(
                        path,
                        "`pk = \"…\"` string-literal form is removed; use bare identifier, \
                         e.g. `pk = HeerIdRecencyBiased` / `pk = HeerId` / `pk = Serial`",
                    ));
                }
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) => {
                    if path.is_ident("table") {
                        if table.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `table` key in #[model(...)]",
                            ));
                        }
                        // Safety: the table name flows
                        // into `Model::table_name` which is pushed as a
                        // raw SQL token by the SQL emitter (e.g.
                        // `OuterRef::as_qualified_expr` → `<table>.<col>`,
                        // and historically by every `FROM <table>`
                        // emission). Without validation a `table = "foo;
                        // DROP TABLE x; --"` would land arbitrary SQL into
                        // emission. Run the same Postgres unquoted-
                        // identifier classifier the column-name validator
                        // uses — non-empty, ≤ 63 bytes, ASCII letter or
                        // underscore first, alphanumeric or underscore
                        // after, never a fully-reserved keyword.
                        crate::ident::check_table_name(&s.value(), s.span())?;
                        table = Some(s.value());
                    } else if path.is_ident("idempotency_key") {
                        if idempotency_key.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `idempotency_key` key in #[model(...)]",
                            ));
                        }
                        let key_val = s.value();
                        // Validate: plain ASCII identifier. Spelled out
                        // byte-level per `feedback_no_regex_in_djogi`
                        // leading letter/underscore, rest alnum/underscore.
                        let bytes = key_val.as_bytes();
                        let ident_ok = !bytes.is_empty()
                            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                            && bytes
                                .iter()
                                .skip(1)
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !ident_ok {
                            return Err(syn::Error::new_spanned(
                                s,
                                "`idempotency_key` value must be a plain ASCII identifier \
                                 (letter or underscore, then alphanumerics/underscores)",
                            ));
                        }
                        idempotency_key = Some(key_val);
                    } else if path.is_ident("tenant_key") {
                        if tenant_key.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `tenant_key` key in #[model(...)]",
                            ));
                        }
                        let key_val = s.value();
                        // Validate: plain ASCII identifier. Byte-level check
                        // per `feedback_no_regex_in_djogi` — leading
                        // letter/underscore, rest alnum/underscore.
                        let bytes = key_val.as_bytes();
                        let ident_ok = !bytes.is_empty()
                            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                            && bytes
                                .iter()
                                .skip(1)
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !ident_ok {
                            return Err(syn::Error::new_spanned(
                                s,
                                "`tenant_key` value must be a plain ASCII identifier \
                                 (letter or underscore, then alphanumerics/underscores)",
                            ));
                        }
                        tenant_key = Some(key_val);
                    } else if path.is_ident("renamed_from") {
                        // `#[model(renamed_from = "old_table")]`
                        // table-rename hint. Same Postgres identifier
                        // grammar that `table = "..."` enforces.
                        if renamed_from.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `renamed_from` key in #[model(...)]",
                            ));
                        }
                        crate::ident::check_table_name(&s.value(), s.span())?;
                        renamed_from = Some(s.value());
                    } else if path.is_ident("tree_edge") {
                        // `#[model(tree_edge = "parent_id")]` default
                        // self-FK column for tree-recursive queries.
                        // Field-existence + self-FK validation lives
                        // at descriptor-emit time (where the user
                        // field list is in scope); here we only enforce
                        // the standard Djogi identifier grammar so the
                        // value can flow safely into the descriptor.
                        if tree_edge.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `tree_edge` key in #[model(...)]",
                            ));
                        }
                        let key_val = s.value();
                        let bytes = key_val.as_bytes();
                        // Standard Djogi identifier rule, byte-level
                        // per `feedback_no_regex_in_djogi`: ASCII letter
                        // or underscore first byte, alphanumerics or
                        // underscores after, ≤ 63 bytes.
                        let ident_ok = !bytes.is_empty()
                            && bytes.len() <= 63
                            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                            && bytes
                                .iter()
                                .skip(1)
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !ident_ok {
                            return Err(syn::Error::new_spanned(
                                s,
                                "`tree_edge` value must be a plain ASCII identifier \
                                 (letter or underscore, then alphanumerics/underscores; \
                                 max 63 bytes)",
                            ));
                        }
                        tree_edge = Some(s.clone());
                    } else if path.is_ident("watermark_field") {
                        // 2 — `#[model(watermark_field = "...")]`.
                        // Names the field that backs the auto-emitted
                        // `DeltaSyncCacheable` impl. Defaults to
                        // `updated_at` (the framework-injected timestamp);
                        // override here when the model's freshness signal
                        // is e.g. `expires_at`, a `version: i64`, or a
                        // domain-specific `recorded_at`.
                        // Field-existence validation runs in
                        // `model::cacheable::expand` where the user field
                        // list is in scope; here we only enforce the
                        // standard Djogi identifier grammar so the value
                        // can flow safely into the descriptor / sassi-
                        // codegen surface.
                        if watermark_field.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `watermark_field` key in #[model(...)]",
                            ));
                        }
                        let key_val = s.value();
                        let bytes = key_val.as_bytes();
                        // Standard Djogi identifier rule, byte-level
                        // per `feedback_no_regex_in_djogi`: ASCII letter
                        // or underscore first byte, alphanumerics or
                        // underscores after, ≤ 63 bytes.
                        let ident_ok = !bytes.is_empty()
                            && bytes.len() <= 63
                            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                            && bytes
                                .iter()
                                .skip(1)
                                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
                        if !ident_ok {
                            return Err(syn::Error::new_spanned(
                                s,
                                "`watermark_field` value must be a plain ASCII identifier \
                                 (letter or underscore, then alphanumerics/underscores; \
                                 max 63 bytes)",
                            ));
                        }
                        watermark_field = Some(s.clone());
                    } else if path.is_ident("table_comment") {
                        // Djogi#217 — adopter-supplied free-text
                        // comment lowered to `COMMENT ON TABLE … IS '…'`.
                        // The composer owns single-quote escaping at
                        // SQL-emission time so the macro accepts any
                        // non-empty / non-whitespace-only string — including
                        // apostrophes, percent signs, and other glyphs.
                        // Reject empty / whitespace-only strings so the
                        // descriptor never carries a meaningless comment
                        // that would lower to `COMMENT ON TABLE … IS ''`.
                        if table_comment.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `table_comment` key in #[model(...)]",
                            ));
                        }
                        let value = s.value();
                        if value.trim().is_empty() {
                            return Err(syn::Error::new_spanned(
                                s,
                                "`table_comment = \"\"` is not allowed — \
                                 comment must be a non-empty / non-whitespace-only \
                                 string. The composer lowers the value verbatim into \
                                 `COMMENT ON TABLE <t> IS '<text>'`; an empty literal \
                                 produces a meaningless no-op statement.",
                            ));
                        }
                        table_comment = Some(value);
                    } else if path.is_ident("storage_params") {
                        if storage_params.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `storage_params` key in #[model(...)]",
                            ));
                        }
                        storage_params = Some(parse_storage_params_literal(s)?);
                    } else if path.is_ident("tablespace") {
                        if tablespace.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `tablespace` key in #[model(...)]",
                            ));
                        }
                        crate::ident::check_table_name(&s.value(), s.span())?;
                        tablespace = Some(s.value());
                    } else if path.is_ident("proxy_for") {
                        // 2 — string-literal form rejected
                        // mirroring the `pk = "..."` rejection. Bare-ident
                        // catches typos at compile time and matches the
                        // `pk = HeerIdRecencyBiased` convention. The
                        // unused `s` binding is intentional — keeping the
                        // value out of the diagnostic prevents leaking the
                        // (likely stringified) parent name into the error
                        // message twice.
                        let _ = s;
                        return Err(syn::Error::new_spanned(
                            path,
                            "`proxy_for = \"…\"` string-literal form is not \
                             supported; use a bare identifier — e.g. \
                             `proxy_for = Vehicle`",
                        ));
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[model] attribute `{}`; expected `table`, `pk`, \
                                 `idempotency_key`, `tenant_key`, `renamed_from`, `tree_edge`, \
                                 `watermark_field`, `fts`, `indexes`, `exclusion`, \
                                 `table_comment`, \
                                 `storage_params`, `tablespace`, \
                                 `no_default`, `through`, `events`, `hooks`, `auditable`, \
                                 `soft_deletable`, `strict_ids`, `proxy_for`, `default_order`, \
                                 `default_filter`, or `visage_scopes`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                // `fts(source = "...", dictionary = "...")` — paren-delimited
                // sub-attribute parsed as a Meta::List.
                Meta::List(list) if list.path.is_ident("fts") => {
                    if fts.is_some() {
                        return Err(syn::Error::new_spanned(
                            &list.path,
                            "duplicate `fts` key in #[model(...)]",
                        ));
                    }
                    fts = Some(FtsSpec::parse_from_list(list)?);
                }
                // `visage_scopes(name = Suffix, ...)` — GH #227.
                // Paren-delimited list of `scope_ident = SuffixIdent`
                // entries. Each pair adds one custom scope to the
                // visage emitter's iteration set (alongside the four
                // built-ins). The dedicated parse helper handles
                // grammar / duplicate / shadow-builtin / identifier
                // validation so the dispatch arm here stays compact.
                Meta::List(list) if list.path.is_ident("visage_scopes") => {
                    if seen_visage_scopes {
                        return Err(syn::Error::new_spanned(
                            &list.path,
                            "duplicate `visage_scopes` key in #[model(...)]",
                        ));
                    }
                    seen_visage_scopes = true;
                    visage_scopes = parse_visage_scopes_list(list)?;
                }
                // `indexes(index(...), unique(...), ...)`
                // Full model-level grammar lives in `crate::model::indexes`;
                // parse here and stash the IR for the descriptor emitter.
                Meta::List(list) if list.path.is_ident("indexes") => {
                    if seen_indexes {
                        return Err(syn::Error::new_spanned(
                            &list.path,
                            "duplicate `indexes` key in #[model(...)]",
                        ));
                    }
                    seen_indexes = true;
                    indexes = crate::model::indexes::parse_indexes_meta_list(list)?;
                }
                // `exclusion(name = "...", using = "...", elements = [...])`
                // PR 7. Multiple `exclusion(...)` entries are
                // valid; each one parses independently and appends to the
                // accumulated Vec. Cross-entry duplicate-name detection
                // runs after the loop completes (see
                // `exclusion::validate_unique_names`).
                Meta::List(list) if list.path.is_ident("exclusion") => {
                    let decl = crate::model::exclusion::parse_exclusion_meta_list(list)?;
                    exclusions.push(decl);
                }
                // `app = Vehicles` — Value is a
                // Rust type path (not a string) since apps are
                // addressed by type per §4B. The
                // descriptor emitter lowers the path to
                // `<Path as ::djogi::App>::LABEL` at const-eval time.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: Expr::Path(expr_path),
                    ..
                }) if path.is_ident("app") => {
                    if app.is_some() {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `app = …` key in #[model(...)]",
                        ));
                    }
                    app = Some(expr_path.path.clone());
                }
                // `moved_from_app = OldBilling`
                // Same path-valued form as `app`; historical metadata
                // only, so the referenced app may be tombstoned.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: Expr::Path(expr_path),
                    ..
                }) if path.is_ident("moved_from_app") => {
                    if moved_from_app.is_some() {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `moved_from_app = …` key in #[model(...)]",
                        ));
                    }
                    moved_from_app = Some(expr_path.path.clone());
                }
                // `app` / `moved_from_app` with a non-path value
                // (commonly a string literal) — known key, wrong shape.
                // Give a specific diagnostic instead of falling into
                // the "expected key = value" generic message.
                Meta::NameValue(nv) if nv.path.is_ident("app") => {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`app = …` must be a type path (e.g. `app = Vehicles`); \
                         apps are addressed by type, not by string label",
                    ));
                }
                Meta::NameValue(nv) if nv.path.is_ident("moved_from_app") => {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`moved_from_app = …` must be a type path (e.g. \
                         `moved_from_app = OldBilling`); apps are addressed \
                         by type, not by string label",
                    ));
                }
                // `proxy_for = ParentType` — bare-identifier
                // form. Path expression matching the `pk = HeerId`
                // convention; the descriptor emitter lowers the ident
                // to a `&'static str` for `ModelDescriptor.proxy_for`.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: Expr::Path(expr_path),
                    ..
                }) if path.is_ident("proxy_for") => {
                    if proxy_for.is_some() {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `proxy_for = …` key in #[model(...)]",
                        ));
                    }
                    let ident = expr_path.path.get_ident().cloned().ok_or_else(|| {
                        syn::Error::new_spanned(
                            &expr_path.path,
                            "`proxy_for = …` must be a single-segment type \
                             identifier (e.g. `proxy_for = Vehicle`); \
                             multi-segment paths are not supported",
                        )
                    })?;
                    crate::model::proxy::validate_proxy_for_ident(&ident)?;
                    proxy_for = Some(ident);
                }
                // `default_order = [(field, Asc|Desc), ...]` — 2.
                // Array of `(ident, dir_ident)` tuples. See
                // `crate::model::proxy::parse_default_order_list` for
                // the byte-level grammar. Only meaningful for proxy
                // models; non-proxy guard runs after the dispatch loop.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: array_expr @ Expr::Array(_),
                    ..
                }) if path.is_ident("default_order") => {
                    if seen_proxy_default_order {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `default_order = …` key in #[model(...)]",
                        ));
                    }
                    seen_proxy_default_order = true;
                    // Record the key span so the
                    // post-loop orphan-attribute guard surfaces a precise
                    // diagnostic location.
                    proxy_default_order_span = Some(path.span());
                    proxy_default_order =
                        crate::model::proxy::parse_default_order_list(array_expr)?;
                }
                Meta::NameValue(nv) if nv.path.is_ident("default_order") => {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`default_order = …` value must be an array of \
                         `(field, Asc|Desc)` tuples — e.g. \
                         `default_order = [(name, Asc), (created_at, Desc)]`",
                    ));
                }
                // `default_filter = |f| <expr>`.
                // Closure expression captured verbatim; the body is walked
                // and lowered to a SQL fragment string.
                Meta::NameValue(MetaNameValue {
                    path,
                    value: closure_expr @ Expr::Closure(_),
                    ..
                }) if path.is_ident("default_filter") => {
                    if proxy_default_filter.is_some() {
                        return Err(syn::Error::new_spanned(
                            path,
                            "duplicate `default_filter = …` key in #[model(...)]",
                        ));
                    }
                    // Record the key span so the
                    // post-loop orphan-attribute guard surfaces a precise
                    // diagnostic location.
                    proxy_default_filter_span = Some(path.span());
                    proxy_default_filter = Some(crate::model::proxy::parse_default_filter_closure(
                        closure_expr,
                    )?);
                }
                Meta::NameValue(nv) if nv.path.is_ident("default_filter") => {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`default_filter = …` value must be a closure \
                         expression — e.g. `default_filter = |f| f.active.eq(true)`",
                    ));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` or `key = TypePath` attribute, \
                         or bare flag (`no_default`, `through`, `events`, `hooks`, \
                         `auditable`, `soft_deletable`); proxy keys take \
                         `proxy_for = TypePath`, `default_order = […]`, and \
                         `default_filter = |f| …`",
                    ));
                }
            }
        }

        // PR 7 — cross-entry duplicate-name check on
        // accumulated `exclusion(...)` declarations. Runs after the
        // dispatch loop so a single span-precise error covers the
        // collision rather than splitting into two diagnostics.
        crate::model::exclusion::validate_unique_names(&exclusions)?;

        // 2 — cross-attribute validation for proxy keys.
        // `default_order` / `default_filter` are only meaningful when
        // `proxy_for` is also set; standalone use surfaces as a
        // span-precise diagnostic so the adopter knows to add
        // `proxy_for = …` (or remove the orphan key).
        // Point the span at the offending key
        // ident (`default_order` / `default_filter`) rather than at
        // `Span::call_site`. The orphan-key span needs to be re-derived
        // here because the dispatch loop above consumed the original
        // `path` reference; we capture the orphan span by keeping a
        // parallel `Option<proc_macro2::Span>` alongside the value
        // collectors so the post-loop validation surfaces the right
        // diagnostic location.
        if proxy_for.is_none() && !proxy_default_order.is_empty() {
            return Err(syn::Error::new(
                proxy_default_order_span.unwrap_or_else(proc_macro2::Span::call_site),
                "`default_order = […]` requires `proxy_for = ParentType` on \
                 the same model — proxy models inherit storage from the \
                 parent and can override ordering / filtering, but a non-\
                 proxy model owns its own storage and uses explicit \
                 `.order_by(...)` calls instead",
            ));
        }
        if proxy_for.is_none() && proxy_default_filter.is_some() {
            return Err(syn::Error::new(
                proxy_default_filter_span.unwrap_or_else(proc_macro2::Span::call_site),
                "`default_filter = |f| …` requires `proxy_for = ParentType` \
                 on the same model — proxy models inherit storage from the \
                 parent and can override filtering, but a non-proxy model \
                 owns its own storage and uses explicit `.filter(...)` calls \
                 instead",
            ));
        }
        // 2 — proxy models share the parent's table by
        // construction. The macro requires `table = "..."` at parse time
        // (the table name flows into many emission sites; auto-deriving
        // from the parent would require cross-type lookup at expand time
        // which the macro pipeline does not support). Adopters declare
        // the SAME `table` value the parent uses; the macro accepts the
        // declaration as-is and the cross-type "tables match" invariant
        // is verified at descriptor-lookup time (migration-differ
        // collision detection). The risk of declaring a different table
        // than the parent is documented in `docs/guide/proxy.md`; the
        // descriptor pipeline catches the mismatch as a duplicate
        // table-name registration with conflicting `proxy_for` values.

        let table = table.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[model] requires `table = \"...\"`",
            )
        })?;

        // `events`-bearing models need the derived `{table}_outbox`
        // identifier to fit Postgres's 63-byte limit. The base table
        // already passed `check_table_name` (≤ 63), but a 60-byte name
        // would compile fine and then crash at runtime when
        // `outbox::emit_event` builds the INSERT or `query::refresh`
        // builds the poll. Reject at macro time so the error surfaces
        // at the call site instead of a deferred runtime failure.
        // The 7-byte `_outbox` suffix is tighter than the 6-byte
        // `djogi_` notify prefix, so satisfying this bound also keeps
        // the notify channel within range.
        if events && table.len() + 7 > 63 {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "#[model(table = \"{table}\", events)] — derived outbox table \
                     `{table}_outbox` would be {} bytes, exceeding Postgres's \
                     63-byte identifier limit. Shorten the source table name to \
                     ≤ 56 bytes to leave room for the `_outbox` suffix.",
                    table.len() + 7
                ),
            ));
        }

        // Flipped the default: omitted `pk` now resolves
        // to `HeerIdDesc` (recency-biased), not `HeerId`. Models that still
        // want ascending PK ordering must declare `pk = HeerId` explicitly.
        // See `docs/spec/primary-keys.md` §3.5a for the recency-biased
        // default rationale (covering-index scans land on the most-recent
        // rows first without a secondary descending index).
        let pk = pk.unwrap_or(PkStrategy::HeerIdDesc);

        Ok(ModelAttrs {
            table,
            pk,
            no_default,
            through,
            events,
            idempotency_key,
            tenant_key,
            fts,
            indexes,
            exclusions,
            app,
            moved_from_app,
            renamed_from,
            tree_edge,
            hooks,
            auditable,
            soft_deletable,
            proxy_for,
            proxy_default_order,
            proxy_default_filter,
            watermark_field,
            // Djogi#217) — adopter
            // `#[model(table_comment = "<text>")]` free-text comment.
            table_comment,
            storage_params,
            tablespace,
            // — opt-in strict HeerId /
            // RanjId structural CHECK propagation flag.
            strict_ids,
            // GH #227 — custom visage scopes.
            visage_scopes,
        })
    }
}

impl PkStrategy {
    /// Lower a `pk = X` path expression to a `PkStrategy`.
    /// Accepts the single-segment identifier set documented on
    /// [`PkStrategy`]. The two recency-biased identifiers carry
    /// public-facing and internal-facing spellings:
    /// - `HeerIdRecencyBiased` and `HeerIdDesc` both lower to
    ///   [`PkStrategy::HeerIdDesc`].
    /// - `RanjIdRecencyBiased` and `RanjIdDesc` both lower to
    ///   [`PkStrategy::RanjIdDesc`].
    ///   Any identifier that is not one of the built-in aliases is treated
    ///   as an adopter-declared custom PK type (`djogi::primary_key!` or
    ///   hand-rolled). Multi-segment paths (e.g. `crate::ids::UserId`) are
    ///   also accepted as Custom — the descriptor emitter routes through
    ///   `<Path as ::djogi::primary_key::PrimaryKey>::KIND` either way, so
    ///   the only constraint is that the path resolves to a type that
    ///   implements `PrimaryKey`. That bound is checked at `#[model]`
    ///   expansion time by the emitted trait impl lookups; a path pointing
    ///   at a non-PK type surfaces a type-error at the const-lookup site,
    ///   not here.
    fn from_path(path: &syn::Path) -> syn::Result<Self> {
        if let Some(ident) = path.get_ident() {
            return Ok(match ident.to_string().as_str() {
                "HeerId" => PkStrategy::HeerId,
                "RanjId" => PkStrategy::RanjId,
                "HeerIdRecencyBiased" | "HeerIdDesc" => PkStrategy::HeerIdDesc,
                "RanjIdRecencyBiased" | "RanjIdDesc" => PkStrategy::RanjIdDesc,
                "Serial" => PkStrategy::Serial,
                "None" => PkStrategy::None,
                _ => PkStrategy::Custom(path.clone()),
            });
        }
        Ok(PkStrategy::Custom(path.clone()))
    }
}

impl FtsSpec {
    /// Parse `{ source = "col1, col2", dictionary = "english" }` from a
    /// `Meta::List` (the `{ ... }` token stream inside `fts = { ... }`).
    /// Both `source` and `dictionary` are required. Values are validated
    /// byte-level per `feedback_no_regex_in_djogi` — no regex engine.
    fn parse_from_list(list: &syn::MetaList) -> syn::Result<Self> {
        let inner_metas = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

        let mut source: Option<String> = Option::None;
        let mut dictionary: Option<String> = Option::None;

        for meta in &inner_metas {
            match meta {
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) => {
                    if path.is_ident("source") {
                        if source.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `source` in fts { ... }",
                            ));
                        }
                        let val = s.value();
                        // Validate each column name in the comma-separated list.
                        // Byte-level checks per `feedback_no_regex_in_djogi`.
                        for col_raw in val.split(',') {
                            let col = col_raw.trim();
                            let bytes = col.as_bytes();
                            if bytes.is_empty() {
                                return Err(syn::Error::new_spanned(
                                    s,
                                    "source column name must not be empty",
                                ));
                            }
                            if bytes.len() > 63 {
                                return Err(syn::Error::new_spanned(
                                    s,
                                    format!(
                                        "source column `{col}` exceeds 63 bytes \
                                         (Postgres identifier length cap)"
                                    ),
                                ));
                            }
                            if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
                                return Err(syn::Error::new_spanned(
                                    s,
                                    format!(
                                        "source column `{col}` must start with an ASCII \
                                         letter or underscore"
                                    ),
                                ));
                            }
                            for b in bytes.iter().skip(1) {
                                if !b.is_ascii_alphanumeric() && *b != b'_' {
                                    return Err(syn::Error::new_spanned(
                                        s,
                                        format!(
                                            "source column `{col}` contains invalid character \
                                             `{}`; only ASCII letters, digits, and underscores \
                                             are allowed",
                                            *b as char
                                        ),
                                    ));
                                }
                            }
                        }
                        source = Some(val);
                    } else if path.is_ident("dictionary") {
                        if dictionary.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `dictionary` in fts { ... }",
                            ));
                        }
                        let val = s.value();
                        // Validate dictionary name: ASCII identifier, max 63 bytes.
                        // Byte-level checks per `feedback_no_regex_in_djogi`.
                        // Rule: letter or underscore start, alphanumerics or
                        // underscores after, up to 63 bytes.
                        let bytes = val.as_bytes();
                        if bytes.is_empty() {
                            return Err(syn::Error::new_spanned(
                                s,
                                "dictionary name must not be empty",
                            ));
                        }
                        if bytes.len() > 63 {
                            return Err(syn::Error::new_spanned(
                                s,
                                format!(
                                    "dictionary name `{val}` exceeds 63 bytes \
                                     (Postgres identifier length cap)"
                                ),
                            ));
                        }
                        if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
                            return Err(syn::Error::new_spanned(
                                s,
                                format!(
                                    "dictionary name `{val}` must start with an ASCII \
                                     letter or underscore"
                                ),
                            ));
                        }
                        for b in bytes.iter().skip(1) {
                            if !b.is_ascii_alphanumeric() && *b != b'_' {
                                return Err(syn::Error::new_spanned(
                                    s,
                                    format!(
                                        "dictionary name `{val}` contains invalid character \
                                         `{}`; only ASCII letters, digits, and underscores \
                                         are allowed",
                                        *b as char
                                    ),
                                ));
                            }
                        }
                        dictionary = Some(val);
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown key `{}` in fts {{ ... }}; expected `source` or `dictionary`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `source = \"...\"` or `dictionary = \"...\"` in fts { ... }",
                    ));
                }
            }
        }

        let source = source.ok_or_else(|| {
            syn::Error::new_spanned(
                list,
                "fts { ... } requires `source = \"col1, col2\"` — list of column names to index",
            )
        })?;
        let dictionary = dictionary.ok_or_else(|| {
            syn::Error::new_spanned(
                list,
                "fts { ... } requires `dictionary = \"english\"` — Postgres text-search config name",
            )
        })?;

        Ok(FtsSpec { source, dictionary })
    }
}

/// Options extracted from a single `#[field(...)]` annotation on a struct field.
/// Parsed via `darling::FromField`. Unknown keys, type mismatches, and
/// duplicate keys are reported by darling with source spans. `ident` and
/// `ty` are darling "magic" fields — the derive auto-populates them from
/// `syn::Field::{ident, ty}` at call time, independent of the attribute
/// list — so [`FieldAttrs::parse`] callers can read them alongside the
/// parsed attrs without threading the `syn::Field` separately.
// Not every field is read on every call site (e.g. `ident`/`ty` are pending
// use by later / codegen). Suppress dead_code at struct
// granularity so new fields don't spuriously re-trip the lint.
#[allow(dead_code)]
#[derive(Debug, FromField)]
#[darling(attributes(field), allow_unknown_fields)]
pub struct FieldAttrs {
    /// The struct field's identifier.
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ident` by magic field name. Always `Some(_)` for
    /// named-field structs; tuple/unit structs are rejected earlier in
    /// `inject::expand`.
    pub ident: Option<syn::Ident>,
    /// The struct field's Rust type.
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ty` by magic field name. The type must be `syn::Type`
    /// (not `Option<syn::Type>`) because the derive emits
    /// `ty: field.ty.clone` verbatim.
    pub ty: syn::Type,

    /// `#[field(unique)]` — emits a `UNIQUE` constraint in migrations.
    #[darling(default)]
    pub unique: bool,
    /// `#[field(index)]` or `#[field(index = "method")]`.
    /// Set entirely via raw attribute parsing in [`FieldAttrs::parse`], not via darling.
    /// - `false` if no index attribute
    /// - `true` if bare `#[field(index)]`
    /// - explicit methods are stored separately in index_method
    #[darling(skip)]
    pub index: bool,
    /// `#[field(index = "btree"|"gin"|"gist"|"brin"|"hash"|"spgist")]` — explicit index method.
    /// Set via raw attribute parsing in [`FieldAttrs::parse`], not via darling.
    /// Only Some if an explicit method string was provided; bare `#[field(index)]` leaves this None.
    #[darling(skip)]
    pub index_method: Option<String>,
    /// `#[field(max_length = N)]` — caps `TEXT` columns at `VARCHAR(N)`.
    #[darling(default)]
    pub max_length: Option<u32>,
    /// `#[field(renamed_from = "old_name")]` — column rename hint for migrations.
    #[darling(default)]
    pub renamed_from: Option<String>,
    /// `#[field(on_delete = "...")]` — only valid on `ForeignKey<T>` fields.
    /// Accepted values: cascade, restrict, set_null, set_default, protect,
    /// do_nothing. Darling validates the literal is a string;
    /// [`FieldAttrs::parse`] post-validates the value is in the accepted
    /// set (darling's derive alone cannot constrain a `String` domain).
    #[darling(default)]
    pub on_delete: Option<String>,
    /// `#[field(deferrable)]` — mark a relation FK as DEFERRABLE.
    #[darling(default)]
    pub deferrable: bool,
    /// `#[field(initially_deferred)]` — only valid together with
    /// `deferrable`; emits INITIALLY DEFERRED instead of INITIALLY IMMEDIATE.
    #[darling(default)]
    pub initially_deferred: bool,
    /// `#[field(outbox = "ignore")]` — strip this column from the
    /// transactional outbox payload emitted by models with
    /// `#[model(events)]`.
    /// Only valid value today is `"ignore"` (the field name appears in
    /// the outbox-exclude set for). The single-valued
    /// enum shape lives behind a string literal so future additions
    /// (`outbox = "encrypt"`, `outbox = "hash"`, etc.) slot in without
    /// reshaping the macro surface. [`FieldAttrs::parse`] post-validates
    /// the literal against the accepted set.
    #[darling(default)]
    pub outbox: Option<String>,
    /// `#[field(version)]` — marks this field as the optimistic-lock
    /// version counter. Exactly one field per model may carry this
    /// attribute, and its type must be `i32` or `i64`. On every
    /// `save` call the macro emits `{col} = {col} + 1` in the SET
    /// list and `AND {col} = $n` in the WHERE clause, binding the
    /// current in-memory value. When Postgres returns zero rows
    /// (another writer already bumped the version) `save` returns
    /// `Err(DjogiError::LockConflict(_))` rather than silently
    /// succeeding with a no-op.
    /// Only bare `i32` / `i64` are accepted — `Option<i32>` and all
    /// other types are rejected at macro-expansion time with a
    /// span-precise compile error.
    #[darling(default)]
    pub version: bool,

    /// `#[field(sequence_within = "parent_fk_column")]` — assigns this
    /// column a monotonically-increasing sequence scoped to the
    /// parent-FK column named in the attribute value. Task
    /// 7.6.
    /// At `create` time the macro wraps the INSERT in a counter
    /// upsert against `<table>_seq_<parent_fk_column>`, captures the
    /// returned `last_seq`, and assigns it to this field before the
    /// main INSERT emits. Rollback of the outer `atomic` cleans
    /// both the counter increment and the main row.
    /// Only one field per model may carry `sequence_within` today.
    /// Multiple-scope sequencing (two scoped counters on the same
    /// model) would require multiple companion tables and is a
    /// future extension.
    /// The attribute value must be a plain ASCII identifier — it is
    /// embedded directly into the counter-upsert SQL. Byte-level
    /// validation per `feedback_no_regex_in_djogi`.
    /// The companion-table DDL emission is DEFERRED to
    /// (migration system). Until then, downstream crates hand-write
    /// the `<table>_seq_<parent_fk_column>` table alongside their
    /// own migrations using the shape documented on
    /// `create_sequence_counter_sql`.
    #[darling(default)]
    pub sequence_within: Option<String>,

    /// `#[field(rationale = "...")]` — free-text justification for why this
    /// field carries a behaviour-modifying attribute such as
    /// `outbox = "ignore"`.
    /// No validation is applied to the string value — it exists purely as
    /// in-source documentation that suppresses the advisory warning emitted
    /// when `outbox = "ignore"` is present without an accompanying
    /// `rationale`. Future attribute advisories (`lazy`, `partition_by`)
    /// will key off the same field once those attributes become functional
    /// features (deferred; see Task 11 in the plan).
    #[darling(default)]
    pub rationale: Option<String>,

    /// `#[field(expose(...))]` — per-attribute parsed specs. Darling
    /// accepts **multiple** `#[field(expose(...))]` attributes on the
    /// same field via `#[darling(multiple)]`; each one is parsed
    /// independently by [`ExposeSpec`]'s `FromMeta` impl. The merged
    /// [`Self::expose`] is assembled in [`FieldAttrs::parse`] by folding
    /// this Vec (cross-attr duplicate detection + `none`/`internal`
    /// exclusivity run at merge time).
    /// Without `#[darling(multiple)]`, darling would raise
    /// `Duplicate field \`expose\`` on the second occurrence — the
    /// multi-attr merge path needs this opt-in to exist at all.
    /// Users never read this field; it is the darling landing site for
    /// the rename. Read [`Self::expose`] instead.
    #[darling(multiple, rename = "expose")]
    pub expose_raw: Vec<ExposeSpec>,

    /// Merged `expose(...)` spec across every `#[field(expose(...))]`
    /// attribute on this field — the single source of truth downstream
    /// code (descriptor emission, visage codegen) reads.
    /// Grammar summary:
    /// - Scalar form: `expose(public, self_view, admin, export)` — the
    ///   field appears in each listed scope under its column name.
    /// - Relation form: `expose(public -> UserPublic)`
    ///   narrow peer visage; `expose(public -> User)` — full peer model
    ///   embed; `expose(public -> User { manager_id -> ManagerPublic })`
    ///   nested traversal (structural metadata only at this time).
    /// - Deprecated relation form: `expose(public = "UserSummary", ...)`
    ///   string-literal shape kept for transitional backward compat.
    /// - Sentinels: `expose(none)` / `expose(internal)` — accepted no-op
    ///   sentinels, identical to an absent `expose` annotation; mutually
    ///   exclusive with real scopes on the same field.
    ///   `#[darling(skip)]` here is safe because users only ever write
    ///   `#[field(expose(...))]` (which lands in [`Self::expose_raw`] via
    ///   the rename above); nobody writes `#[field(expose = ...)]` as a
    ///   name-value targeting this field, so darling's "unknown field"
    ///   error path is never triggered.
    #[darling(skip)]
    pub expose: ExposeSpec,

    /// `#[field(default = "<sql>")]` — column DEFAULT expression used
    /// at migration-emission time.
    /// Wires the *attribute slot* so [`Self::default_volatility`]
    /// can validate "no default → meaningless override". The compose-side
    /// consumer (DDL emitter that surfaces the expression as a Postgres
    /// column DEFAULT) is the parsing
    /// surface plus the cross-attribute validation.
    /// A `default = "..."` attribute on a field has no other effect
    /// beyond satisfying the `default_volatility` precondition.
    #[darling(default)]
    pub default: Option<String>,

    /// `#[field(default_volatility = "immutable" | "stable" | "volatile")]`
    /// adopter-supplied override for the Postgres volatility
    /// classification of this field's default expression.
    /// `None` means "fall through to the static `pg_volatility.rs`
    /// lookup at compose time". Validated at parse time
    /// against the three documented choices; further cross-attr rules
    /// (must coexist with `default = ...`; redundant override warning)
    /// run in [`FieldAttrs::parse`] post-validation.
    #[darling(default)]
    pub default_volatility: Option<String>,

    /// `#[field(protected(...))]` — parsed protected-field metadata.
    /// `None` for the common case (most fields are not protected).
    /// Set entirely via raw attribute walking in
    /// [`crate::model::protected::parse_from_field`], not via darling
    /// the nested `protected(key = "value", ...)` grammar is
    /// inspected for span-precise diagnostics that darling's
    /// declarative derive cannot express. The field is `#[darling(skip)]`
    /// so no name-value `protected = ...` declaration is accepted at
    /// the field level; the only valid surface is the list form.
    #[darling(skip)]
    pub protected: Option<crate::model::protected::ProtectedSpec>,

    /// `#[field(generated = "<sql expr>")]` — PR 7.
    /// Marks this column as a Postgres generated column. Pg18 only
    /// supports `STORED`, so the descriptor's
    /// [`GeneratedColumnSpec::stored`](djogi::descriptor::GeneratedColumnSpec::stored)
    /// flag is hard-coded to `true` at lowering time. Any explicit
    /// `stored = ...` in the attribute syntax is rejected by
    /// [`FieldAttrs::parse`] — one less knob to maintain until Pg19+
    /// `VIRTUAL` support lands.
    /// Set via raw attribute walking in [`FieldAttrs::parse`] (not via
    /// darling) so the parser can reject `stored = ...` co-occurring on
    /// the same field with a span-precise diagnostic. Empty-string
    /// expressions are likewise rejected at parse time.
    #[darling(skip)]
    pub generated: Option<syn::LitStr>,

    /// `#[field(check = "<sql expr>")]` — .
    /// Adopter-supplied CHECK constraint expression. The string is
    /// emitted verbatim into both inline CREATE TABLE form
    /// (`CONSTRAINT <table>_<column>_check CHECK (<expr>)`) and the
    /// migration differ's ALTER TABLE form
    /// (`ALTER TABLE … ADD CONSTRAINT … CHECK (<expr>)`).
    /// **Raw SQL escape.** The expression is treated identically to a
    /// raw SQL fragment — djogi performs **no parsing, no sanitization,
    /// and no semantic validation** beyond rejecting empty /
    /// whitespace-only strings at parse time. Adopters are responsible
    /// for the expression's correctness against their column type and
    /// for ensuring it is idempotent (no side effects, no dependence on
    /// `now` etc.). The same `unsafe`-style cultural posture from
    /// `docs/spec/raw-sql-escape-hatches.md` applies: every callsite
    /// should be reviewable as raw SQL.
    /// **Combination with type-derived CHECKs.** When a column also
    /// receives a type-derived CHECK (e.g. an adopter `u32` field with
    /// `#[field(check = "port > 0")]`), the projection layer combines
    /// the two with logical `AND` into a single constraint slot
    /// (`<table>_<column>_check`). Both clauses must pass for an
    /// INSERT / UPDATE to land. The single constraint slot keeps the
    /// ADD / DROP / AMEND lifecycle in the differ unchanged.
    /// Set via darling. `FieldAttrs::parse` post-validates non-empty.
    #[darling(default)]
    pub check: Option<String>,

    /// `#[field(comment = "<text>")]` — .
    /// Adopter-supplied free-text column comment lowered by the
    /// migration composer to `COMMENT ON COLUMN <t>.<c> IS '<text>'`.
    /// The composer doubles single quotes inside the value at
    /// SQL-emission time (per the standard Postgres lexer rule under
    /// `standard_conforming_strings = on`, the PG 18 default that
    /// djogi requires), so adopters can write apostrophes verbatim.
    /// Set via darling. `FieldAttrs::parse` post-validates non-empty
    /// (whitespace-only also rejected) so the descriptor never carries
    /// a meaningless comment that would lower to `COMMENT ON COLUMN
    /// … IS ''`.
    #[darling(default)]
    pub comment: Option<String>,

    /// `#[field(strict_id_check)]` — .
    /// Opts in to the structural CHECK constraint for this one
    /// HeerId / RanjId / FK column. Equivalent to enabling
    /// `#[model(strict_ids)]` model-wide but scoped to a single
    /// field — useful when an adopter wants to harden one external-
    /// writer-exposed FK without enforcing strict checks across the
    /// rest of the model.
    /// **Idiomatic form is the bare flag** — `#[field(strict_id_check)]`
    /// matching the convention every flag-style field attribute
    /// (`unique`, `lazy`, `version`, etc.) follows. Darling's
    /// `#[darling(default)]` lowering also accepts the explicit
    /// `strict_id_check = true` / `strict_id_check = false` spellings
    /// (consistent with every other `bool` field attribute the
    /// `FieldAttrs` struct exposes); they are unidiomatic and lint
    /// against the rest of the codebase's spellings but neither
    /// darling nor the post-validation pass rejects them.
    /// **Type validation.** `FieldAttrs::parse` rejects this attribute
    /// on a non-applicable field type (anything that is not HeerId,
    /// HeerIdDesc, HeerIdRecencyBiased, RanjId, RanjIdDesc,
    /// RanjIdRecencyBiased, ForeignKey<T>, OneToOneField<T>, or one of
    /// their `Option<…>` / `Tracked<…>` wraps). The validation runs on
    /// the field's declared Rust type rather than waiting for projection
    /// to silently drop the CHECK — a non-applicable type is an
    /// unambiguous user error and surfacing it at parse time gives
    /// span-precise feedback.
    #[darling(default)]
    pub strict_id_check: bool,

    /// `#[field(type_change_using = "<sql expr>")]`
    /// .
    /// Adopter-supplied `USING` expression appended to the
    /// `ALTER TABLE … ALTER COLUMN … TYPE …` statement when the migration
    /// differ detects this column's SQL type changed. Unblocks non-default
    /// cast paths (e.g. `TEXT → UUID`, `TEXT → INTEGER`, custom-domain
    /// flips) that Postgres refuses to convert automatically.
    /// Set via darling; `FieldAttrs::parse` post-validates non-empty /
    /// non-whitespace-only — an empty literal would emit
    /// `USING ` which is invalid SQL and would surface only at apply
    /// time. The expression is otherwise emitted verbatim with no
    /// parsing or sanitisation — the same raw-SQL escape posture the
    /// adjacent `check` attribute uses.
    /// **One-time directive.** The attribute is consulted only at the
    /// moment the differ emits a `ChangeType` for this column. The
    /// `ColumnSchema::type_change_using` slot is `#[serde(skip)]`, so
    /// leaving the attribute on the field after applying produces no
    /// phantom diff. Adopters are encouraged (but not required) to
    /// remove it after the migration applies.
    #[darling(default)]
    pub type_change_using: Option<String>,

    /// `#[field(domain = "<name>")]` — Piece A.
    /// References an adopter-managed Postgres `CREATE DOMAIN <name> AS
    /// <base>` type that already exists in the target database. The
    /// macro lowers this to [`FieldSqlType::Domain`](djogi::FieldSqlType::Domain)
    /// in the descriptor; the migration composer emits the bare domain
    /// name as the column type in `CREATE TABLE` / `ALTER TABLE` DDL.
    /// **Adopter manages the domain.** Piece A does NOT emit
    /// `CREATE DOMAIN` DDL. The domain must already exist on the
    /// target database — typically via a hand-written `raw_ddl`
    /// invocation under `tests/` or an adopter-owned bootstrap
    /// migration. `#[model(domains = [...])]` and `CREATE DOMAIN`
    /// auto-emission are Piece B and deferred.
    /// **Schema-qualified names are out of Piece A scope.** The macro
    /// validates the name via [`check_domain_name`](crate::ident::check_domain_name),
    /// which enforces the Postgres unquoted-identifier byte-shape rule
    /// (no dots, no quotes). Adopters needing `"public.positive_amount"`
    /// fall back to [`FieldSqlType::Custom`](djogi::FieldSqlType::Custom)
    /// until Piece B.
    /// **Conflict guards** (rejected at parse time):
    /// - `domain + max_length` — the domain provides its own type
    /// constraints; layering `VARCHAR(N)` on top would emit
    /// contradictory DDL.
    /// - `domain` on a `ForeignKey<T>` / `OneToOneField<T>` field — FK
    /// column type is the target PK type; the adopter cannot override
    /// it with a domain.
    /// - `domain + generated` — generated columns derive their stored
    /// type from the expression; combining with a domain type is
    /// technically valid SQL but out of Piece A scope. Adopters
    /// needing this hand-write the migration.
    /// - `domain + strict_id_check` — domain columns are not HeerRanjID
    /// strict-id columns; the structural CHECK does not apply.
    /// **Compatible** with `#[field(check = "...")]` (the adopter
    /// CHECK adds to whatever constraints the domain already provides)
    /// and with `#[field(type_change_using = "...")]` (the USING
    /// expression drives a one-time migration from another type to
    /// the domain).
    /// Set via darling; `FieldAttrs::parse` post-validates non-empty
    /// via `check_domain_name` and the conflict guards above.
    #[darling(default)]
    pub domain: Option<String>,
}

/// Per-field visage exposure spec — parsed from `#[field(expose(...))]`.
/// See [`FieldAttrs::expose`] for the grammar summary. Scope names are
/// order-insensitive (stored in a [`HashSet`]/[`HashMap`]); source order
/// only matters for error-span recovery, which falls back to the enclosing
/// attribute list span.
/// The parser stores BOTH the scalar set and the relation map because a
/// field CAN carry both across multiple attrs — e.g.
/// `#[field(expose(public))] #[field(expose(admin -> OwnerDetail))]` marks
/// the field scalar in `public` and relation-nested in `admin`. At codegen
/// time the emitter looks up the relation map to decide if the visage entry
/// is a column name or a peer-visage type.
/// `none` / `internal` set [`Self::suppressed`] and are mutually exclusive
/// with any other scope (per Q11 in the v3 plan). They mean
/// "this field does not appear in any transport visage" — same
/// semantics as omitting the `expose` annotation.
/// Introduced the `->` traversal grammar as the new canonical
/// form; the prior `expose(scope = "Peer")` string-literal form continues to
/// parse for backward compatibility (with a `#[deprecated]` advisory).
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ExposeSpec {
    /// Scopes this field appears in via the scalar form.
    pub scalar_scopes: std::collections::HashSet<String>,
    /// Scopes this field appears in via the relation form; value carries
    /// the peer visage path plus any nested per-field exposures.
    pub relation_scopes: std::collections::HashMap<String, RelationExposure>,
    /// `true` when the user wrote `expose(none)` or `expose(internal)`.
    /// Semantically identical to an absent `expose` annotation.
    pub suppressed: bool,
}

/// One scope's relation-form exposure — parsed from
/// `expose(scope -> Peer)` or `expose(scope -> Peer { nested -> ... })`.
/// `peer` is the full `syn::Path` the user wrote after `->`. The visage
/// emitter inspects the path's last segment to decide between two embed
/// shapes:
/// - **Narrow visage** — last segment looks like `<ModelIdent><Scope>` (e.g.
///   `DepartmentPublic`). The peer field in the visage is typed `peer` and
///   constructed via `<peer as TryFrom<&Target>>::try_from(...)`.
/// - **Full peer model** — last segment equals the relation's target model
///   ident (e.g. `Department`). The peer field carries the full `Target`
///   value cloned out of the resolved relation.
///   The deprecated `expose(scope = "Peer")` string form lowers to the same
///   `RelationExposure` with `peer` parsed from the literal and `nested = []`.
///   `nested` is recursive — each entry carries the same `peer + nested`
///   shape rooted at a named field of the parent's peer model. Nested
///   exposures are STRUCTURAL METADATA only at this point; query-surface
///   machinery that consumes them is in later work.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RelationExposure {
    /// Peer visage / model path the user wrote after `->` (or inside the
    /// deprecated string literal). Preserved verbatim so module-prefixed
    /// peers like `crate::visages::DepartmentPublic` resolve at the
    /// macro-call site without an extra `use` import.
    pub peer: syn::Path,
    /// Nested per-field exposures declared in the optional `{ ... }`
    /// block following the peer path. Empty when no block was written.
    pub nested: Vec<NestedRelationExposure>,
    /// `true` when this entry came from the deprecated
    /// `expose(scope = "Peer")` string-literal form. Reserved for future
    /// `#[deprecated]` advisory wiring; structurally identical to the
    /// `->` form for emit purposes.
    pub from_string_form: bool,
}

/// One nested-block entry — `field_ident -> Peer` or
/// `field_ident -> Peer { ... }` inside an outer relation exposure's
/// `{ ... }` group.
/// Carries the parent-side field identifier alongside the recursive
/// [`RelationExposure`] payload. The field identifier is always a bare
/// identifier (no path), naming a column / relation on the parent peer
/// model. The visage emitter does not currently consume `nested`
/// (the grammar exists without wiring nested embed); later tasks
/// will lower it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct NestedRelationExposure {
    /// Field name on the parent peer model (e.g. `manager_id`).
    pub field: syn::Ident,
    /// Recursive payload — same `peer + nested` grammar as the outer
    /// [`RelationExposure`].
    pub exposure: RelationExposure,
}

impl ExposeSpec {
    /// Canonical built-in scope names. `none` / `internal` are handled
    /// specially (suppression sentinels) and NOT in this list — they are
    /// grammar tokens, not scopes.
    pub const BUILTIN_SCOPES: &'static [&'static str] = &["public", "self_view", "admin", "export"];

    fn is_suppressor(name: &str) -> bool {
        matches!(name, "none" | "internal")
    }

    /// `true` when `name` matches one of [`Self::BUILTIN_SCOPES`].
    /// Used by the visage emitter at expand time to validate that every
    /// user-declared `expose(scope)` either matches a built-in scope or
    /// matches a custom scope declared via
    /// `#[model(visage_scopes(name = Suffix))]`. The pre-Stage-4 use
    /// inside `parse_entries` was removed because custom scopes are not
    /// in scope at field-parse time; the check moved to the emitter
    /// where the full `ModelAttrs` is available.
    pub(crate) fn is_builtin_scope(name: &str) -> bool {
        Self::BUILTIN_SCOPES.contains(&name)
    }

    /// Parse the `expose(...)` argument list from a single
    /// `#[field(expose(...))]` attribute. Returns a fresh [`ExposeSpec`]
    /// covering just that attribute's tokens; cross-attribute merging
    /// lives in [`FieldAttrs::parse`].
    /// The parser is hand-rolled over `ParseStream`
    /// rather than going through `syn::Meta`, because the new arrow
    /// grammar (`scope -> PeerPath { nested -> ... }`) is not a valid
    /// `Meta` shape. The deprecated `scope = "Peer"` string-literal form
    /// is also recognised here for backward compatibility — see
    /// [`RelationExposure::from_string_form`].
    fn parse_list(list: &syn::MetaList) -> syn::Result<Self> {
        let parser = |input: ParseStream<'_>| Self::parse_entries(input);
        let spec = list.parse_args_with(parser)?;
        if spec.scalar_scopes.is_empty() && spec.relation_scopes.is_empty() && !spec.suppressed {
            return Err(syn::Error::new_spanned(
                list,
                "`expose(...)` requires at least one scope; \
                 write `expose(public)` / `expose(none)` / etc.",
            ));
        }
        Ok(spec)
    }

    /// Hand-rolled parser for the `expose(...)` body. Accepts a
    /// comma-separated list of one of:
    /// - bare scope ident → `expose(public, admin)`
    /// - suppressor `none` / `internal` (mutually exclusive with real scopes)
    /// - deprecated `scope = "Peer"` string-literal form
    /// - new `scope -> Peer` arrow form (with optional `{ nested }`)
    ///   Per-attr duplicate detection runs here; cross-attr merge lives in
    ///   [`FieldAttrs::parse`].
    fn parse_entries(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut spec = ExposeSpec::default();
        let mut suppressor_span: Option<syn::Ident> = None;
        let mut saw_real_scope = false;
        let mut first = true;
        while !input.is_empty() {
            if !first {
                input.parse::<Token![,]>()?;
                if input.is_empty() {
                    break; // trailing comma
                }
            }
            first = false;

            let scope_ident: syn::Ident = input.parse()?;
            let scope_name = scope_ident.to_string();

            // Suppressor sentinel → bare ident, no `=` or `->` follows.
            if Self::is_suppressor(&scope_name) {
                if input.peek(Token![=]) || input.peek(Token![->]) {
                    return Err(syn::Error::new_spanned(
                        &scope_ident,
                        format!(
                            "the `{scope_name}` scope does not accept a nested \
                             visage name; write `expose({scope_name})` alone"
                        ),
                    ));
                }
                if suppressor_span.is_none() {
                    suppressor_span = Some(scope_ident.clone());
                }
                spec.suppressed = true;
                continue;
            }

            // GH #227 — custom-scope names declared via
            // `#[model(visage_scopes(name = Suffix, ...))]` are not in
            // scope at field-attribute-parse time (the model attributes
            // have not been fully read by the time darling invokes
            // `FromMeta` for the field). Defer the membership check to
            // the visage emitter, which has the full `ModelAttrs` and
            // can validate `scope_name in BUILTIN_SCOPES ∪ visage_scopes`.
            // The byte-level identifier grammar still runs here so
            // typos / illegal characters surface at parse time with a
            // span-precise diagnostic.
            let scope_bytes = scope_name.as_bytes();
            let ident_ok = !scope_bytes.is_empty()
                && scope_bytes.len() <= 63
                && (scope_bytes[0].is_ascii_alphabetic() || scope_bytes[0] == b'_')
                && scope_bytes
                    .iter()
                    .skip(1)
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
            if !ident_ok {
                return Err(syn::Error::new_spanned(
                    &scope_ident,
                    format!(
                        "scope key `{scope_name}` must be a plain ASCII identifier \
                         (letter or underscore first byte, alphanumerics / \
                         underscores after, ≤ 63 bytes). Built-in scopes are \
                         `public`, `self_view`, `admin`, `export`; custom scopes \
                         are declared via `#[model(visage_scopes(name = Suffix))]`."
                    ),
                ));
            }

            saw_real_scope = true;

            // Three follow-on forms:
            // 1. `,` or end → bare-scope (scalar) form
            // 2. `= "Peer"` → deprecated string-literal relation form
            // 3. `-> Peer { nested? }` → new arrow relation form
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                let peer_lit: syn::LitStr = input.parse()?;
                let peer_path: syn::Path = syn::parse_str(&peer_lit.value())
                    .map_err(|e| syn::Error::new_spanned(&peer_lit, format!(
                        "deprecated `expose({scope_name} = \"...\")` form requires a valid path: {e}"
                    )))?;
                Self::insert_relation(
                    &mut spec,
                    &scope_ident,
                    scope_name,
                    RelationExposure {
                        peer: peer_path,
                        nested: Vec::new(),
                        from_string_form: true,
                    },
                )?;
            } else if input.peek(Token![->]) {
                input.parse::<Token![->]>()?;
                let exposure = Self::parse_relation_exposure(input, false)?;
                Self::insert_relation(&mut spec, &scope_ident, scope_name, exposure)?;
            } else {
                // Bare scope — scalar form.
                if spec.relation_scopes.contains_key(&scope_name) {
                    return Err(syn::Error::new_spanned(
                        &scope_ident,
                        format!(
                            "scope `{scope_name}` already declared with a peer \
                             visage name; pick one form per scope"
                        ),
                    ));
                }
                if !spec.scalar_scopes.insert(scope_name.clone()) {
                    return Err(syn::Error::new_spanned(
                        &scope_ident,
                        format!("scope `{scope_name}` listed more than once"),
                    ));
                }
            }
        }

        if let Some(supp) = &suppressor_span
            && saw_real_scope
        {
            return Err(syn::Error::new_spanned(
                supp,
                "`none` / `internal` cannot be combined with other scopes; \
                 omit them when declaring real scopes",
            ));
        }

        Ok(spec)
    }

    /// Parse `Peer` or `Peer { nested -> ... }` after the `->` token has
    /// already been consumed. `inside_nested_block` is currently always
    /// `false` at the top level; nested block parsing recurses through
    /// [`Self::parse_nested_block`].
    fn parse_relation_exposure(
        input: ParseStream<'_>,
        _inside_nested_block: bool,
    ) -> syn::Result<RelationExposure> {
        let peer: syn::Path = input.parse()?;
        // The nested-brace grammar (`-> Peer { field -> Peer2 }`)
        // is parseable in principle and the structural types
        // (`NestedRelationExposure`, `Self::parse_nested_block`) are
        // ready for a later task to consume, but it is not yet lowered
        // into the visage emitter. Silently parsing and discarding
        // nested traversal would be a partial-feature trap, so reject
        // any brace with an actionable compile error until the emitter
        // consumes it.
        if input.peek(syn::token::Brace) {
            return Err(input.error(
                "nested traversal `{ ... }` inside `expose(scope -> Peer { ... })` \
                 is not yet implemented by the visage emitter — ships in a later \
                 phase task. Use `scope -> PeerPath` (no braces) for now.",
            ));
        }
        Ok(RelationExposure {
            peer,
            nested: Vec::new(),
            from_string_form: false,
        })
    }

    /// Parse the body of a nested `{ ... }` block — comma-separated
    /// `field_ident -> Peer` entries, each with an optional further
    /// `{ ... }` recursion.
    /// Kept structurally alongside [`NestedRelationExposure`] even
    /// though the parser now rejects any brace at the entry site
    /// (the emitter does not yet consume nested traversal — see the
    /// rejection in `parse_relation_exposure`). The function
    /// is unreachable from the current public parser; when a later
    /// task wires nested consumption through the visage emitter, the
    /// rejection falls away and this helper becomes live again.
    #[allow(dead_code)]
    fn parse_nested_block(input: ParseStream<'_>) -> syn::Result<Vec<NestedRelationExposure>> {
        let mut out: Vec<NestedRelationExposure> = Vec::new();
        let mut first = true;
        while !input.is_empty() {
            if !first {
                input.parse::<Token![,]>()?;
                if input.is_empty() {
                    break;
                }
            }
            first = false;
            let field: syn::Ident = input.parse()?;
            // Field name is followed by `->` Peer, with optional nested.
            // A name-value `=` form is NOT supported inside nested blocks
            // the new grammar uses `->` exclusively at this level.
            input.parse::<Token![->]>()?;
            let exposure = Self::parse_relation_exposure(input, true)?;
            out.push(NestedRelationExposure { field, exposure });
        }
        Ok(out)
    }

    fn insert_relation(
        spec: &mut ExposeSpec,
        span_src: &syn::Ident,
        scope_name: String,
        exposure: RelationExposure,
    ) -> syn::Result<()> {
        if spec.scalar_scopes.contains(&scope_name) {
            return Err(syn::Error::new_spanned(
                span_src,
                format!(
                    "scope `{scope_name}` already declared as bare scope; \
                     pick one form per scope"
                ),
            ));
        }
        if spec
            .relation_scopes
            .insert(scope_name.clone(), exposure)
            .is_some()
        {
            return Err(syn::Error::new_spanned(
                span_src,
                format!("scope `{scope_name}` listed more than once"),
            ));
        }
        Ok(())
    }
}

impl FromMeta for ExposeSpec {
    fn from_meta(item: &syn::Meta) -> darling::Result<Self> {
        match item {
            syn::Meta::List(list) => Self::parse_list(list).map_err(darling::Error::from),
            _ => Err(darling::Error::custom(
                "`expose` requires the list form `expose(...)`; \
                 write `expose(public)` / `expose(public = \"UserSummary\")` / \
                 `expose(none)` / etc.",
            )
            .with_span(item)),
        }
    }
}

impl FieldAttrs {
    /// Parse `#[field(...)]` from a struct field.
    /// Returns an all-default instance if no `#[field]` attr is present
    /// (darling's `#[darling(default)]` container attr handles the no-attr
    /// case). Darling emits span-aware errors for:
    /// - Unknown attribute keys (e.g. `#[field(nonexistent)]`).
    /// - Type mismatches (e.g. `max_length = "x"` where an integer is required).
    /// - Duplicate keys across multiple `#[field(...)]` attrs.
    ///   `on_delete` is a string with a constrained value set that darling's
    ///   type-level parsing cannot enforce. We post-validate the value below
    ///   and — when rejecting — walk the field's raw `#[field(...)]` attrs
    ///   to recover the literal's `Span`, so the error underlines the bad
    ///   value rather than the entire field declaration. Matches the pre-
    ///   darling hand-rolled behaviour; keeps the surface consistent with
    ///   how `pk = X` span-points at its own path in `ModelAttrs`.
    pub fn parse(field: &syn::Field) -> syn::Result<Self> {
        // `darling::Error` carries source spans from the originating
        // attribute tokens; `From<darling::Error> for syn::Error` preserves
        // them, so rely on the built-in conversion rather than collapsing
        // everything onto the whole field with `new_spanned`.
        let mut attrs =
            <Self as darling::FromField>::from_field(field).map_err(syn::Error::from)?;

        // Manually parse index from raw attributes.
        // Support both bare `#[field(index)]` and `#[field(index = "method")]`.
        // We do this entirely outside of darling to have fine-grained control over both forms.
        attrs.index = false;
        attrs.index_method = None;
        for attr in &field.attrs {
            if !attr.path().is_ident("field") {
                continue;
            }
            let Ok(inner) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                // If the attr doesn't parse as a comma-separated Meta list,
                // darling has already emitted a better diagnostic; skip.
                continue;
            };
            for nested in &inner {
                match nested {
                    // Bare `#[field(index)]` form — Meta::Path.
                    Meta::Path(path) if path.is_ident("index") => {
                        attrs.index = true;
                    }
                    // `#[field(index = "method")]` form — Meta::NameValue with string literal.
                    Meta::NameValue(MetaNameValue {
                        path,
                        value:
                            Expr::Lit(ExprLit {
                                lit: lit @ Lit::Str(lit_str),
                                ..
                            }),
                        ..
                    }) if path.is_ident("index") => {
                        attrs.index = true; // Also set the bare flag for indexed=true
                        let method = lit_str.value();
                        let span = lit.span();
                        // Validate the method string.
                        parse_index_method(&method, span)?;
                        attrs.index_method = Some(method);
                        break; // Only one index per field.
                    }
                    // Reject non-string values for `#[field(index = X)]`.
                    Meta::NameValue(mnv) if mnv.path.is_ident("index") => {
                        return Err(syn::Error::new_spanned(
                            &mnv.value,
                            "`#[field(index)]` takes an optional string method (e.g. `index = \"gin\"`) or no value",
                        ));
                    }
                    // PR 7 — `#[field(generated = "<expr>")]`.
                    // Marks the column as `GENERATED ALWAYS AS (<expr>)
                    // STORED`. The lit is stashed onto FieldAttrs so the
                    // descriptor emitter can lower it to a
                    // `GeneratedColumnSpec`. Empty strings are rejected
                    // here so the diagnostic carries the offending
                    // literal's span.
                    Meta::NameValue(MetaNameValue {
                        path,
                        value:
                            Expr::Lit(ExprLit {
                                lit: Lit::Str(lit_str),
                                ..
                            }),
                        ..
                    }) if path.is_ident("generated") => {
                        if attrs.generated.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `#[field(generated = \"...\")]` on the same field",
                            ));
                        }
                        let val = lit_str.value();
                        if val.trim().is_empty() {
                            return Err(syn::Error::new_spanned(
                                lit_str,
                                "`#[field(generated = \"\")]` is not allowed — \
                                 expression must be a non-empty SQL fragment",
                            ));
                        }
                        attrs.generated = Some(lit_str.clone());
                    }
                    // Reject non-string values for `#[field(generated = X)]`.
                    Meta::NameValue(mnv) if mnv.path.is_ident("generated") => {
                        return Err(syn::Error::new_spanned(
                            &mnv.value,
                            "`#[field(generated = \"...\")]` requires a string literal SQL \
                             expression (e.g. `generated = \"LOWER(email)\"`)",
                        ));
                    }
                    _ => {}
                }
            }
        }

        // Validate that all field attribute keys are known.
        // Since we use `allow_unknown_fields`, darling won't reject them,
        // so we must validate manually to reject invalid keys.
        const VALID_FIELD_KEYS: &[&str] = &[
            "ident",
            "ty", // Auto-populated by darling
            "unique",
            "index",
            "max_length",
            "renamed_from",
            "on_delete",
            "deferrable",
            "initially_deferred",
            "outbox",
            "sequence_within",
            "expose",
            "rationale",
            "lazy",
            "version", // Task 3 — optimistic lock version counter
            // Protected-field metadata + default-volatility override.
            // The `protected(...)` list form is recognised here as the
            // bare key `protected`; `default` and `default_volatility`
            // carry name-value string literals.
            "default",
            "default_volatility",
            "protected",
            // PR 7 — `generated = "<expr>"` marks a Postgres
            // generated column. `stored` is intentionally NOT in this
            // list: the field-level redirect table below catches it
            // with a dedicated "implicit STORED" diagnostic.
            "generated",
            // Djogi#105 — adopter-supplied `#[field(check = "<expr>")]`
            // raw-SQL CHECK constraint. Validated as non-empty in
            // `FieldAttrs::parse`; emitted verbatim into descriptors,
            // snapshots, and migration SQL.
            "check",
            // Djogi#217 — adopter-supplied `#[field(comment = "<text>")]`
            // free-text column comment. Validated as non-empty /
            // non-whitespace-only in `FieldAttrs::parse`; emitted
            // verbatim into descriptors and snapshots. The composer
            // doubles single quotes at SQL-emission time.
            "comment",
            // Djogi#189 — adopter-supplied `#[field(strict_id_check)]`
            // opt-in flag for the HeerId / RanjId structural CHECK on this
            // field. `FieldAttrs::parse` validates that the field's Rust
            // type is one of the applicable shapes (HeerId / RanjId
            // family, ForeignKey<T>, OneToOneField<T>, or their
            // Option<…> wraps).
            "strict_id_check",
            // Djogi#220 — adopter-supplied
            // `#[field(type_change_using = "<sql expr>")]` USING clause
            // for non-default-cast column type changes. Validated as
            // non-empty / non-whitespace-only in `FieldAttrs::parse`;
            // emitted verbatim into the descriptor so the SQL emitter
            // can append `USING (<expr>)` to `ALTER COLUMN … TYPE`.
            "type_change_using",
            // Djogi#216 Piece A — adopter-supplied
            // `#[field(domain = "<name>")]` reference to a
            // pre-existing Postgres domain. The macro lowers to
            // `FieldSqlType::Domain { name, base }`; the migration
            // composer emits the domain name in the column-type slot.
            // Validated via `check_domain_name` (Postgres
            // unquoted-identifier byte shape, no reserved-keyword /
            // `__djogi_` checks). Conflict-guarded against
            // `max_length` / FK / O2O / `generated` / `strict_id_check`.
            "domain",
        ];
        // Q2/v2 #8 — `nulls_not_distinct` is deliberately
        // out of scope at the field level. The feature lives on the model-
        // level `#[model(indexes(unique(...)))]` grammar where the full
        // opclass / predicate / multi-column surface is available. Catching
        // the key here (before the generic "unknown field attribute"
        // rejection below) lets the error point users directly at the
        // fixing syntax rather than making them hunt.
        let field_name_for_redirect = field
            .ident
            .as_ref()
            .map(|i| i.to_string())
            .unwrap_or_else(|| "<anonymous>".to_string());
        let nulls_not_distinct_redirect = format!(
            "`#[field(unique, nulls_not_distinct = true)]` on `{field_name_for_redirect}`: \
             `nulls_not_distinct` is only supported at the model level. Move `{field_name_for_redirect}` \
             into a model-level unique index: \
             `#[model(indexes(unique(fields = [{field_name_for_redirect}], nulls_not_distinct = true)))]`."
        );
        // PR 7 — `stored = ...` is rejected as a field-level
        // attribute. Pg18 only supports STORED generated columns, so
        // the macro hard-codes `stored: true` at lowering time; an
        // explicit `stored = ...` would imply user-controlled choice
        // between STORED and VIRTUAL, which Pg18 does not offer. The
        // redirect prevents the generic "unknown field attribute"
        // diagnostic and instead points users at the implicit-STORED
        // rule.
        let stored_redirect = format!(
            "`#[field(stored = ...)]` on `{field_name_for_redirect}`: `stored` is implicit \
             on `#[field(generated = \"...\")]` (Pg18 supports only STORED generated columns). \
             Drop the explicit `stored = ...` — the macro emits `STORED` automatically."
        );
        let field_level_redirects: &[(&str, &str)] = &[
            ("nulls_not_distinct", nulls_not_distinct_redirect.as_str()),
            ("stored", stored_redirect.as_str()),
        ];
        for attr in &field.attrs {
            if !attr.path().is_ident("field") {
                continue;
            }
            let Ok(inner) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                continue;
            };
            for nested in &inner {
                let key = match nested {
                    Meta::Path(path) => path.get_ident().map(|i| i.to_string()),
                    Meta::NameValue(nv) => nv.path.get_ident().map(|i| i.to_string()),
                    _ => None,
                };
                if let Some(key_str) = key.as_ref() {
                    // Redirects take priority over the
                    // generic "unknown field attribute" rejection so the
                    // error actively guides the user toward the right
                    // syntax.
                    if let Some((_, message)) = field_level_redirects
                        .iter()
                        .find(|(k, _)| *k == key_str.as_str())
                    {
                        return Err(syn::Error::new_spanned(nested, message.to_string()));
                    }
                    if !VALID_FIELD_KEYS.contains(&key_str.as_str()) {
                        return Err(syn::Error::new_spanned(
                            nested,
                            format!("unknown field attribute: `{key_str}`"),
                        ));
                    }
                }
            }
        }

        // Q3 + #83
        // `#[field(unique, index = "<non-btree>")]` is rejected.
        // Field-level `unique` is lowered to an inline `UNIQUE` column
        // constraint (btree-backed by PostgreSQL); field-level
        // `index = "<method>"` is lowered to a separate secondary index
        // of that method. Combining them in one `#[field(...)]` mixes two
        // distinct schema objects under one declaration and is ambiguous:
        // - Read as "a unique index using <method>" — impossible: PG
        // unique indexes are btree-only.
        // - Read as "a unique column constraint plus a secondary <method>
        // index on the same column" — valid two-object intent, but
        // that intent is better expressed at the model level so the
        // two objects are spelled out explicitly:
        // `#[field(unique)]` + `#[model(indexes(index(fields = [col],
        // using = "<method>")))]`.
        // The rejection is broader than the original
        // hash-only rule. Hash was originally rejected on the theory
        // "hash indexes cannot enforce uniqueness" (true for
        // `CREATE UNIQUE INDEX … USING hash`, but field-level `unique`
        // does not lower to that — it lowers to an inline UNIQUE column
        // constraint Postgres backs with btree). The rejection here
        // re-grounds on the broader principle: PostgreSQL unique indexes
        // are btree-only, and mixing field-level `unique` with a
        // non-btree `index = "<method>"` is ambiguous shorthand that
        // should be spelled out at the model level.
        if attrs.unique
            && let Some(method) = attrs.index_method.as_deref()
            && method != "btree"
        {
            let field_name = field
                .ident
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let span = find_named_str_lit_span(field, "index").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                format!(
                    "`#[field(index = \"{method}\", unique)]` on `{field_name}`: PostgreSQL \
                     unique indexes are btree-only, so a non-btree `index = \"{method}\"` \
                     combined with `unique` is ambiguous. Either: (a) use \
                     `#[field(index = \"btree\", unique)]` (or just `#[field(unique)]`), \
                     (b) drop `unique` if a non-unique `{method}` lookup index is what you \
                     want, or (c) keep `#[field(unique)]` and declare the secondary \
                     `{method}` index at the model level via \
                     `#[model(indexes(index(fields = [{field_name}], using = \"{method}\")))]`."
                ),
            ));
        }

        // Q4 — `#[field(index = "gin")]` is type-gated.
        // Accepted on `Jsonb<T>`, `Vec<T>` (Postgres array columns), and the
        // FTS `TsVector` column. Anything else must declare the index via
        // the model-level `#[model(indexes(...))]` syntax, where the opclass
        // can be named (`text_pattern_ops`, `gin_trgm_ops`, etc.).
        if attrs.index_method.as_deref() == Some("gin") && !is_gin_compatible_type(&attrs.ty) {
            let field_name = field
                .ident
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let span = find_named_str_lit_span(field, "index").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                format!(
                    "`#[field(index = \"gin\")]` on `{field_name}`: GIN indexes at the field \
                     level are only valid on `Jsonb<T>`, `Vec<T>`, and `TsVector`. For other \
                     types, declare the index at the model level: \
                     `#[model(indexes(index(fields = [{field_name}], using = \"gin\", opclass = \"...\")))]`."
                ),
            ));
        }

        if let Some(on_delete) = &attrs.on_delete {
            let valid = [
                "cascade",
                "restrict",
                "set_null",
                "set_default",
                "protect",
                "do_nothing",
            ];
            if !valid.contains(&on_delete.as_str()) {
                // Locate the original `on_delete = "..."` literal in the
                // field's raw attribute tokens so the error carries the
                // literal's span, not the whole field's. Falls back to the
                // field span if the structure is unexpected — darling only
                // hands us a `String`, so the only way to recover the span
                // is a second walk. This is cheap: one field has at most a
                // handful of attrs, each with a handful of keys.
                let span = find_on_delete_lit_span(field).unwrap_or_else(|| field.span());
                return Err(syn::Error::new(
                    span,
                    format!(
                        "unknown on_delete value `{on_delete}`; expected one of: {}",
                        valid.join(", ")
                    ),
                ));
            }
        }

        if attrs.initially_deferred && !attrs.deferrable {
            let span = find_path_only_attr_span(field, "initially_deferred")
                .unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(initially_deferred)]` requires `#[field(deferrable)]` on the same field",
            ));
        }

        if let Some(outbox) = &attrs.outbox {
            // Only `"ignore"` is accepted today; future + values
            // (`"encrypt"`, `"hash"`) would slot into this list without
            // reshaping the attr surface. Mirrors the on_delete span
            // recovery pattern so the error underlines the literal,
            // not the whole field.
            let valid = ["ignore"];
            if !valid.contains(&outbox.as_str()) {
                let span = find_named_str_lit_span(field, "outbox").unwrap_or_else(|| field.span());
                return Err(syn::Error::new(
                    span,
                    format!(
                        "unknown outbox value `{outbox}`; expected one of: {}",
                        valid.join(", ")
                    ),
                ));
            }
        }

        // Validate `#[field(max_length = N)]`.
        // Postgres enforces:
        // - `VARCHAR(0)` is invalid (`length for type varchar must be at least 1`);
        // - `VARCHAR(N)` is bounded above by 10_485_760 (same as the
        // protocol row-size cap).
        // The attribute is also only meaningful on `String` fields.
        // Any non-String type gets the explicit compile-time error instead of
        // being silently retained as metadata.
        if let Some(max_length) = attrs.max_length {
            let span = find_named_int_lit_span(field, "max_length").unwrap_or_else(|| field.span());
            if max_length == 0 {
                return Err(syn::Error::new(
                    span,
                    "`#[field(max_length = 0)]` is invalid — Postgres requires \
                     `VARCHAR(N)` where N >= 1 (received N = 0)",
                ));
            }
            if max_length > 10_485_760 {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "`#[field(max_length = {max_length})]` is invalid — Postgres caps `VARCHAR` at \
                         10_485_760 (received N = {max_length})"
                    ),
                ));
            }

            let (inner_ty, _nullable) = unwrap_schema_type(&attrs.ty);
            if rust_type_to_sql(&inner_ty) != Some("TEXT") {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "`#[field(max_length = {max_length})]` is only valid on `String` fields. \
                         `String` columns emit `VARCHAR(N)`; non-String fields ignore the length \
                         contract at runtime and must not use this attribute"
                    ),
                ));
            }
        }

        // Walk raw `#[field(expose(...))]` attrs — darling's declarative
        // derive cannot destructure the two-form `expose(scope)` vs
        // `expose(scope = "Peer")` grammar, so we recover the tokens
        // ourselves. Multiple `#[field(expose(...))]` attrs on the same
        // field are merged into one `ExposeSpec`; conflict detection
        // (duplicate scope across attrs, `none` combined with anything)
        // runs at merge time.
        let mut expose = ExposeSpec::default();
        for attr in &field.attrs {
            if !attr.path().is_ident("field") {
                continue;
            }
            let Ok(inner) = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                // If the attr doesn't parse as a comma-separated Meta list,
                // darling has already emitted a better diagnostic; skip.
                continue;
            };
            for nested in &inner {
                let Meta::List(list) = nested else { continue };
                if !list.path.is_ident("expose") {
                    continue;
                }
                let parsed = ExposeSpec::parse_list(list)?;
                if parsed.suppressed {
                    if !expose.scalar_scopes.is_empty() || !expose.relation_scopes.is_empty() {
                        return Err(syn::Error::new_spanned(
                            list,
                            "cannot combine `none`/`internal` with other `expose` \
                             annotations on the same field",
                        ));
                    }
                    expose.suppressed = true;
                } else {
                    if expose.suppressed {
                        return Err(syn::Error::new_spanned(
                            list,
                            "cannot combine other `expose` annotations with a prior \
                             `none`/`internal` on the same field",
                        ));
                    }
                    for scope in parsed.scalar_scopes {
                        if expose.relation_scopes.contains_key(&scope)
                            || !expose.scalar_scopes.insert(scope.clone())
                        {
                            return Err(syn::Error::new_spanned(
                                list,
                                format!(
                                    "scope `{scope}` declared more than once across \
                                     attributes on this field"
                                ),
                            ));
                        }
                    }
                    for (scope, exposure) in parsed.relation_scopes {
                        if expose.scalar_scopes.contains(&scope)
                            || expose
                                .relation_scopes
                                .insert(scope.clone(), exposure)
                                .is_some()
                        {
                            return Err(syn::Error::new_spanned(
                                list,
                                format!(
                                    "scope `{scope}` declared more than once across \
                                     attributes on this field"
                                ),
                            ));
                        }
                    }
                }
            }
        }

        if let Some(seq) = &attrs.sequence_within {
            // Byte-level identifier check — no regex engine, no
            // regex notation per `feedback_no_regex_in_djogi`.
            // ASCII letter or underscore start, alphanumerics or
            // underscores after.
            let bytes = seq.as_bytes();
            let ident_ok = !bytes.is_empty()
                && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                && bytes
                    .iter()
                    .skip(1)
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
            if !ident_ok {
                let span = find_named_str_lit_span(field, "sequence_within")
                    .unwrap_or_else(|| field.span());
                return Err(syn::Error::new(
                    span,
                    "`sequence_within` value must be a plain ASCII identifier \
                     (letter or underscore, then alphanumerics/underscores) — \
                     it is embedded directly into SQL",
                ));
            }
        }

        attrs.expose = expose;

        // Parse `#[field(protected(...))]` via the dedicated walker.
        // Validation runs immediately so the four §6 rules surface at
        // attribute-parse time rather than deferring to descriptor
        // emission.
        attrs.protected = crate::model::protected::parse_from_field(field)?;
        if let Some(spec) = &attrs.protected {
            crate::model::protected::validate(spec, field)?;
        }

        // `#[field(default_volatility = "...")]` adopter-supplied
        // override for default-expression volatility. Validate against
        // the three documented variants and require a companion
        // `#[field(default = "...")]` so the override has something to
        // classify. The redundancy warning (override matches the
        // static-table classification) is deferred until the
        // `pg_volatility.rs` lookup table exists.
        if let Some(value) = &attrs.default_volatility {
            let span = find_named_str_lit_span(field, "default_volatility")
                .unwrap_or_else(|| field.span());
            // Reject unknown variants with the canonical error
            // message — `DefaultVolatilityLit::parse` lists the three
            // valid choices verbatim.
            crate::model::protected::DefaultVolatilityLit::parse(value, span)?;
            // Reject overrides without a paired `default = "..."`. The
            // override classifies a default expression that does not
            // exist; either add the default or drop the override.
            if attrs.default.is_none() {
                return Err(syn::Error::new(
                    span,
                    "`default_volatility` is meaningless without a \
                     `default = \"...\"` attribute on the same field — \
                     the override classifies a default expression that \
                     does not exist. Either add a `default = \"...\"` \
                     or drop the override.",
                ));
            }
            // TODO: once `pg_volatility.rs` ships, warn (not error)
            // when the override matches the static-table
            // classification. Until the lookup table exists we accept
            // the redundancy silently.
        }

        // PR 7 — `#[field(generated = "...")]` and `#[field(default
        // = "...")]` are mutually exclusive. Postgres rejects a column
        // declaration that carries both a DEFAULT clause and a
        // GENERATED ALWAYS AS (...) STORED clause; we surface the
        // conflict at macro time so the operator sees the rule before
        // any DDL is emitted.
        if attrs.generated.is_some() && attrs.default.is_some() {
            let span = attrs
                .generated
                .as_ref()
                .map(|s| s.span())
                .unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(generated = \"...\")]` cannot be combined with \
                 `#[field(default = \"...\")]`. Postgres rejects a column \
                 declaration with both a DEFAULT and a GENERATED ALWAYS \
                 AS (...) STORED clause — the generation expression is \
                 the value source. Drop the `default` attribute.",
            ));
        }

        // Djogi#105 — `#[field(check = "<expr>")]` raw-SQL CHECK
        // expression validation. The string is treated as a raw SQL fragment
        // and emitted verbatim into migration DDL; djogi does NOT parse or
        // sanitize the SQL. The only parse-time guard is that the expression
        // must be non-empty / non-whitespace-only — an empty CHECK would
        // produce `CHECK ` which is invalid SQL and would only surface as
        // an obscure failure at `cargo build` / migration apply time.
        // The span is recovered from the field's raw attribute tokens so the
        // diagnostic points at the offending literal rather than the whole
        // field declaration — mirrors the on_delete / outbox / generated
        // validation pattern above.
        if let Some(expr) = &attrs.check
            && expr.trim().is_empty()
        {
            let span = find_named_str_lit_span(field, "check").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(check = \"\")]` is not allowed — \
                 expression must be a non-empty SQL fragment. \
                 The string is emitted verbatim into the column's \
                 CHECK constraint; an empty literal produces \
                 invalid SQL.",
            ));
        }

        // Djogi#217 — `#[field(comment = "<text>")]` free-text
        // column comment. Validated as non-empty / non-whitespace-only at
        // parse time. The composer accepts arbitrary glyphs (including
        // apostrophes) inside the value and doubles single quotes at
        // SQL-emission time per the standard Postgres lexer rule under
        // `standard_conforming_strings = on`. An empty or whitespace-only
        // comment is rejected here because it would lower to a no-op
        // `COMMENT ON COLUMN … IS ''` that adds no information and
        // signals an adopter mistake.
        if let Some(text) = &attrs.comment
            && text.trim().is_empty()
        {
            let span = find_named_str_lit_span(field, "comment").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(comment = \"\")]` is not allowed — \
                 comment must be a non-empty / non-whitespace-only string. \
                 The composer lowers the value verbatim into \
                 `COMMENT ON COLUMN <t>.<c> IS '<text>'`; an empty literal \
                 produces a meaningless no-op statement.",
            ));
        }

        // Djogi#220 — `#[field(type_change_using = "<expr>")]`
        // is a raw-SQL escape consumed by the SQL emitter when the column's
        // sql_type changes. djogi performs no parsing or sanitisation of
        // the expression — it is emitted verbatim into the migration's
        // `USING (<expr>)` clause; the adopter owns correctness. The
        // only parse-time guards are:
        // 1. Non-empty / non-whitespace-only literal — an empty
        // literal would lower to `USING ` which is invalid SQL
        // and surfaces only at apply time.
        // 2. Not paired with `#[field(generated = "...")]` — a stored
        // generated column derives its storage type from the
        // expression; an adopter USING cannot meaningfully drive
        // the resulting type, and Postgres semantics for
        // `ALTER COLUMN ... TYPE ... USING (<expr>)` on a stored
        // generated column are surprising at best.
        // 3. Not applied to a `ForeignKey<T>` / `OneToOneField<T>`
        // field — FK type changes flow through the PK-flip
        // orchestration on the parent model, not as direct column
        // type changes on the child. An adopter USING here cannot
        // drive the typed flip apparatus.
        // Field-level `#[field(identity)]` is not a user-facing
        // attribute (identity flows through the projection from
        // `pk = Serial`), so a `type_change_using` × identity
        // combination cannot arise here.
        // The span is recovered from the field's raw attribute tokens
        // so each diagnostic points at the offending literal /
        // attribute rather than the whole field — mirrors the
        // `check` / `comment` validation pattern above.
        if let Some(expr) = &attrs.type_change_using
            && expr.trim().is_empty()
        {
            let span =
                find_named_str_lit_span(field, "type_change_using").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(type_change_using = \"\")]` is not allowed — \
                 expression must be a non-empty SQL fragment. The string \
                 is emitted verbatim into the migration's `USING (<expr>)` \
                 clause; an empty literal produces invalid SQL that fails \
                 only at apply time.",
            ));
        }

        if attrs.type_change_using.is_some() && attrs.generated.is_some() {
            // The diagnostic points at the `type_change_using` literal
            // because that is the attribute the operator should remove
            // the `generated` expression is the column's defining
            // contract, and an adopter who wants to flip a generated
            // column's storage type hand-writes the migration.
            let span =
                find_named_str_lit_span(field, "type_change_using").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(type_change_using = \"...\")]` is not allowed on a \
                 `#[field(generated = \"...\")]` column — the generated \
                 expression derives the column's stored type, and an adopter \
                 USING cannot meaningfully drive the resulting type. Postgres' \
                 semantics for `ALTER COLUMN ... TYPE ... USING (<expr>)` on a \
                 stored generated column are surprising at best; hand-edit \
                 the migration if a generated column needs to flip storage type.",
            ));
        }

        if attrs.type_change_using.is_some() && detect_relation(&attrs.ty).is_some() {
            let span =
                find_named_str_lit_span(field, "type_change_using").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(type_change_using = \"...\")]` is not allowed on a \
                 relation field (`ForeignKey<T>` / `OneToOneField<T>`, optionally \
                 wrapped in `Option<...>`). FK type changes flow through the \
                 PK-flip orchestration on the parent model — the child \
                 column's storage type follows the parent's PK, and an adopter \
                 USING on the child cannot drive the typed flip apparatus. Drop \
                 the attribute; if the parent's PK shape is changing, the \
                 migration emitter routes it through the PK-flip path \
                 automatically.",
            ));
        }

        // Djogi#189 — `#[field(strict_id_check)]` is only valid on
        // HeerId / RanjId / FK / O2O fields. The projection layer would
        // silently drop the CHECK on a non-applicable column (resolved
        // SQL type ≠ BIGINT / UUID), but silent failure of an explicit
        // opt-in is a poor UX — surfacing the mismatch at parse time
        // with a span-precise diagnostic is far more discoverable than
        // wondering why a `#[field(strict_id_check)]` on a `String`
        // column produces no CHECK in the snapshot. Model-wide
        // `#[model(strict_ids)]` does NOT trip this check — it's a
        // bulk opt-in and silently skipping non-applicable fields is
        // the intended behaviour.
        if attrs.strict_id_check && !is_strict_id_check_compatible(&attrs.ty) {
            let span =
                find_path_only_attr_span(field, "strict_id_check").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                "`#[field(strict_id_check)]` is only valid on HeerId / RanjId family fields \
                 (HeerId, HeerIdDesc, HeerIdRecencyBiased, RanjId, RanjIdDesc, \
                 RanjIdRecencyBiased) or relation fields (ForeignKey<T>, \
                 OneToOneField<T>), optionally wrapped in `Option<…>`. \
                 The structural CHECK applies to BIGINT (HeerId family) and \
                 UUID (RanjId family) columns; other column types have no \
                 HeerRanjID bit-layout invariant to enforce. \
                 Drop the attribute or use the model-wide `#[model(strict_ids)]` \
                 (which silently skips non-applicable fields).",
            ));
        }

        // Djogi#216 Piece A — `#[field(domain = "<name>")]`
        // post-validation. The attribute names a pre-existing Postgres
        // domain that the macro lowers to `FieldSqlType::Domain { name,
        // base: <inferred> }` for descriptor consumers. Validation:
        // 1. The name string must satisfy the Postgres unquoted-
        // identifier byte shape (`check_domain_name`). Reserved-
        // keyword and `__djogi_`-prefix checks are intentionally
        // skipped — domain names are SQL type names, not
        // column / table identifiers.
        // 2. The attribute is rejected on relation fields (FK / O2O)
        // the column's SQL type is the target model's PK type,
        // which the adopter cannot override with a domain
        // reference.
        // 3. The attribute is rejected when paired with `max_length`
        // the domain provides its own column constraints; layering
        // `VARCHAR(N)` on top would emit contradictory DDL.
        // 4. The attribute is rejected when paired with `generated`
        // generated columns derive their stored type from the
        // expression. Combining with a domain type is technically
        // valid SQL but out of Piece A scope; adopters needing
        // this hand-write the migration.
        // 5. The attribute is rejected when paired with
        // `strict_id_check` — domain columns are not HeerRanjID
        // strict-id columns; the structural CHECK does not apply.
        // Allowed pairings (deliberately not rejected):
        // - `domain + check` — the adopter CHECK adds to whatever
        // constraints the domain already provides; the projection
        // layer ANDs them into the single per-column constraint
        // slot.
        // - `domain + type_change_using` — the USING expression drives
        // a one-time migration from another type to the domain.
        // The span is recovered from the field's raw attribute tokens
        // so each diagnostic points at the offending literal /
        // attribute rather than the whole field — mirrors the
        // `check` / `comment` / `type_change_using` validation pattern
        // above.
        if let Some(name) = &attrs.domain {
            let domain_span =
                find_named_str_lit_span(field, "domain").unwrap_or_else(|| field.span());
            crate::ident::check_domain_name(name, domain_span)?;

            if detect_relation(&attrs.ty).is_some() {
                return Err(syn::Error::new(
                    domain_span,
                    "`#[field(domain = \"...\")]` is not allowed on a relation field \
                     (`ForeignKey<T>` / `OneToOneField<T>`, optionally wrapped in \
                     `Option<...>`). The FK column's SQL type is determined by the \
                     target model's PK type — the adopter cannot override it with a \
                     domain reference. Drop the attribute; if the target's PK needs \
                     to flow through a domain, declare the domain on the target's \
                     PK column instead.",
                ));
            }

            if attrs.max_length.is_some() {
                return Err(syn::Error::new(
                    domain_span,
                    "`#[field(domain = \"...\")]` cannot be combined with \
                     `#[field(max_length = N)]`. The domain provides its own \
                     column constraints (Postgres `CREATE DOMAIN <name> AS <base> \
                     [CHECK (...)]` carries length / range / regex checks baked \
                     into the type definition); layering `VARCHAR(N)` on top \
                     would emit contradictory column DDL. Drop the `max_length` \
                     attribute — the domain definition is the single source of \
                     truth for the column's length / range constraints.",
                ));
            }

            if attrs.generated.is_some() {
                return Err(syn::Error::new(
                    domain_span,
                    "`#[field(domain = \"...\")]` cannot be combined with \
                     `#[field(generated = \"...\")]` in Piece A. Postgres stored \
                     generated columns derive their column type from the generation \
                     expression; combining with a domain type is technically valid \
                     SQL but out of Piece A scope (the macro does not validate \
                     domain-vs-expression type agreement). Adopters needing a \
                     generated column whose stored type is a domain should hand-\
                     write the migration via raw DDL until djogi#216 Piece B lands.",
                ));
            }

            if attrs.strict_id_check {
                return Err(syn::Error::new(
                    domain_span,
                    "`#[field(domain = \"...\")]` cannot be combined with \
                     `#[field(strict_id_check)]`. The strict-id structural CHECK \
                     applies only to HeerId / RanjId family columns (BIGINT / UUID \
                     with bit-layout invariants); domain columns reference an \
                     adopter-managed Postgres type with its own constraints baked \
                     into the `CREATE DOMAIN` definition. Drop one of the two \
                     attributes — the domain's own CHECK constraints (declared \
                     when the adopter created the domain) are the appropriate \
                     enforcement point for domain-column invariants.",
                ));
            }
        }

        Ok(attrs)
    }
}

/// Parse an index method string to validate it against known `IndexType` methods.
/// Valid methods: `btree`, `gin`, `gist`, `brin`, `hash`, `spgist`.
/// Returns an error with a clean message listing all valid methods if the
/// string does not match. The returned error's span points to the offending
/// literal, not the whole field. Returns `Ok` if the method is valid.
fn parse_index_method(s: &str, span: proc_macro2::Span) -> syn::Result<()> {
    match s {
        "btree" | "gin" | "gist" | "brin" | "hash" | "spgist" => Ok(()),
        other => Err(syn::Error::new(
            span,
            format!(
                "unknown index method: '{other}'; expected one of btree, gin, gist, brin, hash, spgist"
            ),
        )),
    }
}

/// Parse `#[model(visage_scopes(name = Suffix, ...))]` — GH #227.
/// Returns the parsed `(scope_key, struct_suffix)` pairs in source order.
/// Validation rules (all enforced here so the diagnostic anchors at the
/// offending ident):
/// 1. Scope keys must not shadow [`ExposeSpec::BUILTIN_SCOPES`]. Shadowing
///    a built-in would attempt to emit two visage structs with the same
///    `{Model}{Suffix}` name (or with different suffixes for the same
///    scope), which contradicts the single-visage-per-scope invariant.
/// 2. Scope keys follow the standard Djogi identifier grammar (ASCII
///    letter / underscore start, alphanumerics / underscores after,
///    ≤ 63 bytes) per `feedback_no_regex_in_djogi`.
/// 3. Struct suffix idents must start with an uppercase ASCII letter so
///    `{Model}{Suffix}` mirrors the `{Model}Public` casing convention.
/// 4. Scope keys must be unique within the same `visage_scopes(...)`
///    block — the second occurrence is rejected with a span-precise
///    diagnostic anchored at the duplicate ident.
fn parse_visage_scopes_list(list: &syn::MetaList) -> syn::Result<Vec<(String, String)>> {
    let entries: Punctuated<Meta, Token![,]> =
        list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

    let mut out: Vec<(String, String)> = Vec::new();
    for meta in &entries {
        let Meta::NameValue(MetaNameValue {
            path,
            value: Expr::Path(expr_path),
            ..
        }) = meta
        else {
            return Err(syn::Error::new(
                meta.span(),
                "every `visage_scopes(...)` entry must be \
                 `scope_ident = SuffixIdent`",
            ));
        };

        let Some(scope_ident) = path.get_ident() else {
            return Err(syn::Error::new(
                path.span(),
                "`visage_scopes(...)` scope key must be a single-segment ident",
            ));
        };
        let scope_name = scope_ident.to_string();

        // Rule (1): shadowing built-in scopes is rejected. The built-in
        // visage emitter unconditionally emits `{Model}Public` etc.; an
        // adopter cannot replace those via `visage_scopes(...)`.
        if crate::model::attrs::ExposeSpec::BUILTIN_SCOPES.contains(&scope_name.as_str()) {
            return Err(syn::Error::new(
                scope_ident.span(),
                format!(
                    "`visage_scopes(...)` scope `{scope_name}` collides with a built-in \
                     scope. The four built-in scopes (public, self_view, admin, export) \
                     are emitted automatically — `visage_scopes(...)` is for declaring \
                     ADDITIONAL custom scopes only.",
                ),
            ));
        }

        // Rule (2): byte-level identifier grammar — no regex per
        // `feedback_no_regex_in_djogi`. Spelled out: ASCII letter or
        // underscore first byte, alphanumerics / underscores after,
        // ≤ 63 bytes (Postgres unquoted-identifier cap mirrored for
        // consistency with the rest of the macro's identifier rules).
        let bytes = scope_name.as_bytes();
        let ident_ok = !bytes.is_empty()
            && bytes.len() <= 63
            && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            && bytes
                .iter()
                .skip(1)
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if !ident_ok {
            return Err(syn::Error::new(
                scope_ident.span(),
                format!(
                    "`visage_scopes(...)` scope key `{scope_name}` must be a plain \
                     ASCII identifier — letter or underscore first byte, \
                     alphanumerics / underscores after, at most 63 bytes",
                ),
            ));
        }

        // Rule (4): duplicate scope keys are rejected. The diagnostic
        // anchors at the second occurrence so the underline points at
        // the offending ident rather than the whole `visage_scopes(...)`
        // block.
        if out.iter().any(|(k, _)| k == &scope_name) {
            return Err(syn::Error::new(
                scope_ident.span(),
                format!("`visage_scopes(...)` scope key `{scope_name}` declared twice",),
            ));
        }

        // Right-hand side: a path expression naming the struct suffix
        // ident — e.g. `Support` in `support = Support`. Must be a
        // single-segment ident whose first byte is an uppercase ASCII
        // letter so `{Model}{Suffix}` reads naturally.
        let Some(suffix_ident) = expr_path.path.get_ident() else {
            return Err(syn::Error::new(
                expr_path.path.span(),
                "`visage_scopes(...)` struct suffix must be a single-segment ident",
            ));
        };
        let suffix_name = suffix_ident.to_string();
        let suffix_bytes = suffix_name.as_bytes();
        let suffix_ok = !suffix_bytes.is_empty()
            && suffix_bytes[0].is_ascii_uppercase()
            && suffix_bytes
                .iter()
                .skip(1)
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if !suffix_ok {
            return Err(syn::Error::new(
                suffix_ident.span(),
                format!(
                    "`visage_scopes(...)` struct suffix `{suffix_name}` must start \
                     with an uppercase ASCII letter (matching the `Public` / \
                     `SelfView` casing convention)",
                ),
            ));
        }

        out.push((scope_name, suffix_name));
    }

    Ok(out)
}

/// `true` when `ty` is a bare HeerId / RanjId family scalar (any of the
/// six type names from `HeerId` / `HeerIdDesc` / `HeerIdRecencyBiased` /
/// `RanjId` / `RanjIdDesc` / `RanjIdRecencyBiased`, in bare / `djogi::*`
/// / `djogi::types::*` form). Routes through [`unwrap_schema_type`], so
/// `Option<…>`, `Tracked<…>`, and combined `Tracked<Option<…>>` /
/// `Option<Tracked<…>>` wrappers are all stripped to the underlying
/// scalar before the family match runs.
/// Returns `false` for relation fields (`ForeignKey<T>` etc.) and for
/// every non-HeerId / non-RanjId scalar. Used at descriptor-emit time
/// by the `#[model(strict_ids)]` propagation logic to set
/// `strict_id_check: true` on bare HeerId / RanjId user fields — the
/// relation-field arm is handled by [`detect_relation`] one branch
/// over.
/// .
pub fn is_bare_heeranjid_family_type(ty: &syn::Type) -> bool {
    let (inner, _nullable) = unwrap_schema_type(ty);
    let s = quote::quote!(#inner).to_string().replace(' ', "");
    let s = s.strip_prefix("::").unwrap_or(&s);
    matches!(
        s,
        "HeerId"
            | "HeerIdDesc"
            | "HeerIdRecencyBiased"
            | "djogi::HeerId"
            | "djogi::HeerIdDesc"
            | "djogi::HeerIdRecencyBiased"
            | "djogi::types::HeerId"
            | "djogi::types::HeerIdDesc"
            | "djogi::types::HeerIdRecencyBiased"
            | "RanjId"
            | "RanjIdDesc"
            | "RanjIdRecencyBiased"
            | "djogi::RanjId"
            | "djogi::RanjIdDesc"
            | "djogi::RanjIdRecencyBiased"
            | "djogi::types::RanjId"
            | "djogi::types::RanjIdDesc"
            | "djogi::types::RanjIdRecencyBiased"
    )
}

/// `true` when `ty` is an accepted target for `#[field(strict_id_check)]`
/// i.e. a bare HeerId / HeerIdDesc / HeerIdRecencyBiased / RanjId /
/// RanjIdDesc / RanjIdRecencyBiased field, OR a relation field
/// (`ForeignKey<T>` / `OneToOneField<T>`). `Option<…>` and `Tracked<…>`
/// are unwrapped via [`unwrap_schema_type`] so nullable / dirty-tracked
/// columns are accepted.
/// **Why FK / O2O are accepted without checking the target's PK type.**
/// The macro cannot inspect the FK target's `pk` strategy at parse time
/// the target type may live in another crate or below the current
/// model in source order. The projection layer resolves the FK target's
/// HeerRanjID semantic family at descriptor → snapshot lowering time
/// (via `type_to_pk_family`) and silently drops the CHECK when the
/// target's PK family is not HeerId / RanjId — for example, an FK to a
/// `PkType::Serial`, `PkType::Custom`, `PkType::Composite`, or
/// `PkType::None` target. Accepting all FK / O2O fields here preserves
/// the explicit opt-in surface without forcing the adopter to coordinate
/// parse-order between target and source models. The applicability
/// filter is the projection's responsibility; the macro's job is the
/// per-field opt-in surface.
/// **Family-based filter, not SQL-type-based filter (
/// post-review hardening).** The projection layer dispatches the strict
/// CHECK on the target's HeerRanjID semantic family ([`StrictIdFamily`]
/// in `djogi/src/migrate/projection.rs`), not on the resolved SQL type
/// string. An FK to a `PkType::Custom { sql_type: "BIGINT" / "UUID", .. }`
/// would have inherited the HeerId / RanjId CHECK under the earlier
/// SQL-type-only dispatch by SQL-carrier collision; the family-based
/// dispatch correctly maps it to `StrictIdFamily::None`.
/// Path forms accepted: bare identifier (after `use djogi::prelude::*;`),
/// `djogi::HeerId`, `djogi::types::HeerId`, and equivalent shapes for
/// every alias in the HeerId / RanjId family. The
/// `detect_relation` helper handles fully-qualified `ForeignKey` and
/// `OneToOneField` paths the same way the relation-detection pipeline
/// does elsewhere.
fn is_strict_id_check_compatible(ty: &syn::Type) -> bool {
    // Relation fields (FK / O2O) — accept; projection resolves the
    // target's HeerRanjID semantic family and gates the CHECK.
    if detect_relation(ty).is_some() {
        return true;
    }
    // Bare HeerId / RanjId family — accept.
    is_bare_heeranjid_family_type(ty)
}

/// `true` when `ty` is an accepted target for `#[field(index = "gin")]`
/// i.e. `Jsonb<T>`, `MirJzSON`, `Vec<T>`, or `TsVector`, unwrapping one
/// layer of `Option<…>` so nullable columns are accepted too.
/// Uses last-segment name matching so bare idents, `djogi::Jsonb<T>`,
/// `djogi::jsonb::MirJzSON`, `djogi::fts::TsVector`, and similar qualified
/// forms all resolve. Q4 codifies this set; anything
/// outside it must declare the index at the model level where opclass
/// can be specified.
/// #195 adds `MirJzSON` to the set — it is the raw-JSONB
/// sibling of `Jsonb<T>` and shares the same GIN-index suitability for
/// containment / `jsonb_path_ops` lookups.
fn is_gin_compatible_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| {
            matches!(
                seg.ident.to_string().as_str(),
                "Jsonb" | "MirJzSON" | "Vec" | "TsVector"
            )
        })
        .unwrap_or(false)
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the literal bound to `on_delete`, if present.
/// Thin wrapper around [`find_named_str_lit_span`] kept for readability at
/// the callsite.
fn find_on_delete_lit_span(field: &syn::Field) -> Option<proc_macro2::Span> {
    find_named_str_lit_span(field, "on_delete")
}

fn find_path_only_attr_span(field: &syn::Field, key: &str) -> Option<proc_macro2::Span> {
    for attr in &field.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        let metas = attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()?;
        for meta in &metas {
            if let Meta::Path(path) = meta
                && path.is_ident(key)
            {
                return Some(path.span());
            }
        }
    }
    None
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the string literal bound to `key`, if present.
/// Used by [`FieldAttrs::parse`] to recover the offending literal's span
/// after darling has already reduced the attribute to a `String`. Returns
/// `None` only if the attribute structure does not match the expected
/// `key = "literal"` shape — darling's own parse will have rejected that
/// upstream, so the caller falls back to the field's span as a last
/// resort without losing the error. Shared between `on_delete` and
/// `outbox` validation to keep the span-recovery logic in one place.
fn find_named_str_lit_span(field: &syn::Field, key: &str) -> Option<proc_macro2::Span> {
    for attr in &field.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        let metas = attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()?;
        for meta in &metas {
            if let Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: lit @ Lit::Str(_),
                        ..
                    }),
                ..
            }) = meta
                && path.is_ident(key)
            {
                return Some(lit.span());
            }
        }
    }
    None
}

/// Walk `#[field(...)]` attrs and return the span of the integer literal
/// paired with `key = <integer>`. Used by post-parse validators to
/// produce span-precise errors that underline the offending literal
/// rather than the whole field.
/// Mirrors [`find_named_str_lit_span`] but matches integer literals
/// (`Lit::Int`) instead of string literals (`Lit::Str`).
fn find_named_int_lit_span(field: &syn::Field, key: &str) -> Option<proc_macro2::Span> {
    for attr in &field.attrs {
        if !attr.path().is_ident("field") {
            continue;
        }
        let metas = attr
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()?;
        for meta in &metas {
            if let Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: lit @ Lit::Int(_),
                        ..
                    }),
                ..
            }) = meta
                && path.is_ident(key)
            {
                return Some(lit.span());
            }
        }
    }
    None
}

/// The SQL column type for a Rust type string.
/// Returns `None` for `Option<T>` (handled by the caller via `unwrap_option`) and
/// for unrecognized types (the caller should emit a compile error).
/// Called by `descriptor::expand` and `inject::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn rust_type_to_sql(ty: &syn::Type) -> Option<&'static str> {
    // Structural recognition for `Jsonb<T>` wrappers — runs BEFORE the
    // string-based match so any path-form `Jsonb<…>` resolves to JSONB
    // regardless of how it was spelled. Catches `Jsonb<T>`,
    // `djogi::Jsonb<T>`, `djogi::jsonb::Jsonb<T>`, `crate::Jsonb<T>`,
    // `super::Jsonb<T>`, `::djogi::Jsonb<T>`, etc. — anywhere the path's
    // last segment is `Jsonb` AND it carries angle-bracket generic
    // arguments.
    // The previous string-prefix matcher only recognized three exact
    // prefixes (`Jsonb<`, `djogi::Jsonb<`,
    // `djogi::jsonb::Jsonb<`), silently mapping `crate::Jsonb<T>` /
    // `super::Jsonb<T>` to TEXT and producing INSERT failures on the
    // typed Jsonb round-trip. Switching to last-segment ident check
    // closes the family.
    if let syn::Type::Path(syn::TypePath { qself: None, path }) = ty
        && let Some(last) = path.segments.last()
        && last.ident == "Jsonb"
        && matches!(last.arguments, syn::PathArguments::AngleBracketed(_))
    {
        return Some("JSONB");
    }

    // #195 — `MirJzSON` (Djogi's raw / unschemed JSONB
    // escape hatch) lowers to JSONB. Mirrors the `Jsonb<T>` matcher
    // shape: last-segment ident match, no generic arguments
    // (`MirJzSON` is nullary). Covers bare `MirJzSON`, `djogi::MirJzSON`,
    // `djogi::jsonb::MirJzSON`, `crate::MirJzSON`, `super::MirJzSON`,
    // and `::djogi::*::MirJzSON` path forms uniformly.
    // The macro-level `#[mirjzson(justification = "...")]` gate
    // (`super::mirjzson`) enforces the adopter-visible discipline; this
    // arm just maps the type to the right SQL column type so descriptor
    // emission and migration diffs work without extra surface.
    if let syn::Type::Path(syn::TypePath { qself: None, path }) = ty
        && let Some(last) = path.segments.last()
        && last.ident == "MirJzSON"
        && matches!(last.arguments, syn::PathArguments::None)
    {
        return Some("JSONB");
    }

    // runtime-backed
    // `djogi::Range<T>` wrappers lower to the matching Postgres range
    // type based on the element type. The outer wrapper intentionally
    // uses an explicit namespace policy instead of last-segment matching:
    // accept only bare `Range<T>` (for `djogi::prelude::*` / direct
    // `djogi::Range` imports), `djogi::Range<T>`,
    // `djogi::types::Range<T>`, `::djogi::Range<T>`, and
    // `::djogi::types::Range<T>`.
    // Do not accept `std::ops::Range<T>`, `core::ops::Range<T>`,
    // `crate::Range<T>`, `self::Range<T>`, `super::Range<T>`, or
    // adopter-module `foo::Range<T>` lookalikes: those are not the
    // runtime codec type and must fall through to the caller's
    // `DjogiSqlType` field-site check.
    // Element-type mapping is deliberately narrower than
    // `rust_type_to_sql(inner)`: only the six element Rust types with a
    // matching `RangeSubtype` impl are accepted here. This avoids
    // silently widening `Range<u32>` to `int8range` or `Range<HeerId>`
    // to `int8range` when no runtime range codec exists for those
    // element types. Unsupported elements fall through to `None`, where
    // the caller's `field_sql_type_tokens` fallback attempts
    // `<#ty as ::djogi::descriptor::DjogiSqlType>::SQL_TYPE` and rustc
    // reports the missing `DjogiSqlType` bound at the field site.
    // Future siblings: (`btree_gist` EXCLUDE grammar) and
    // (PG18 temporal DDL) build on top of this lowering.
    if let syn::Type::Path(syn::TypePath { qself: None, path }) = ty
        && is_djogi_range_outer_path(path)
        && let Some(last) = path.segments.last()
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && args.args.len() == 1
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        let inner_path = quote::quote!(#inner).to_string().replace(' ', "");
        let inner_path = inner_path.strip_prefix("::").unwrap_or(&inner_path);
        return match inner_path {
            "i32" => Some("INT4RANGE"),
            "i64" => Some("INT8RANGE"),
            "Decimal" | "rust_decimal::Decimal" => Some("NUMRANGE"),
            "PrimitiveDateTime" | "time::PrimitiveDateTime" => Some("TSRANGE"),
            "DateTime"
            | "OffsetDateTime"
            | "time::OffsetDateTime"
            | "djogi::DateTime"
            | "djogi::types::DateTime" => Some("TSTZRANGE"),
            "Date" | "time::Date" | "djogi::Date" | "djogi::types::Date" => Some("DATERANGE"),
            _ => None,
        };
    }

    // Normalise whitespace and strip an optional leading `::` so absolute
    // paths (`::djogi::types::HeerId`) match the same arms as their
    // relative counterparts (`djogi::types::HeerId`).
    let s = quote::quote!(#ty).to_string().replace(' ', "");
    let s = s.strip_prefix("::").unwrap_or(&s);
    match s {
        "String" => Some("TEXT"),
        // Narrow / unsigned integer widening (+ ).
        // Postgres has no native unsigned integer types, and its `i8`
        // column type (the `"char"` pseudo-type) is not suitable for
        // user-facing model fields. Djogi widens each type to the
        // smallest signed Postgres integer type whose positive range
        // covers the full Rust type range:
        // i8 → SMALLINT (INT2) range: -128..=127
        // u8 → SMALLINT (INT2) range: 0..=255
        // u16 → INTEGER (INT4) range: 0..=65535
        // u32 → BIGINT (INT8) range: 0..=4294967295
        // u64 → NUMERIC range: 0..=18446744073709551615 (+ integrality CHECK)
        // The macro emits bind shims (widen before binding to tokio-postgres)
        // and decode shims (narrow with a bounds-checked try_from). The
        // FieldDescriptor carries a `rust_source_type` discriminator so
        // the migration projection layer can emit a range CHECK for the
        // column that matches the Rust type's representable range.
        "i8" => Some("SMALLINT"),
        "u8" => Some("SMALLINT"),
        "u16" => Some("INTEGER"),
        "u32" => Some("BIGINT"),
        // u64 uses bare NUMERIC (no precision/scale) so Postgres
        // does not silently round fractional inputs before the CHECK fires.
        // The CHECK emitted by the projection layer enforces integrality
        // (col = trunc(col)) in addition to the range bounds.
        "u64" => Some("NUMERIC"),
        "i16" => Some("SMALLINT"),
        "i32" => Some("INTEGER"),
        "i64" => Some("BIGINT"),
        "f32" => Some("REAL"),
        "f64" => Some("DOUBLE PRECISION"),
        "bool" => Some("BOOLEAN"),
        // Regression guard: the original arm matched only the bare `DateTime`
        // and
        // `time::*` short forms, so user fields spelled
        // `djogi::DateTime` / `djogi::types::DateTime` (the canonical
        // adopter spellings, since `pub use crate::types::DateTime`
        // re-exports those at the crate root) silently fell through
        // and lowered to TEXT in the descriptor. Same alias family as
        // GH issue #40.
        "DateTime"
        | "time::OffsetDateTime"
        | "OffsetDateTime"
        | "djogi::DateTime"
        | "djogi::types::DateTime" => Some("TIMESTAMPTZ"),
        "Date" | "time::Date" | "djogi::Date" | "djogi::types::Date" => Some("DATE"),
        "Decimal" | "rust_decimal::Decimal" => Some("NUMERIC"),
        "Uuid" | "uuid::Uuid" => Some("UUID"),
        // `Interval` from `djogi::prelude::*`, `djogi::Interval`
        // (the canonical explicit spelling), and `djogi::types::Interval`
        // (the internal path) all lower to the typed descriptor variant.
        "Interval" | "djogi::Interval" | "djogi::types::Interval" => Some("INTERVAL"),
        // Postgres network family (`network` feature on the
        // runtime crate). The macro recognises type names unconditionally
        // (matching the spatial family pattern); if the user has the
        // feature off, the compile error surfaces at the model struct as
        // "unresolved type `MacAddr`" rather than from the macro.
        // INET routes through `std::net::IpAddr` — the postgres-types
        // crate ships the native ToSql/FromSql for INET already.
        // The path forms accepted here mirror the bare / std-qualified /
        // absolute spellings adopters might reach for.
        "IpAddr"
        | "std::net::IpAddr"
        | "::std::net::IpAddr"
        | "core::net::IpAddr"
        | "::core::net::IpAddr" => Some("INET"),
        // CIDR routes through `djogi::CidrAddr` (typed newtype with a
        // hand-rolled codec — no third-party crate dependency). Two
        // distinct Rust types drive INET vs CIDR; the macro does not
        // need a `#[field(ip_type = "...")]` attribute.
        "CidrAddr" | "djogi::CidrAddr" | "djogi::types::CidrAddr" => Some("CIDR"),
        // MACADDR routes through `djogi::MacAddr` (typed newtype over
        // `[u8; 6]` with a hand-rolled codec — no `eui48` / `mac_address`
        // dependency).
        "MacAddr" | "djogi::MacAddr" | "djogi::types::MacAddr" => Some("MACADDR"),
        // Built-in PK types (HeerId / RanjId family) are
        // usable as ambient fields outside the framework-injected `id` slot.
        // Map each name (bare, `djogi::types::*`, and `djogi::*` forms) to the
        // column type the matching `PrimaryKey::SQL_TYPE` advertises.
        // `HeerIdRecencyBiased` / `RanjIdRecencyBiased` are Djogi-side aliases
        // over heeranjid's `HeerIdDesc` / `RanjIdDesc` (spec §3.5a public
        // naming); both surface the same underlying SQL shape.
        "HeerId"
        | "HeerIdDesc"
        | "HeerIdRecencyBiased"
        | "djogi::types::HeerId"
        | "djogi::types::HeerIdDesc"
        | "djogi::types::HeerIdRecencyBiased"
        | "djogi::HeerId"
        | "djogi::HeerIdDesc"
        | "djogi::HeerIdRecencyBiased" => Some("BIGINT"),
        "RanjId"
        | "RanjIdDesc"
        | "RanjIdRecencyBiased"
        | "djogi::types::RanjId"
        | "djogi::types::RanjIdDesc"
        | "djogi::types::RanjIdRecencyBiased"
        | "djogi::RanjId"
        | "djogi::RanjIdDesc"
        | "djogi::RanjIdRecencyBiased" => Some("UUID"),
        "serde_json::Value" | "Value" => Some("JSONB"),
        // `Vec<u8>` is raw binary (BYTEA), NOT a `SMALLINT[]`
        // array. This arm sits BEFORE the generic `Vec<T>` array arms so
        // the byte-vector shape wins: a scalar `u8` field lowers to
        // `SMALLINT`, but a `Vec<u8>` field is a binary blob, matching
        // tokio-postgres' native `Vec<u8>` ↔ BYTEA codec. (There is no
        // `Vec<u8>` array arm below, so without this the type would fall
        // through to the `DjogiSqlType` field-site bound and fail to
        // compile.)
        "Vec<u8>" => Some("BYTEA"),
        "Vec<String>" => Some("TEXT[]"),
        "Vec<i16>" => Some("SMALLINT[]"),
        "Vec<i32>" => Some("INTEGER[]"),
        "Vec<i64>" => Some("BIGINT[]"),
        "Vec<f32>" => Some("REAL[]"),
        "Vec<f64>" => Some("DOUBLE PRECISION[]"),
        "Vec<bool>" => Some("BOOLEAN[]"),
        // Timestamp/date arrays — all canonical path forms (bare, djogi::, djogi::types::)
        "Vec<DateTime>"
        | "Vec<time::OffsetDateTime>"
        | "Vec<OffsetDateTime>"
        | "Vec<djogi::DateTime>"
        | "Vec<djogi::types::DateTime>" => Some("TIMESTAMPTZ[]"),
        "Vec<Date>" | "Vec<time::Date>" | "Vec<djogi::Date>" | "Vec<djogi::types::Date>" => {
            Some("DATE[]")
        }
        // UUID arrays
        "Vec<Uuid>" | "Vec<uuid::Uuid>" => Some("UUID[]"),
        // Decimal arrays
        "Vec<Decimal>" | "Vec<rust_decimal::Decimal>" => Some("NUMERIC[]"),
        // Built-in PK type arrays — HeerId family → BIGINT[], RanjId family → UUID[]
        "Vec<HeerId>"
        | "Vec<HeerIdDesc>"
        | "Vec<HeerIdRecencyBiased>"
        | "Vec<djogi::HeerId>"
        | "Vec<djogi::HeerIdDesc>"
        | "Vec<djogi::HeerIdRecencyBiased>"
        | "Vec<djogi::types::HeerId>"
        | "Vec<djogi::types::HeerIdDesc>"
        | "Vec<djogi::types::HeerIdRecencyBiased>" => Some("BIGINT[]"),
        "Vec<RanjId>"
        | "Vec<RanjIdDesc>"
        | "Vec<RanjIdRecencyBiased>"
        | "Vec<djogi::RanjId>"
        | "Vec<djogi::RanjIdDesc>"
        | "Vec<djogi::RanjIdRecencyBiased>"
        | "Vec<djogi::types::RanjId>"
        | "Vec<djogi::types::RanjIdDesc>"
        | "Vec<djogi::types::RanjIdRecencyBiased>" => Some("UUID[]"),
        // Spatial — all GeographyValue-implementing types map to the
        // corresponding GEOGRAPHY(<SUBTYPE>, 4326) SQL type. The `spatial`
        // feature flag lives on the `djogi` runtime crate, not here; the
        // macro recognises type names unconditionally so it emits the correct
        // descriptor regardless of feature state. If the user has the spatial
        // feature off, the compile error comes from "unresolved type" at the
        // struct definition, not from the macro.
        // Path forms accepted: bare ident, `geo::T`, `djogi::T`,
        // `djogi::geo::T`. The match strips the module prefix via the
        // `replace(' ', "")` normalization above, so e.g. `djogi :: geo ::
        // LineString` becomes `djogi::geo::LineString`.
        "GeoPoint" | "geo::GeoPoint" | "djogi::geo::GeoPoint" | "djogi::GeoPoint" => {
            Some("GEOGRAPHY(Point, 4326)")
        }
        "LineString" | "geo::LineString" | "djogi::geo::LineString" | "djogi::LineString" => {
            Some("GEOGRAPHY(LineString, 4326)")
        }
        "Polygon" | "geo::Polygon" | "djogi::geo::Polygon" | "djogi::Polygon" => {
            Some("GEOGRAPHY(Polygon, 4326)")
        }
        "MultiPoint" | "geo::MultiPoint" | "djogi::geo::MultiPoint" | "djogi::MultiPoint" => {
            Some("GEOGRAPHY(MultiPoint, 4326)")
        }
        "MultiPolygon"
        | "geo::MultiPolygon"
        | "djogi::geo::MultiPolygon"
        | "djogi::MultiPolygon" => Some("GEOGRAPHY(MultiPolygon, 4326)"),
        // Option<T> is handled at call site — strip and recurse via unwrap_option
        _ if s.starts_with("Option<") => None,
        // (Jsonb<T> recognition lives at the top of this fn — structural
        // last-segment match handles bare / djogi:: / djogi::jsonb:: /
        // crate:: / super:: / ::djogi:: forms uniformly.)
        _ => None,
    }
}

fn is_djogi_range_outer_path(path: &syn::Path) -> bool {
    if path.leading_colon.is_some() {
        return path_segments_eq(path, &["djogi", "Range"])
            || path_segments_eq(path, &["djogi", "types", "Range"]);
    }

    path_segments_eq(path, &["Range"])
        || path_segments_eq(path, &["djogi", "Range"])
        || path_segments_eq(path, &["djogi", "types", "Range"])
}

fn path_segments_eq(path: &syn::Path, expected: &[&str]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .zip(expected)
            .all(|(segment, expected)| segment.ident == *expected)
}

/// Convert a pre-validated `on_delete` string into the matching
/// `::djogi::OnDelete` token path.
/// Caller contract: `s` must be one of the six validated values — any other
/// input is a bug in the caller, since [`FieldAttrs::parse`] has already
/// rejected out-of-domain strings with a span-carrying error. The function
/// falls back to `OnDelete::Restrict` on an unrecognized value so a caller
/// skipping the validator does not produce a bogus token stream; that
/// matches the framework's cascade-off-by-default posture and keeps the
/// descriptor emitter total.
/// Lives here rather than in `descriptor.rs` because `descriptor.rs` can
/// stay schema-focused: the attr crate owns the mapping between
/// `#[field(on_delete = "...")]` string values and runtime enum variants,
/// and descriptor emission calls through this helper.
#[allow(dead_code)]
pub fn on_delete_str_to_tokens(s: &str) -> proc_macro2::TokenStream {
    match s {
        "cascade" => quote::quote! { ::djogi::descriptor::OnDelete::Cascade },
        "restrict" => quote::quote! { ::djogi::descriptor::OnDelete::Restrict },
        "set_null" => quote::quote! { ::djogi::descriptor::OnDelete::SetNull },
        "set_default" => quote::quote! { ::djogi::descriptor::OnDelete::SetDefault },
        "protect" => quote::quote! { ::djogi::descriptor::OnDelete::Protect },
        "do_nothing" => quote::quote! { ::djogi::descriptor::OnDelete::DoNothing },
        // Fallback: anything else is out of the validator's domain and the
        // caller is expected to have rejected it. Emit `Restrict` (the
        // framework default) so the macro output is still well-formed
        // tokens rather than a half-written ident.
        _ => quote::quote! { ::djogi::descriptor::OnDelete::Restrict },
    }
}

/// Relation cardinality as seen from a Rust field type inside the macro crate.
/// This enum is the macro-crate mirror of `djogi::relation::RelationKind`:
/// `djogi-macros` cannot depend on `djogi` (the macro is compiled first),
/// so when the macro wants to *reason* about a relation shape before
/// emitting tokens, it works with this local copy. The emitter converts to
/// `::djogi::relation::RelationKind::…` token paths at code-generation
/// time — the two enums only need to stay in sync on the set of variants.
/// Covers `ForeignKey<T>` and `OneToOneField<T>` (plus their
/// `Option<…>` nullable forms). Task 7's `ManyToMany` variant is
/// intentionally absent here: the M2M macro pipeline recognizes M2M
/// through-models by a separate mechanism (a marker trait on the through
/// struct, not a field type on the source struct), so no macro-side
/// detection is needed in this helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// `ForeignKey<T>` — many-to-one.
    ForeignKey,
    /// `OneToOneField<T>` — one-to-one, unique-constrained.
    OneToOne,
}

/// Everything a macro caller needs to know about a detected relation field.
/// Returned by [`detect_relation`]. Two facets of the relation target are
/// carried separately because they have different consumers:
/// - [`target_name`](Self::target_name) is a short, path-free string used
///   only for the descriptor field `target_type_name` (the migration
///   emitter looks models up by this name against the `inventory`-registered
///   descriptors, so keeping it segmented matches that lookup key).
/// - [`target_type`](Self::target_type) is the **full `syn::Type`** the user
///   wrote inside the wrapper (`Owner`, `models::Owner`, `crate::models::Owner`,
///   or even `inner::Widget`). Emitted verbatim into positions such as
///   `RelationPath<#source, #target_type>` and `<#target_type as Model>::…`
///   so fully-qualified paths resolve at the user's macro-call site without
///   requiring an extra `use …;` import.
///   Splitting the two lets the emitter use the right form in the right place
///   collapsing down to just the last-segment ident (as the previous
///   `(RelationKind, String, bool)` tuple did) silently broke codegen for any
///   target type the user spelled with a path prefix.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RelationInfo {
    /// FK vs O2O — drives both the emitted `RelationKind` variant and the
    /// descriptor-side `relation_kind` field.
    pub kind: RelationKind,
    /// Short name for descriptor use (e.g. `"Owner"` from any of:
    /// `Owner`, `models::Owner`, `crate::models::Owner`). Always the last
    /// segment ident of the inner type path — this is the name the
    /// migration differ matches against `ModelDescriptor::type_name`.
    pub target_name: String,
    /// Full target type preserved for codegen. Use in `quote! { #target_type }`
    /// positions so fully-qualified paths like
    /// `ForeignKey<crate::models::Owner>` still emit a resolvable
    /// `RelationPath<Self, crate::models::Owner>` without requiring a
    /// separate `use crate::models::Owner;` at the call site.
    pub target_type: syn::Type,
    /// `true` when the outermost wrapper was `Option<…>` (nullable column).
    pub nullable: bool,
}

/// Inspect a field's declared Rust type and decide whether it names a Djogi
/// relation wrapper.
/// Returns [`Some(RelationInfo)`](RelationInfo) when the outermost
/// (post-`Option<…>`-strip) type is one of the recognized relation wrappers:
/// - `ForeignKey<T>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: false })`
/// - `Option<ForeignKey<T>>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: true })`
/// - `OneToOneField<T>` → `Some(RelationInfo { kind: OneToOne, …, nullable: false })`
/// - `Option<OneToOneField<T>>` → `Some(RelationInfo { kind: OneToOne, …, nullable: true })`
/// - anything else → `None`
/// # Robustness
/// The check inspects the *last* path segment's ident, which makes it
/// resilient to fully-qualified user spellings like
/// `::djogi::relation::ForeignKey<Owner>` or `djogi::ForeignKey<Owner>`
/// users who reach for an explicit path instead of `use djogi::prelude::*`
/// still get the correct relation detection. The trade-off: a user-defined
/// type with the literal name `ForeignKey` shadows the detection. That
/// matches how `unwrap_option` already handles `Option`: the macro works
/// from the spelling the user wrote, and the behaviour is consistent and
/// easy to reason about.
/// # Target preservation
/// Both the short `target_name` and the full `target_type` are preserved.
/// Consumers that emit `type`-position tokens (e.g. `relations::expand`)
/// must use `target_type` so that `ForeignKey<crate::models::Owner>` still
/// emits a resolvable `RelationPath<_, crate::models::Owner>`; consumers
/// that only need the short name for descriptor lookup (`descriptor::expand`)
/// use `target_name`.
#[allow(dead_code)]
pub fn detect_relation(ty: &syn::Type) -> Option<RelationInfo> {
    // First, strip one layer of `Option<…>` using the shared helper. The
    // returned `nullable` flag propagates straight through to the caller
    // it's the same nullability semantic.
    let (stripped, nullable) = unwrap_option(ty);

    // The stripped type must be a path type like `ForeignKey<T>` or
    // `some::path::ForeignKey<T>`. Anything else (references, tuples,
    // arrays, fn pointers) is a scalar column from the macro's POV.
    let syn::Type::Path(syn::TypePath { path, .. }) = &stripped else {
        return None;
    };

    // Inspect the last segment only — robust to fully-qualified spellings.
    let last = path.segments.last()?;
    let kind = match last.ident.to_string().as_str() {
        "ForeignKey" => RelationKind::ForeignKey,
        "OneToOneField" => RelationKind::OneToOne,
        _ => return None,
    };

    // Extract the single generic argument's type. The segment must carry
    // `<T>`; a bare `ForeignKey` with no type parameter is a user error
    // that earlier parsing (or the later descriptor emission) will surface
    // with a better error — return `None` here rather than panicking the
    // macro.
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    let syn::GenericArgument::Type(inner) = args.args.first()? else {
        return None;
    };

    // The target's short name is the last segment of the inner type's path
    // (`Owner` out of `crate::models::Owner`, or just `Owner` out of
    // `Owner`). Non-path inner types (e.g. references) aren't legal relation
    // targets — the `Model` bound on `ForeignKey<T>` would reject them at
    // the type-check stage — so return `None` and let the normal compile
    // error surface in the user's code.
    let syn::Type::Path(syn::TypePath {
        path: inner_path, ..
    }) = inner
    else {
        return None;
    };
    let target_name = inner_path.segments.last()?.ident.to_string();

    // Preserve the full inner `syn::Type` for codegen — emitters that place
    // the target in type position must use this, not the stringified short
    // name, so fully-qualified paths (e.g. `crate::models::Owner`) resolve
    // at the macro-call site without requiring a separate `use` import.
    let target_type = inner.clone();

    Some(RelationInfo {
        kind,
        target_name,
        target_type,
        nullable,
    })
}

/// Strip `Option<T>` → returns the inner type and `nullable = true`.
/// Only recognizes the prelude's `Option` — specifically the path forms
/// `Option<T>`, `std::option::Option<T>`, and `core::option::Option<T>`. A user
/// type that happens to be named `Option` in their own module (e.g.
/// `my_crate::Option<T>`) is left unchanged, because treating it as nullable
/// silently would produce wrong migrations. This matches how users actually
/// read the type: `Option<T>` in the prelude means "SQL NULL allowed"; anything
/// else is a user type that must map via `rust_type_to_sql`.
/// Non-`Option` types are returned unchanged with `nullable = false`.
/// Called by `inject::expand` and `descriptor::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn unwrap_option(ty: &syn::Type) -> (syn::Type, bool) {
    if let syn::Type::Path(syn::TypePath { path, .. }) = ty
        && is_prelude_option_path(path)
        && let Some(last) = path.segments.last()
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (inner.clone(), true);
    }
    (ty.clone(), false)
}

/// Strip `Tracked<T>` → returns the inner type.
/// Detection follows the same last-segment convention as relation wrappers:
/// `Tracked<T>`, `djogi::Tracked<T>`, `::djogi::Tracked<T>`, and
/// `djogi::prelude::Tracked<T>` all route through this helper. The caller is
/// responsible for combining this with [`unwrap_option`] when computing SQL
/// nullability.
#[allow(dead_code)]
pub fn unwrap_tracked(ty: &syn::Type) -> Option<syn::Type> {
    if let syn::Type::Path(syn::TypePath { path, .. }) = ty
        && let Some(last) = path.segments.last()
        && last.ident == "Tracked"
        && let syn::PathArguments::AngleBracketed(args) = &last.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return Some(inner.clone());
    }
    None
}

/// Strip the transparent wrappers that affect schema shape.
/// `Option<T>` marks the SQL column nullable. `Tracked<T>` is a dirty-tracking
/// wrapper whose storage type is `T`; it does not itself affect nullability.
/// Both wrappers can appear together, so `Tracked<Option<String>>` projects as
/// `TEXT NULL` and `Option<Tracked<String>>` does the same.
#[allow(dead_code)]
pub fn unwrap_schema_type(ty: &syn::Type) -> (syn::Type, bool) {
    let mut current = ty.clone();
    let mut nullable = false;

    loop {
        let (inner, was_option) = unwrap_option(&current);
        if was_option {
            current = inner;
            nullable = true;
            continue;
        }

        if let Some(inner) = unwrap_tracked(&current) {
            current = inner;
            continue;
        }

        return (current, nullable);
    }
}

/// True if `path` is one of the three canonical prelude `Option` forms:
/// bare `Option`, `std::option::Option`, or `core::option::Option`.
fn is_prelude_option_path(path: &syn::Path) -> bool {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    match segs.as_slice() {
        [sole] => sole == "Option",
        [root, module, ty] => {
            (root == "std" || root == "core") && module == "option" && ty == "Option"
        }
        _ => false,
    }
}

/// Simplified SQL type category for tenant-key cast selection in RLS DDL.
/// `rust_type_to_sql` returns a full SQL type string; this categorises the
/// result into the three buckets the RLS `current_setting(...)::cast`
/// expression needs. Types outside the accepted set are returned as
/// `Unsupported(sql_type_string)` so the caller can emit a span-precise
/// compile error at the `tenant_key` attribute.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSqlTypeCategory {
    /// `BIGINT` — cast is `::bigint` (HeerId tenant keys).
    BigInt,
    /// `UUID` — cast is `::uuid` (RanjId tenant keys).
    Uuid,
    /// `TEXT` or `CITEXT` — no cast needed (text comparison is exact).
    Text,
    /// Any SQL type not in the accepted tenant-key set. The inner string
    /// is the full SQL type name for use in the compile-error message.
    Unsupported(String),
}

/// Derive the `FieldSqlTypeCategory` for a Rust field type.
/// Used by the RLS DDL emitter to pick the correct `current_setting(...)::cast`
/// expression. `HeerId` → `BigInt`; `Uuid` / `RanjId` → `Uuid`; `String` →
/// `Text`; `ForeignKey<T>` / `OneToOneField<T>` → `BigInt` (the framework
/// default — see assumption note below); everything else → `Unsupported`.
/// One `Option<…>` layer is stripped first so nullable tenant columns
/// (`Option<HeerId>`, `Option<ForeignKey<T>>`) are also accepted.
/// # FK assumption (GH issue #37)
/// When the tenant-key column is typed `ForeignKey<T>` or
/// `OneToOneField<T>` (or a nullable variant), this function returns
/// `BigInt` unconditionally. The macro cannot resolve `T::Pk` at
/// expansion time — proc-macro evaluation order is non-deterministic
/// and the target's `ModelDescriptor` is not visible from the FK-using
/// model's expansion. Defaulting to `BigInt` covers the canonical case
/// (every model that uses the framework default `HeerId` PK), at the
/// cost of producing a wrong RLS cast if the FK target opts into
/// `RanjId` or a custom PK.
/// **Adopters whose FK target uses a non-default PK MUST declare
/// `tenant_key` against a non-FK column** (a plain `tenant_id: HeerId`,
/// `RanjId`, or `String` field). The pre-fix behaviour silently emitted
/// an empty cast and fell back to text comparison, which masked the
/// bug; the post-fix behaviour produces a `::bigint` cast that fails
/// loudly at policy-application time if the actual column is a UUID.
/// Loud failure beats silent miscompare for tenant isolation.
#[allow(dead_code)]
pub fn field_sql_type_category(ty: &syn::Type) -> FieldSqlTypeCategory {
    let (inner, _nullable) = unwrap_option(ty);

    // Detect `ForeignKey<T>` / `OneToOneField<T>` wrappers. The relation
    // detector also handles `Option<ForeignKey<T>>` / `Option<OneToOneField<T>>`,
    // so we route through it on the *original* `ty` rather than the
    // already-`unwrap_option`'d inner.
    if detect_relation(ty).is_some() {
        return FieldSqlTypeCategory::BigInt;
    }

    // Build a normalised type-string for pattern matching.
    let s = quote::quote!(#inner).to_string().replace(' ', "");
    match s.as_str() {
        // HeerId is a Djogi type alias for i64 BIGINT.
        "HeerId" | "i64" => FieldSqlTypeCategory::BigInt,
        // RanjId is a Djogi type alias for Uuid.
        "RanjId" | "Uuid" | "uuid::Uuid" => FieldSqlTypeCategory::Uuid,
        "String" => FieldSqlTypeCategory::Text,
        // Fall through to the sql-type table for anything else.
        _ => match rust_type_to_sql(&inner) {
            Some("BIGINT") | Some("SMALLINT") | Some("INTEGER") => FieldSqlTypeCategory::BigInt,
            Some("UUID") => FieldSqlTypeCategory::Uuid,
            Some("TEXT") | Some("CITEXT") => FieldSqlTypeCategory::Text,
            Some(other) => FieldSqlTypeCategory::Unsupported(other.to_owned()),
            None => FieldSqlTypeCategory::Unsupported(s),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{rust_type_to_sql, unwrap_schema_type};
    use syn::parse_quote;

    /// `Jsonb<T>` for any `T: JsonbSchema` must lower to
    /// `JSONB`. The three substrate fixes (this one +
    /// framework-col defaults +
    /// FK type substitution) only had indirect coverage through the
    /// live integration suite. Direct lock-in here so a regression
    /// surfaces without needing Postgres.
    /// All three accepted path forms are covered: bare ident, the
    /// `djogi::Jsonb<T>` re-export, and the canonical
    /// `djogi::jsonb::Jsonb<T>` path. The leading `::` form rides on
    /// the function's strip-prefix normalization (line ~1724) so it
    /// shares a code path with the relative form and one assertion
    /// across the three is sufficient.
    #[test]
    fn jsonb_wrapper_lowers_to_jsonb_across_path_forms() {
        let bare: syn::Type = parse_quote!(Jsonb<MyPayload>);
        let djogi: syn::Type = parse_quote!(djogi::Jsonb<MyPayload>);
        let djogi_jsonb: syn::Type = parse_quote!(djogi::jsonb::Jsonb<MyPayload>);
        assert_eq!(rust_type_to_sql(&bare), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&djogi), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&djogi_jsonb), Some("JSONB"));
    }

    /// Leading `::` absolute path forms must match the
    /// same arm as their relative counterparts. Locks in the structural
    /// last-segment matcher's path-walking semantics (the leading `::`
    /// only changes `path.leading_colon`, not the segment list, so the
    /// last-segment ident check fires identically).
    #[test]
    fn jsonb_wrapper_absolute_paths_match() {
        let abs_djogi: syn::Type = parse_quote!(::djogi::Jsonb<P>);
        let abs_jsonb: syn::Type = parse_quote!(::djogi::jsonb::Jsonb<P>);
        assert_eq!(rust_type_to_sql(&abs_djogi), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&abs_jsonb), Some("JSONB"));
    }

    /// Round-2 `crate::Jsonb<T>` and `super::Jsonb<T>`
    /// path forms must also resolve to `JSONB`. The previous
    /// string-prefix matcher only recognized `Jsonb<` / `djogi::Jsonb<`
    /// / `djogi::jsonb::Jsonb<` and silently mapped these crate-relative
    /// shapes to TEXT, producing INSERT failures at runtime. The
    /// structural last-segment matcher closes that gap because both
    /// `crate` and `super` keywords are valid path-segment idents in
    /// `syn::Type::Path` and the last segment is still `Jsonb`.
    #[test]
    fn jsonb_wrapper_crate_relative_path_forms_match() {
        let crate_path: syn::Type = parse_quote!(crate::Jsonb<P>);
        let super_path: syn::Type = parse_quote!(super::Jsonb<P>);
        let nested_crate: syn::Type = parse_quote!(crate::jsonb::Jsonb<P>);
        let deep_module: syn::Type = parse_quote!(crate::models::common::Jsonb<P>);
        assert_eq!(rust_type_to_sql(&crate_path), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&super_path), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&nested_crate), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&deep_module), Some("JSONB"));
    }

    /// Negative case — types that just *contain* `Jsonb` somewhere but
    /// aren't the wrapper must NOT fall into the JSONB arm. The
    /// structural matcher checks `path.segments.last.ident == "Jsonb"`
    /// AND that the last segment carries angle-bracket generics, so:
    /// - `MyJsonb<T>` — last segment ident is `MyJsonb`, not `Jsonb` →
    ///   no match.
    /// - `Vec<Jsonb<T>>` — last segment ident is `Vec` (the inner
    ///   `Jsonb<T>` lives inside `Vec`'s generic args, not on the
    ///   outer path) → no match.
    /// - `Jsonb` (no generics) — fails the `AngleBracketed` arm guard
    ///   so a hypothetical non-generic `Jsonb` type is not coerced to
    ///   JSONB just because the ident matches.
    #[test]
    fn jsonb_lookalikes_do_not_match() {
        let lookalike: syn::Type = parse_quote!(MyJsonb<P>);
        let in_generic: syn::Type = parse_quote!(Vec<Jsonb<P>>);
        let no_generics: syn::Type = parse_quote!(Jsonb);
        assert_eq!(rust_type_to_sql(&lookalike), None);
        assert_eq!(rust_type_to_sql(&in_generic), None);
        assert_eq!(rust_type_to_sql(&no_generics), None);
    }

    /// Round-2 `Option<Jsonb<T>>` returns `None`
    /// directly because callers strip the `Option<…>` wrapper via
    /// `unwrap_option` before calling `rust_type_to_sql`. This locks
    /// in the convention so a refactor that bypasses `unwrap_option`
    /// surfaces here. (The structural Jsonb check at the top of the
    /// fn does NOT recurse through `Option`'s generic args — last
    /// segment of `Option<Jsonb<T>>` is `Option`, not `Jsonb`.)
    #[test]
    fn jsonb_inside_option_is_unwrap_responsibility() {
        let optioned: syn::Type = parse_quote!(Option<Jsonb<P>>);
        // `rust_type_to_sql` itself returns None — Option-stripping is
        // a caller concern. The string-based fallback then matches the
        // `_ if s.starts_with("Option<") => None` arm.
        assert_eq!(rust_type_to_sql(&optioned), None);
    }

    #[test]
    fn schema_type_unwraps_tracked_option_for_nullable_storage() {
        let tracked_option: syn::Type = parse_quote!(Tracked<Option<String>>);
        let (inner, nullable) = unwrap_schema_type(&tracked_option);
        assert!(nullable);
        assert_eq!(rust_type_to_sql(&inner), Some("TEXT"));

        let option_tracked: syn::Type = parse_quote!(Option<djogi::Tracked<String>>);
        let (inner, nullable) = unwrap_schema_type(&option_tracked);
        assert!(nullable);
        assert_eq!(rust_type_to_sql(&inner), Some("TEXT"));
    }

    // GH issue #37 — `ForeignKey<T>` / `OneToOneField<T>` columns route
    // through `BigInt` in `field_sql_type_category`. The macro cannot
    // resolve `T::Pk` at expansion time, so it assumes the framework-
    // default HeerId (BIGINT). The pre-fix behaviour silently emitted
    // an empty cast, which masked tenant-isolation bugs by falling back
    // to text comparison.
    use super::{FieldSqlTypeCategory, field_sql_type_category};

    #[test]
    fn foreign_key_tenant_key_routes_to_bigint() {
        let ty: syn::Type = parse_quote!(ForeignKey<Owner>);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn nullable_foreign_key_tenant_key_routes_to_bigint() {
        let ty: syn::Type = parse_quote!(Option<ForeignKey<Owner>>);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn one_to_one_field_tenant_key_routes_to_bigint() {
        let ty: syn::Type = parse_quote!(OneToOneField<Owner>);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn nullable_one_to_one_field_tenant_key_routes_to_bigint() {
        let ty: syn::Type = parse_quote!(Option<OneToOneField<Owner>>);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn fully_qualified_foreign_key_tenant_key_routes_to_bigint() {
        // Adopters who don't use `djogi::prelude::*` may write the FK type
        // with its full path. The relation detector inspects only the last
        // segment, so this case must work too.
        let ty: syn::Type = parse_quote!(::djogi::ForeignKey<Owner>);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
        let ty2: syn::Type = parse_quote!(djogi::relation::ForeignKey<Owner>);
        assert_eq!(field_sql_type_category(&ty2), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn plain_heer_id_tenant_key_still_routes_to_bigint() {
        // Regression check: the FK-detection branch must NOT eat the
        // existing `HeerId` arm.
        let ty: syn::Type = parse_quote!(HeerId);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::BigInt);
    }

    #[test]
    fn plain_string_tenant_key_still_routes_to_text() {
        let ty: syn::Type = parse_quote!(String);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::Text);
    }

    #[test]
    fn plain_uuid_tenant_key_still_routes_to_uuid() {
        let ty: syn::Type = parse_quote!(Uuid);
        assert_eq!(field_sql_type_category(&ty), FieldSqlTypeCategory::Uuid);
    }

    // Regression guard: `rust_type_to_sql` must accept the canonical
    // djogi aliases for
    // temporal types so user fields spelled `djogi::DateTime` /
    // `djogi::types::DateTime` (etc.) lower to `TIMESTAMPTZ` instead
    // of falling through to TEXT.
    #[test]
    fn djogi_datetime_alias_lowers_to_timestamptz() {
        let bare: syn::Type = parse_quote!(DateTime);
        let djogi: syn::Type = parse_quote!(djogi::DateTime);
        let djogi_types: syn::Type = parse_quote!(djogi::types::DateTime);
        let absolute: syn::Type = parse_quote!(::djogi::types::DateTime);
        assert_eq!(rust_type_to_sql(&bare), Some("TIMESTAMPTZ"));
        assert_eq!(rust_type_to_sql(&djogi), Some("TIMESTAMPTZ"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("TIMESTAMPTZ"));
        assert_eq!(rust_type_to_sql(&absolute), Some("TIMESTAMPTZ"));
    }

    #[test]
    fn djogi_date_alias_lowers_to_date() {
        let bare: syn::Type = parse_quote!(Date);
        let djogi: syn::Type = parse_quote!(djogi::Date);
        let djogi_types: syn::Type = parse_quote!(djogi::types::Date);
        let absolute: syn::Type = parse_quote!(::djogi::types::Date);
        assert_eq!(rust_type_to_sql(&bare), Some("DATE"));
        assert_eq!(rust_type_to_sql(&djogi), Some("DATE"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("DATE"));
        assert_eq!(rust_type_to_sql(&absolute), Some("DATE"));
    }

    #[test]
    fn djogi_interval_alias_lowers_to_interval() {
        let bare: syn::Type = parse_quote!(Interval);
        let djogi: syn::Type = parse_quote!(djogi::Interval);
        let djogi_types: syn::Type = parse_quote!(djogi::types::Interval);
        let absolute: syn::Type = parse_quote!(::djogi::types::Interval);
        assert_eq!(rust_type_to_sql(&bare), Some("INTERVAL"));
        assert_eq!(rust_type_to_sql(&djogi), Some("INTERVAL"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("INTERVAL"));
        assert_eq!(rust_type_to_sql(&absolute), Some("INTERVAL"));
    }

    // ── ) — network family ─────────────────

    #[test]
    fn ipaddr_alias_lowers_to_inet() {
        let bare: syn::Type = parse_quote!(IpAddr);
        let std_net: syn::Type = parse_quote!(std::net::IpAddr);
        let absolute_std: syn::Type = parse_quote!(::std::net::IpAddr);
        let core_net: syn::Type = parse_quote!(core::net::IpAddr);
        let absolute_core: syn::Type = parse_quote!(::core::net::IpAddr);
        assert_eq!(rust_type_to_sql(&bare), Some("INET"));
        assert_eq!(rust_type_to_sql(&std_net), Some("INET"));
        assert_eq!(rust_type_to_sql(&absolute_std), Some("INET"));
        assert_eq!(rust_type_to_sql(&core_net), Some("INET"));
        assert_eq!(rust_type_to_sql(&absolute_core), Some("INET"));
    }

    #[test]
    fn djogi_cidr_addr_alias_lowers_to_cidr() {
        let bare: syn::Type = parse_quote!(CidrAddr);
        let djogi: syn::Type = parse_quote!(djogi::CidrAddr);
        let djogi_types: syn::Type = parse_quote!(djogi::types::CidrAddr);
        let absolute: syn::Type = parse_quote!(::djogi::types::CidrAddr);
        assert_eq!(rust_type_to_sql(&bare), Some("CIDR"));
        assert_eq!(rust_type_to_sql(&djogi), Some("CIDR"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("CIDR"));
        assert_eq!(rust_type_to_sql(&absolute), Some("CIDR"));
    }

    #[test]
    fn djogi_mac_addr_alias_lowers_to_macaddr() {
        let bare: syn::Type = parse_quote!(MacAddr);
        let djogi: syn::Type = parse_quote!(djogi::MacAddr);
        let djogi_types: syn::Type = parse_quote!(djogi::types::MacAddr);
        let absolute: syn::Type = parse_quote!(::djogi::types::MacAddr);
        assert_eq!(rust_type_to_sql(&bare), Some("MACADDR"));
        assert_eq!(rust_type_to_sql(&djogi), Some("MACADDR"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("MACADDR"));
        assert_eq!(rust_type_to_sql(&absolute), Some("MACADDR"));
    }

    #[test]
    fn djogi_range_outer_namespace_policy_accepts_runtime_surface() {
        let bare: syn::Type = parse_quote!(Range<i32>);
        let djogi: syn::Type = parse_quote!(djogi::Range<i32>);
        let djogi_types: syn::Type = parse_quote!(djogi::types::Range<i32>);
        let absolute_djogi: syn::Type = parse_quote!(::djogi::Range<i32>);
        let absolute_types: syn::Type = parse_quote!(::djogi::types::Range<i32>);
        let ts: syn::Type = parse_quote!(Range<time::PrimitiveDateTime>);
        assert_eq!(rust_type_to_sql(&bare), Some("INT4RANGE"));
        assert_eq!(rust_type_to_sql(&djogi), Some("INT4RANGE"));
        assert_eq!(rust_type_to_sql(&djogi_types), Some("INT4RANGE"));
        assert_eq!(rust_type_to_sql(&absolute_djogi), Some("INT4RANGE"));
        assert_eq!(rust_type_to_sql(&absolute_types), Some("INT4RANGE"));
        assert_eq!(rust_type_to_sql(&ts), Some("TSRANGE"));
    }

    #[test]
    fn djogi_range_outer_namespace_policy_rejects_lookalikes() {
        let std_ops: syn::Type = parse_quote!(std::ops::Range<i32>);
        let core_ops: syn::Type = parse_quote!(core::ops::Range<i32>);
        let crate_relative: syn::Type = parse_quote!(crate::Range<i32>);
        let self_relative: syn::Type = parse_quote!(self::Range<i32>);
        let super_relative: syn::Type = parse_quote!(super::Range<i32>);
        let adopter_module: syn::Type = parse_quote!(adopter::Range<i32>);
        let nested_adopter_module: syn::Type = parse_quote!(adopter::types::Range<i32>);
        assert_eq!(rust_type_to_sql(&std_ops), None);
        assert_eq!(rust_type_to_sql(&core_ops), None);
        assert_eq!(rust_type_to_sql(&crate_relative), None);
        assert_eq!(rust_type_to_sql(&self_relative), None);
        assert_eq!(rust_type_to_sql(&super_relative), None);
        assert_eq!(rust_type_to_sql(&adopter_module), None);
        assert_eq!(rust_type_to_sql(&nested_adopter_module), None);
    }

    // ── `#[model(tree_edge = "...")]` ──
    // Parser-side coverage. Field-existence + self-FK validation runs
    // at descriptor-emit time (where the user-field list is in scope)
    // and is exercised by 's lihaaf compile-fail fixtures
    // the parser itself only enforces the standard Djogi identifier
    // grammar.
    use super::ModelAttrs;
    use proc_macro2::TokenStream;
    use std::str::FromStr;

    fn parse_attrs(src: &str) -> syn::Result<ModelAttrs> {
        let ts = TokenStream::from_str(src).expect("token stream parses");
        ModelAttrs::parse(ts)
    }

    #[test]
    fn tree_edge_accepts_plain_identifier() {
        let attrs = parse_attrs(r#"table = "nodes", tree_edge = "parent_id""#)
            .expect("valid identifier accepted");
        let lit = attrs.tree_edge.expect("tree_edge populated");
        assert_eq!(lit.value(), "parent_id");
    }

    #[test]
    fn tree_edge_accepts_underscore_prefix() {
        let attrs = parse_attrs(r#"table = "nodes", tree_edge = "_parent""#)
            .expect("leading underscore accepted");
        assert_eq!(attrs.tree_edge.expect("populated").value(), "_parent");
    }

    #[test]
    fn tree_edge_default_is_none() {
        let attrs = parse_attrs(r#"table = "nodes""#).expect("default is fine");
        assert!(attrs.tree_edge.is_none());
    }

    #[test]
    fn ddl_metadata_model_attrs_parse() {
        let attrs = parse_attrs(
            r#"table = "widgets",
               table_comment = "Operational table",
               storage_params = "fillfactor=70, autovacuum_enabled=false",
               tablespace = "fastspace""#,
        )
        .expect("DDL metadata attrs parse");

        assert_eq!(attrs.table_comment.as_deref(), Some("Operational table"));
        assert_eq!(
            attrs.storage_params.as_deref(),
            Some("fillfactor=70, autovacuum_enabled=false")
        );
        assert_eq!(attrs.tablespace.as_deref(), Some("fastspace"));
    }

    #[test]
    fn ddl_metadata_model_attrs_reject_empty_storage_params() {
        let err = parse_attrs(r#"table = "widgets", storage_params = "   ""#)
            .expect_err("empty storage params rejected");
        assert!(
            err.to_string().contains("storage_params"),
            "diagnostic mentions storage_params: {err}"
        );
    }

    #[test]
    fn ddl_metadata_model_attrs_reject_malformed_storage_params() {
        let err = parse_attrs(r#"table = "widgets", storage_params = "fillfactor""#)
            .expect_err("missing key-value delimiter rejected");
        assert!(
            err.to_string().contains("key=value"),
            "diagnostic explains key-value form: {err}"
        );
    }

    #[test]
    fn ddl_metadata_model_attrs_reject_storage_params_injection_fragments() {
        for params in [
            "fillfactor=70); DROP TABLE x; --",
            "fillfactor=70--comment",
            "fillfactor=70/*comment*/",
            "fillfactor=(70)",
            "fillfactor=DROP",
        ] {
            let err = parse_attrs(&format!(
                r#"table = "widgets", storage_params = "{params}""#
            ))
            .expect_err("storage params injection fragment rejected");
            assert!(
                err.to_string().contains("storage_params"),
                "diagnostic names storage_params for {params:?}: {err}"
            );
        }
    }

    #[test]
    fn ddl_metadata_model_attrs_reject_duplicate_storage_params_keys() {
        let err =
            parse_attrs(r#"table = "widgets", storage_params = "fillfactor=70, FillFactor=80""#)
                .expect_err("duplicate storage params key rejected");

        assert!(
            err.to_string().contains("duplicate"),
            "diagnostic mentions duplicate key: {err}"
        );
    }

    #[test]
    fn ddl_metadata_model_attrs_reject_dotted_storage_params_keys() {
        let err =
            parse_attrs(r#"table = "widgets", storage_params = "toast.autovacuum_enabled=true""#)
                .expect_err("dotted storage params key rejected");

        assert!(
            err.to_string().contains("storage_params"),
            "diagnostic names storage_params: {err}"
        );
    }

    #[test]
    fn tree_edge_rejects_empty_string() {
        let err =
            parse_attrs(r#"table = "nodes", tree_edge = """#).expect_err("empty value rejected");
        assert!(
            err.to_string().contains("tree_edge"),
            "diagnostic mentions the attribute name: {err}"
        );
    }

    #[test]
    fn tree_edge_rejects_leading_digit() {
        let err = parse_attrs(r#"table = "nodes", tree_edge = "1col""#)
            .expect_err("leading digit rejected");
        assert!(err.to_string().contains("ASCII identifier"));
    }

    #[test]
    fn tree_edge_rejects_hyphen() {
        let err = parse_attrs(r#"table = "nodes", tree_edge = "parent-id""#)
            .expect_err("hyphen rejected");
        assert!(err.to_string().contains("ASCII identifier"));
    }

    #[test]
    fn tree_edge_rejects_overlength() {
        // 64 chars — one over the Postgres unquoted-identifier cap.
        let oversize = "a".repeat(64);
        let src = format!(r#"table = "nodes", tree_edge = "{oversize}""#);
        let err = parse_attrs(&src).expect_err("64-byte value rejected");
        assert!(err.to_string().contains("63 bytes"));
    }

    #[test]
    fn tree_edge_accepts_max_length() {
        // 63 chars — exactly at the cap.
        let max = "a".repeat(63);
        let src = format!(r#"table = "nodes", tree_edge = "{max}""#);
        let attrs = parse_attrs(&src).expect("63-byte value accepted");
        assert_eq!(attrs.tree_edge.expect("populated").value().len(), 63);
    }

    #[test]
    fn tree_edge_rejects_duplicate_keys() {
        let err =
            parse_attrs(r#"table = "nodes", tree_edge = "parent_id", tree_edge = "manager_id""#)
                .expect_err("duplicate keys rejected");
        assert!(err.to_string().contains("duplicate `tree_edge`"));
    }

    #[test]
    fn unknown_attribute_diagnostic_lists_tree_edge() {
        // Verifies the unknown-key error message advertises `tree_edge`
        // alongside the other model-level keys, so users discover the
        // attribute via natural error-driven exploration.
        let err =
            parse_attrs(r#"table = "nodes", widget = "x""#).expect_err("unknown key surfaced");
        assert!(
            err.to_string().contains("tree_edge"),
            "diagnostic enumerates known keys including tree_edge: {err}"
        );
    }

    // ── 2 — proxy attribute parsing ─────────────────────────────

    /// `proxy_for = ParentType` populates the bare identifier on the
    /// returned `ModelAttrs`. Locks the bare-identifier convention; a
    /// subsequent commit that switched silently to string-literals would
    /// trip this test.
    #[test]
    fn proxy_for_accepts_bare_identifier() {
        let attrs = parse_attrs(r#"table = "vehicles", proxy_for = Vehicle"#)
            .expect("bare identifier accepted");
        let id = attrs.proxy_for.expect("proxy_for populated");
        assert_eq!(id.to_string(), "Vehicle");
    }

    /// `proxy_for = "Vehicle"` (string-literal form) is rejected with a
    /// span-precise error mirroring the `pk = "..."` rejection.
    #[test]
    fn proxy_for_rejects_string_literal() {
        let err = parse_attrs(r#"table = "vehicles", proxy_for = "Vehicle""#)
            .expect_err("string literal rejected");
        let msg = err.to_string();
        assert!(msg.contains("string-literal"));
        assert!(msg.contains("bare identifier"));
    }

    /// Duplicate `proxy_for` is rejected at parse time — silently
    /// accepting the second occurrence would surprise adopters who
    /// merge two `#[model(...)]` attributes.
    #[test]
    fn proxy_for_rejects_duplicate() {
        let err = parse_attrs(r#"table = "vehicles", proxy_for = Vehicle, proxy_for = Truck"#)
            .expect_err("duplicate proxy_for rejected");
        assert!(err.to_string().contains("duplicate `proxy_for"));
    }

    /// `default_order = [...]` populates the order list on the returned
    /// `ModelAttrs` when paired with `proxy_for`.
    #[test]
    fn default_order_populates_when_paired_with_proxy_for() {
        let attrs = parse_attrs(
            r#"table = "vehicles", proxy_for = Vehicle, default_order = [(name, Asc), (created_at, Desc)]"#,
        )
        .expect("default_order parsed alongside proxy_for");
        assert_eq!(attrs.proxy_default_order.len(), 2);
        assert_eq!(attrs.proxy_default_order[0].0.to_string(), "name");
        assert_eq!(attrs.proxy_default_order[1].0.to_string(), "created_at");
    }

    /// `default_order` without `proxy_for` is rejected — the key is only
    /// meaningful for proxy models. Non-proxies use explicit
    /// `.order_by(...)` calls.
    #[test]
    fn default_order_rejected_without_proxy_for() {
        let err = parse_attrs(r#"table = "vehicles", default_order = [(name, Asc)]"#)
            .expect_err("orphan default_order rejected");
        assert!(err.to_string().contains("requires `proxy_for"));
    }

    /// `default_filter = |f| ...` populates the closure on the returned
    /// `ModelAttrs` when paired with `proxy_for`.
    #[test]
    fn default_filter_populates_when_paired_with_proxy_for() {
        let attrs = parse_attrs(
            r#"table = "vehicles", proxy_for = Vehicle, default_filter = |f| f.active.eq(true)"#,
        )
        .expect("default_filter parsed alongside proxy_for");
        let closure = attrs.proxy_default_filter.expect("populated");
        assert_eq!(closure.inputs.len(), 1);
    }

    /// `default_filter` without `proxy_for` is rejected — same rule as
    /// `default_order`.
    #[test]
    fn default_filter_rejected_without_proxy_for() {
        let err = parse_attrs(r#"table = "vehicles", default_filter = |f| f.active.eq(true)"#)
            .expect_err("orphan default_filter rejected");
        assert!(err.to_string().contains("requires `proxy_for"));
    }

    /// Unknown-attribute diagnostic enumerates the proxy keys so adopters
    /// discover them naturally.
    #[test]
    fn unknown_attribute_diagnostic_lists_proxy_keys() {
        let err =
            parse_attrs(r#"table = "vehicles", widget = "x""#).expect_err("unknown key surfaced");
        let msg = err.to_string();
        assert!(msg.contains("proxy_for"));
        assert!(msg.contains("default_order"));
        assert!(msg.contains("default_filter"));
    }

    /// Class-A regression guard: the resolution-(c) snippet emitted by the
    /// `#[field(unique, index = "<non-btree>")]` diagnostic (attrs.rs ~2652)
    /// must be parseable as a valid Rust attribute. If parentheses become
    /// unbalanced again (three opens, two closes), `syn::parse_str` will
    /// return `Err` and this test fails — catching the regression without
    /// needing a full `lihaaf` run or a compiler invocation.
    /// Fixed substitutions used here: `field_name = "slug"`, `method = "gin"`.
    /// `syn::Attribute` does not implement `Parse` in syn 2.x, so we append
    /// `" struct _DjogiSnippet;"` and parse the whole thing as
    /// `syn::ItemStruct`. The attribute's delimiter balance is fully validated
    /// by the item parser — an unbalanced snippet causes an `Err`.
    #[test]
    fn field_unique_non_btree_recommendation_snippet_parses() {
        let snippet = r##"#[model(indexes(index(fields = [slug], using = "gin")))]"##;
        let input = format!("{snippet} struct _DjogiSnippet;");
        syn::parse_str::<syn::ItemStruct>(&input)
            .expect("resolution-(c) snippet must parse as a valid attribute (paren balance)");
    }

    /// Class-A regression guard for the adjacent gin-on-unsupported-type
    /// recommendation (attrs.rs ~2675). The `)))]` form was already correct
    /// at the time of #83; this guard locks it in.
    /// Uses the same `struct _DjogiSnippet;` wrapping as the sibling test
    /// above — `syn::Attribute` is not `Parse` in syn 2.x.
    #[test]
    fn field_gin_unsupported_type_recommendation_snippet_parses() {
        let snippet =
            r##"#[model(indexes(index(fields = [slug], using = "gin", opclass = "...")))]"##;
        let input = format!("{snippet} struct _DjogiSnippet;");
        syn::parse_str::<syn::ItemStruct>(&input)
            .expect("gin-unsupported-type snippet must parse as a valid attribute (paren balance)");
    }

    /// `Vec<u8>` lowers to `BYTEA`, NOT to a `SMALLINT[]` array.
    /// The byte-vector arm must be reached *before* the generic `Vec<T>`
    /// array arms in `rust_type_to_sql`. This is the macro-unit regression
    /// guard for that ordering: it runs without a Postgres connection or a
    /// `lihaaf` compiler invocation, so a future reorder that let `Vec<u8>`
    /// fall through to `None` (the `DjogiSqlType` field-site bound) or that
    /// added a `Vec<u8>` array arm would fail here immediately.
    #[test]
    fn vec_u8_lowers_to_bytea() {
        let ty: syn::Type = parse_quote!(Vec<u8>);
        assert_eq!(rust_type_to_sql(&ty), Some("BYTEA"));
    }

    /// the byte-vector vs scalar-byte distinction must hold:
    /// a *scalar* `u8` field lowers to `SMALLINT` (widened, with a
    /// projection-side range CHECK), while a `Vec<u8>` is raw `BYTEA`.
    /// Pins both sides so a regression that conflated them — e.g. routing
    /// `Vec<u8>` through the `u8` scalar arm — is caught directly.
    #[test]
    fn scalar_u8_lowers_to_smallint_not_bytea() {
        let scalar: syn::Type = parse_quote!(u8);
        let vector: syn::Type = parse_quote!(Vec<u8>);
        assert_eq!(rust_type_to_sql(&scalar), Some("SMALLINT"));
        assert_eq!(rust_type_to_sql(&vector), Some("BYTEA"));
        assert_ne!(rust_type_to_sql(&scalar), rust_type_to_sql(&vector));
    }

    /// `Option<Vec<u8>>` projects as a nullable `BYTEA` column.
    /// The descriptor emitter strips `Option<…>` via `unwrap_schema_type`
    /// before calling `rust_type_to_sql`, so the inner `Vec<u8>` must
    /// resolve to `BYTEA` and the wrapper must mark the column nullable.
    /// This mirrors the call shape in `model::descriptor` (line ~393).
    #[test]
    fn option_vec_u8_strips_to_bytea_and_is_nullable() {
        let ty: syn::Type = parse_quote!(Option<Vec<u8>>);
        let (inner, nullable) = unwrap_schema_type(&ty);
        assert!(nullable, "Option<Vec<u8>> must mark the column nullable");
        assert_eq!(rust_type_to_sql(&inner), Some("BYTEA"));
    }
}
