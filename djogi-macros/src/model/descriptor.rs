//! Generates `inventory::submit!(ModelDescriptor {...})` from the `#[model]`
//! struct definition. Runs AFTER `inject::expand` has mutated `struct_item`.
//!
//! Phase 1.5: `ModelDescriptor::fields` is the **complete** schema contract.
//! Framework-injected columns (`id`, `created_at`, `updated_at`) are emitted
//! first (in injection order), followed by user-declared fields in source
//! order. Downstream consumers (migration differ, admin UI, `djogi docs`, RLS
//! generator) iterate `descriptor.fields` as the single schema source and
//! never synthesize framework columns out-of-band.
//!
//! For `pk = "none"`, `id` is omitted from the framework prefix (the user's
//! own PK field appears as a regular user field in declared order).
//!
//! The emitted submission uses Phase 1 defaults for every amended field
//! (partition_by, has_outbox, idempotency_key, tenant_key, cache_ttl,
//! rationale, indexes) — those attrs are populated by later phases' parser
//! extensions. Per-field defaults (rationale, outbox_exclude, index_type)
//! follow the same convention.

use crate::model::attrs::{
    FieldAttrs, FieldSqlTypeCategory, FtsSpec, ModelAttrs, PkStrategy,
    RelationKind as MacroRelationKind, detect_relation, field_sql_type_category,
    on_delete_str_to_tokens, rust_type_to_sql, unwrap_option,
};
use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemStruct;

