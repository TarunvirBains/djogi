//! Attribute parsing for `#[model(table = "...", pk = X)]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
//!
//! `pk` takes a bare identifier (Phase 7-Zero-2 T2) — `HeerId`,
//! `HeerIdRecencyBiased`, `RanjId`, etc. The pre-T2 string-literal form
//! (`pk = "heerid"`) is rejected with a span-carrying diagnostic.
//!
//! `ModelAttrs` keeps a hand-rolled parser: the surface is three keys, the
//! error messages from `syn::Error::new_spanned` already carry precise
//! source spans, and there is no incentive to grow it.
//!
//! `FieldAttrs` parses via `darling::FromField`. Per-field attrs grow over
//! time (later phases add `db_column`, `choices`, `validators`, etc.), and
//! darling's declarative derive gives us span-aware errors for unknown
//! keys, type mismatches, and duplicate keys for free — matching the prior
//! hand-rolled behaviour without each new key duplicating the same
//! `Meta::NameValue` match arm.

use darling::{FromField, FromMeta};
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token};

/// Parsed `#[model(fts(source = "...", dictionary = "..."))]` sub-attribute.
///
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
    ///
    /// Use `#[model(table = "...", no_default)]` for models that contain
    /// field types that do not implement `Default` (e.g. `time::Date`).
    /// Without this flag the generated `Default` impl would fail to compile.
    /// Users must then initialise all fields explicitly instead of relying
    /// on struct-update syntax (`..Model::default()`).
    pub no_default: bool,
    /// When `true`, this model is a many-to-many through / junction model.
    ///
    /// Set via `#[model(table = "...", through)]`. The flag flows through
    /// to [`ModelDescriptor::is_through`](djogi::descriptor::ModelDescriptor::is_through)
    /// at codegen time, where it acts as a marker that:
    ///
    /// - This table is the junction for a specific `impl ManyToMany<Target> for Source`.
    /// - Phase 6's migration differ may later suppress standalone-model
    ///   admin/routing affordances for through tables (deferred).
    ///
    /// Through models remain ordinary queryable `Model`s — the flag is
    /// documentation and future differentiation, not a structural
    /// constraint. Users still `#[derive(Model)]` them as normal and
    /// query them with the standard `QuerySet` API.
    pub through: bool,
    /// When `true`, this model emits a transactional outbox row on every
    /// successful `create` / `save` / `delete` performed through a
    /// `DjogiContext`.
    ///
    /// Set via `#[model(table = "...", events)]`. The flag flows through
    /// to [`ModelDescriptor::has_outbox`](djogi::descriptor::ModelDescriptor::has_outbox)
    /// at codegen time, where `djogi::outbox::emit_event` keys off it to
    /// decide whether to write to `{table}_outbox` inside the active
    /// transaction. Phase 4 Task 6 lands the CRUD-side emission; macro-
    /// side DDL emission to `target/djogi_outbox/{table}_outbox.sql` (so
    /// the Phase 7 migration differ can consume it) is **deferred**. For
    /// now, downstream crates hand-write the `{table}_outbox` DDL
    /// alongside their own migrations.
    ///
    /// Models without `events` skip the outbox call entirely at
    /// macro-expansion time — there is no runtime cost for opt-out.
    pub events: bool,
    /// The user-field name that serves as the idempotency key —
    /// Phase 4 Task 7.5 consumer wiring.
    ///
    /// Set via `#[model(table = "...", idempotency_key = "request_id")]`.
    /// When present, the macro emits two inherent methods:
    ///
    /// - `create_or_find(ctx, row)` — attempts an
    ///   `INSERT ... ON CONFLICT (<this col>) DO NOTHING RETURNING *`
    ///   and, on conflict, re-SELECTs the existing row. Returns
    ///   `(Self, bool /* created */)`.
    /// - `bulk_upsert_by_descriptor(ctx, rows)` — thin wrapper over
    ///   [`bulk_upsert`] that reads this column as the sole ON
    ///   CONFLICT target.
    ///
    /// When not set, both methods are still emitted as thin stubs
    /// that return [`DjogiError::MissingIdempotencyKey`] at runtime —
    /// simplest-possible pointer at the attribute the caller needs
    /// to add. This mirrors Phase 1's approach of populating the
    /// descriptor slot even for models that don't consume it yet.
    ///
    /// The inner string must be a plain ASCII-identifier column name
    /// (letter/underscore start, alphanumerics and underscores after).
    /// The parser rejects anything else so the value can be safely
    /// embedded into the emitted SQL.
    pub idempotency_key: Option<String>,

    /// The user-field name that serves as the tenant discriminator for
    /// Row Level Security — Phase 5 Task 9.
    ///
    /// Set via `#[model(table = "...", tenant_key = "org_id")]`. When present,
    /// the macro emits a side-channel `target/djogi_rls/{table}_rls.sql` file
    /// containing `ALTER TABLE … ENABLE ROW LEVEL SECURITY` and a
    /// `CREATE POLICY … USING (col = current_setting('app.tenant_id')::type)`
    /// statement. Phase 7's migration differ will consume this file; for now
    /// it is hand-applied in integration tests.
    ///
    /// The column referenced by `tenant_key` must be one of: `BigInt`
    /// (HeerId), `Uuid` (RanjId), `Text`, or `Citext`. Any other SQL type
    /// triggers a span-precise compile error at the `tenant_key` attribute.
    ///
    /// At runtime, call [`DjogiContext::set_tenant`] inside an `atomic()`
    /// block to activate the RLS policy for a request.
    pub tenant_key: Option<String>,

    /// Full-text search specification — Phase 5 Task 14.
    ///
    /// Set via `#[model(fts(source = "col1, col2", dictionary = "english"))]`.
    /// When present, the macro emits an `FtsDescriptor` into the
    /// `ModelDescriptor` and the `{Model}Fields` struct gains a `search()`
    /// accessor that returns a typed `FtsFieldRef` for building
    /// `@@` / `ts_rank` predicates.
    ///
    /// Both `source` and `dictionary` are required; omitting either is a
    /// compile error. Both values are validated byte-level at parse time
    /// per `feedback_no_regex_in_djogi`.
    pub fts: Option<FtsSpec>,

    /// Model-level index declarations parsed from `#[model(indexes(...))]`
    /// — Phase 7-Zero v3 T3.
    ///
    /// Each entry lowers to one `IndexSpec` struct literal in the
    /// descriptor's `indexes` slice. Empty when no `indexes(...)` group
    /// is present; otherwise the order follows the user's source-order
    /// declarations, and the descriptor emitter alphabetises by
    /// generated name before producing the final slice literal.
    pub indexes: Vec<crate::model::indexes::ModelIndexDecl>,

    /// Compile-time schema ownership domain — the type path of the app
    /// this model belongs to. Set via `#[model(app = Vehicles)]`.
    /// `None` places the model in the synthetic global bucket, which
    /// Phase 7's differ files under `<default-database>/<empty-label>/`.
    /// Resolved at emission time via `<Path as ::djogi::App>::LABEL` so
    /// the descriptor carries the stable string identifier, not the
    /// Rust type path.
    pub app: Option<syn::Path>,

    /// Historical-metadata pointer to the model's prior app. Set via
    /// `#[model(moved_from_app = OldBilling)]`. Enables Phase 7's
    /// differ to track model-across-app moves without forcing the old
    /// app to stay declared. The pointed-at app may be tombstoned;
    /// that's the intended lifecycle shape for retirements. Resolved
    /// via `<Path as ::djogi::App>::LABEL` same as `app`.
    pub moved_from_app: Option<syn::Path>,

    /// Prior table name when the model has been renamed via
    /// `#[model(table = "...", renamed_from = "old_table")]` —
    /// Phase 7 T2.
    ///
    /// String literal value. The differ uses this to emit
    /// `ALTER TABLE old_table RENAME TO new_table` rather than the
    /// destructive DROP+CREATE pair. The macro validates the string
    /// against the same Postgres identifier grammar that `table = "..."`
    /// uses (ASCII letter/underscore start, ASCII alphanumerics/
    /// underscores after, ≤63 bytes).
    pub renamed_from: Option<String>,
}

