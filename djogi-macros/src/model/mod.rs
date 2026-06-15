//! Orchestrates the `#[model]` attribute macro expansion.
//! Each sub-module handles one concern. Modules that are not yet implemented
//! expose a no-op `expand(...)` that returns an empty `TokenStream`, so the
//! overall pipeline compiles in and each subsequent task can replace its
//! stub in isolation without touching this file.

pub mod attrs;
pub mod cacheable;
pub mod computed;
pub mod crud;
pub mod derived;
pub mod descriptor;
pub mod exclusion;
pub mod filter;
pub mod from_joined_row;
pub mod from_row;
pub mod hooks;
pub mod indexes;
pub mod inject;
pub mod mirjzson;
pub mod outer_ref;
pub mod portable_field_emit;
pub mod protected;
pub mod proxy;
pub mod relations;
pub mod schema_const;
pub mod sql_bind;
pub mod stubs;
pub mod visage_ctx;
pub mod visage_fields;
pub mod visage_query;
pub mod visages;

use attrs::ModelAttrs;
use proc_macro2::TokenStream;
use quote::format_ident;
use syn::{Fields, ItemStruct, parse2};

/// Called from `lib.rs`. Returns the full expanded token stream, or a
/// compile-error token stream on parse/validation failure.
pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_inner(attr, item).unwrap_or_else(|e| e.to_compile_error())
}

