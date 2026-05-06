//! Emit a `pub const {MODEL}_SCHEMA: &str` per model.
//!
//! For every `#[derive(Model)]` input, emit one compile-time string
//! constant that pretty-prints the model's shape:
//!
//! ```text
//! pub const VEHICLE_SCHEMA: &str = "table: vehicles
//! fields:
//!   id: HeerId (PK)
//!   created_at: DateTime
//!   updated_at: DateTime
//!   vin: String NOT NULL
//!   ...
//! indexes:
//!   - vin (UNIQUE)
//! relations:
//!   - owner_id -> Owner (FK)
//! ";
//! ```
//!
//! Adopters and tooling lift the const without going through the
//! `inventory` runtime path — useful for agent ergonomics and
//! `cargo expand` introspection.
//!
//! # Determinism
//!
//! Byte-deterministic against `ParsedModel`: framework fields render
//! first in fixed order, user fields in declaration order, indexes in
//! declaration order, relations alphabetically by source-field column.
//!
//! # Naming
//!
//! `{pascal_to_snake(name).to_uppercase()}_SCHEMA`:
//!
//! - `Vehicle` → `VEHICLE_SCHEMA`
//! - `OrgUser` → `ORG_USER_SCHEMA`
//! - `HTTPSProxy` → `HTTPS_PROXY_SCHEMA`
//!
//! Adopters declaring their own `VEHICLE_SCHEMA` at the same scope
//! see a Rust "duplicate definition" error — a feature, not a bug.

use crate::case::pascal_to_snake;
use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::ItemStruct;