/// Parsed `pk = X` value.
///
/// Grammar is a bare identifier (Phase 7-Zero-2 T2). The pre-T2
/// string-literal grammar (`pk = "heerid"`) is rejected with a
/// span-carrying diagnostic directing callers at the new form. The
/// accepted identifier set:
///
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
///
/// Phase 7-Zero-2 T2 flipped the attribute's default: omitted `pk` now
/// resolves to [`PkStrategy::HeerIdDesc`] (recency-biased), not
/// [`PkStrategy::HeerId`].
///
/// Phase 7-Zero-2 T3 adds [`PkStrategy::Custom`] — the attribute parser's
/// fall-through bucket for any identifier that is not one of the built-in
/// aliases. Carries the full `syn::Path` so the descriptor emitter can
/// reference the user's newtype via `<Path as ::djogi::primary_key::PrimaryKey>::KIND`,
/// which lowers to `PkType::Custom(CustomPrimaryKeyKind { .. })` at
/// `inventory::submit!` registration time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkStrategy {
    HeerId,
    RanjId,
    /// `pk = HeerIdRecencyBiased` (canonical) or `pk = HeerIdDesc`
    /// (internal-name alias) — reverse-chronological HeerId variant
    /// added in Phase 7-Zero v3. Lowers to `PkType::HeerIdDesc`;
    /// injects `id: HeerIdDesc` into the struct.
    HeerIdDesc,
    /// `pk = RanjIdRecencyBiased` (canonical) or `pk = RanjIdDesc`
    /// (internal-name alias) — reverse-chronological RanjId variant
    /// added in Phase 7-Zero v3. Lowers to `PkType::RanjIdDesc`;
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
    /// Parse `#[model(table = "posts", pk = HeerId)]` from the attribute token stream.
    ///
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
        let mut idempotency_key: Option<String> = Option::None;
        let mut tenant_key: Option<String> = Option::None;
        let mut fts: Option<FtsSpec> = Option::None;
        let mut indexes: Vec<crate::model::indexes::ModelIndexDecl> = Vec::new();
        let mut seen_indexes = false;
        let mut app: Option<syn::Path> = Option::None;
        let mut moved_from_app: Option<syn::Path> = Option::None;
        let mut renamed_from: Option<String> = Option::None;

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
                // `pk = X` bare-identifier form (Phase 7-Zero-2 T2). Accepts
                // only single-segment paths matching the alias set in
                // `PkStrategy::from_path`. Multi-segment paths and unknown
                // identifiers are rejected so that custom PK types (a T3
                // feature) can't sneak through the T2 parser.
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
                // `pk = "…"` — the pre-T2 string-literal form. Dedicated
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
                        // Phase 7-Zero-2 T13b safety: the table name flows
                        // into `Model::table_name()` which is pushed as a
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
                        // byte-level per `feedback_no_regex_in_djogi` —
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
                        // Phase 7 T2 — `#[model(renamed_from = "old_table")]`
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
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[model] attribute `{}`; expected `table`, `pk`, \
                                 `idempotency_key`, `tenant_key`, `renamed_from`, `fts`, \
                                 `no_default`, `through`, or `events`",
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
                // `indexes(index(...), unique(...), ...)` — Phase 7-Zero v3 T3.
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
                // `app = Vehicles` — Phase 7-Zero v3 T8. Value is a
                // Rust type path (not a string) since apps are
                // addressed by type per §4B (Codex P0-03). The
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
                // `moved_from_app = OldBilling` — Phase 7-Zero v3 T8.
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
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` or `key = TypePath` attribute, \
                         or bare flag (`no_default`, `through`, `events`)",
                    ));
                }
            }
        }

        let table = table.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "#[model] requires `table = \"...\"`",
            )
        })?;
        // Phase 7-Zero-2 T2 flipped the default: omitted `pk` now resolves
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
            app,
            moved_from_app,
            renamed_from,
        })
    }
}

