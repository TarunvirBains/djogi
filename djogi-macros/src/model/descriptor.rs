//! Generates `inventory::submit!(ModelDescriptor {...})` from the `#[model]`
//! struct definition. Runs AFTER `inject::expand` has mutated `struct_item`.
//! `ModelDescriptor::fields` is the **complete** schema contract.
//! Framework-injected columns (`id`, `created_at`, `updated_at`) are emitted
//! first (in injection order), followed by user-declared fields in source
//! order. Downstream consumers (migration differ, admin UI, `djogi docs`, RLS
//! generator) iterate `descriptor.fields` as the single schema source and
//! never synthesize framework columns out-of-band.
//! For `pk = None`, `id` is omitted from the framework prefix (the user's
//! own PK field appears as a regular user field in declared order).
//! The emitted submission uses defaults for every amended field
//! (partition_by, has_outbox, idempotency_key, tenant_key, cache_ttl,
//! rationale, indexes) — those attrs are populated by later phases' parser
//! extensions. Per-field defaults (rationale, outbox_exclude, index_type)
//! follow the same convention.

use crate::model::attrs::{
    FieldAttrs, FieldSqlTypeCategory, FtsSpec, ModelAttrs, PkStrategy,
    RelationKind as MacroRelationKind, detect_relation, field_sql_type_category,
    on_delete_str_to_tokens, rust_type_to_sql, unwrap_option, unwrap_schema_type,
};
use crate::model::sql_bind::rust_source_type_tokens_for_type;
use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

