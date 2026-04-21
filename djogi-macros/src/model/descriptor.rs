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
    FieldAttrs, ModelAttrs, PkStrategy, RelationKind as MacroRelationKind, detect_relation,
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
                // PK, not an FK, and Phase 4.5's projection hookup lands
                // per-user-field only.
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                projection_map: &[
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
                projection_map: &[
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
                projection_map: &[
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
            projection_map: &[
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
            projection_map: &[
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
            let indexed = fa.index;
            let max_length = match fa.max_length {
                Some(n) => quote! { Some(#n) },
                None => quote! { None },
            };
            let renamed_from = match &fa.renamed_from {
                Some(s) => quote! { Some(#s) },
                None => quote! { None },
            };

            // Phase 4.5 — populate projection_map from the parsed
            // `#[field(expose(...))]` spec. Scalar scopes map to this
            // column's name; relation scopes map to the peer projection
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
                    index_type: None,
                    // Phase 3 Task 2 — relation metadata emitted only for FK/O2O
                    // columns. Non-relation columns keep `None`/`&[]`.
                    relation_kind: #relation_kind_tokens,
                    on_delete: #on_delete_tokens,
                    target_type_name: #target_type_name_tokens,
                    projection_map: #projection_map_tokens,
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
    // Phase 4 Task 7.5 — `#[model(idempotency_key = "column")]` emits the
    // column name into the descriptor so runtime consumers
    // (`create_or_find`, `bulk_upsert_by_descriptor`) can discover the
    // conflict key. Non-idempotent models keep `None`.
    let idempotency_key_tokens = match &model_attrs.idempotency_key {
        Some(col) => quote! { ::std::option::Option::Some(#col) },
        None => quote! { ::std::option::Option::None },
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
                tenant_key: None,
                cache_ttl: None,
                rationale: None,
                indexes: &[],
                // Task 6 (phase3-relations): `#[model(table = "...", through)]`
                is_through: #is_through,
            }
        }
    }
}

/// Emit a `&'static [(&'static str, &'static str)]` literal from an
/// `ExposeSpec`. Scalar scope entries map scope → column name; relation
/// scope entries map scope → peer projection type name. Empty / suppressed
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
        other => {
            let s = other.to_string();
            quote! { ::djogi::FieldSqlType::Custom(#s) }
        }
    }
}