impl PkStrategy {
    /// Lower a `pk = X` path expression to a `PkStrategy`.
    ///
    /// Accepts the single-segment identifier set documented on
    /// [`PkStrategy`]. The two recency-biased identifiers carry
    /// public-facing and internal-facing spellings:
    ///
    /// - `HeerIdRecencyBiased` and `HeerIdDesc` both lower to
    ///   [`PkStrategy::HeerIdDesc`].
    /// - `RanjIdRecencyBiased` and `RanjIdDesc` both lower to
    ///   [`PkStrategy::RanjIdDesc`].
    ///
    /// Any identifier that is not one of the built-in aliases is treated
    /// as an adopter-declared custom PK type (`djogi::primary_key!` or
    /// hand-rolled). Multi-segment paths (e.g. `crate::ids::UserId`) are
    /// also accepted as Custom — the descriptor emitter routes through
    /// `<Path as ::djogi::primary_key::PrimaryKey>::KIND` either way, so
    /// the only constraint is that the path resolves to a type that
    /// implements `PrimaryKey`. That bound is checked at `#[model]`
    /// expansion time by the emitted trait impl lookups; a path pointing
    /// at a non-PK type surfaces a type-error at the const-lookup site,
    /// not here.
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
    ///
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
///
/// Parsed via `darling::FromField`. Unknown keys, type mismatches, and
/// duplicate keys are reported by darling with source spans. `ident` and
/// `ty` are darling "magic" fields — the derive auto-populates them from
/// `syn::Field::{ident, ty}` at call time, independent of the attribute
/// list — so [`FieldAttrs::parse`] callers can read them alongside the
/// parsed attrs without threading the `syn::Field` separately.
// Not every field is read on every call site (e.g. `ident`/`ty` are pending
// use by later Phase 2 / Phase 3 codegen). Suppress dead_code at struct
// granularity so new fields don't spuriously re-trip the lint.
#[allow(dead_code)]
#[derive(Debug, FromField)]
#[darling(attributes(field), allow_unknown_fields)]
pub struct FieldAttrs {
    /// The struct field's identifier.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ident` by magic field name. Always `Some(_)` for
    /// named-field structs; tuple/unit structs are rejected earlier in
    /// `inject::expand`.
    pub ident: Option<syn::Ident>,
    /// The struct field's Rust type.
    ///
    /// Darling's `FromField` derive auto-populates this from
    /// `syn::Field::ty` by magic field name. The type must be `syn::Type`
    /// (not `Option<syn::Type>`) because the derive emits
    /// `ty: field.ty.clone()` verbatim.
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
    /// `#[field(outbox = "ignore")]` — strip this column from the
    /// transactional outbox payload emitted by models with
    /// `#[model(events)]`.
    ///
    /// Only valid value today is `"ignore"` (the field name appears in
    /// the outbox-exclude set for Phase 4 Task 6). The single-valued
    /// enum shape lives behind a string literal so future additions
    /// (`outbox = "encrypt"`, `outbox = "hash"`, etc.) slot in without
    /// reshaping the macro surface. [`FieldAttrs::parse`] post-validates
    /// the literal against the accepted set.
    #[darling(default)]
    pub outbox: Option<String>,
    /// `#[field(version)]` — marks this field as the optimistic-lock
    /// version counter. Exactly one field per model may carry this
    /// attribute, and its type must be `i32` or `i64`. On every
    /// `save()` call the macro emits `{col} = {col} + 1` in the SET
    /// list and `AND {col} = $n` in the WHERE clause, binding the
    /// current in-memory value. When Postgres returns zero rows
    /// (another writer already bumped the version) `save()` returns
    /// `Err(DjogiError::LockConflict(_))` rather than silently
    /// succeeding with a no-op.
    ///
    /// Only bare `i32` / `i64` are accepted — `Option<i32>` and all
    /// other types are rejected at macro-expansion time with a
    /// span-precise compile error.
    #[darling(default)]
    pub version: bool,