/// Emits a `FieldDescriptor {... }` literal for a framework-injected
/// column (`id`, `created_at`, `updated_at`).
/// Framework fields share every descriptor knob except `name`,
/// `sql_type`, and the `unique` bit: PK columns are unique,
/// timestamp columns are neither. The visage map always projects the
/// column under its own name across the four built-in scopes.
/// ## Why `indexed: false` for the PK column
/// Postgres creates an implicit unique BTree index for every `PRIMARY
/// KEY` constraint — a second explicit BTree index on the same `id`
/// column would be structurally redundant. Setting `indexed: true`
/// here causes the projection's field-level index fanout loop to
/// synthesise `<table>_id_idx` for every model, emitting a
/// `CREATE INDEX` that Postgres silently accepts but never uses for
/// query planning (the PRIMARY KEY index is always preferred). The
/// slot stays `false` on all four non-`None` PK strategies;
/// `unique: #pk` carries the correct uniqueness signal that the
/// snapshot comparer reads.
/// Relation metadata is unconditionally `None` — `id` is a primary
/// key, not a foreign key, and `created_at` / `updated_at` are
/// scalars; the visage hookup attaches per-user-field only.
/// Centralising the emission means future descriptor field additions
/// land in one place rather than rippling through every PK-strategy
/// arm.
fn framework_field_descriptor(
    name: &str,
    sql_type_tokens: TokenStream,
    pk: bool,
    strict_id_check: bool,
) -> TokenStream {
    quote! {
     ::djogi::FieldDescriptor {
      name: #name,
      sql_type: #sql_type_tokens,
      nullable: false,
      unique: #pk,
      // The PRIMARY KEY constraint's implicit BTree index already covers
      // the `id` column — `indexed: true` would cause the projection's
      // field-level index fanout to synthesise a redundant
      // `<table>_id_idx` on every model. Always `false` here;
      // `unique: #pk` is the correct uniqueness signal for the snapshot.
      indexed: false,
      max_length: None,
      renamed_from: None,
      rationale: None,
      outbox_exclude: false,
      sequence_within: None,
      index_type: None,
      relation_kind: None,
      on_delete: None,
      target_type_name: None,
      // Framework-injected columns (`id`, `created_at`,
      // `updated_at`) are never relation fields, so the
      // self-FK flag is always `false`.
      is_self_fk: false,
      visage_map: &[
       ("admin", #name),
       ("export", #name),
       ("public", #name),
       ("self_view", #name),
      ],
      protected: ::std::option::Option::None,
      default_volatility_override: ::std::option::Option::None,
      generated: ::std::option::Option::None,
      // 5 — framework-injected columns are never
      // contributed by a composition derive.
      composed_via: ::std::option::Option::None,
      // Framework-injected columns use identity-mapped types
      // (HeerId → BIGINT, DateTime → TIMESTAMPTZ, etc.); no shim.
      rust_source_type: ::std::option::Option::None,
      // Framework-injected columns carry no adopter
      // `#[field(check = "...")]`; the slot is always `None`.
      check_sql: ::std::option::Option::None,
      // Djogi#217) — framework-injected
      // columns carry no adopter `#[field(comment)]`; the slot
      // is always `None`. Adopters who want descriptive labels
      // on the framework columns can read from the descriptor's
      // `rationale` slot instead.
      comment: ::std::option::Option::None,
      // Djogi#189 — propagated from `#[model(strict_ids)]`
      // for the `id` column. Strict ID dispatch matches on the
      // HeerRanjID semantic family (HeerId / HeerIdDesc / RanjId /
      // RanjIdDesc) derived from the parent model's PkType; the
      // resolved SQL column type is not consulted for this decision.
      // Always `false` for `created_at` / `updated_at`.
      strict_id_check: #strict_id_check,
      // Djogi#220 — framework-injected columns
      // (`id`, `created_at`, `updated_at`) carry no adopter
      // `#[field(type_change_using)]`. Their SQL types are fixed
      // by the framework and never participate in adopter-driven
      // type-change migrations.
      type_change_using: ::std::option::Option::None,
     }
    }
}

pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
    computed_attrs: &[(syn::Ident, crate::model::computed::ComputedAttr)],
) -> TokenStream {
    match try_expand(struct_item, model_attrs, field_attrs, computed_attrs) {
        Ok(tokens) => tokens,
        Err(err) => err.to_compile_error(),
    }
}

/// Inner emission entry point — returns `syn::Result` so the new
/// `#[model(tree_edge = "...")]` validation can surface a
/// span-precise compile error pointing at the offending literal.
/// Previously the descriptor emitter was infallible, but `tree_edge`
/// requires cross-checking the named field against the struct's
/// declared user fields and their detected relation shape — a
/// validation that can fail when the column does not exist or is
/// not a self-FK. Routing the entire emitter through `Result` keeps
/// the error path unified and lets later attribute additions reuse
/// the same fallible shape without another wrapper layer.
fn try_expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
    // 5 — `#[computed(sql = "...")]` field metadata.
    // Populated from captured attributes by emitting one
    // ComputedFieldDescriptor literal per entry into the
    // inventory::submit! body. Empty slice when no computed fields
    // are declared.
    computed_attrs: &[(syn::Ident, crate::model::computed::ComputedAttr)],
) -> syn::Result<TokenStream> {
    let source_ident = &struct_item.ident;
    let source_name_string = source_ident.to_string();
    let type_name = source_name_string.clone();
    let table_name = &model_attrs.table;

    // Exhaustive over `PkStrategy`. `Custom(path)` reads the user type's
    // `KIND` associated-const at registration time so the descriptor
    // snapshot carries the full `CustomPrimaryKeyKind { type_name, sql_type,
    // default_sql }` the `djogi::primary_key!` macro emits — no separate
    // translation from Rust path to discriminant lives here.
    let pk_type_tokens = match &model_attrs.pk {
        PkStrategy::HeerId => quote! { ::djogi::PkType::HeerId },
        PkStrategy::RanjId => quote! { ::djogi::PkType::RanjId },
        PkStrategy::HeerIdDesc => quote! { ::djogi::PkType::HeerIdDesc },
        PkStrategy::RanjIdDesc => quote! { ::djogi::PkType::RanjIdDesc },
        PkStrategy::Serial => quote! { ::djogi::PkType::Serial },
        PkStrategy::None => quote! { ::djogi::PkType::None },
        PkStrategy::Custom(path) => quote! {
         <#path as ::djogi::primary_key::PrimaryKey>::KIND
        },
    };

    // field_attrs was collected BEFORE injection (user fields only, 0-indexed).
    // struct_item.fields now has framework fields at the front — skip them so
    // zip() aligns field_attrs[0] with the first USER field, not id.
    let n_framework = match &model_attrs.pk {
        PkStrategy::None => 2, // created_at, updated_at only
        _ => 3,                // id, created_at, updated_at
    };
    let user_fields: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    // ── Self-FK metadata ─────────────────────
    // For each user field that resolves to a `ForeignKey<T>` /
    // `OneToOneField<T>` (or its nullable form), compare the detected
    // target's last-segment ident to the source struct's short name.
    // A match marks the field as a *self-FK* — an edge from the model
    // to itself — which 's recursive-query
    // builder uses to validate `RelationPath<T, T>` and to disambiguate
    // multi-edge tree models.
    // The check is name-based on purpose: the descriptor's
    // `target_type_name` is also the short ident, and the
    // migration differ already matches relations through that string.
    // Re-using the same heuristic keeps every descriptor consumer on
    // the same lookup key.
    // `#[model(tree_edge = "...")]` validation reads this set to confirm
    // the named column is a self-FK before emitting the
    // descriptor's `tree_edge` slot.
    let self_fk_field_names: std::collections::BTreeSet<String> = user_fields
        .iter()
        .filter_map(|(field, _fa)| {
            let info = detect_relation(&field.ty)?;
            (info.target_name == source_name_string)
                .then(|| crate::syn_util::column_name_from_field(field))
        })
        .collect();

    // ── #[model(tree_edge = "...")] validation ────────────────────────
    // The named column must exist on the user's struct AND must be a
    // self-FK per the set computed above. Any mismatch surfaces a
    // span-precise compile error pointing at the literal so the
    // underline isolates the offender.
    if let Some(lit) = &model_attrs.tree_edge {
        let edge_name = lit.value();
        let user_field_names: std::collections::BTreeSet<String> = user_fields
            .iter()
            .map(|(field, _)| crate::syn_util::column_name_from_field(field))
            .collect();
        if !user_field_names.contains(&edge_name) {
            return Err(syn::Error::new_spanned(
                lit,
                format!(
                    "`tree_edge = \"{edge_name}\"` does not match any field on `{source_name_string}`; \
      declare a `ForeignKey<{source_name_string}>` (or its `Option<…>` form) field \
      with that name first",
                ),
            ));
        }
        if !self_fk_field_names.contains(&edge_name) {
            return Err(syn::Error::new_spanned(
                lit,
                format!(
                    "`tree_edge = \"{edge_name}\"` must name a self-FK field; the column exists \
      on `{source_name_string}` but its target type is not `{source_name_string}`. \
      Tree-recursive queries walk a self-referential parent edge — point \
      `tree_edge` at a `ForeignKey<{source_name_string}>` or \
      `Option<ForeignKey<{source_name_string}>>` field on the same struct",
                ),
            ));
        }
    }

    // ── Framework-field FieldDescriptors ─────────────────────────────────────
    // Framework columns are emitted FIRST so `descriptor.fields` is
    // the complete schema contract. `id` varies by pk strategy; `created_at`
    // and `updated_at` are always Timestamptz, non-null, not unique/indexed.
    // For `pk = None`, skip `id` entirely — the user's own PK appears as a
    // regular user field in declared order.

    // HeerIdDesc / RanjIdDesc share the same stored column type as their
    // ascending siblings (BIGINT / UUID). The PK-type flip lives on
    // `ModelDescriptor::pk_type` and is consumed by the migration
    // differ, not here.
    // Custom PK types delegate the column type through the user type's
    // `PrimaryKey::SQL_TYPE` associated const; `FieldSqlType::Custom`
    // stores it verbatim so the migration differ can compare by string
    // equality.
    // propagate `#[model(strict_ids)]` to the framework `id`
    // column ONLY when the PK strategy uses a built-in HeerId / RanjId
    // family carrier. The projection layer reads the descriptor's
    // `strict_id_check` flag alongside the parent PK's semantic family
    // (`strict_id_family_of_pk`); Serial / None / Custom PKs all map to
    // `StrictIdFamily::None` there, but setting the descriptor flag on
    // those carriers would be a misleading signal at the descriptor
    // surface (downstream consumers walking `descriptor.fields[0].strict_id_check`
    // would read `true` for a `PkType::Custom { sql_type: "BIGINT" }`
    // model whose ID semantics carry no HeerRanjID invariant). The
    // descriptor stays honest: the flag is `true` only when the
    // structural CHECK applies.
    let id_strict_id_check = model_attrs.strict_ids
        && matches!(
            &model_attrs.pk,
            PkStrategy::HeerId
                | PkStrategy::HeerIdDesc
                | PkStrategy::RanjId
                | PkStrategy::RanjIdDesc
        );
    let id_framework_desc: Option<TokenStream> = match &model_attrs.pk {
        PkStrategy::HeerId | PkStrategy::HeerIdDesc => Some(framework_field_descriptor(
            "id",
            quote! { ::djogi::FieldSqlType::BigInt },
            true,
            id_strict_id_check,
        )),
        PkStrategy::RanjId | PkStrategy::RanjIdDesc => Some(framework_field_descriptor(
            "id",
            quote! { ::djogi::FieldSqlType::Uuid },
            true,
            id_strict_id_check,
        )),
        PkStrategy::Serial => Some(framework_field_descriptor(
            "id",
            quote! { ::djogi::FieldSqlType::Integer },
            true,
            // Serial PKs receive no strict-ID CHECK regardless of
            // `#[model(strict_ids)]` — INTEGER columns have no
            // HeerRanjID bit-layout invariant to enforce.
            false,
        )),
        PkStrategy::None => None,
        PkStrategy::Custom(path) => Some(framework_field_descriptor(
            "id",
            quote! {
             ::djogi::FieldSqlType::Custom(
              <#path as ::djogi::primary_key::PrimaryKey>::SQL_TYPE,
             )
            },
            true,
            // (post-review hardening) — Custom PK types are
            // NEVER candidates for the HeerId / RanjId structural CHECK,
            // regardless of `#[model(strict_ids)]`. The Custom carrier
            // may share its SQL type with HeerId / RanjId (`"BIGINT"` /
            // `"UUID"`), but the column is not a HeerRanjID identifier:
            // its bit layout is defined by the adopter's `PrimaryKey`
            // impl, not by HeeRanjID. Emitting `col >= 0` against a
            // Custom BIGINT PK would constrain the adopter's value
            // domain at the DB layer without their consent; emitting
            // the UUIDv8 + RFC 4122 CHECK against a Custom UUID PK
            // would reject every valid UUIDv4 the adopter inserts.
            // Adopters whose Custom PK happens to share HeerRanjID's
            // bit layout (e.g., a thin newtype around `HeerId`) and
            // who genuinely want the structural CHECK should declare
            // it explicitly via `#[field(check = "<predicate>")]`
            // the typed-and-explicit path is preferred over inferring
            // the family from a coincidental SQL-carrier match.
            false,
        )),
    };

    let created_at_desc = framework_field_descriptor(
        "created_at",
        quote! { ::djogi::FieldSqlType::Timestamptz },
        false,
        // Timestamp columns are never HeerId / RanjId carriers.
        false,
    );
    let updated_at_desc = framework_field_descriptor(
        "updated_at",
        quote! { ::djogi::FieldSqlType::Timestamptz },
        false,
        false,
    );

    // ── User-field FieldDescriptors ───────────────────────────────────────────

    let user_field_descriptors: Vec<TokenStream> = user_fields
 .iter()
 .map(|(field, fa)| {
   let name = crate::syn_util::column_name_from_field(field);
   // `#[field(outbox = "ignore")]` marks the
   // column as excluded from the transactional outbox payload.
   // `FieldAttrs::parse` has already validated the string value,
   // so a non-`None` `outbox` attr always means "ignore" here.
   let outbox_exclude = fa.outbox.as_deref() == Some("ignore");
   // `#[field(sequence_within = "col")]`
   // scopes a monotonic sequence to a parent FK column. The
   // macro runtime uses this descriptor slot to detect the
   // scoped-sequence field in `Model::create`.
   let sequence_within_tokens = match &fa.sequence_within {
    Some(col) => quote! { ::std::option::Option::Some(#col) },
    None => quote! { ::std::option::Option::None },
   };

   // Detect FK / O2O relation shape before the generic scalar
   // `unwrap_option` strip — `detect_relation` itself handles the
   // `Option<…>` layer so nullable FK fields are recognized.
   let relation = detect_relation(&field.ty);

   let (inner_ty, nullable) = unwrap_schema_type(&field.ty);

   // For relation fields the SQL column type is the target's PK
   // type, not the Rust wrapper type. the migration emitter
   // consumes `sql_type` alongside `target_type_name` to produce
   // `REFERENCES` clauses; uses the `target_type_name`
   // as the primary signal and leaves `sql_type` as the
   // best-effort scalar mapping. A future amendment
   // can extend this to look the target PK type up via a second
   // `ModelDescriptor` pass.
   // Djogi#216 Piece A — `#[field(domain = "<name>")]`
   // intercepts the sql_type selection BEFORE the type-driven
   // mapping runs. The descriptor's `FieldSqlType::Domain {
   // name, base }` variant carries the bare domain name (the
   // Display contract — rendered into column DDL by the
   // migration composer) and the inferred Rust-side base type
   // for documentation / future Piece B use. The relation
   // arm above wins over the domain arm because `attrs.parse`
   // already rejects `domain` on relation fields, so the
   // ordering here is defense-in-depth: an FK field with
   // `domain` set could not reach this point, but the
   // explicit `relation.is_some()` branch keeps the FK SQL
   // type stable even if the parse-time guard ever regresses.
   // `base: &<base_tokens>` relies on Rust's constant-promotion
   // rule: a `&EXPR` where EXPR is const-evaluable promotes to
   // `&'static T`. Every `FieldSqlType` variant the macro
   // emits — `Text`, `Numeric`, `BigInt`, `Custom(<T as
   // DjogiSqlType>::SQL_TYPE)`, etc. — is const-evaluable, so
   // the reference promotes cleanly into the descriptor's
   // static-lifetime slot. `&` (not `Box::new(...)`) keeps the
   // enclosing enum trivially droppable, which the existing
   // `pub const fn field_descriptor` constructor and every
   // static / const `FieldDescriptor` literal in the test
   // suite depend on.
   let sql_type = if relation.is_some() {
    // FK columns use the target's PK SQL type, resolved at
    // projection time. Emit TEXT as a placeholder here;
    // `migrate::projection` overwrites it with the actual
    // target PK type when it has access to all descriptors.
    sql_str_to_tokens("TEXT")
   } else if fa
   .protected
   .as_ref()
   .and_then(|p| p.codec.as_deref())
   .is_some()
   {
    // A field with an at-rest codec stores its ciphertext as
    // BYTEA — the codec's Encoded type is `Vec<u8>` regardless of
    // the decoded Rust type (e.g. an encrypted `String` column is
    // BYTEA, not VARCHAR/TEXT). The codec ID is validated against
    // KNOWN_CODEC_IDS at parse time; every shipped codec uses
    // BYTEA storage. Ordered AFTER the FK arm so a codec on an FK
    // field cannot override the FK's target-PK type — FK + codec
    // is not a supported combination and the FK arm wins safely.
    sql_str_to_tokens("BYTEA")
   } else if let Some(domain_name) = &fa.domain {
    let domain_name = domain_name.as_str();
    let base = field_sql_type_tokens(&inner_ty);
    quote! {
     ::djogi::FieldSqlType::Domain {
      name: #domain_name,
      base: &#base,
     }
    }
   } else if let Some(n) = fa.max_length {
    // `#[field(max_length = N)]` on a `String` field emits
    // `VARCHAR(N)` instead of `TEXT`. The attribute is now
    // validated at macro-expansion time, so non-`String` fields
    // cannot reach this branch with `max_length` set.
    match rust_type_to_sql(&inner_ty) {
     Some("TEXT") => quote! { ::djogi::FieldSqlType::Varchar(#n) },
     _ => field_sql_type_tokens(&inner_ty),
    }
   } else {
    field_sql_type_tokens(&inner_ty)
   };
   let unique = fa.unique;
   let indexed = fa.index || fa.index_method.is_some();
   let max_length = match fa.max_length {
    Some(n) => quote! { Some(#n) },
    None => quote! { None },
   };
   let renamed_from = match &fa.renamed_from {
    Some(s) => quote! { Some(#s) },
    None => quote! { None },
   };

   // `#[field(index)]` or `#[field(index = "method")]`.
   // Three cases:
   // 1. fa.index = true, index_method = None → bare `#[field(index)]`, apply auto-default
   // 2. fa.index_method = Some(method) → explicit method (may or may not have bare index)
   // 3. Neither → no index
   let index_type_tokens = if let Some(method) = &fa.index_method {
    // Explicit method string. The method was already validated
    // in FieldAttrs::parse, so this should not fail. Re-parse to
    // get the token stream for emission.
    // In theory we could cache the tokens in FieldAttrs, but the
    // two-pass pattern (validate in attrs, emit in descriptor)
    // mirrors on_delete and keeps spans recoverable if needed.
    match method.as_str() {
     "btree" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::BTree) },
     "gin" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gin) },
     "gist" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gist) },
     "brin" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Brin) },
     "hash" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Hash) },
     "spgist" => quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Spgist) },
     _ => quote! { ::std::option::Option::None }, // Unreachable — already validated.
    }
   } else if fa.index {
    // Bare `#[field(index)]` — apply auto-default.
    default_index_type(&field.ty)
   } else {
    quote! { ::std::option::Option::None }
   };

   // populate visage_map from the parsed
   // `#[field(expose(...))]` spec. Scalar scopes map to this
   // column's name; relation scopes map to the peer visage
   // type name. Empty / suppressed specs emit `&[]`.
   let projection_map_tokens = build_projection_map_tokens(&fa.expose, &name);
   // Relation metadata — `None`/`&[]` for scalar columns.
   // Descriptor lookup keys off the short target name (last path
   // segment) — the migration differ matches this against
   // `ModelDescriptor::type_name`, which is also just the short
   // ident — so we deliberately use `info.target_name` here rather
   // than the full `info.target_type`. The full type path is only
   // needed by codegen sites that emit the target in type position
   // (see `relations::expand`).
   let (relation_kind_tokens, on_delete_tokens, target_type_name_tokens, is_self_fk_lit) =
    match &relation {
     Some(info) => {
      let kind_tokens = match info.kind {
       MacroRelationKind::ForeignKey => {
        quote! { Some(::djogi::descriptor::RelationKind::ForeignKey) }
       }
       MacroRelationKind::OneToOne => {
        quote! { Some(::djogi::descriptor::RelationKind::OneToOne) }
       }
      };
      let on_delete = match &fa.on_delete {
       Some(s) => {
        let variant = on_delete_str_to_tokens(s);
        quote! { Some(#variant) }
       }
       None => quote! { None },
      };
      let target_lit = info.target_name.as_str();
      // Name-based self-FK
      // detection. Matches the detector's `target_name`
      // (last-segment ident of the inner type) against the
      // source struct's short name. Same heuristic the
      // migration differ uses to resolve relations across
      // descriptors, so the descriptor consumers stay on a
      // single lookup key.
      let is_self_fk = info.target_name == source_name_string;
      (
       kind_tokens,
       on_delete,
       quote! { Some(#target_lit) },
       is_self_fk,
      )
     }
     None => (quote! { None }, quote! { None }, quote! { None }, false),
    };

   // `#[field(protected(...))]` lowers to
   // `Some(::djogi::ProtectedFieldMetadata {... })`; absent
   // attribute keeps the explicit `None` (distinct from
   // `Sensitivity::None` per the descriptor's contract).
   let protected_tokens = match &fa.protected {
    Some(spec) => spec.to_tokens(),
    None => quote! { ::std::option::Option::None },
   };
   // `#[field(default_volatility = "...")]` override. Already
   // validated in `FieldAttrs::parse`, so a non-`None` value
   // is guaranteed to parse cleanly here.
   let default_volatility_tokens = match fa.default_volatility.as_deref() {
    Some(value) => {
     let lit = crate::model::protected::DefaultVolatilityLit::parse(
      value,
      ::proc_macro2::Span::call_site(),
     )
    .expect("invariant: default_volatility validated in FieldAttrs::parse");
     let path = lit.to_tokens();
     quote! { ::std::option::Option::Some(#path) }
    }
    None => quote! { ::std::option::Option::None },
   };
   // PR 7 — `#[field(generated = "<expr>")]`. The
   // expression is emitted verbatim; `stored: true` is hard-
   // coded because Pg18 supports only stored generated columns
   // (the descriptor's `stored` flag is reserved for future
   // Pg19+ VIRTUAL support, but the macro syntax does not
   // accept an explicit `stored =...` today — see
   // `FieldAttrs::parse`).
   let generated_tokens = match &fa.generated {
    Some(lit_str) => {
     let expr = lit_str.value();
     let expr = expr.as_str();
     quote! {
      ::std::option::Option::Some(::djogi::descriptor::GeneratedColumnSpec {
       expression: #expr,
       stored: true,
      })
     }
    }
    None => quote! { ::std::option::Option::None },
   };

   // 5 — composition-derive provenance.
   // Both composition surfaces are now driven by `#[model(...)]`
   // attributes (pivoted 2026-05-03 / 2026-05-04):
   // - `#[model(auditable)]`: `model_attrs.auditable
   // == true` flips the `created_by` column to
   // `composed_via: Some("Auditable")`.
   // - `#[model(soft_deletable)]` (supersedes the legacy
   // `#[derive(SoftDeletable)]`): `model_attrs.soft_deletable
   // == true` flips the `deleted_at` column to
   // `composed_via: Some("SoftDeletable")`. Tightens the
   // detection from field-name-only to field-name-plus-flag,
   // eliminating the false-positive risk that an adopter who
   // declares a `deleted_at` column without opting into the
   // composition would see the (informational) tag on that
   // column.
   // Order matters: `created_by` checked first so a model that
   // declares both `auditable` and `soft_deletable` tags each
   // column with its own provenance independently.
   // Per spec line 1124, `composed_via` is metadata only — the
   // migration differ does NOT key off it. Reading
   // `composed_via` to decide migration strategy or
   // default-filter composition would re-introduce a different
   // kind of mismatch (a future column-rename override would
   // diverge from the tag); the descriptor doc comment at
   // `djogi/src/descriptor.rs` carries that warning explicitly.
   let composed_via_tokens: TokenStream = if name == "created_by" && model_attrs.auditable
   {
    quote! { ::std::option::Option::Some("Auditable") }
   } else if name == "deleted_at" && model_attrs.soft_deletable {
    quote! { ::std::option::Option::Some("SoftDeletable") }
   } else {
    quote! { ::std::option::Option::None }
   };

   // — source-type discriminator.
   // FK columns always get `None` (their type is the target's PK,
   // which is always identity-width). Relation fields are scalar
   // proxies for the FK column; the source type is irrelevant there.
   let rust_source_type_tokens: TokenStream = if relation.is_some() {
    quote! { ::std::option::Option::None }
   } else {
    rust_source_type_tokens_for_type(&inner_ty)
   };

   // — adopter `#[field(check = "...")]`
   // raw-SQL CHECK expression. The string is already validated as
   // non-empty / non-whitespace by `FieldAttrs::parse`; emit it
   // verbatim into the descriptor so the projection layer can
   // combine it with any type-derived CHECK.
   let check_sql_tokens: TokenStream = match &fa.check {
    Some(expr) => {
     let expr_str = expr.as_str();
     quote! { ::std::option::Option::Some(#expr_str) }
    }
    None => quote! { ::std::option::Option::None },
   };

   // Djogi#217) — adopter
   // `#[field(comment = "<text>")]` column-level free-text
   // comment. Validated as non-empty / non-whitespace-only
   // by `FieldAttrs::parse`; emit verbatim into the
   // descriptor so the migration composer can lower it to
   // `COMMENT ON COLUMN <t>.<c> IS '<text>'`. The composer
   // owns single-quote escaping at SQL-emission time so the
   // descriptor carries the adopter's original prose.
   let comment_tokens: TokenStream = match &fa.comment {
    Some(text) => {
     let text_str = text.as_str();
     quote! { ::std::option::Option::Some(#text_str) }
    }
    None => quote! { ::std::option::Option::None },
   };

   // Djogi#220) — adopter
   // `#[field(type_change_using = "<sql expr>")]` USING clause
   // for non-default-cast column type changes. Validated as
   // non-empty / non-whitespace-only by `FieldAttrs::parse`;
   // emit verbatim into the descriptor so the SQL emitter can
   // append `USING (<expr>)` to `ALTER COLUMN … TYPE` whenever
   // the differ records a `ColumnChange::ChangeType` for this
   // column.
   let type_change_using_tokens: TokenStream = match &fa.type_change_using {
    Some(expr) => {
     let expr_str = expr.as_str();
     quote! { ::std::option::Option::Some(#expr_str) }
    }
    None => quote! { ::std::option::Option::None },
   };

   // Djogi#189 — opt-in HeerId / RanjId structural CHECK.
   // Set `strict_id_check: true` on the descriptor when:
   // 1. `#[field(strict_id_check)]` was declared on this field
   // (already validated as type-compatible by `FieldAttrs::parse`).
   // 2. `#[model(strict_ids)]` is on AND the field is a
   // bare HeerId / RanjId family scalar OR a relation
   // field (`ForeignKey<T>` / `OneToOneField<T>`).
   // For (2), the macro relies on the field's declared Rust type;
   // it does not (and cannot) inspect FK target PK types here.
   // Relation-field propagation is deliberately broad — every FK
   // carries the flag — because the macro cannot reach across
   // crates to discover the target's PK semantic family. The
   // projection layer is the single place that has every
   // descriptor in scope; it resolves each FK target's HeerRanjID
   // family via `type_to_pk_family` and silently skips the CHECK
   // for FK-to-Serial, FK-to-Custom, FK-to-None, and FK-to-Composite
   // targets. The macro propagates; the projection filters.
   // (post-review hardening): the projection filter
   // is family-based, not SQL-type-based, so an FK to a
   // `PkType::Custom { sql_type: "BIGINT" / "UUID",.. }` no
   // longer accidentally inherits the HeerId / RanjId CHECK
   // from a coincidental SQL-carrier match.
   let strict_id_check_lit: bool = fa.strict_id_check
    || (model_attrs.strict_ids
     && (relation.is_some()
      || crate::model::attrs::is_bare_heeranjid_family_type(&field.ty)));

   quote! {
    ::djogi::FieldDescriptor {
     name: #name,
     sql_type: #sql_type,
     nullable: #nullable,
     unique: #unique,
     indexed: #indexed,
     max_length: #max_length,
     renamed_from: #renamed_from,
     rationale: None,
     // `#[field(outbox = "ignore")]` toggles
     // per-column outbox exclusion. The outbox helper walks
     // `descriptor.fields` at emit time and strips any key
     // flagged here from the JSONB payload.
     outbox_exclude: #outbox_exclude,
     sequence_within: #sequence_within_tokens,
     index_type: #index_type_tokens,
     // relation metadata emitted only for FK/O2O
     // columns. Non-relation columns keep `None`/`&[]`.
     relation_kind: #relation_kind_tokens,
     on_delete: #on_delete_tokens,
     target_type_name: #target_type_name_tokens,
     // True when the FK / O2O target is the same model
     // the field belongs to. Always `false` for scalar
     // columns and for relation fields whose target is a
     // different model.
     is_self_fk: #is_self_fk_lit,
     visage_map: #projection_map_tokens,
     protected: #protected_tokens,
     default_volatility_override: #default_volatility_tokens,
     // PR 7 — stored generated column metadata.
     // Lowered from `#[field(generated = "<expr>")]`;
     // `stored: true` is implicit (Pg18 supports only
     // STORED). `None` for non-generated columns.
     generated: #generated_tokens,
     // Composition-derive provenance.
     // `Some("Auditable")` for the `created_by` column on
     // a `#[model(auditable)]` model; `Some("SoftDeletable")`
     // for the `deleted_at` column on a
     // `#[model(soft_deletable)]` model (detection
     // tightened from field-name-only to
     // field-name + attribute opt-in to eliminate the
     // adopter false-positive risk); `None` otherwise.
     composed_via: #composed_via_tokens,
     // — bind/decode
     // source-type discriminator. `Some(RustSourceType::*)`
     // for i8/u8/u16/u32/u64; `None` for direct-mapped types.
     rust_source_type: #rust_source_type_tokens,
     // — adopter
     // `#[field(check = "<sql>")]` raw-SQL CHECK expression.
     // `None` for fields without an adopter check.
     check_sql: #check_sql_tokens,
     // Djogi#217) — adopter
     // `#[field(comment = "<text>")]` column-level
     // comment. `None` for fields without an adopter
     // comment.
     comment: #comment_tokens,
     // Djogi#189 — opt-in strict HeerRanjID CHECK
     // propagation. `true` when the field carries
     // `#[field(strict_id_check)]` or the model carries
     // `#[model(strict_ids)]` and the field qualifies.
     // The projection layer dispatches CHECK shape via
     // three branches: (1) the framework `id` field uses
     // the parent model's PkType semantic family; (2) FK /
     // O2O relation columns use the FK target's PkType
     // semantic family; (3) bare user scalar fields use
     // the field's sql_type only after macro parse-time
     // HeerRanjID family validation confirms membership.
     strict_id_check: #strict_id_check_lit,
     // Djogi#220 — adopter
     // `#[field(type_change_using = "<sql expr>")]` USING
     // clause for non-default-cast column type changes.
     // The SQL emitter appends `USING (<expr>)` only when
     // the differ emits `ColumnChange::ChangeType` for
     // this column; leaving the attribute on a field whose
     // type does not change is a dormant no-op.
     type_change_using: #type_change_using_tokens,
    }
   }
  })
 .collect();
    let deferrability_submits: Vec<TokenStream> = user_fields
        .iter()
        .filter_map(|(field, fa)| {
            let name = crate::syn_util::column_name_from_field(field);
            let relation = detect_relation(&field.ty);
            match relation {
                Some(_) => {
                    let deferrable = fa.deferrable;
                    let initially_deferred = fa.initially_deferred;
                    Some(quote! {
                     ::djogi::__private::inventory::submit! {
                      ::djogi::DeferrabilitySpec {
                       model_type_name: #type_name,
                       field_name: #name,
                       deferrable: #deferrable,
                       initially_deferred: #initially_deferred,
                      }
                     }
                    })
                }
                None => None,
            }
        })
        .collect();

    // Codec startup submissions — one per unique codec used by protected
    // fields. Each submission registers a FieldCodecStartupRequirement so
    // pool startup validation will call the codec's validate function
    // before any CRUD operations. This replaces the unconditional
    // inventory::submit! in aes.rs so binaries with no encrypted fields
    // start without DJOGI_FIELD_CODEC_KEY. Deduplicated per codec_id
    // so multiple encrypted fields on the same model emit only one
    // registration per codec type.
    let mut seen_codecs: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let codec_startup_submits: Vec<TokenStream> = user_fields
        .iter()
        .filter_map(|(_, fa)| {
            let codec_id = fa.protected.as_ref().and_then(|p| p.codec.as_deref())?;
            if !seen_codecs.insert(codec_id) {
                return None; // Already emitted for this codec.
            }
            match codec_id {
                "aes256_gcm_v1" => Some(quote! {
                 ::djogi::__private::inventory::submit! {
                  ::djogi::field_codec::FieldCodecStartupRequirement::const_new(
                   "aes256_gcm_v1",
                   ::djogi::__private::field_codec_aes::ENV_VAR,
                   ::djogi::__private::field_codec_aes::load_ring,
                  )
                 }
                }),
                _ => None, // Unknown codec — already rejected by rule (c) at parse time.
            }
        })
        .collect();

    // Combine in injection order: id (if any), created_at, updated_at, then
    // user fields in source order. This is the complete schema contract.
    let mut all_field_descriptors: Vec<TokenStream> = Vec::new();
    if let Some(id) = id_framework_desc {
        all_field_descriptors.push(id);
    }
    all_field_descriptors.push(created_at_desc);
    all_field_descriptors.push(updated_at_desc);
    all_field_descriptors.extend(user_field_descriptors);

    let is_through = model_attrs.through;
    let has_outbox = model_attrs.events;

    // emit FtsDescriptor tokens when `#[model(fts =...)]`
    // is set. Both `source` and `dictionary` are `&'static str` literals.
    let fts_tokens = fts_descriptor_tokens(&model_attrs.fts);
    // `#[model(idempotency_key = "column")]` emits the
    // column name into the descriptor so runtime consumers
    // (`create_or_find`, `bulk_upsert_by_descriptor`) can discover the
    // conflict key. Non-idempotent models keep `None`.
    let idempotency_key_tokens = match &model_attrs.idempotency_key {
        Some(col) => quote! { ::std::option::Option::Some(#col) },
        None => quote! { ::std::option::Option::None },
    };

    // `#[model(tenant_key = "col")]` wires the column name
    // into the descriptor AND emits a side-channel `target/djogi_rls/` SQL
    // file for the migration differ to consume.
    let tenant_key_tokens = match &model_attrs.tenant_key {
        Some(col) => quote! { ::std::option::Option::Some(#col) },
        None => quote! { ::std::option::Option::None },
    };

    // Djogi#217) — `#[model(table_comment = "<text>")]`
    // free-text table comment. Validated as non-empty / non-whitespace-only
    // by `ModelAttrs::parse`; emit verbatim into the descriptor so the
    // migration composer can lower it to `COMMENT ON TABLE <t> IS '<text>'`.
    // The composer owns single-quote escaping at SQL-emission time so the
    // descriptor carries the adopter's original prose.
    let table_comment_tokens = match &model_attrs.table_comment {
        Some(text) => quote! { ::std::option::Option::Some(#text) },
        None => quote! { ::std::option::Option::None },
    };
    let storage_params_tokens = match &model_attrs.storage_params {
        Some(text) => quote! { ::std::option::Option::Some(#text) },
        None => quote! { ::std::option::Option::None },
    };
    let tablespace_tokens = match &model_attrs.tablespace {
        Some(text) => quote! { ::std::option::Option::Some(#text) },
        None => quote! { ::std::option::Option::None },
    };

    // Side-channel RLS DDL emission — only when tenant_key is declared.
    if let Some(tenant_col) = &model_attrs.tenant_key {
        emit_rls_side_channel(
            struct_item,
            model_attrs,
            &user_fields,
            tenant_col,
            field_attrs,
        );
    }

    // ── Implicit GiST indexes for geography fields ────────
    // For every user field whose Rust type is any `GeographyValue`-implementing
    // geometry (`GeoPoint`, `LineString`, `Polygon`, `MultiPoint`,
    // `MultiPolygon`), emit one `IndexSpec` entry. widened the
    // IndexSpec shape — the column list is now `IndexTarget::Columns(&[
    // IndexColumnSpec::simple(...)])`, the `unique: bool` flag is now
    // `kind: IndexKind::NonUnique`, and three new optional fields (`predicate`,
    // `include`, `nulls_not_distinct`) default to benign values. No behavior
    // change: the emitted DDL under the differ is still
    // `CREATE INDEX CONCURRENTLY... USING gist ("<col>")` after the
    // `CREATE EXTENSION IF NOT EXISTS postgis` guard.
    // The names are baked in as `'static str` string literals — they are
    // compile-time constants derived from the model attrs and field names.
    // No `Box::leak` is needed because the entire `inventory::submit!` block
    // is emitted as a single `static` initialiser; all nested `&[...]` slices
    // are literal arrays with `'static` lifetimes.
    let mut named_index_specs: Vec<(String, TokenStream)> = Vec::new();

    // Spatial GiST indexes — implicit, one per GeographyValue field.
    // The generated name `<table>_<col>_gix` is reserved against user-
    // declared collisions below.
    let mut reserved_generated_names: Vec<String> = Vec::new();
    for (field, _fa) in user_fields.iter() {
        if !is_geography_field_type(&field.ty) {
            continue;
        }
        let col = crate::syn_util::column_name_from_field(field);
        let index_name = format!("{table_name}_{col}_gix");
        reserved_generated_names.push(index_name.clone());
        let col_str = col.as_str();
        let tokens = quote! {
         ::djogi::descriptor::IndexSpec {
          name: #index_name,
          target: ::djogi::descriptor::IndexTarget::Columns(&[
           ::djogi::descriptor::IndexColumnSpec::simple(#col_str),
          ]),
          kind: ::djogi::descriptor::IndexKind::NonUnique,
          index_type: ::djogi::descriptor::IndexType::Gist,
          predicate: ::std::option::Option::None,
          include: &[],
          nulls_not_distinct: false,
          requires_out_of_transaction: true,
          extension_dependency: ::std::option::Option::Some("postgis"),
         }
        };
        named_index_specs.push((index_name, tokens));
    }

    // Lower every `#[model(indexes(...))]` declaration.
    // Column-name validation walks the user-declared field set (raw-ident-
    // stripped) to catch typos at macro-expansion time. Name collisions
    // with the spatial GiST reserved names above are rejected in the
    // lowerer so users cannot silently shadow framework-emitted indexes.
    let mut declared_columns: Vec<String> = user_fields
        .iter()
        .map(|(field, _fa)| crate::syn_util::column_name_from_field(field))
        .collect();
    // Framework-injected columns (`id` when pk != none, plus `created_at`
    // and `updated_at`) are valid index targets too — include them in the
    // declared set so `indexes(index(fields = [created_at]))` compiles.
    if !matches!(model_attrs.pk, PkStrategy::None) {
        declared_columns.push("id".to_string());
    }
    declared_columns.push("created_at".to_string());
    declared_columns.push("updated_at".to_string());
    let lowering_ctx = crate::model::indexes::LoweringCtx {
        table_name: table_name.as_str(),
        declared_columns: &declared_columns,
        reserved_generated_names: &reserved_generated_names,
    };
    for decl in &model_attrs.indexes {
        // Previously the descriptor emitter was infallible and lowered
        // index-emission errors to inline `compile_error!` tokens; now
        // that `try_expand` returns `syn::Result`, propagate the error
        // through the existing failure channel for a single error path.
        let (name, tokens) = crate::model::indexes::emit_index_spec_tokens(decl, &lowering_ctx)?;
        named_index_specs.push((name, tokens));
    }

    // Alphabetise by generated name — deterministic emission means minor
    // reorderings in the user's source do not produce spurious migration
    // diffs. Matches the `visage_map` alphabetisation per
    // `feedback_verify_api_shape_conventions.md`.
    named_index_specs.sort_by(|a, b| a.0.cmp(&b.0));
    let index_spec_tokens: Vec<TokenStream> =
        named_index_specs.into_iter().map(|(_, ts)| ts).collect();

    let indexes_tokens = if index_spec_tokens.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[ #(#index_spec_tokens,)* ] }
    };

    // Apps subsystem linkage.
    // `#[model(app = Vehicles)]` becomes `app: Some(<Vehicles as
    // ::djogi::App>::LABEL)` in the descriptor. Resolution happens at
    // const-eval time; `None` maps to the synthetic global bucket.
    // When `app = X` is set we also emit a compile-time assertion
    // `const _: () = assert!(!<X as ::djogi::App>::TOMBSTONE)` — an
    // active model cannot point at a tombstoned (retired) app.
    // `moved_from_app = OldBilling` does *not* get this assertion;
    // pointing historical metadata at a tombstoned app is the whole
    // point of `moved_from_app`.
    let (app_tokens, tombstone_guard_tokens) = match &model_attrs.app {
        Some(path) => (
            quote! {
             ::core::option::Option::Some(
              <#path as ::djogi::apps::App>::LABEL,
             )
            },
            quote! {
             const _: () = {
              assert!(
               !<#path as ::djogi::apps::App>::TOMBSTONE,
               "cannot declare an active model on a tombstoned \
                app; use `#[model(app = NewApp, moved_from_app = \
                OldApp)]` to record historical metadata",
              );
             };
            },
        ),
        None => (quote! { ::core::option::Option::None }, quote! {}),
    };
    let moved_from_app_tokens = match &model_attrs.moved_from_app {
        Some(path) => quote! {
         ::core::option::Option::Some(
          <#path as ::djogi::apps::App>::LABEL,
         )
        },
        None => quote! { ::core::option::Option::None },
    };
    // `#[model(renamed_from = "old_table")]` carries the
    // prior table name as a string literal. None for unrenamed models.
    let renamed_from_tokens = match &model_attrs.renamed_from {
        Some(s) => quote! { ::core::option::Option::Some(#s) },
        None => quote! { ::core::option::Option::None },
    };

    // PR 7 — `#[model(exclusion(...))]` lowering. Each parsed
    // ExclusionDecl emits one ExclusionConstraintSpec struct literal;
    // the descriptor field receives the wrapped `&[... ]` slice. Empty
    // slice when no exclusion(...) entry was declared.
    let exclusion_constraints_tokens = if model_attrs.exclusions.is_empty() {
        quote! { &[] }
    } else {
        let entries: Vec<proc_macro2::TokenStream> = model_attrs
            .exclusions
            .iter()
            .map(crate::model::exclusion::emit_exclusion_spec_tokens)
            .collect();
        quote! { &[ #(#entries,)* ] }
    };

    // `#[model(tree_edge = "col")]`.
    // The string was validated above (field-existence + self-FK
    // resolution) before reaching here, so emission is unconditional.
    let tree_edge_tokens = match &model_attrs.tree_edge {
        Some(lit) => {
            let value = lit.value();
            quote! { ::core::option::Option::Some(#value) }
        }
        None => quote! { ::core::option::Option::None },
    };

    // 3 — `#[model(proxy_for = ParentType)]` lowers the bare
    // identifier to a `&'static str` carrying the parent's Rust type
    // name. The migration differ uses this discriminator to skip DDL
    // emission for proxies; the runtime composer uses it to
    // identify proxy querysets that need default-filter / default-order
    // composition.
    let proxy_for_tokens = match &model_attrs.proxy_for {
        Some(ident) => {
            let value = ident.to_string();
            quote! { ::core::option::Option::Some(#value) }
        }
        None => quote! { ::core::option::Option::None },
    };

    // 3 — `#[model(default_filter = |f|...)]` is lowered to
    // a SQL fragment string at expand time. The closure body is walked
    // through the closed grammar in `crate::model::proxy::lower_default_filter_to_sql`;
    // anything outside that grammar surfaces a span-precise compile
    // error here, before any descriptor emission runs.
    // Empty / `None` for non-proxy models and for proxies without a
    // `default_filter` clause. The runtime composer reads this at
    // QuerySet construction time and AND-composes it into the seeded
    // `Condition` tree.
    let default_filter_sql_tokens = match &model_attrs.proxy_default_filter {
        Some(closure) => {
            let sql = crate::model::proxy::lower_default_filter_to_sql(closure)?;
            quote! { ::core::option::Option::Some(#sql) }
        }
        None => quote! { ::core::option::Option::None },
    };

    // 5 — populate `ModelDescriptor.computed_fields` from
    // the captured `#[computed(sql = "...")]` attributes. Emits one
    // `ComputedFieldDescriptor {... }` literal per entry; empty slice
    // when no computed fields are declared. The descriptor's
    // `value_type` is set to `FieldSqlType::Custom` keyed off the
    // declared Rust return-type token stream — the migration differ
    // does not consult this field in v0.1.0 (computed fields are non-
    // stored), so a permissive `Custom` mapping is sufficient. Future
    // phases that want stricter typing can map the Rust type more
    // precisely.
    let computed_fields_tokens = if computed_attrs.is_empty() {
        quote! { &[] }
    } else {
        let entries: Vec<proc_macro2::TokenStream> = computed_attrs
            .iter()
            .map(|(field_ident, attr)| {
                let name = field_ident.to_string();
                let sql = &attr.sql;
                let return_ty = &attr.return_type;
                // Map the Rust return type to a `FieldSqlType` variant
                // via the existing `rust_type_to_sql` helper +
                // `sql_str_to_tokens` lookup. Fall back to
                // `Custom("<rendered>")` for shapes the helper does not
                // recognise — the migration differ does not consult
                // this field in v0.1.0 (computed fields are non-stored)
                // so the Custom mapping is documentation-only.
                let value_type_ts = match rust_type_to_sql(return_ty) {
                    Some(s) => sql_str_to_tokens(s),
                    None => {
                        let rendered = quote::quote!(#return_ty).to_string().replace(' ', "");
                        quote! { ::djogi::FieldSqlType::Custom(#rendered) }
                    }
                };
                quote! {
                 ::djogi::descriptor::ComputedFieldDescriptor {
                  name: #name,
                  sql: #sql,
                  value_type: #value_type_ts,
                 }
                }
            })
            .collect();
        quote! { &[ #(#entries,)* ] }
    };

    Ok(quote! {
     #tombstone_guard_tokens

     ::djogi::__private::inventory::submit! {
      ::djogi::ModelDescriptor {
       type_name: #type_name,
       table_name: #table_name,
       pk_type: #pk_type_tokens,
       fields: &[
        #(#all_field_descriptors,)*
       ],
       // Defaults — populated by later phases' attr parsers.
       partition_by: None,
       // `#[model(table = "...", events)]` toggles
       // transactional outbox emission on every ctx-scoped
       // create/save/delete. `djogi::outbox::emit_event` keys off
       // this flag at codegen time.
       has_outbox: #has_outbox,
       idempotency_key: #idempotency_key_tokens,
       // RLS tenant discriminator column.
       tenant_key: #tenant_key_tokens,
       cache_ttl: None,
       rationale: None,
       // implicit GiST IndexSpec for every GeoPoint
       // field. Non-spatial models keep the empty slice default.
       indexes: #indexes_tokens,
       // (phase3-relations): `#[model(table = "...", through)]`
       is_through: #is_through,
       // Full-Text Search.
       fts: #fts_tokens,
       // Apps subsystem linkage.
       app: #app_tokens,
       moved_from_app: #moved_from_app_tokens,
       // Table-rename hint.
       renamed_from: #renamed_from_tokens,
       // PR 7 — `EXCLUDE` constraint declarations.
       // Lowered from the parsed `#[model(exclusion(...))]`
       // entries on `model_attrs.exclusions`. Empty slice when
       // no `exclusion(...)` group is present.
       exclusion_constraints: #exclusion_constraints_tokens,
       // `#[model(tree_edge = "col")]` — default self-FK column
       // for tree-recursive queries. Validated at the top of
       // `try_expand` against the user-field list and the
       // self-FK detector; reaches here only when the named
       // column resolves to a self-FK on this model.
       tree_edge: #tree_edge_tokens,
       // Proxy-model schema-passthrough surface.
       // Populated from `#[model(proxy_for = ParentType,
       // default_filter = |f|...)]`. The migration differ keys
       // off `proxy_for.is_some()` to skip DDL emission for proxy
       // descriptors; the runtime composer keys off
       // `default_filter_sql` to AND-compose the lowered fragment
       // into every `QuerySet<Self>::new`.
       proxy_for: #proxy_for_tokens,
       default_filter_sql: #default_filter_sql_tokens,
       // Computed-field descriptors populated from
       // `#[computed(sql = "...")]` field attributes;
       // empty slice for non-computed models.
       computed_fields: #computed_fields_tokens,
       // Djogi#217) — adopter
       // `#[model(table_comment = "<text>")]` free-text
       // table comment. `None` when the attribute is absent.
       table_comment: #table_comment_tokens,
       // Djogi#218/#219) — adopter
       // `#[model(storage_params = "...")]` and
       // `#[model(tablespace = "...")]` metadata.
       storage_params: #storage_params_tokens,
       tablespace: #tablespace_tokens,
      }
     }
     #(#deferrability_submits)*
     #(#codec_startup_submits)*
    })
}

/// Emit a side-channel `target/djogi_rls/{table}_rls.sql` file when the model
/// declares a `tenant_key`.
/// The emitted SQL contains two statements:
/// 1. `ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;`
/// 2. `CREATE POLICY {table}_tenant_isolation ON {table} USING (col = current_setting(...));`
/// The cast in the `USING` expression depends on the tenant column's SQL type:
/// - `BigInt` → `::bigint`
/// - `Uuid` → `::uuid`
/// - `Text` → no cast
/// - Any other type → compile error (via `proc_macro_error` note, non-fatal).
/// The migration differ will consume this file. Until then, the file
/// serves as documentation and as an integration-test fixture the test can
/// verify was created.
/// The function is intentionally non-fatal on I/O errors (uses `eprintln!` not
/// `panic!`) so a proc macro failure due to a missing `target/` directory does
/// not break builds in unusual environments.
fn emit_rls_side_channel(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    user_fields: &[(&syn::Field, &FieldAttrs)],
    tenant_col: &str,
    _field_attrs: &[FieldAttrs],
) {
    let table = &model_attrs.table;

    // Find the user field whose name matches `tenant_col` so we can determine
    // its SQL type for the cast expression. Field names are raw-ident stripped
    // to bare names (matching the SQL column name) before comparison.
    let tenant_field_ty: Option<&syn::Type> = user_fields.iter().find_map(|(field, _)| {
        if crate::syn_util::column_name_from_field(field) == tenant_col {
            Some(&field.ty)
        } else {
            None
        }
    });

    // Determine the cast suffix for the `current_setting(...)` expression.
    // `true` in `current_setting('app.tenant_id', true)` makes a missing
    // GUC return NULL instead of raising — safer for connections that
    // haven't called set_tenant yet.
    let cast_suffix: &str = if let Some(ty) = tenant_field_ty {
        match field_sql_type_category(ty) {
            FieldSqlTypeCategory::BigInt => "::bigint",
            FieldSqlTypeCategory::Uuid => "::uuid",
            FieldSqlTypeCategory::Text => "",
            FieldSqlTypeCategory::Unsupported(ref other) => {
                // Emit a build-time warning to stderr. The RLS file is still
                // written with an empty cast so the build stays green — a
                // Compile-fail test will tighten this to a hard error.
                // GH issue #37 — `ForeignKey<T>` / `OneToOneField<T>` columns
                // route through `BigInt` in `field_sql_type_category`, so they
                // do not reach this branch even if the FK target uses RanjId
                // or a custom PK. Adopters who hit a wrong-cast policy at
                // runtime must declare `tenant_key` against a non-FK column.
                eprintln!(
                    "djogi-macros: tenant_key column `{tenant_col}` on struct `{}` \
      has unsupported SQL type `{other}`; RLS cast will be empty. \
      Only BigInt, Uuid, and Text/Citext are supported.",
                    struct_item.ident
                );
                ""
            }
        }
    } else {
        // Field not found — possibly a framework column (id, created_at, updated_at)
        // or a typo. Emit with empty cast; will tighten validation.
        eprintln!(
            "djogi-macros: tenant_key value `{tenant_col}` does not match any \
    user-declared field on struct `{}`; check spelling.",
            struct_item.ident
        );
        ""
    };

    let sql = format!(
        "-- RLS DDL for {table} — generated by #[model(tenant_key = \"{tenant_col}\")].\n\
   -- Djogi's migration differ consumes this file to apply RLS policies.\n\
   -- The `true` flag in current_setting makes a missing GUC return NULL\n\
   -- instead of raising, keeping connections without set_tenant() safe.\n\
   \n\
   ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;\n\
   \n\
   CREATE POLICY {table}_tenant_isolation ON {table}\n\
    USING ({tenant_col} = current_setting('app.tenant_id', true){cast_suffix});\n",
    );

    // Write to `target/djogi_rls/{table}_rls.sql`. Proc macros run with
    // `CARGO_MANIFEST_DIR` set to the crate being compiled (the user crate),
    // and `OUT_DIR` is the canonical location for build artefacts.
    // We mirror the outbox pattern and write relative to the workspace root
    // by locating the Cargo manifest directory.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_owned());
    let rls_dir = std::path::Path::new(&manifest_dir)
        .join("target")
        .join("djogi_rls");

    if let Err(e) = std::fs::create_dir_all(&rls_dir) {
        eprintln!(
            "djogi-macros: could not create target/djogi_rls/: {e}; skipping RLS DDL emission"
        );
        return;
    }

    let rls_path = rls_dir.join(format!("{table}_rls.sql"));
    if let Err(e) = std::fs::write(&rls_path, &sql) {
        eprintln!("djogi-macros: could not write {}: {e}", rls_path.display());
    }
}

/// Emit a `&'static [(&'static str, &'static str)]` literal from an
/// `ExposeSpec`. Scalar scope entries map scope → column name; relation
/// scope entries map scope → peer visage type name. Empty / suppressed
/// specs emit `&[]`.
/// Entries are sorted by scope name so descriptor snapshots don't churn
/// based on the underlying `HashSet` / `HashMap` iteration order.
fn build_projection_map_tokens(
    spec: &crate::model::attrs::ExposeSpec,
    column_name: &str,
) -> TokenStream {
    if spec.suppressed || (spec.scalar_scopes.is_empty() && spec.relation_scopes.is_empty()) {
        return quote! { &[] };
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for scope in &spec.scalar_scopes {
        pairs.push((scope.clone(), column_name.to_string()));
    }
    for (scope, exposure) in &spec.relation_scopes {
        // Render the peer path as a string for descriptor consumers.
        // `quote!` preserves the user-written path verbatim (including
        // module prefixes); the surrounding whitespace is collapsed by
        // `to_string()` into a stable canonical form (e.g. `crate :: visages :: DepartmentPublic`
        // → trim_runs handled by callers / migration differ if needed).
        let peer_path = &exposure.peer;
        let rendered = quote::quote! { #peer_path }.to_string();
        // Collapse any inserted whitespace around path separators so the
        // emitted descriptor literal matches the user's original spelling
        // for the common no-prefix case (e.g. `DepartmentPublic`).
        let cleaned = rendered.replace(' ', "");
        pairs.push((scope.clone(), cleaned));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let entries: Vec<TokenStream> = pairs.iter().map(|(s, e)| quote! { (#s, #e) }).collect();
    quote! { &[ #(#entries,)* ] }
}

fn field_sql_type_tokens(ty: &syn::Type) -> TokenStream {
    match rust_type_to_sql(ty) {
        Some(sql) => sql_str_to_tokens(sql),
        None => quote! {
         ::djogi::FieldSqlType::Custom(
          <#ty as ::djogi::descriptor::DjogiSqlType>::SQL_TYPE
         )
        },
    }
}

fn sql_str_to_tokens(s: &str) -> TokenStream {
    match s {
        "TEXT" => quote! { ::djogi::FieldSqlType::Text },
        "SMALLINT" => quote! { ::djogi::FieldSqlType::SmallInt },
        "INTEGER" => quote! { ::djogi::FieldSqlType::Integer },
        "BIGINT" => quote! { ::djogi::FieldSqlType::BigInt },
        "REAL" => quote! { ::djogi::FieldSqlType::Real },
        "DOUBLE PRECISION" => quote! { ::djogi::FieldSqlType::DoublePrecision },
        "BOOLEAN" => quote! { ::djogi::FieldSqlType::Boolean },
        "TIMESTAMPTZ" => quote! { ::djogi::FieldSqlType::Timestamptz },
        "DATE" => quote! { ::djogi::FieldSqlType::Date },
        // u64 now uses bare NUMERIC (no precision/scale) so the
        // migration projection layer can match on `FieldSqlType::Numeric` with
        // `RustSourceType::U64` and emit the integrality CHECK. The old
        // "NUMERIC(20, 0)" arm is removed because u64 no longer emits that
        // SQL type string.
        "NUMERIC" => quote! { ::djogi::FieldSqlType::Numeric },
        "UUID" => quote! { ::djogi::FieldSqlType::Uuid },
        "JSONB" => quote! { ::djogi::FieldSqlType::Jsonb },
        // `Vec<u8>` lowers to BYTEA (see `rust_type_to_sql`).
        // The descriptor variant is unconditional; tokio-postgres' native
        // `Vec<u8>` codec handles the wire round-trip with no feature flag.
        "BYTEA" => quote! { ::djogi::FieldSqlType::Bytea },
        "TEXT[]" => quote! { ::djogi::FieldSqlType::TextArray },
        "SMALLINT[]" => quote! { ::djogi::FieldSqlType::SmallIntArray },
        "INTEGER[]" => quote! { ::djogi::FieldSqlType::IntegerArray },
        "BIGINT[]" => quote! { ::djogi::FieldSqlType::BigIntArray },
        "REAL[]" => quote! { ::djogi::FieldSqlType::RealArray },
        "DOUBLE PRECISION[]" => quote! { ::djogi::FieldSqlType::DoublePrecisionArray },
        "BOOLEAN[]" => quote! { ::djogi::FieldSqlType::BoolArray },
        "TIMESTAMPTZ[]" => quote! { ::djogi::FieldSqlType::TimestamptzArray },
        "DATE[]" => quote! { ::djogi::FieldSqlType::DateArray },
        "UUID[]" => quote! { ::djogi::FieldSqlType::UuidArray },
        "NUMERIC[]" => quote! { ::djogi::FieldSqlType::NumericArray },
        // Spatial — all GeographyValue types map to the typed Geography variant
        // with the matching GeographySubtype discriminant so the
        // migration differ can compare subtypes by discriminant rather than
        // Display text. This match covers all five geometry types.
        "GEOGRAPHY(Point, 4326)" => {
            quote! {
             ::djogi::FieldSqlType::Geography {
              subtype: ::djogi::descriptor::GeographySubtype::Point,
              srid: 4326u32,
             }
            }
        }
        "GEOGRAPHY(LineString, 4326)" => {
            quote! {
             ::djogi::FieldSqlType::Geography {
              subtype: ::djogi::descriptor::GeographySubtype::LineString,
              srid: 4326u32,
             }
            }
        }
        "GEOGRAPHY(Polygon, 4326)" => {
            quote! {
             ::djogi::FieldSqlType::Geography {
              subtype: ::djogi::descriptor::GeographySubtype::Polygon,
              srid: 4326u32,
             }
            }
        }
        "GEOGRAPHY(MultiPoint, 4326)" => {
            quote! {
             ::djogi::FieldSqlType::Geography {
              subtype: ::djogi::descriptor::GeographySubtype::MultiPoint,
              srid: 4326u32,
             }
            }
        }
        "GEOGRAPHY(MultiPolygon, 4326)" => {
            quote! {
             ::djogi::FieldSqlType::Geography {
              subtype: ::djogi::descriptor::GeographySubtype::MultiPolygon,
              srid: 4326u32,
             }
            }
        }
        // `INTERVAL` lowers to the typed `FieldSqlType::Interval`
        // variant so the differ / projection / docs surfaces work without
        // a string-comparison shortcut. The mapping tracks
        // `rust_type_to_sql`'s `djogi::Interval` arm.
        "INTERVAL" => quote! { ::djogi::FieldSqlType::Interval },
        // Postgres network family. The descriptor variants
        // are unconditional (declared in `descriptor.rs` regardless of
        // feature state) so migration snapshots / docs stay stable when
        // the `network` feature toggles. The matching Rust types
        // (`std::net::IpAddr` for INET, `djogi::CidrAddr` for CIDR,
        // `djogi::MacAddr` for MACADDR) are gated on the runtime crate's
        // `network` feature.
        "INET" => quote! { ::djogi::FieldSqlType::Inet },
        "CIDR" => quote! { ::djogi::FieldSqlType::Cidr },
        "MACADDR" => quote! { ::djogi::FieldSqlType::Macaddr },
        // range SQL types
        // lower to the typed `FieldSqlType::Range { subtype }` variant.
        // The mapping tracks `rust_type_to_sql`'s explicit outer-wrapper
        // namespace policy for runtime-backed `djogi::Range<T>`; one arm
        // per Postgres built-in range type the public surface ships.
        "INT4RANGE" | "int4range" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Int4,
         }
        },
        "INT8RANGE" | "int8range" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Int8,
         }
        },
        "NUMRANGE" | "numrange" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Num,
         }
        },
        "TSRANGE" | "tsrange" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Ts,
         }
        },
        "TSTZRANGE" | "tstzrange" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Tstz,
         }
        },
        "DATERANGE" | "daterange" => quote! {
         ::djogi::FieldSqlType::Range {
          subtype: ::djogi::descriptor::RangeSubtypeKind::Date,
         }
        },
        other => {
            let s = other.to_string();
            quote! { ::djogi::FieldSqlType::Custom(#s) }
        }
    }
}

/// Detect if a field type is `Jsonb<T>` by checking the last-segment ident.
/// Returns `true` if the type path ends in `Jsonb`, after stripping `Option`.
/// Uses last-segment path matching to accept bare `Jsonb<T>`, `djogi::Jsonb<T>`, etc.
fn is_jsonb_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| seg.ident == "Jsonb")
        .unwrap_or(false)
}

/// Detect if a field type is any `GeographyValue`-implementing geometry type
/// by checking the last-segment ident.
/// Returns `true` if the type path ends in one of the five recognized geometry
/// type names (`GeoPoint`, `LineString`, `Polygon`, `MultiPoint`,
/// `MultiPolygon`), after stripping `Option`. Uses last-segment path matching
/// so qualified paths (`djogi::geo::LineString`, `geo::Polygon`, etc.) and bare
/// idents are all accepted.
/// The `spatial` feature flag lives on the `djogi` runtime crate, not on
/// `djogi-macros`. The macro recognises type names unconditionally so it emits
/// the correct descriptor regardless of feature state. If the user omits the
/// `spatial` feature, the compile error surfaces at the struct definition as
/// "unresolved type", not here.
fn is_geography_field_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| {
            matches!(
                seg.ident.to_string().as_str(),
                "GeoPoint" | "LineString" | "Polygon" | "MultiPoint" | "MultiPolygon"
            )
        })
        .unwrap_or(false)
}

/// Detect if a field type is `GeoPoint` specifically.
/// Retained as a narrower helper for callers that need to distinguish
/// `GeoPoint` from other geometry types (e.g. test assertions). For the
/// common "is this any geography?" check, prefer `is_geography_field_type`.
#[allow(dead_code)]
fn is_geopoint_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| seg.ident == "GeoPoint")
        .unwrap_or(false)
}

/// Detect if a field type is `Geography` by checking the last-segment ident.
/// Returns `true` if the type path ends in `Geography`, after stripping `Option`.
/// Uses last-segment path matching to accept bare `Geography<T>`, `djogi::Geography<T>`, etc.
/// This helper is retained for the `#[field(index)]` auto-index default so that
/// any user who annotates a field typed as a raw `Geography<…>` wrapper (not
/// the canonical `GeoPoint`) also gets a GiST index by default.
fn is_geography_type(ty: &syn::Type) -> bool {
    let (inner, _) = unwrap_option(ty);
    let syn::Type::Path(syn::TypePath { path, qself: None }) = inner else {
        return false;
    };
    path.segments
        .last()
        .map(|seg| seg.ident == "Geography")
        .unwrap_or(false)
}

/// Compute the default `IndexType` for a field when `#[field(index)]` is
/// present without an explicit method.
/// - `Jsonb<T>` → `IndexType::Gin`
/// - Any geography type (`GeoPoint`, `LineString`, `Polygon`, `MultiPoint`,
/// `MultiPolygon`) or raw `Geography<…>` wrapper → `IndexType::Gist`
/// - Everything else → `IndexType::BTree`
fn default_index_type(ty: &syn::Type) -> TokenStream {
    if is_jsonb_type(ty) {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gin) }
    } else if is_geography_field_type(ty) || is_geography_type(ty) {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gist) }
    } else {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::BTree) }
    }
}

/// Emit the `fts: Option<FtsDescriptor>` token stream for the
/// `inventory::submit!` block.
/// When `spec` is `None`, emits `::std::option::Option::None`. When `Some`,
/// emits `::std::option::Option::Some(::djogi::descriptor::FtsDescriptor {... })`.
/// The `source` and `dictionary` strings become `&'static str` literals baked
/// into the descriptor at compile time — they are constant data, not runtime
/// allocations.
fn fts_descriptor_tokens(spec: &Option<FtsSpec>) -> TokenStream {
    match spec {
        None => quote! { ::std::option::Option::None },
        Some(s) => {
            let source = &s.source;
            let dictionary = &s.dictionary;
            quote! {
             ::std::option::Option::Some(::djogi::descriptor::FtsDescriptor {
              // Default generated column name. will add a
              // `column = "..."` override; until then it is always "search".
              column: "search",
              source: #source,
              dictionary: #dictionary,
             })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_jsonb_type_bare() {
        let ty = syn::parse_str::<syn::Type>("Jsonb<String>").unwrap();
        assert!(is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_jsonb_type_qualified() {
        let ty = syn::parse_str::<syn::Type>("djogi::Jsonb<String>").unwrap();
        assert!(is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_jsonb_type_option() {
        let ty = syn::parse_str::<syn::Type>("Option<Jsonb<String>>").unwrap();
        assert!(is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_jsonb_type_option_qualified() {
        let ty = syn::parse_str::<syn::Type>("Option<djogi::Jsonb<String>>").unwrap();
        assert!(is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_jsonb_type_string_false() {
        let ty = syn::parse_str::<syn::Type>("String").unwrap();
        assert!(!is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_jsonb_type_vec_false() {
        let ty = syn::parse_str::<syn::Type>("Vec<String>").unwrap();
        assert!(!is_jsonb_type(&ty));
    }

    #[test]
    fn test_is_geography_type_bare() {
        let ty = syn::parse_str::<syn::Type>("Geography<Point>").unwrap();
        assert!(is_geography_type(&ty));
    }

    #[test]
    fn test_is_geography_type_qualified() {
        let ty = syn::parse_str::<syn::Type>("djogi::Geography<Point>").unwrap();
        assert!(is_geography_type(&ty));
    }

    #[test]
    fn test_is_geography_type_option() {
        let ty = syn::parse_str::<syn::Type>("Option<Geography<Point>>").unwrap();
        assert!(is_geography_type(&ty));
    }

    #[test]
    fn test_is_geography_type_option_qualified() {
        let ty = syn::parse_str::<syn::Type>("Option<djogi::Geography<Point>>").unwrap();
        assert!(is_geography_type(&ty));
    }

    #[test]
    fn test_is_geography_type_string_false() {
        let ty = syn::parse_str::<syn::Type>("String").unwrap();
        assert!(!is_geography_type(&ty));
    }

    #[test]
    fn test_is_geography_type_jsonb_false() {
        let ty = syn::parse_str::<syn::Type>("Jsonb<String>").unwrap();
        assert!(!is_geography_type(&ty));
    }

    // ── is_geography_field_type ───────────────────────────────────────────────

    #[test]
    fn test_is_geography_field_type_geopoint() {
        let ty = syn::parse_str::<syn::Type>("GeoPoint").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_linestring() {
        let ty = syn::parse_str::<syn::Type>("LineString").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_polygon() {
        let ty = syn::parse_str::<syn::Type>("Polygon").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_multipoint() {
        let ty = syn::parse_str::<syn::Type>("MultiPoint").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_multipolygon() {
        let ty = syn::parse_str::<syn::Type>("MultiPolygon").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_qualified_linestring() {
        let ty = syn::parse_str::<syn::Type>("djogi::geo::LineString").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_option_polygon() {
        let ty = syn::parse_str::<syn::Type>("Option<Polygon>").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_option_multipolygon() {
        let ty = syn::parse_str::<syn::Type>("Option<MultiPolygon>").unwrap();
        assert!(is_geography_field_type(&ty));
    }

    #[test]
    fn test_is_geography_field_type_string_false() {
        let ty = syn::parse_str::<syn::Type>("String").unwrap();
        assert!(!is_geography_field_type(&ty));
    }

    // ── is_geopoint_type ─────────────────────────────────────────────────────

    #[test]
    fn test_is_geopoint_type_bare() {
        let ty = syn::parse_str::<syn::Type>("GeoPoint").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_djogi_qualified() {
        let ty = syn::parse_str::<syn::Type>("djogi::GeoPoint").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_geo_qualified() {
        let ty = syn::parse_str::<syn::Type>("geo::GeoPoint").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_full_path() {
        let ty = syn::parse_str::<syn::Type>("djogi::geo::GeoPoint").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_option_bare() {
        let ty = syn::parse_str::<syn::Type>("Option<GeoPoint>").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_option_qualified() {
        let ty = syn::parse_str::<syn::Type>("Option<djogi::GeoPoint>").unwrap();
        assert!(is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_string_false() {
        let ty = syn::parse_str::<syn::Type>("String").unwrap();
        assert!(!is_geopoint_type(&ty));
    }

    #[test]
    fn test_is_geopoint_type_geography_false() {
        // `Geography<Point>` is a different type from `GeoPoint`.
        let ty = syn::parse_str::<syn::Type>("Geography<Point>").unwrap();
        assert!(!is_geopoint_type(&ty));
    }

    #[test]
    fn test_sql_str_to_tokens_range_subtypes_accept_lowercase_variants() {
        let normalize = |s: String| s.replace(' ', "");
        let int4 = normalize(sql_str_to_tokens("int4range").to_string());
        let int8 = normalize(sql_str_to_tokens("int8range").to_string());
        let num = normalize(sql_str_to_tokens("numrange").to_string());
        let ts = normalize(sql_str_to_tokens("tsrange").to_string());
        let tstz = normalize(sql_str_to_tokens("tstzrange").to_string());
        let date = normalize(sql_str_to_tokens("daterange").to_string());
        assert!(
            int4.contains("RangeSubtypeKind::Int4"),
            "lowercase int4range should map to FieldSqlType::Range subtype Int4"
        );
        assert!(
            int8.contains("RangeSubtypeKind::Int8"),
            "lowercase int8range should map to FieldSqlType::Range subtype Int8"
        );
        assert!(
            num.contains("RangeSubtypeKind::Num"),
            "lowercase numrange should map to FieldSqlType::Range subtype Num"
        );
        assert!(
            ts.contains("RangeSubtypeKind::Ts"),
            "lowercase tsrange should map to FieldSqlType::Range subtype Ts"
        );
        assert!(
            tstz.contains("RangeSubtypeKind::Tstz"),
            "lowercase tstzrange should map to FieldSqlType::Range subtype Tstz"
        );
        assert!(
            date.contains("RangeSubtypeKind::Date"),
            "lowercase daterange should map to FieldSqlType::Range subtype Date"
        );
    }

    /// the `"BYTEA"` SQL string (produced by `rust_type_to_sql`
    /// for a `Vec<u8>` field) must lower to the typed `FieldSqlType::Bytea`
    /// token, not the `Custom("BYTEA")` fallback. If this arm were dropped,
    /// the `other =>` tail would emit `FieldSqlType::Custom("BYTEA")`, which
    /// the migration differ would still compare equal by string but which
    /// loses the typed-variant guarantee the descriptor surface depends on.
    #[test]
    fn test_sql_str_to_tokens_bytea_maps_to_typed_variant() {
        let normalize = |s: String| s.replace(' ', "");
        let bytea = normalize(sql_str_to_tokens("BYTEA").to_string());
        assert!(
            bytea.contains("FieldSqlType::Bytea"),
            "\"BYTEA\" should map to the typed FieldSqlType::Bytea variant, got: {bytea}"
        );
        assert!(
            !bytea.contains("Custom"),
            "\"BYTEA\" must not fall through to FieldSqlType::Custom, got: {bytea}"
        );
    }
}
