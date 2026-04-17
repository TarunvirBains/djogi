//! Generates `inventory::submit!(ModelDescriptor {...})` from the `#[model]`
//! struct definition. Runs AFTER `inject::expand` has mutated `struct_item`,
//! but iterates user-only fields by skipping the N framework fields at the
//! front (3 for heerid/ranjid/serial, 2 for pk=none).
//!
//! The emitted submission uses Phase 1 defaults for every amended field
//! (partition_by, has_outbox, idempotency_key, tenant_key, cache_ttl,
//! rationale, indexes) — those attrs are populated by later phases' parser
//! extensions. Per-field defaults (rationale, outbox_exclude, index_type)
//! follow the same convention.

use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, rust_type_to_sql, unwrap_option};
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

    let field_descriptors: Vec<TokenStream> = user_fields
        .iter()
        .map(|(field, fa)| {
            let name = field.ident.as_ref().unwrap().to_string();
            let (inner_ty, nullable) = unwrap_option(&field.ty);
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
                }
            }
        })
        .collect();

    quote! {
        ::inventory::submit! {
            ::djogi::ModelDescriptor {
                type_name: #type_name,
                table_name: #table_name,
                pk_type: #pk_type_tokens,
                fields: &[
                    #(#field_descriptors,)*
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