    /// `#[field(sequence_within = "parent_fk_column")]` — assigns this
    /// column a monotonically-increasing sequence scoped to the
    /// parent-FK column named in the attribute value. Phase 4 Task
    /// 7.6.
    ///
    /// At `create` time the macro wraps the INSERT in a counter
    /// upsert against `<table>_seq_<parent_fk_column>`, captures the
    /// returned `last_seq`, and assigns it to this field before the
    /// main INSERT emits. Rollback of the outer `atomic()` cleans
    /// both the counter increment and the main row.
    ///
    /// Only one field per model may carry `sequence_within` today.
    /// Multiple-scope sequencing (two scoped counters on the same
    /// model) would require multiple companion tables and is a
    /// future extension.
    ///
    /// The attribute value must be a plain ASCII identifier — it is
    /// embedded directly into the counter-upsert SQL. Byte-level
    /// validation per `feedback_no_regex_in_djogi`.
    ///
    /// The companion-table DDL emission is DEFERRED to Phase 7
    /// (migration system). Until then, downstream crates hand-write
    /// the `<table>_seq_<parent_fk_column>` table alongside their
    /// own migrations using the shape documented on
    /// `create_sequence_counter_sql`.
    #[darling(default)]
    pub sequence_within: Option<String>,

    /// `#[field(rationale = "...")]` — free-text justification for why this
    /// field carries a behaviour-modifying attribute such as
    /// `outbox = "ignore"`.
    ///
    /// No validation is applied to the string value — it exists purely as
    /// in-source documentation that suppresses the advisory warning emitted
    /// when `outbox = "ignore"` is present without an accompanying
    /// `rationale`. Future attribute advisories (`lazy`, `partition_by`)
    /// will key off the same field once those attributes become functional
    /// features (deferred; see Task 11 in the Phase 5 plan).
    #[darling(default)]
    pub rationale: Option<String>,

    /// `#[field(expose(...))]` — per-attribute parsed specs. Darling
    /// accepts **multiple** `#[field(expose(...))]` attributes on the
    /// same field via `#[darling(multiple)]`; each one is parsed
    /// independently by [`ExposeSpec`]'s `FromMeta` impl. The merged
    /// [`Self::expose`] is assembled in [`FieldAttrs::parse`] by folding
    /// this Vec (cross-attr duplicate detection + `none`/`internal`
    /// exclusivity run at merge time).
    ///
    /// Without `#[darling(multiple)]`, darling would raise
    /// `Duplicate field \`expose\`` on the second occurrence — the
    /// multi-attr merge path needs this opt-in to exist at all.
    ///
    /// Users never read this field; it is the darling landing site for
    /// the rename. Read [`Self::expose`] instead.
    #[darling(multiple, rename = "expose")]
    pub expose_raw: Vec<ExposeSpec>,