pub fn expand(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> TokenStream {
    let type_name = struct_item.ident.to_string();
    let table_name = &model_attrs.table;

    let pk_type_tokens = match model_attrs.pk {
        PkStrategy::HeerId => quote! { ::djogi::PkType::HeerId },
        PkStrategy::RanjId => quote! { ::djogi::PkType::RanjId },
        PkStrategy::Serial => quote! { ::djogi::PkType::Serial },
        PkStrategy::None => quote! { ::djogi::PkType::None },
    };

    // field_attrs was collected BEFORE injection (user fields only, 0-indexed).
    // struct_item.fields now has framework fields at the front — skip them so
    // zip() aligns field_attrs[0] with the first USER field, not id.
    let n_framework = match model_attrs.pk {
        PkStrategy::None => 2, // created_at, updated_at only
        _ => 3,                // id, created_at, updated_at
    };
    let user_fields: Vec<_> = struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
        .collect();

    // ── Framework-field FieldDescriptors ─────────────────────────────────────
    // Phase 1.5: framework columns are emitted FIRST so `descriptor.fields` is
    // the complete schema contract. `id` varies by pk strategy; `created_at`
    // and `updated_at` are always Timestamptz, non-null, not unique/indexed.
    // For `pk = "none"`, skip `id` entirely — the user's own PK appears as a
    // regular user field in declared order.

    let id_framework_desc: Option<TokenStream> = match model_attrs.pk {
        PkStrategy::HeerId => Some(quote! {
            ::djogi::FieldDescriptor {
                name: "id",
                sql_type: ::djogi::FieldSqlType::BigInt,
                nullable: false,
                unique: true,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                // Framework columns carry no relation metadata — `id` is a
                // PK, not an FK, and Phase 4.5's visage hookup lands
                // per-user-field only.
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[
                    ("admin", "id"),
                    ("export", "id"),
                    ("public", "id"),
                    ("self_view", "id"),
                ],
            }
        }),
        PkStrategy::RanjId => Some(quote! {
            ::djogi::FieldDescriptor {
                name: "id",
                sql_type: ::djogi::FieldSqlType::Uuid,
                nullable: false,
                unique: true,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[
                    ("admin", "id"),
                    ("export", "id"),
                    ("public", "id"),
                    ("self_view", "id"),
                ],
            }
        }),
        PkStrategy::Serial => Some(quote! {
            ::djogi::FieldDescriptor {
                name: "id",
                sql_type: ::djogi::FieldSqlType::Integer,
                nullable: false,
                unique: true,
                indexed: true,
                max_length: None,
                renamed_from: None,
                rationale: None,
                outbox_exclude: false,
                sequence_within: None,
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                visage_map: &[
                    ("admin", "id"),
                    ("export", "id"),
                    ("public", "id"),
                    ("self_view", "id"),
                ],
            }
        }),
        PkStrategy::None => None,
    };

    let created_at_desc = quote! {
        ::djogi::FieldDescriptor {
            name: "created_at",
            sql_type: ::djogi::FieldSqlType::Timestamptz,
            nullable: false,
            unique: false,
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
            visage_map: &[
                ("admin", "created_at"),
                ("export", "created_at"),
                ("public", "created_at"),
                ("self_view", "created_at"),
            ],
        }
    };
    let updated_at_desc = quote! {
        ::djogi::FieldDescriptor {
            name: "updated_at",
            sql_type: ::djogi::FieldSqlType::Timestamptz,
            nullable: false,
            unique: false,
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
            visage_map: &[
                ("admin", "updated_at"),
                ("export", "updated_at"),
                ("public", "updated_at"),
                ("self_view", "updated_at"),
            ],
        }
    };

    // ── User-field FieldDescriptors ───────────────────────────────────────────

    let user_field_descriptors: Vec<TokenStream> = user_fields
        .iter()
        .map(|(field, fa)| {
            let raw_name = field.ident.as_ref().unwrap().to_string();
            // Raw identifiers (`r#type`) must serialize to the bare SQL
            // column name — matches the stripping pattern in `stubs.rs`.
            let name = raw_name.strip_prefix("r#").unwrap_or(&raw_name).to_string();
            // Phase 4 Task 6 — `#[field(outbox = "ignore")]` marks the
            // column as excluded from the transactional outbox payload.
            // `FieldAttrs::parse` has already validated the string value,
            // so a non-`None` `outbox` attr always means "ignore" here.
            let outbox_exclude = fa.outbox.as_deref() == Some("ignore");
            // Phase 4 Task 7.6 — `#[field(sequence_within = "col")]`
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

            let (inner_ty, nullable) = unwrap_option(&field.ty);

            // For relation fields the SQL column type is the target's PK
            // type, not the Rust wrapper type. Phase 6's migration emitter
            // consumes `sql_type` alongside `target_type_name` to produce
            // `REFERENCES` clauses; Phase 3 uses the `target_type_name`
            // as the primary signal and leaves `sql_type` as the Phase 1
            // best-effort scalar mapping. A future amendment (Phase 6)
            // can extend this to look the target PK type up via a second
            // `ModelDescriptor` pass.
            let sql_type_str = rust_type_to_sql(&inner_ty).unwrap_or("TEXT");
            let sql_type = sql_str_to_tokens(sql_type_str);
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

            // Phase 5 — `#[field(index)]` or `#[field(index = "method")]`.
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

            // Phase 4.5 — populate visage_map from the parsed
            // `#[field(expose(...))]` spec. Scalar scopes map to this
            // column's name; relation scopes map to the peer visage
            // type name. Empty / suppressed specs emit `&[]`.
            let projection_map_tokens = build_projection_map_tokens(&fa.expose, &name);

            // Relation metadata — `None`/`&[]` for scalar columns.
            //
            // Descriptor lookup keys off the short target name (last path
            // segment) — Phase 6's migration differ matches this against
            // `ModelDescriptor::type_name`, which is also just the short
            // ident — so we deliberately use `info.target_name` here rather
            // than the full `info.target_type`. The full type path is only
            // needed by codegen sites that emit the target in type position
            // (see `relations::expand`).
            let (relation_kind_tokens, on_delete_tokens, target_type_name_tokens) = match &relation
            {
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
                    (kind_tokens, on_delete, quote! { Some(#target_lit) })
                }
                None => (quote! { None }, quote! { None }, quote! { None }),
            };

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
                    // Phase 4 Task 6 — `#[field(outbox = "ignore")]` toggles
                    // per-column outbox exclusion. The outbox helper walks
                    // `descriptor.fields` at emit time and strips any key
                    // flagged here from the JSONB payload.
                    outbox_exclude: #outbox_exclude,
                    sequence_within: #sequence_within_tokens,
                    index_type: #index_type_tokens,
                    // Phase 3 Task 2 — relation metadata emitted only for FK/O2O
                    // columns. Non-relation columns keep `None`/`&[]`.
                    relation_kind: #relation_kind_tokens,
                    on_delete: #on_delete_tokens,
                    target_type_name: #target_type_name_tokens,
                    visage_map: #projection_map_tokens,
                }
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

    // Phase 5 Task 14 — emit FtsDescriptor tokens when `#[model(fts = ...)]`
    // is set. Both `source` and `dictionary` are `&'static str` literals.
    let fts_tokens = fts_descriptor_tokens(&model_attrs.fts);
    // Phase 4 Task 7.5 — `#[model(idempotency_key = "column")]` emits the
    // column name into the descriptor so runtime consumers
    // (`create_or_find`, `bulk_upsert_by_descriptor`) can discover the
    // conflict key. Non-idempotent models keep `None`.
    let idempotency_key_tokens = match &model_attrs.idempotency_key {
        Some(col) => quote! { ::std::option::Option::Some(#col) },
        None => quote! { ::std::option::Option::None },
    };

    // Phase 5 Task 9 — `#[model(tenant_key = "col")]` wires the column name
    // into the descriptor AND emits a side-channel `target/djogi_rls/` SQL
    // file for the Phase 7 migration differ to consume.
    let tenant_key_tokens = match &model_attrs.tenant_key {
        Some(col) => quote! { ::std::option::Option::Some(#col) },
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

    // ── Phase 6 Task 2: implicit GiST indexes for GeoPoint fields ────────────
    //
    // For every user field whose Rust type is `GeoPoint` (or a path ending in
    // `GeoPoint`), emit one `IndexSpec` entry with:
    //   - name:       "<table>_<column>_gix"
    //   - columns:    &["<column>"]
    //   - unique:     false
    //   - index_type: IndexType::Gist
    //
    // The names are baked in as `'static str` string literals — they are
    // compile-time constants derived from the model attrs and field names.
    // No `Box::leak` is needed because the entire `inventory::submit!` block
    // is emitted as a single `static` initialiser; all nested `&[...]` slices
    // are literal arrays with `'static` lifetimes.
    //
    // `requires_out_of_transaction` and `extension_dependency` are the
    // T5-added `IndexSpec` fields that carry GiST-on-PostGIS migration
    // policy forward to Phase 7 without relying on type-name inference.
    let geopoint_index_specs: Vec<TokenStream> = user_fields
        .iter()
        .filter(|(field, _fa)| is_geopoint_type(&field.ty))
        .map(|(field, _fa)| {
            let raw_name = field.ident.as_ref().unwrap().to_string();
            let col = raw_name.strip_prefix("r#").unwrap_or(&raw_name).to_string();
            // Index name convention: "<table>_<column>_gix"
            let index_name = format!("{table_name}_{col}_gix");
            let col_str = col.as_str();
            quote! {
                ::djogi::descriptor::IndexSpec {
                    name: #index_name,
                    columns: &[#col_str],
                    unique: false,
                    index_type: ::djogi::descriptor::IndexType::Gist,
                    requires_out_of_transaction: true,
                    extension_dependency: ::std::option::Option::Some("postgis"),
                }
            }
        })
        .collect();

    // Emit the indexes slice. If there are no GeoPoint fields, the slice is
    // empty (`&[]`) — identical to the Phase 1 default. If there are one or
    // more GeoPoint fields, emit a `&[IndexSpec { … }, …]` literal.
    let indexes_tokens = if geopoint_index_specs.is_empty() {
        quote! { &[] }
    } else {
        quote! { &[ #(#geopoint_index_specs,)* ] }
    };

    quote! {
        ::djogi::__private::inventory::submit! {
            ::djogi::ModelDescriptor {
                type_name: #type_name,
                table_name: #table_name,
                pk_type: #pk_type_tokens,
                fields: &[
                    #(#all_field_descriptors,)*
                ],
                // Phase 1 defaults — populated by later phases' attr parsers.
                partition_by: None,
                // Phase 4 Task 6 — `#[model(table = "...", events)]` toggles
                // transactional outbox emission on every ctx-scoped
                // create/save/delete. `djogi::outbox::emit_event` keys off
                // this flag at codegen time.
                has_outbox: #has_outbox,
                idempotency_key: #idempotency_key_tokens,
                // Phase 5 Task 9 — RLS tenant discriminator column.
                tenant_key: #tenant_key_tokens,
                cache_ttl: None,
                rationale: None,
                // Phase 6 Task 2 — implicit GiST IndexSpec for every GeoPoint
                // field. Non-spatial models keep the empty slice default.
                indexes: #indexes_tokens,
                // Task 6 (phase3-relations): `#[model(table = "...", through)]`
                is_through: #is_through,
                // Phase 5 Task 14 — Full-Text Search.
                fts: #fts_tokens,
            }
        }
    }
}

/// Emit a side-channel `target/djogi_rls/{table}_rls.sql` file when the model
/// declares a `tenant_key`.
///
/// The emitted SQL contains two statements:
/// 1. `ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;`
/// 2. `CREATE POLICY {table}_tenant_isolation ON {table} USING (col = current_setting(...));`
///
/// The cast in the `USING` expression depends on the tenant column's SQL type:
/// - `BigInt` → `::bigint`
/// - `Uuid`   → `::uuid`
/// - `Text`   → no cast
/// - Any other type → compile error (via `proc_macro_error` note, non-fatal).
///
/// Phase 7's migration differ will consume this file. Until then, the file
/// serves as documentation and as an integration-test fixture the test can
/// verify was created.
///
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
        let raw = field.ident.as_ref()?.to_string();
        let name = raw.strip_prefix("r#").unwrap_or(&raw);
        if name == tenant_col {
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
                // Phase 7 compile-fail test will tighten this to a hard error.
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
        // or a typo. Emit with empty cast; Phase 7 will tighten validation.
        eprintln!(
            "djogi-macros: tenant_key value `{tenant_col}` does not match any \
             user-declared field on struct `{}`; check spelling.",
            struct_item.ident
        );
        ""
    };

    let sql = format!(
        "-- RLS DDL for {table} — generated by #[model(tenant_key = \"{tenant_col}\")].\n\
         -- Phase 7's migration differ consumes this file to apply RLS policies.\n\
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
///
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
    for (scope, peer) in &spec.relation_scopes {
        pairs.push((scope.clone(), peer.clone()));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let entries: Vec<TokenStream> = pairs.iter().map(|(s, e)| quote! { (#s, #e) }).collect();
    quote! { &[ #(#entries,)* ] }
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
        "NUMERIC" => quote! { ::djogi::FieldSqlType::Numeric },
        "UUID" => quote! { ::djogi::FieldSqlType::Uuid },
        "JSONB" => quote! { ::djogi::FieldSqlType::Jsonb },
        "TEXT[]" => quote! { ::djogi::FieldSqlType::TextArray },
        "INTEGER[]" => quote! { ::djogi::FieldSqlType::IntegerArray },
        "BIGINT[]" => quote! { ::djogi::FieldSqlType::BigIntArray },
        "BOOLEAN[]" => quote! { ::djogi::FieldSqlType::BoolArray },
        // Spatial — GeoPoint fields are mapped to GEOGRAPHY(Point, 4326) in
        // rust_type_to_sql. Emit the typed Geography variant so downstream
        // consumers (migration differ, djogi docs, admin UI) never need to
        // parse a Custom("...") string to discover the SRID.
        "GEOGRAPHY(Point, 4326)" => {
            quote! { ::djogi::FieldSqlType::Geography { srid: 4326u32 } }
        }
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

/// Detect if a field type is `GeoPoint` by checking the last-segment ident.
///
/// Returns `true` if the type path ends in `GeoPoint`, after stripping `Option`.
/// Uses last-segment path matching so all of the following are accepted:
/// - bare `GeoPoint`
/// - `djogi::GeoPoint`
/// - `geo::GeoPoint`
/// - `djogi::geo::GeoPoint`
/// - `Option<GeoPoint>` and the above qualified forms under `Option`
///
/// The `spatial` feature flag lives on the `djogi` runtime crate, not on
/// `djogi-macros`. The macro recognises the type name unconditionally so
/// it emits the correct descriptor regardless of feature state. If the
/// user omits the `spatial` feature, the compile error surfaces at the
/// struct definition as "unresolved type `GeoPoint`", not here.
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
///
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
/// - `GeoPoint` or `Geography<…>` → `IndexType::Gist`
/// - Everything else → `IndexType::BTree`
fn default_index_type(ty: &syn::Type) -> TokenStream {
    if is_jsonb_type(ty) {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gin) }
    } else if is_geopoint_type(ty) || is_geography_type(ty) {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::Gist) }
    } else {
        quote! { ::std::option::Option::Some(::djogi::descriptor::IndexType::BTree) }
    }
}

/// Emit the `fts: Option<FtsDescriptor>` token stream for the
/// `inventory::submit!` block.
///
/// When `spec` is `None`, emits `::std::option::Option::None`. When `Some`,
/// emits `::std::option::Option::Some(::djogi::descriptor::FtsDescriptor { ... })`.
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
                    // Default generated column name. Phase 8 will add a
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
}
