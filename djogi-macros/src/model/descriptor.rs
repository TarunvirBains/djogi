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
                index_type: None,
                // Framework columns carry no relation metadata — `id` is a
                // PK, not an FK, and Phase 4.5's projection hookup lands
                // per-user-field only.
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                projection_map: &[],
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
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                projection_map: &[],
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
                index_type: None,
                relation_kind: None,
                on_delete: None,
                target_type_name: None,
                projection_map: &[],
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
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            projection_map: &[],
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
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            projection_map: &[],
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
                    // Phase 1 defaults — populated by later phases' attr parsers.
                    rationale: None,
                    outbox_exclude: false,
                    index_type: None,
                    // Phase 3 Task 2 — relation metadata emitted only for FK/O2O
                    // columns. Non-relation columns keep `None`/`&[]`.
                    relation_kind: #relation_kind_tokens,
                    on_delete: #on_delete_tokens,
                    target_type_name: #target_type_name_tokens,
                    projection_map: &[],
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
                has_outbox: false,
                idempotency_key: None,
                tenant_key: None,
                cache_ttl: None,
                rationale: None,
                indexes: &[],
            }
        }
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