    /// Merged `expose(...)` spec across every `#[field(expose(...))]`
    /// attribute on this field — the single source of truth downstream
    /// code (descriptor emission, visage codegen) reads.
    ///
    /// Grammar summary:
    /// - Scalar form: `expose(public, self_view, admin, export)` — the
    ///   field appears in each listed scope under its column name.
    /// - Relation form (T6+): `expose(public -> UserPublic)` —
    ///   narrow peer visage; `expose(public -> User)` — full peer model
    ///   embed; `expose(public -> User { manager_id -> ManagerPublic })`
    ///   — nested traversal (structural metadata only at this time).
    /// - Deprecated relation form: `expose(public = "UserSummary", ...)`
    ///   — string-literal shape kept for transitional backward compat.
    /// - Sentinels: `expose(none)` / `expose(internal)` — accepted no-op
    ///   sentinels, identical to an absent `expose` annotation; mutually
    ///   exclusive with real scopes on the same field.
    ///
    /// `#[darling(skip)]` here is safe because users only ever write
    /// `#[field(expose(...))]` (which lands in [`Self::expose_raw`] via
    /// the rename above); nobody writes `#[field(expose = ...)]` as a
    /// name-value targeting this field, so darling's "unknown field"
    /// error path is never triggered.
    #[darling(skip)]
    pub expose: ExposeSpec,
}

/// Per-field visage exposure spec — parsed from `#[field(expose(...))]`.
///
/// See [`FieldAttrs::expose`] for the grammar summary. Scope names are
/// order-insensitive (stored in a [`HashSet`]/[`HashMap`]); source order
/// only matters for error-span recovery, which falls back to the enclosing
/// attribute list span.
///
/// The parser stores BOTH the scalar set and the relation map because a
/// field CAN carry both across multiple attrs — e.g.
/// `#[field(expose(public))] #[field(expose(admin -> OwnerDetail))]` marks
/// the field scalar in `public` and relation-nested in `admin`. At codegen
/// time the emitter looks up the relation map to decide if the visage entry
/// is a column name or a peer-visage type.
///
/// `none` / `internal` set [`Self::suppressed`] and are mutually exclusive
/// with any other scope (per Q11 in the Phase 4.5 v3 plan). They mean
/// "this field does not appear in any transport visage" — same
/// semantics as omitting the `expose` annotation.
///
/// Phase 7-Zero-2 T6 introduced the `->` traversal grammar as the new canonical
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
///
/// `peer` is the full `syn::Path` the user wrote after `->`. The visage
/// emitter inspects the path's last segment to decide between two embed
/// shapes:
///
/// - **Narrow visage** — last segment looks like `<ModelIdent><Scope>` (e.g.
///   `DepartmentPublic`). The peer field in the visage is typed `peer` and
///   constructed via `<peer as TryFrom<&Target>>::try_from(...)`.
/// - **Full peer model** — last segment equals the relation's target model
///   ident (e.g. `Department`). The peer field carries the full `Target`
///   value cloned out of the resolved relation.
///
/// The deprecated `expose(scope = "Peer")` string form lowers to the same
/// `RelationExposure` with `peer` parsed from the literal and `nested = []`.
///
/// `nested` is recursive — each entry carries the same `peer + nested`
/// shape rooted at a named field of the parent's peer model. Nested
/// exposures are STRUCTURAL METADATA only at this point; query-surface
/// machinery that consumes them lands in later T7+ work.
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
///
/// Carries the parent-side field identifier alongside the recursive
/// [`RelationExposure`] payload. The field identifier is always a bare
/// identifier (no path), naming a column / relation on the parent peer
/// model. The visage emitter does not currently consume `nested`
/// (T6 freezes the grammar without wiring nested embed); later tasks
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

    fn is_builtin_scope(name: &str) -> bool {
        Self::BUILTIN_SCOPES.contains(&name)
    }

    /// Parse the `expose(...)` argument list from a single
    /// `#[field(expose(...))]` attribute. Returns a fresh [`ExposeSpec`]
    /// covering just that attribute's tokens; cross-attribute merging
    /// lives in [`FieldAttrs::parse`].
    ///
    /// Phase 7-Zero-2 T6 — the parser is hand-rolled over `ParseStream`
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
    ///
    /// - bare scope ident → `expose(public, admin)`
    /// - suppressor `none` / `internal` (mutually exclusive with real scopes)
    /// - deprecated `scope = "Peer"` string-literal form
    /// - new `scope -> Peer` arrow form (with optional `{ nested }`)
    ///
    /// Per-attr duplicate detection runs here; cross-attr merge lives in
    /// [`FieldAttrs::parse`].
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

            if !Self::is_builtin_scope(&scope_name) {
                return Err(syn::Error::new_spanned(
                    &scope_ident,
                    format!(
                        "unknown scope `{scope_name}`; expected one of: \
                         public, self_view, admin, export, none, internal"
                    ),
                ));
            }

            saw_real_scope = true;