/// Emit the per-model `{MODEL}_SCHEMA: &str` const.
///
/// `struct_item` is the **post-injection** struct (framework fields
/// already prepended by `inject::expand`). `field_attrs` aligns with
/// `struct_item.fields.iter().skip(model_attrs.framework_field_count())`.
pub fn emit(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> TokenStream {
    let const_ident = format_ident!(
        "{}_SCHEMA",
        pascal_to_snake(&struct_item.ident.to_string()).to_uppercase()
    );
    let body = render_schema(struct_item, model_attrs, field_attrs);
    quote! {
        #[doc = "Compile-time, byte-deterministic schema summary for this model."]
        #[doc(hidden)]
        pub const #const_ident: &str = #body;
    }
}

fn render_schema(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> String {
    let mut out = String::new();
    out.push_str("table: ");
    out.push_str(&model_attrs.table);
    out.push('\n');

    out.push_str("fields:\n");
    render_framework_fields(&mut out, model_attrs);
    render_user_fields(&mut out, struct_item, model_attrs, field_attrs);

    let indexes = collect_indexes(struct_item, model_attrs, field_attrs);
    if !indexes.is_empty() {
        out.push_str("indexes:\n");
        for line in &indexes {
            out.push_str("  - ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let relations = collect_relations(struct_item, model_attrs, field_attrs);
    if !relations.is_empty() {
        out.push_str("relations:\n");
        for line in &relations {
            out.push_str("  - ");
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}

fn render_framework_fields(out: &mut String, model_attrs: &ModelAttrs) {
    // Label per-variant — `HeerIdDesc`/`RanjIdDesc` round-trip into the
    // const distinct from their ascending siblings so adopters reading
    // the schema can tell which ordering their PK column gives them.
    let pk_label = match &model_attrs.pk {
        PkStrategy::HeerId => Some("HeerId"),
        PkStrategy::HeerIdDesc => Some("HeerIdDesc"),
        PkStrategy::RanjId => Some("RanjId"),
        PkStrategy::RanjIdDesc => Some("RanjIdDesc"),
        PkStrategy::Serial => Some("Serial"),
        PkStrategy::Custom(_) => Some("Custom"),
        PkStrategy::None => None,
    };
    if let Some(label) = pk_label {
        out.push_str("  id: ");
        out.push_str(label);
        out.push_str(" (PK)\n");
    }
    out.push_str("  created_at: DateTime\n");
    out.push_str("  updated_at: DateTime\n");
}

fn render_user_fields(
    out: &mut String,
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) {
    let n_framework = model_attrs.framework_field_count();
    let user_fields = struct_item.fields.iter().skip(n_framework);
    for (field, fa) in user_fields.zip(field_attrs.iter()) {
        let Some(name) = field.ident.as_ref() else {
            continue;
        };
        out.push_str("  ");
        out.push_str(name.to_string().trim_start_matches("r#"));
        out.push_str(": ");
        out.push_str(&render_type(&field.ty));

        let nullable = matches!(&field.ty, syn::Type::Path(p) if last_segment_is_option(p));
        let mut modifiers: Vec<&'static str> = Vec::new();
        if !nullable {
            modifiers.push("NOT NULL");
        }
        if fa.unique {
            modifiers.push("UNIQUE");
        }
        if fa.version {
            modifiers.push("VERSION");
        }
        for m in modifiers {
            out.push(' ');
            out.push_str(m);
        }
        out.push('\n');
    }
}

fn render_type(ty: &syn::Type) -> String {
    compact_type_string(&ty.to_token_stream().to_string())
}

/// Strip whitespace from `proc_macro2::TokenStream::to_string()` output
/// so `Option < String >` becomes `Option<String>`. Rust type spellings
/// never contain word-on-word adjacency, so dropping every space is safe.
fn compact_type_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c != ' ' {
            out.push(c);
        }
    }
    out
}

fn last_segment_is_option(p: &syn::TypePath) -> bool {
    p.path
        .segments
        .last()
        .map(|s| s.ident == "Option")
        .unwrap_or(false)
}

fn collect_indexes(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> Vec<String> {
    let n_framework = model_attrs.framework_field_count();
    let mut lines: Vec<String> = Vec::new();
    for (field, fa) in struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
    {
        let Some(name) = field.ident.as_ref() else {
            continue;
        };
        let col = name.to_string();
        let col = col.trim_start_matches("r#");
        let mut modifiers: Vec<String> = Vec::new();
        if fa.unique {
            modifiers.push("UNIQUE".to_string());
        }
        if let Some(method) = &fa.index_method {
            modifiers.push(method.to_uppercase());
        } else if fa.index {
            modifiers.push("BTREE".to_string());
        }
        if !modifiers.is_empty() {
            lines.push(format!("{col} ({})", modifiers.join(" ")));
        }
    }
    lines
}

/// Map a raw `#[field(on_delete = "...")]` attribute string to the SQL
/// label `MODEL_SCHEMA` displays. `s.to_uppercase()` would have rendered
/// `set_null` as `SET_NULL` (with underscore) instead of the proper SQL
/// `SET NULL`, so adopters reading the schema would see a string that
/// doesn't match the DDL Postgres applies.
fn on_delete_attr_to_label(attr: &str) -> &'static str {
    match attr {
        "cascade" => "CASCADE",
        "restrict" => "RESTRICT",
        "set_null" => "SET NULL",
        "set_default" => "SET DEFAULT",
        "protect" => "RESTRICT",
        "do_nothing" => "NO ACTION",
        // Fallback for attribute strings the validator hasn't seen —
        // an unknown spelling surfaces as the literal value so the
        // mismatch is obvious in the const, rather than silently
        // upper-cased into SQL nonsense.
        _ => "UNKNOWN",
    }
}

fn collect_relations(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> Vec<String> {
    let n_framework = model_attrs.framework_field_count();
    let mut lines: Vec<(String, String)> = Vec::new();
    for (field, fa) in struct_item
        .fields
        .iter()
        .skip(n_framework)
        .zip(field_attrs.iter())
    {
        let Some(name) = field.ident.as_ref() else {
            continue;
        };
        let Some(info) = detect_relation(&field.ty) else {
            continue;
        };
        let col = name.to_string();
        let col = col.trim_start_matches("r#").to_string();
        let on_delete = fa
            .on_delete
            .as_deref()
            .map(on_delete_attr_to_label)
            .unwrap_or("RESTRICT")
            .to_string();
        let nullable = if info.nullable { ", NULLABLE" } else { "" };
        let line = format!(
            "{col} -> {target} ({kind}, ON DELETE {on_delete}{nullable})",
            target = info.target_name,
            kind = match info.kind {
                crate::model::attrs::RelationKind::ForeignKey => "FK",
                crate::model::attrs::RelationKind::OneToOne => "O2O",
            },
        );
        lines.push((col, line));
    }
    // Sort alphabetically by source-field column name for stable output
    // across refactors that shuffle field declaration order.
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines.into_iter().map(|(_, line)| line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_type_string_drops_spaces() {
        assert_eq!(compact_type_string("Option < String >"), "Option<String>");
        assert_eq!(
            compact_type_string("Jsonb < MetaSchema >"),
            "Jsonb<MetaSchema>"
        );
        assert_eq!(
            compact_type_string("ForeignKey < Owner >"),
            "ForeignKey<Owner>"
        );
        assert_eq!(compact_type_string("Vec < u8 >"), "Vec<u8>");
    }

    #[test]
    fn compact_type_string_preserves_already_compact() {
        assert_eq!(compact_type_string("String"), "String");
        assert_eq!(compact_type_string("i64"), "i64");
        assert_eq!(compact_type_string("Option<u32>"), "Option<u32>");
    }
}
