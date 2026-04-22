//! Attribute parsing for `#[model(table = "...", pk = "...")]`
//! and `#[field(unique, index, max_length = N, renamed_from = "...", on_delete = "...")]`.
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
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, Lit, Meta, MetaNameValue, Token};

/// Options extracted from `#[model(table = "...", pk = "...")]`.
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
}

/// Parsed `pk = "..."` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkStrategy {
    HeerId,
    RanjId,
    Serial,
    None,
}

impl ModelAttrs {
    /// Parse `#[model(table = "posts", pk = "heerid")]` from the attribute token stream.
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
                        table = Some(s.value());
                    } else if path.is_ident("pk") {
                        if pk.is_some() {
                            return Err(syn::Error::new_spanned(
                                path,
                                "duplicate `pk` key in #[model(...)]",
                            ));
                        }
                        pk = Some(
                            PkStrategy::from_str(&s.value())
                                .map_err(|msg| syn::Error::new_spanned(s, msg))?,
                        );
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
                    } else {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown #[model] attribute `{}`; expected `table`, `pk`, `idempotency_key`, `no_default`, `through`, or `events`",
                                path.get_ident().map(|i| i.to_string()).unwrap_or_default()
                            ),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `key = \"value\"` attribute or bare flag (`no_default`, `through`, `events`)",
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
        let pk = pk.unwrap_or(PkStrategy::HeerId);

        Ok(ModelAttrs {
            table,
            pk,
            no_default,
            through,
            events,
            idempotency_key,
        })
    }
}

impl PkStrategy {
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "heerid" => Ok(PkStrategy::HeerId),
            "ranjid" => Ok(PkStrategy::RanjId),
            "serial" => Ok(PkStrategy::Serial),
            "none" => Ok(PkStrategy::None),
            other => Err(format!(
                "unknown pk strategy `{other}`; expected one of: heerid, ranjid, serial, none"
            )),
        }
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
    /// - Relation form: `expose(public = "UserSummary", ...)` — the field
    ///   is the named peer visage in each listed scope.
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
/// `#[field(expose(public))] #[field(expose(admin = "OwnerDetail"))]` marks
/// the field scalar in `public` and relation-nested in `admin`. At codegen
/// time (Phase 4.5 Task 3 / Task 5) the scope membership is the union; the
/// emitter looks up the relation map to decide if the visage entry is
/// a column name or a peer-visage type.
///
/// `none` / `internal` set [`Self::suppressed`] and are mutually exclusive
/// with any other scope (per Q11 in the Phase 4.5 v3 plan). They mean
/// "this field does not appear in any transport visage" — same
/// semantics as omitting the `expose` annotation.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ExposeSpec {
    /// Scopes this field appears in via the scalar form.
    pub scalar_scopes: std::collections::HashSet<String>,
    /// Scopes this field appears in via the relation form; value is the
    /// peer visage type name.
    pub relation_scopes: std::collections::HashMap<String, String>,
    /// `true` when the user wrote `expose(none)` or `expose(internal)`.
    /// Semantically identical to an absent `expose` annotation.
    pub suppressed: bool,
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
    fn parse_list(list: &syn::MetaList) -> syn::Result<Self> {
        let mut spec = ExposeSpec::default();
        let nested = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;

        if nested.is_empty() {
            return Err(syn::Error::new_spanned(
                list,
                "`expose(...)` requires at least one scope; \
                 write `expose(public)` / `expose(none)` / etc.",
            ));
        }

        let mut saw_suppressor = false;
        for meta in &nested {
            match meta {
                Meta::Path(path) => {
                    let ident = path.get_ident().ok_or_else(|| {
                        syn::Error::new_spanned(path, "expected a scope name (identifier)")
                    })?;
                    let name = ident.to_string();

                    if Self::is_suppressor(&name) {
                        saw_suppressor = true;
                        spec.suppressed = true;
                        continue;
                    }
                    if !Self::is_builtin_scope(&name) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown scope `{name}`; expected one of: \
                                 public, self_view, admin, export, none, internal"
                            ),
                        ));
                    }
                    if spec.relation_scopes.contains_key(&name) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "scope `{name}` already declared with a peer \
                                 visage name; pick one form per scope"
                            ),
                        ));
                    }
                    if !spec.scalar_scopes.insert(name.clone()) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!("scope `{name}` listed more than once"),
                        ));
                    }
                }
                Meta::NameValue(MetaNameValue {
                    path,
                    value:
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }),
                    ..
                }) => {
                    let ident = path.get_ident().ok_or_else(|| {
                        syn::Error::new_spanned(path, "expected a scope name (identifier)")
                    })?;
                    let name = ident.to_string();

                    if Self::is_suppressor(&name) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "the `{name}` scope does not accept a nested \
                                 visage name; write `expose({name})` alone"
                            ),
                        ));
                    }
                    if !Self::is_builtin_scope(&name) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "unknown scope `{name}`; expected one of: \
                                 public, self_view, admin, export"
                            ),
                        ));
                    }
                    if spec.scalar_scopes.contains(&name) {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!(
                                "scope `{name}` already declared as bare scope; \
                                 pick one form per scope"
                            ),
                        ));
                    }
                    if spec
                        .relation_scopes
                        .insert(name.clone(), s.value())
                        .is_some()
                    {
                        return Err(syn::Error::new_spanned(
                            path,
                            format!("scope `{name}` listed more than once"),
                        ));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "expected `scope` or `scope = \"PeerProjection\"`",
                    ));
                }
            }
        }

        if saw_suppressor && (!spec.scalar_scopes.is_empty() || !spec.relation_scopes.is_empty()) {
            return Err(syn::Error::new_spanned(
                list,
                "`none` / `internal` cannot be combined with other scopes; \
                 omit them when declaring real scopes",
            ));
        }

        Ok(spec)
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
    /// how `pk = "..."` span-points at its own literal in `ModelAttrs`.
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
                if let Some(key) = key.as_ref().filter(|k| !VALID_FIELD_KEYS.contains(&k.as_str())) {
                    return Err(syn::Error::new_spanned(
                        nested,
                        format!("unknown field attribute: `{key}`"),
                    ));
                }
            }
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
                    for (scope, peer) in parsed.relation_scopes {
                        if expose.scalar_scopes.contains(&scope)
                            || expose.relation_scopes.insert(scope.clone(), peer).is_some()
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
    let s = quote::quote!(#ty).to_string().replace(' ', "");
    match s.as_str() {
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
        "serde_json::Value" | "Value" => Some("JSONB"),
        "Vec<String>" => Some("TEXT[]"),
        "Vec<i32>" => Some("INTEGER[]"),
        "Vec<i64>" => Some("BIGINT[]"),
        "Vec<bool>" => Some("BOOLEAN[]"),
        // Option<T> is handled at call site — strip and recurse via unwrap_option
        _ if s.starts_with("Option<") => None,
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