            // Three follow-on forms:
            //   1. `,` or end → bare-scope (scalar) form
            //   2. `= "Peer"` → deprecated string-literal relation form
            //   3. `-> Peer { nested? }` → new arrow relation form
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
        // T6 fixup — the nested-brace grammar (`-> Peer { field -> Peer2 }`)
        // is parseable in principle and the structural types
        // (`NestedRelationExposure`, `Self::parse_nested_block`) are
        // ready for a later task to consume, but T6 does not yet lower
        // it into the visage emitter. Silently parsing and discarding
        // nested traversal would be a partial-feature trap, so reject
        // any brace with an actionable compile error until the emitter
        // consumes it. The T6 Codex review flagged the original
        // parse-and-discard shape as a P0.
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
    ///
    /// Kept structurally alongside [`NestedRelationExposure`] even
    /// though the T6 parser now rejects any brace at the entry site
    /// (the emitter does not yet consume nested traversal — see the
    /// T6 fixup rejection in `parse_relation_exposure`). The function
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
            // A name-value `=` form is NOT supported inside nested blocks —
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
    ///
    /// Returns an all-default instance if no `#[field]` attr is present
    /// (darling's `#[darling(default)]` container attr handles the no-attr
    /// case). Darling emits span-aware errors for:
    /// - Unknown attribute keys (e.g. `#[field(nonexistent)]`).
    /// - Type mismatches (e.g. `max_length = "x"` where an integer is required).
    /// - Duplicate keys across multiple `#[field(...)]` attrs.
    ///
    /// `on_delete` is a string with a constrained value set that darling's
    /// type-level parsing cannot enforce. We post-validate the value below
    /// and — when rejecting — walk the field's raw `#[field(...)]` attrs
    /// to recover the literal's `Span`, so the error underlines the bad
    /// value rather than the entire field declaration. Matches the pre-
    /// darling hand-rolled behaviour; keeps the surface consistent with
    /// how `pk = X` span-points at its own path in `ModelAttrs`.
    pub fn parse(field: &syn::Field) -> syn::Result<Self> {
        // `darling::Error` carries source spans from the originating
        // attribute tokens; `From<darling::Error> for syn::Error` preserves
        // them, so rely on the built-in conversion rather than collapsing
        // everything onto the whole field with `new_spanned`.
        let mut attrs =
            <Self as darling::FromField>::from_field(field).map_err(syn::Error::from)?;

        // Phase 5 — manually parse index from raw attributes.
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
            "outbox",
            "sequence_within",
            "expose",
            "rationale",
            "lazy",
            "version", // Task 3 — optimistic lock version counter
        ];
        // Phase 7-Zero v3 T2 Q2/v2 #8 — `nulls_not_distinct` is deliberately
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
        let field_level_redirects: &[(&str, &str)] =
            &[("nulls_not_distinct", nulls_not_distinct_redirect.as_str())];
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
                    // Phase 7-Zero T2 redirects take priority over the
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

        // Phase 7-Zero v3 T2 Q3 — hash indexes cannot enforce uniqueness.
        // Postgres hash indexes store only the hash of the key, so they
        // physically cannot support `UNIQUE`; Postgres itself errors out
        // on `CREATE UNIQUE INDEX ... USING HASH(...)`. Catching it at the
        // declaration gives the user a span-precise error pointing at the
        // field rather than at a migration failure much later.
        if attrs.unique && attrs.index_method.as_deref() == Some("hash") {
            let field_name = field
                .ident
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let span = find_named_str_lit_span(field, "index").unwrap_or_else(|| field.span());
            return Err(syn::Error::new(
                span,
                format!(
                    "`#[field(index = \"hash\", unique)]` on `{field_name}`: hash indexes \
                     cannot enforce uniqueness. Use `index = \"btree\"` with `unique`, or \
                     drop `unique` if a non-unique hash lookup index is what you want."
                ),
            ));
        }

        // Phase 7-Zero v3 T2 Q4 — `#[field(index = "gin")]` is type-gated.
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

        if let Some(outbox) = &attrs.outbox {
            // Only `"ignore"` is accepted today; future Phase 6+ values
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
        Ok(attrs)
    }
}

/// Parse an index method string to validate it against known `IndexType` methods.
///
/// Valid methods: `btree`, `gin`, `gist`, `brin`, `hash`, `spgist`.
/// Returns an error with a clean message listing all valid methods if the
/// string does not match. The returned error's span points to the offending
/// literal, not the whole field. Returns `Ok(())` if the method is valid.
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

/// `true` when `ty` is an accepted target for `#[field(index = "gin")]` —
/// i.e. `Jsonb<T>`, `Vec<T>`, or `TsVector`, unwrapping one layer of
/// `Option<…>` so nullable columns are accepted too.
///
/// Uses last-segment name matching so bare idents, `djogi::Jsonb<T>`,
/// `djogi::fts::TsVector`, and similar qualified forms all resolve. Phase
/// 7-Zero v3 T2 Q4 codifies this set; anything outside it must declare
/// the index at the model level where opclass can be specified.
fn is_gin_compatible_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| matches!(seg.ident.to_string().as_str(), "Jsonb" | "Vec" | "TsVector"))
        .unwrap_or(false)
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the literal bound to `on_delete`, if present.
///
/// Thin wrapper around [`find_named_str_lit_span`] kept for readability at
/// the callsite.
fn find_on_delete_lit_span(field: &syn::Field) -> Option<proc_macro2::Span> {
    find_named_str_lit_span(field, "on_delete")
}

/// Walk the raw `#[field(...)]` attrs on `field` and return the `Span` of
/// the string literal bound to `key`, if present.
///
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