fn expand_inner(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let model_attrs = ModelAttrs::parse(attr)?;

    // Parse the struct (all user attributes remain intact).
    let mut struct_item: ItemStruct = parse2(item)?;

    // Collect per-field attribute options before we add framework fields.
    let field_attrs: Vec<attrs::FieldAttrs> = struct_item
        .fields
        .iter()
        .map(attrs::FieldAttrs::parse)
        .collect::<syn::Result<_>>()?;

    // 3 — parse `#[computed(sql = "...")]` annotations.
    // Surfaces span-precise errors for the deferred `stored` keyword,
    // empty-SQL strings, unknown keys, and `#[field(...)]`-with-
    // `#[computed(...)]` collisions.
    let computed_attrs = computed::parse_computed_attrs(&struct_item)?;

    // #231 — parse struct-level `#[derived(...)]` attributes.
    // Each attribute declares one visage-derived field projection entry
    // scoped to one or more of the canonical visages (`public` /
    // `self_view` / `admin` / `export`). The parser runs BEFORE the
    // attribute-strip pass below so the `#[derived(...)]` payloads are
    // still on the struct when the walker runs. Span-precise errors
    // surface for missing required keys, identifier-shape violations,
    // unknown scopes, SQL surface bugs (statement separators, leading
    // DDL, `$N` placeholders, aggregate misuse), and parse failures on
    // the embedded `rust` expression. The cross-attribute checks
    // (column collisions, derived-derived collisions, `pk = None`
    // incompatibility) run further below once the model's column set is
    // in scope.
    let derived_attrs = derived::parse_derived_attrs(&struct_item)?;

    // #195 — MirJzSON gate. Every `MirJzSON` /
    // `Option<MirJzSON>` field must carry
    // `#[mirjzson(justification = "...")]`, and the attribute is
    // rejected on any other field type. Captured here for the strip
    // step below; future consumers (descriptor emission, doc surfaces)
    // can read the justification without re-parsing. Runs BEFORE the
    // attribute-strip pass so the `#[mirjzson(...)]` attributes are
    // still on the struct when the validator walks the fields.
    let _mirjzson_attrs = mirjzson::parse_mirjzson_attrs(&struct_item)?;

    // Remove computed fields from the struct
    // before any downstream pass walks `struct_item.fields`. Computed
    // fields are virtual: no storage column, no INSERT/UPDATE binding,
    // no row decode. Leaving them on the struct produces three silent
    // failures:
    // 1. `from_row::expand` would put them in `COLUMN_LIST` /
    // `COLUMNS`, so every `SELECT {COLUMN_LIST} FROM t` would
    // include `total_price` (a column that never existed) and
    // every row decode would error or panic.
    // 2. `descriptor::expand` would emit them as regular
    // `FieldDescriptor` entries, and the migration differ would
    // generate `ADD COLUMN total_price FLOAT8` DDL — drift the
    // adopter never authored.
    // 3. The `Default` / `FromPgRow` impl bodies would reference
    // `total_price:...` as a real field assignment.
    // `field_attrs` was collected upstream by zip-walking the same
    // field iterator, so its index alignment with `struct_item.fields`
    // is positional. After removal the indices shift; downstream
    // consumers (`from_row`, `descriptor`, `stubs`, `filter`,
    // `relations`, `outer_ref`, `visages`, `from_joined_row`,
    // `emit_rationale_advisories`) all walk `struct_item.fields`
    // and `field_attrs` together, so we filter both in lockstep here
    // to keep them aligned.
    let computed_field_names: std::collections::BTreeSet<String> = computed_attrs
        .iter()
        .map(|(ident, _)| ident.to_string())
        .collect();
    // Guard: a non-computed field must not share a name with a computed field.
    // In practice this is unreachable — Rust forbids duplicate field names and
    // `parse_computed_attrs` only collects fields that carry `#[computed(...)]`
    // but the check prevents a future refactor that populates `computed_attrs`
    // from an external source from silently stripping the wrong field and
    // corrupting `FromPgRow` ordinal decoding.
    if !computed_field_names.is_empty()
        && let Fields::Named(named) = &struct_item.fields
    {
        for field in &named.named {
            let has_computed_attr = field.attrs.iter().any(|a| a.path().is_ident("computed"));
            if let Some(id) = &field.ident
                && !has_computed_attr
                && computed_field_names.contains(&id.to_string())
            {
                return Err(syn::Error::new_spanned(
                    field,
                    format!(
                        "field `{id}` appears as both a computed field and a \
       non-computed field — this should never happen under \
       normal Rust syntax; this is a djogi internal error"
                    ),
                ));
            }
        }
    }
    if !computed_field_names.is_empty()
        && let Fields::Named(named) = &mut struct_item.fields
    {
        named.named = std::mem::take(&mut named.named)
            .into_iter()
            .filter(|f| {
                f.ident
                    .as_ref()
                    .is_none_or(|id| !computed_field_names.contains(&id.to_string()))
            })
            .collect();
    }
    let field_attrs: Vec<attrs::FieldAttrs> = field_attrs
        .into_iter()
        .filter(|fa| {
            fa.ident
                .as_ref()
                .is_none_or(|id| !computed_field_names.contains(&id.to_string()))
        })
        .collect();

    validate_through_model_shape(&struct_item, &model_attrs)?;
    validate_version_fields(&struct_item, &field_attrs)?;

    // #231 — cross-attribute validation for `#[derived(...)]`.
    // Runs once the column set is in scope (after the computed-strip
    // pass and validate_through_model_shape, but before any token
    // emission). Checks:
    // - `pk = None` is incompatible with any derived attribute
    // (E_DJG_VDF_015).
    // - Per-scope column collisions between derived `name` and
    // exposed model columns (E_DJG_VDF_002).
    // - Per-scope collisions between two derived attributes sharing
    // a `name` in an overlapping scope (E_DJG_VDF_003).
    // - no derived entry may target a scope that also carries a
    // relation-form embed (`expose(scope -> Peer)`) until the
    // relation projector can render derived expressions
    // (E_DJG_VDF_010).
    // The column-exposure list is materialised from the
    // `FieldAttrs::expose` shape walked alongside the struct's named
    // fields. Suppressed exposures (`expose(none)` / `expose(internal)`)
    // contribute nothing because the column never reaches the visage.
    if !derived_attrs.is_empty() {
        let mut column_exposures: Vec<(String, Vec<&'static str>)> = Vec::new();
        let mut relation_form_exposures: Vec<(String, Vec<&'static str>)> = Vec::new();
        for (field, fa) in struct_item.fields.iter().zip(field_attrs.iter()) {
            let Some(ident) = field.ident.as_ref() else {
                continue;
            };
            if fa.expose.suppressed {
                continue;
            }
            let col = crate::syn_util::column_name_from_ident(ident);
            let mut scopes: Vec<&'static str> = Vec::new();
            for s in &fa.expose.scalar_scopes {
                if let Some(canon) = match s.as_str() {
                    "public" => Some("public"),
                    "self_view" => Some("self_view"),
                    "admin" => Some("admin"),
                    "export" => Some("export"),
                    _ => None,
                } && !scopes.contains(&canon)
                {
                    scopes.push(canon);
                }
            }
            for s in fa.expose.relation_scopes.keys() {
                if let Some(canon) = match s.as_str() {
                    "public" => Some("public"),
                    "self_view" => Some("self_view"),
                    "admin" => Some("admin"),
                    "export" => Some("export"),
                    _ => None,
                } && !scopes.contains(&canon)
                {
                    scopes.push(canon);
                }
            }
            column_exposures.push((col.clone(), scopes));

            let mut relation_scopes: Vec<&'static str> = Vec::new();
            for s in fa.expose.relation_scopes.keys() {
                if let Some(canon) = match s.as_str() {
                    "public" => Some("public"),
                    "self_view" => Some("self_view"),
                    "admin" => Some("admin"),
                    "export" => Some("export"),
                    _ => None,
                } && !relation_scopes.contains(&canon)
                {
                    relation_scopes.push(canon);
                }
            }
            if !relation_scopes.is_empty() {
                relation_form_exposures.push((col, relation_scopes));
            }
        }
        derived::cross_check(
            &derived_attrs,
            &column_exposures,
            &relation_form_exposures,
            matches!(model_attrs.pk, attrs::PkStrategy::None),
        )?;
    }

    // Strip the struct-level `#[derived(...)]` attributes from the
    // re-emitted struct. rustc does not recognise `derived` as a
    // helper attribute on the `#[model]` ATTRIBUTE macro (helper
    // attributes only live on `#[derive(...)]`); leaving the
    // attribute on the surviving struct triggers an "unknown
    // attribute" diagnostic downstream. The `#[derive(Model)]`
    // entry point registers `derived` so rustc accepts it pre-
    // expansion, but the post-expansion struct must shed the
    // attribute before being emitted. The captured semantics live
    // in `derived_attrs`.
    struct_item.attrs.retain(|a| !a.path().is_ident("derived"));

    // Field names become unquoted SQL column names in the emitted
    // `COLUMN_LIST` — reject any that would break `SELECT` /
    // `RETURNING` text. Runs BEFORE `inject::expand` so the error span
    // lands on the user-declared field, not on an injected one, and
    // BEFORE any token emission so a malformed name produces one crisp
    // diagnostic instead of a cascade of downstream failures. See
    // `crate::ident` for the full rule set and rationale.
    crate::ident::validate_field_column_names(&struct_item)?;

    // Strip `#[field(...)]` attributes from user fields. We already captured
    // their semantics into `field_attrs` above; leaving the raw attribute on
    // the struct surface would confuse rustc, which does not recognise
    // `field` as a helper attribute for the `#[model]` attribute macro
    // (helper attributes only exist on `#[derive(...)]` macros — see the
    // `proc_macro_derive(Model, attributes(field))` declaration in lib.rs,
    // which governs a separate no-op `Model` derive). Stripping here keeps
    // the emitted struct valid Rust without forcing users to also write
    // `#[derive(Model)]` solely to legalise the `#[field(...)]` parsing.
    if let syn::Fields::Named(named) = &mut struct_item.fields {
        for field in &mut named.named {
            // 3 — strip `#[computed(...)]` for the same
            // reason `#[field(...)]` is stripped: rustc does not
            // recognise it as a helper attribute on the `#[model]`
            // attribute macro. The semantics were captured into
            // `_computed_attrs` above; the emitter consumes the
            // captured state.
            // #195 — `#[mirjzson(...)]` rides the same strip
            // path. The MirJzSON gate captured the justification above;
            // rustc has no notion of `mirjzson` as a helper attribute on
            // `#[model]` so leaving it in place would produce an
            // `unknown attribute` rustc error instead of the typed
            // gate-violation diagnostic this macro already emits.
            field.attrs.retain(|a| {
                !a.path().is_ident("field")
                    && !a.path().is_ident("computed")
                    && !a.path().is_ident("mirjzson")
            });
        }
    }

    // 1. Inject framework fields (`id`, `created_at`, `updated_at`) and emit
    // the `Default` impl. Returns `syn::Error` for tuple/unit structs and
    // for user fields that collide with reserved framework names.
    let expanded = inject::expand(&mut struct_item, &model_attrs)?;

    // 1b. Hooks opt-in — 3. Emits the `Sealed` + `HasHooks`
    // impl pair when `#[model(hooks)]` is set; otherwise returns an
    // empty `TokenStream` so opt-out models pay zero macro-output
    // overhead. See `model::hooks` for the rationale on why we cannot
    // emit `impl ModelHooks for #ident {}` automatically.
    let hooks_impl = hooks::expand(&struct_item.ident, &model_attrs);

    // 1c. Auditable opt-in — 4. Emits both the
    // `impl ::djogi::Auditable for #ident {... }` trait impl AND
    // the `__djogi_auditable_populate` inherent helper invoked from
    // `Model::create` between `auto_set_tenant` and the user
    // `before_create` hook. Returns an empty `TokenStream` when
    // `#[model(auditable)]` is absent so opt-out models pay zero
    // macro-output overhead.
    // `#[model(auditable)]` supersedes the legacy `#[derive(Auditable)]` per spec line
    // 1037 (locked 2026-05-03). Single attribute drives the
    // trait impl + the populator + the create_body wiring in one
    // expansion — proc macros cannot observe sibling derives, so
    // a derive could not deterministically signal to
    // `#[model(...)]` that the populator should be wired.
    let auditable_impl = crate::compose::auditable::expand(&struct_item.ident, &model_attrs);

    // 1d. SoftDeletable opt-in — 6. Emits the
    // `impl ::djogi::SoftDeletable for #ident {... }` trait impl
    // when `#[model(soft_deletable)]` is set; otherwise returns an
    // empty `TokenStream` so opt-out models pay zero macro-output
    // overhead.
    // `#[model(soft_deletable)]` supersedes the legacy `#[derive(SoftDeletable)]`
    // for the same proc-macros-cannot-observe-sibling-derives constraint
    // that drove the auditable pivot. Both opt-ins now route
    // through `#[model(...)]`. Automatic default-filter
    // composition will need to know the model is soft-deletable AT
    // model-macro expansion time, which a sibling derive cannot
    // signal — doing the migration NOW is cheaper than later.
    let soft_deletable_impl =
        crate::compose::soft_deletable::expand(&struct_item.ident, &model_attrs);

    // 2. FromPgRow — positional ordinal decode against the canonical projection.
    let from_row = from_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 2b. FromJoinedPgRow — sibling decoder that accepts a `prefix` parameter,
    // used by `QuerySet::select_related` to decode both parent (empty
    // prefix) and child (`"rel_{source_column}."`) from a single joined
    // row. Emitted for every model so `.select_related(path)` never fails
    // at compile time for lack of a decoder.
    let from_joined_row = from_joined_row::expand(&struct_item, &model_attrs, &field_attrs);

    // 2c. shared per-field metadata for portable SQL
    // emission. Built once here so both `crud.rs` (the
    // `Model::__djogi_emit_field_predicate` override emission) and
    // `stubs.rs` (the `{Model}Fields` accessor emission and the
    // SQL-only `{Model}SqlFields` view emission) read the same
    // facts. Computing it twice would let column-name / portable-
    // kind classifications drift silently between the two
    // consumers; sharing the vector keeps stubs and crud in
    // lock-step on raw-identifier column stripping, JSONB wrapper
    // detection, `Option<U>` inner-type recovery, `Tracked<>`
    // stripping, and the protected-codec / relation-wrapper
    // short-circuits.
    // `pk = None` models receive an empty vector (matches
    // `crud::expand`'s gate — no `Model` impl, no portable arms).
    let portable_field_info = portable_field_emit::build(&struct_item, &model_attrs, &field_attrs);

    // 3. Model trait impl (CRUD).
    let model_impl = crud::expand(
        &struct_item,
        &model_attrs,
        &field_attrs,
        &portable_field_info,
    );

    // 4. ModelDescriptor + inventory::submit! for migration-differ consumption.
    // `computed_attrs` is threaded through so the emitter can
    // populate `ModelDescriptor.computed_fields` with one
    // `ComputedFieldDescriptor` literal per parsed `#[computed]` field.
    let descriptor = descriptor::expand(&struct_item, &model_attrs, &field_attrs, &computed_attrs);

    // 4b. Cacheable + DeltaSyncCacheable auto-emission — 2.
    // Both arms route through `sassi-codegen` so the surface stays in
    // lock-step with sassi's own `#[derive(Cacheable)]`. The Cacheable
    // arm uses `CacheableFieldsMode::external(...)` so sassi-codegen's
    // `generate_fields_struct` does not run — djogi's own
    // `model::stubs::expand` already owns the `{Model}Fields` name with
    // a different (`DjogiField<Self, V>`) accessor shape. Skipped
    // entirely for `pk = None` models — the cache surface cannot ride
    // the `QuerySet`-driven path without a `Model` impl. See
    // `model::cacheable` for the full rationale + the macro-path-
    // routing contract.
    let cacheable = cacheable::expand(&struct_item, &model_attrs, &field_attrs);

    // 5. {Model}Fields — root ZST with one `DjogiField<Self, V>`
    // accessor per column, plus `{Model}SqlFields` — the SQL-only
    // path-aware sibling view used by relation/visage traversal
    // sites. Reads field types off the shared portable field
    // metadata so `{Model}SqlFields`, `{Model}Fields`, and
    // `crud::expand`'s `__djogi_emit_field_predicate` arms agree on
    // column names and Rust types.
    let stubs = stubs::expand(&struct_item, &model_attrs, &portable_field_info);

    // 6. {Model}Filter — runtime struct carrying `Vec<FilterClause>` with one
    // setter per user field. Separate codegen path from `{Model}Fields`;
    // see `filter::expand`'s module docs for the typed-vs-erased rationale.
    let filter = filter::expand(
        &struct_item,
        &model_attrs,
        &field_attrs,
        &portable_field_info,
    );

    // 7. {Model}Related — typed relation-path constructors. Inspects field
    // types directly via `detect_relation`; emits a ZST with one method
    // per FK / O2O field, consumed by QuerySet `prefetch` / `select_related`.
    let related = relations::expand(&struct_item);

    // 8. {Model}OuterRef — typed outer-scope column references for correlated
    // subqueries. Same shape as `{Model}Fields` but the accessors are
    // associated functions (no receiver) returning `OuterRef<Self, V>`
    // instead of `FieldRef<Self, V>`. Consumed by `Subquery::new` /
    // `Exists::new` when building EXISTS / scalar-subquery predicates.
    let outer = outer_ref::expand(&struct_item, &model_attrs);

    // 9. Visage structs + conversion impls. Emits `{Model}Public` / `SelfView`
    // / `Admin` / `Export` plus scalar `From<&Self>` and relation-nesting
    // `TryFrom<&Self>` impls. Reads `FieldAttrs.expose` for scope
    // membership; framework columns default into every visage.
    // #231 — `derived_attrs` carries one entry per parsed
    // `#[derived(...)]` attribute. Each scope's visage projection
    // list grows by one entry per derived attribute whose
    // `scopes = [...]` includes that scope; entries surface as
    // visage struct fields, alias-bearing SELECT projection
    // entries, positional FromPgRow decode arms, and From/TryFrom
    // init blocks per the spec.
    let projections_ts = visages::expand(&struct_item, &model_attrs, &field_attrs, &derived_attrs);

    // 10. Advisory warnings — emit a `#[deprecated]` const for each field
    // that carries `outbox = "ignore"` without a matching `rationale`.
    // Stable Rust does not expose `proc_macro::Diagnostic` at warn level;
    // the idiomatic stable trick is emitting a deprecated const and
    // immediately referencing it so the deprecated-use lint fires at the
    // reference site, surfacing as a build-time warning.
    let rationale_advisories = emit_rationale_advisories(
        &struct_item,
        // field_attrs was collected before inject::expand mutated
        // struct_item, so indices align with user-declared fields.
        &field_attrs,
    );

    // `emit_rust_getters` is now a no-op (returns
    // an empty token stream regardless of input). The earlier shape
    // emitted one inherent `pub fn <field>(&self) -> <T> {
    // unimplemented!() }` stub per `#[computed(sql =...)]` field on
    // the premise that Rust would prefer a hand-written impl over the
    // stub — but Rust does not allow two inherent methods with the
    // same name on the same type (E0201). The wiring point is kept so
    // a future task that adds a non-conflicting Rust-side surface
    // does not have to re-thread the orchestrator. Adopters who need
    // a Rust-side computation today write a plain inherent method
    // with any name they choose; the SQL-side path through
    // `Vehicle::computed()` covers the server-side cases.
    let computed_getters = computed::emit_rust_getters(&struct_item.ident, &computed_attrs);

    // 5 — `{Model}Computed` ZST + accessor methods +
    // `Vehicle::computed()` inherent constructor.
    // Adopters access computed fields via `Vehicle::computed()
    //.total_price()` returning `Expr<f64>`, suitable for use in
    // `.annotate()`, `.filter_expr()`, `.order_by()`. The ZST is
    // independent of `{Model}Fields` for v0.1.0 simplicity (see the
    // module-level comment in `model::computed` for the bundling
    // tradeoff with plan §7 #10).
    // Empty token stream when no computed fields are declared.
    let computed_zst = computed::emit_computed_zst(&struct_item.ident, &computed_attrs);

    // 11. 1 — `{MODEL}_SCHEMA: &str` agent-ergonomic
    // pretty-printed schema constant. Compile-time, byte-deterministic
    // against the post-injection struct + parsed attrs. See
    // `model::schema_const` for the rendering contract.
    let schema_const = schema_const::emit(&struct_item, &model_attrs, &field_attrs);

    Ok(quote::quote! {
     #expanded
     #hooks_impl
     #auditable_impl
     #soft_deletable_impl
     #from_row
     #from_joined_row
     #model_impl
     #descriptor
     #cacheable
     #stubs
     #filter
     #related
     #outer
     #projections_ts
     #rationale_advisories
     #computed_getters
     #computed_zst
     #schema_const
    })
}

/// Emit advisory deprecation warnings for fields that carry behaviour-modifying
/// attributes without a `rationale = "..."` annotation.
/// # Mechanism
/// Stable Rust does not expose `proc_macro::Diagnostic` at warn level. The
/// idiomatic stable-Rust approach is emitting a `#[deprecated]` const that is
/// immediately referenced via a second `const _: () =...;` expression. The
/// compiler fires the deprecated-use lint at the reference site, which surfaces
/// as a warning in the user's build output without preventing compilation.
/// # Trigger today: `outbox = "ignore"` without `rationale`
/// When `#[field(outbox = "ignore")]` is present on a field but no
/// `#[field(rationale = "...")]` accompanies it, an advisory warning fires.
/// The message prompts the user to document why the field is excluded from the
/// outbox payload (e.g. PII, derived data).
/// # Deferred triggers: `lazy`, `partition_by`
/// `lazy` is allowlisted in `VALID_FIELD_KEYS` today but has no struct field
/// and no runtime behaviour. `partition_by` has no attribute-level parsing at
/// all (only `ModelDescriptor::partition_by` exists, always `None`). Adding
/// advisory-only parsers for those keys before the attributes become functional
/// would be cart-before-horse — their advisories land in the same phase the
/// attributes become real features. (in the implementation
/// plan.)
/// # Const naming
/// The emitted const name encodes both the struct name and the field name so
/// the same advisory key can appear on multiple fields across multiple models
/// in the same compilation unit without a const-naming collision. The name uses
/// only ASCII alphanumerics and underscores, so it is always a valid Rust
/// identifier — no byte-level escaping is needed beyond what `format_ident!`
/// provides.
fn emit_rationale_advisories(
    struct_item: &ItemStruct,
    field_attrs: &[attrs::FieldAttrs],
) -> TokenStream {
    let mut ts = TokenStream::new();
    let struct_name = &struct_item.ident;

    let Fields::Named(_) = &struct_item.fields else {
        // Tuple / unit structs are rejected upstream; nothing to do here.
        return ts;
    };

    for fa in field_attrs {
        // Only fire when `outbox = "ignore"` is present AND `rationale` is absent.
        if fa.outbox.as_deref() != Some("ignore") || fa.rationale.is_some() {
            continue;
        }

        // Recover the field identifier. `FieldAttrs::ident` is always
        // `Some(_)` for named-field structs (darling's magic field populates
        // it from `syn::Field::ident`); tuple structs are rejected upstream.
        let Some(field_ident) = &fa.ident else {
            continue;
        };

        // Build a unique, stable const name from struct + field.
        // Shape: `__djogi_rationale_outbox_{StructName}_{field_name}`
        // ASCII alphanumeric + underscore only — always a valid Rust ident.
        let const_ident = format_ident!("__djogi_rationale_outbox_{}_{}", struct_name, field_ident);

        let deprecation_msg = format!(
            "field `{field_ident}` on `{struct_name}` uses \
    `#[field(outbox = \"ignore\")]` without a `rationale`. \
    Consider adding `#[field(outbox = \"ignore\", rationale = \"...\")]` \
    to document why this field is excluded from the outbox payload \
    (e.g. PII, derived data, ephemeral value)."
        );

        // Emit the deprecated const and an immediate reference so the lint fires.
        ts.extend(quote::quote! {
         #[deprecated(note = #deprecation_msg)]
         #[allow(non_upper_case_globals)]
         const #const_ident: () = ();
         const _: () = #const_ident;
        });
    }

    ts
}

/// Validate `#[field(version)]` annotations across all user fields.
/// Rules enforced here (after `FieldAttrs::parse` accepts the bare `version`
/// flag permissively):
/// 1. At most one field per model may carry `#[field(version)]`.
/// A second occurrence produces a span-precise compile error at the
/// second field.
/// 2. The annotated field's type must be exactly `i32` or `i64`. Accepted
/// spellings:
/// - bare `i32` / `i64` (single-segment path);
/// - `std::primitive::i32` / `std::primitive::i64`;
/// - `core::primitive::i32` / `core::primitive::i64`.
/// Any other multi-segment path — including user-defined module aliases
/// like `my_mod::i32` — is rejected at macro-expansion time so a
/// misleadingly named type alias cannot silently satisfy the contract.
/// `Option<i32>` (last segment `Option`) is likewise rejected.
/// Type resolution is unavailable at macro-expansion time. The validator
/// therefore accepts a small, explicit allowlist of qualified primitive
/// spellings plus the bare form. A user who writes `type i32 = String;`
/// in scope of the annotated field can still fool the check; this is the
/// inherent ceiling of syntactic detection and matches the limitation of
/// every other macro in the ecosystem that inspects field types.
fn validate_version_fields(
    struct_item: &ItemStruct,
    field_attrs: &[attrs::FieldAttrs],
) -> syn::Result<()> {
    let mut first_version_idx: Option<usize> = None;

    for (i, (field, fa)) in struct_item
        .fields
        .iter()
        .zip(field_attrs.iter())
        .enumerate()
    {
        if !fa.version {
            continue;
        }

        // Duplicate check — if we already saw a version field, reject this one.
        if first_version_idx.is_some() {
            return Err(syn::Error::new_spanned(
                field,
                "duplicate #[field(version)]: at most one version field is allowed per model",
            ));
        }

        // Type check — accept bare `i32`/`i64` or explicit qualified
        // spellings via `std::primitive` / `core::primitive`.
        // Other multi-segment paths (including `my_mod::i32`) are rejected.
        let is_valid_type = if let syn::Type::Path(syn::TypePath {
            path, qself: None, ..
        }) = &field.ty
        {
            is_version_primitive_path(path)
        } else {
            false
        };

        if !is_valid_type {
            let ty = &field.ty;
            let type_str = quote::quote!(#ty).to_string().replace(' ', "");
            return Err(syn::Error::new_spanned(
                field,
                format!(
                    "#[field(version)] must be i32 or i64 (got {type_str}); \
      accepted spellings: bare `i32`/`i64`, \
      `std::primitive::i32`/`i64`, `core::primitive::i32`/`i64`"
                ),
            ));
        }

        first_version_idx = Some(i);
    }

    Ok(())
}

/// Returns `true` when `path` is one of the accepted version-field type
/// spellings. Accepts only:
/// - single-segment `i32` / `i64`
/// - `std::primitive::i32` / `std::primitive::i64`
/// - `core::primitive::i32` / `core::primitive::i64`
/// Every other shape returns `false`, including `my_mod::i32` and other
/// user-module paths that happen to end in `i32` / `i64`.
fn is_version_primitive_path(path: &syn::Path) -> bool {
    // Reject any segment that carries angle-bracketed or parenthesized args
    // (e.g. a typo like `i32<()>`). Version fields must be exactly the
    // bare primitive type.
    if path
        .segments
        .iter()
        .any(|seg| !matches!(seg.arguments, syn::PathArguments::None))
    {
        return false;
    }

    let segs = &path.segments;
    match segs.len() {
        1 => segs[0].ident == "i32" || segs[0].ident == "i64",
        3 => {
            (segs[0].ident == "std" || segs[0].ident == "core")
                && segs[1].ident == "primitive"
                && (segs[2].ident == "i32" || segs[2].ident == "i64")
        }
        _ => false,
    }
}

/// `#[model(..., through)]` marks a many-to-many junction model and must
/// therefore carry at least two `ForeignKey<T>` columns.
/// The relation macros depend on one FK back to each side of the relation.
/// Treating `through` as a pure marker would let obviously-invalid junction
/// structs compile and only fail much later when macros tried to use
/// them. pins this earlier with a compile-fail fixture.
fn validate_through_model_shape(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
) -> syn::Result<()> {
    if !model_attrs.through {
        return Ok(());
    }

    let fk_count = match &struct_item.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .filter(|field| {
                matches!(
                    attrs::detect_relation(&field.ty),
                    Some(attrs::RelationInfo {
                        kind: attrs::RelationKind::ForeignKey,
                        ..
                    })
                )
            })
            .count(),
        Fields::Unnamed(_) | Fields::Unit => 0,
    };

    if fk_count >= 2 {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(
            &struct_item.ident,
            "a `#[model(through)]` struct must declare at least two `ForeignKey<T>` fields",
        ))
    }
}