/// The SQL column type for a Rust type string.
///
/// Returns `None` for `Option<T>` (handled by the caller via `unwrap_option`) and
/// for unrecognized types (the caller should emit a compile error).
///
/// Called by `descriptor::expand` and `inject::expand` starting in Tasks 4–6.
#[allow(dead_code)]
pub fn rust_type_to_sql(ty: &syn::Type) -> Option<&'static str> {
    // Normalise whitespace and strip an optional leading `::` so absolute
    // paths (`::djogi::types::HeerId`) match the same arms as their
    // relative counterparts (`djogi::types::HeerId`).
    let s = quote::quote!(#ty).to_string().replace(' ', "");
    let s = s.strip_prefix("::").unwrap_or(&s);
    match s {
        "String" => Some("TEXT"),
        "i16" => Some("SMALLINT"),
        "i32" => Some("INTEGER"),
        "i64" => Some("BIGINT"),
        "f32" => Some("REAL"),
        "f64" => Some("DOUBLE PRECISION"),
        "bool" => Some("BOOLEAN"),
        "DateTime" | "time::OffsetDateTime" | "OffsetDateTime" => Some("TIMESTAMPTZ"),
        "Date" | "time::Date" => Some("DATE"),
        "Decimal" | "rust_decimal::Decimal" => Some("NUMERIC"),
        "Uuid" | "uuid::Uuid" => Some("UUID"),
        // Phase 7-Zero-2 T4 — built-in PK types (HeerId / RanjId family) are
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
        "Vec<String>" => Some("TEXT[]"),
        "Vec<i32>" => Some("INTEGER[]"),
        "Vec<i64>" => Some("BIGINT[]"),
        "Vec<bool>" => Some("BOOLEAN[]"),
        // Spatial — all GeographyValue-implementing types map to the
        // corresponding GEOGRAPHY(<SUBTYPE>, 4326) SQL type. The `spatial`
        // feature flag lives on the `djogi` runtime crate, not here; the
        // macro recognises type names unconditionally so it emits the correct
        // descriptor regardless of feature state. If the user has the spatial
        // feature off, the compile error comes from "unresolved type" at the
        // struct definition, not from the macro.
        //
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
        // `Jsonb<T>` for any `T: JsonbSchema` lowers to a Postgres
        // `JSONB` column. The runtime descriptor's `sql_type` slot
        // therefore needs `FieldSqlType::Jsonb`. Detect the wrapper by
        // its head ident, accepting bare `Jsonb<…>`, `djogi::Jsonb<…>`,
        // and `::djogi::Jsonb<…>` forms (the leading `::` is stripped
        // above). Surfaced by Phase 7 T10 — without this rule, every
        // `sync_models`'d table with a `Jsonb<T>` field rendered the
        // column as `TEXT` and round-tripping the typed Jsonb value
        // failed at INSERT time.
        _ if s.starts_with("Jsonb<")
            || s.starts_with("djogi::Jsonb<")
            || s.starts_with("djogi::jsonb::Jsonb<") =>
        {
            Some("JSONB")
        }
        _ => None,
    }
}

/// Convert a pre-validated `on_delete` string into the matching
/// `::djogi::OnDelete` token path.
///
/// Caller contract: `s` must be one of the six validated values — any other
/// input is a bug in the caller, since [`FieldAttrs::parse`] has already
/// rejected out-of-domain strings with a span-carrying error. The function
/// falls back to `OnDelete::Restrict` on an unrecognized value so a caller
/// skipping the validator does not produce a bogus token stream; that
/// matches the framework's cascade-off-by-default posture and keeps the
/// descriptor emitter total.
///
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
///
/// This enum is the macro-crate mirror of `djogi::relation::RelationKind`:
/// `djogi-macros` cannot depend on `djogi` (the macro is compiled first),
/// so when the macro wants to *reason* about a relation shape before
/// emitting tokens, it works with this local copy. The emitter converts to
/// `::djogi::relation::RelationKind::…` token paths at code-generation
/// time — the two enums only need to stay in sync on the set of variants.
///
/// Phase 3 Task 2 covers `ForeignKey<T>` and `OneToOneField<T>` (plus their
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
///
/// Returned by [`detect_relation`]. Two facets of the relation target are
/// carried separately because they have different consumers:
///
/// - [`target_name`](Self::target_name) is a short, path-free string used
///   only for the descriptor field `target_type_name` (Phase 6's migration
///   emitter looks models up by this name against the `inventory`-registered
///   descriptors, so keeping it segmented matches that lookup key).
/// - [`target_type`](Self::target_type) is the **full `syn::Type`** the user
///   wrote inside the wrapper (`Owner`, `models::Owner`, `crate::models::Owner`,
///   or even `inner::Widget`). Emitted verbatim into positions such as
///   `RelationPath<#source, #target_type>` and `<#target_type as Model>::…`
///   so fully-qualified paths resolve at the user's macro-call site without
///   requiring an extra `use …;` import.
///
/// Splitting the two lets the emitter use the right form in the right place —
/// collapsing down to just the last-segment ident (as the previous
/// `(RelationKind, String, bool)` tuple did) silently broke codegen for any
/// target type the user spelled with a path prefix.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RelationInfo {
    /// FK vs O2O — drives both the emitted `RelationKind` variant and the
    /// descriptor-side `relation_kind` field.
    pub kind: RelationKind,
    /// Short name for descriptor use (e.g. `"Owner"` from any of:
    /// `Owner`, `models::Owner`, `crate::models::Owner`). Always the last
    /// segment ident of the inner type path — this is the name Phase 6's
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
///
/// Returns [`Some(RelationInfo)`](RelationInfo) when the outermost
/// (post-`Option<…>`-strip) type is one of the recognized relation wrappers:
///
/// - `ForeignKey<T>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: false })`
/// - `Option<ForeignKey<T>>` → `Some(RelationInfo { kind: ForeignKey, …, nullable: true })`
/// - `OneToOneField<T>` → `Some(RelationInfo { kind: OneToOne, …, nullable: false })`
/// - `Option<OneToOneField<T>>` → `Some(RelationInfo { kind: OneToOne, …, nullable: true })`
/// - anything else → `None`
///
/// # Robustness
///
/// The check inspects the *last* path segment's ident, which makes it
/// resilient to fully-qualified user spellings like
/// `::djogi::relation::ForeignKey<Owner>` or `djogi::ForeignKey<Owner>` —
/// users who reach for an explicit path instead of `use djogi::prelude::*`
/// still get the correct relation detection. The trade-off: a user-defined
/// type with the literal name `ForeignKey` shadows the detection. That
/// matches how `unwrap_option` already handles `Option`: the macro works
/// from the spelling the user wrote, and the behaviour is consistent and
/// easy to reason about.
///
/// # Target preservation
///
/// Both the short `target_name` and the full `target_type` are preserved.
/// Consumers that emit `type`-position tokens (e.g. `relations::expand`)
/// must use `target_type` so that `ForeignKey<crate::models::Owner>` still
/// emits a resolvable `RelationPath<_, crate::models::Owner>`; consumers
/// that only need the short name for descriptor lookup (`descriptor::expand`)
/// use `target_name`.
#[allow(dead_code)]
pub fn detect_relation(ty: &syn::Type) -> Option<RelationInfo> {
    // First, strip one layer of `Option<…>` using the shared helper. The
    // returned `nullable` flag propagates straight through to the caller —
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
///
/// Only recognizes the prelude's `Option` — specifically the path forms
/// `Option<T>`, `std::option::Option<T>`, and `core::option::Option<T>`. A user
/// type that happens to be named `Option` in their own module (e.g.
/// `my_crate::Option<T>`) is left unchanged, because treating it as nullable
/// silently would produce wrong migrations. This matches how users actually
/// read the type: `Option<T>` in the prelude means "SQL NULL allowed"; anything
/// else is a user type that must map via `rust_type_to_sql`.
///
/// Non-`Option` types are returned unchanged with `nullable = false`.
///
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
///
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
///
/// Used by the RLS DDL emitter to pick the correct `current_setting(...)::cast`
/// expression. `HeerId` → `BigInt`; `Uuid` / `RanjId` → `Uuid`; `String` →
/// `Text`; everything else → `Unsupported`.
///
/// One `Option<…>` layer is stripped first so nullable tenant columns
/// (`Option<HeerId>`) are also accepted.
#[allow(dead_code)]
pub fn field_sql_type_category(ty: &syn::Type) -> FieldSqlTypeCategory {
    let (inner, _nullable) = unwrap_option(ty);
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
    use super::rust_type_to_sql;
    use syn::parse_quote;

    /// Phase 7 T10 — `Jsonb<T>` for any `T: JsonbSchema` must lower to
    /// `JSONB`. Codex round-1 Concern 1 PARTIAL flagged that the three
    /// substrate fixes T10 made (this one + framework-col defaults +
    /// FK type substitution) only had indirect coverage through the
    /// live integration suite. Direct lock-in here so a regression
    /// surfaces without needing Postgres.
    ///
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

    /// Phase 7 T10 — leading `::` absolute path forms must match the
    /// same arm as their relative counterparts. Locks in the
    /// strip-prefix normalization the function applies before the
    /// match, since `Jsonb<T>` recognition uses `starts_with(...)` on
    /// the normalized string.
    #[test]
    fn jsonb_wrapper_absolute_paths_match() {
        let abs_djogi: syn::Type = parse_quote!(::djogi::Jsonb<P>);
        let abs_jsonb: syn::Type = parse_quote!(::djogi::jsonb::Jsonb<P>);
        assert_eq!(rust_type_to_sql(&abs_djogi), Some("JSONB"));
        assert_eq!(rust_type_to_sql(&abs_jsonb), Some("JSONB"));
    }

    /// Negative case — types that just *contain* the substring `Jsonb`
    /// but aren't the wrapper must NOT fall into the JSONB arm. The
    /// recognition uses `starts_with("Jsonb<")` (etc.) precisely to
    /// avoid this kind of accidental match.
    #[test]
    fn jsonb_lookalikes_do_not_match() {
        // `MyJsonb<T>` is a hypothetical user wrapper — must not lower
        // to JSONB just because the string contains "Jsonb".
        let lookalike: syn::Type = parse_quote!(MyJsonb<P>);
        // Type that mentions Jsonb only inside generics shouldn't
        // either, since the head-ident is `Vec`.
        let in_generic: syn::Type = parse_quote!(Vec<Jsonb<P>>);
        assert_eq!(rust_type_to_sql(&lookalike), None);
        assert_eq!(rust_type_to_sql(&in_generic), None);
    }
}
