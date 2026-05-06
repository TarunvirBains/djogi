//! Cluster 8ζ T12.1 — emit a `pub const {MODEL}_SCHEMA: &str` per model.
//!
//! # What
//!
//! For every `#[derive(Model)]` (equivalently `#[model(...)]`) input,
//! emit one compile-time string constant that pretty-prints the model's
//! shape:
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
//! Adopters and tooling can lift the const without going through any
//! runtime path — useful for agent ergonomics (LLMs, schema browsers,
//! `djogi schema` CLI fallback) and for `cargo expand` introspection.
//!
//! # Why a const, not a runtime descriptor lookup
//!
//! The `ModelDescriptor` lives behind `inventory::iter::<&'static
//! ModelDescriptor>()` and requires the `inventory` linker dance to
//! enumerate. Tools that want a model's shape *at compile time*
//! (proc-macro pretty-printers, doc-generators, agents) need a stable
//! const reference. Emitting one removes the runtime requirement and
//! sidesteps the `inventory` linker quirks that occur when adopter
//! crates use `#[link]`-stripping `--gc-sections`.
//!
//! # Determinism contract
//!
//! The emitted string is **byte-deterministic** against `ParsedModel`:
//!
//! - User fields render in declaration order (the order Phase 1's
//!   inject pass preserves through `struct_item.fields`).
//! - Framework fields (`id`, `created_at`, `updated_at`) render at the
//!   top in fixed order.
//! - Indexes render in the order they appear on user fields (declaration
//!   order again).
//! - Relations render alphabetically by source-field column name (the
//!   spec calls for an explicit sort to keep the output stable across
//!   refactors that shuffle field order).
//!
//! Two macro invocations on the same input produce byte-equal const
//! values. `schema_const_is_byte_deterministic` pins this in
//! `djogi-macros/tests/`.
//!
//! # Naming
//!
//! `{MODEL_NAME_UPPER_SNAKE}_SCHEMA`. Conversion is
//! `pascal_to_snake(name).to_uppercase()`. Examples:
//!
//! - `Vehicle` → `VEHICLE_SCHEMA`
//! - `OrgUser` → `ORG_USER_SCHEMA`
//! - `HTTPSProxy` → `HTTPS_PROXY_SCHEMA`
//!
//! The `{MODEL}_SCHEMA` suffix is a reserved prefix, like `{MODEL}Fields`
//! and `{MODEL}Filter` from Phase 1 — adopters who declare their own
//! `VEHICLE_SCHEMA` const at the same scope will see a "duplicate
//! definition" error from the Rust compiler, which is a feature, not
//! a bug.

use crate::case::pascal_to_snake;
use crate::model::attrs::{FieldAttrs, ModelAttrs, PkStrategy, detect_relation};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::ItemStruct;

/// Emit the per-model `{MODEL}_SCHEMA: &str` const.
///
/// # Arguments
///
/// - `struct_item` — the **post-injection** struct (framework fields
///   already prepended by `inject::expand`).
/// - `model_attrs` — parsed `#[model(...)]` attributes; supplies
///   `table` and `pk`.
/// - `field_attrs` — per-user-field attribute metadata, aligned by
///   index with `struct_item.fields.iter().skip(n_framework)`.
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
        #[doc = ""]
        #[doc = "Cluster 8ζ T12.1 — agent ergonomics. The string is derived"]
        #[doc = "from the model's `ModelDescriptor` and is stable across"]
        #[doc = "macro invocations on the same input."]
        #[doc(hidden)]
        pub const #const_ident: &str = #body;
    }
}

/// Render the schema body. Pure string assembly — no token tricks, so
/// the output is straightforward to unit-test inside the proc macro
/// crate without expanding tokens.
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
    let pk_label = match &model_attrs.pk {
        PkStrategy::HeerId | PkStrategy::HeerIdDesc => Some("HeerId"),
        PkStrategy::RanjId | PkStrategy::RanjIdDesc => Some("RanjId"),
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
    let n_framework = framework_field_count(model_attrs);
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
    // Token-stream → compact string. Strip whitespace produced by
    // `proc_macro2::TokenStream::to_string` between every adjacent
    // token so `Option < String >` becomes `Option<String>` and
    // `Jsonb < MetaSchema >` becomes `Jsonb<MetaSchema>`.
    let raw = ty.to_token_stream().to_string();
    compact_type_string(&raw)
}

fn compact_type_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_was_word_char = false;
    for c in raw.chars() {
        if c == ' ' {
            // Drop the space if either neighbour is non-word
            // (punctuation like `<`, `>`, `,`). Keep it only between
            // two word-like tokens, which doesn't happen in any Rust
            // type spelling we'd see here, so this collapses every
            // space.
            continue;
        }
        let is_word = c.is_ascii_alphanumeric() || c == '_';
        out.push(c);
        prev_was_word_char = is_word;
    }
    let _ = prev_was_word_char; // keep variable for future expansion
    out
}

fn last_segment_is_option(p: &syn::TypePath) -> bool {
    p.path
        .segments
        .last()
        .map(|s| s.ident == "Option")
        .unwrap_or(false)
}

fn framework_field_count(model_attrs: &ModelAttrs) -> usize {
    match model_attrs.pk {
        PkStrategy::None => 2,
        _ => 3,
    }
}

fn collect_indexes(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> Vec<String> {
    let n_framework = framework_field_count(model_attrs);
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

fn collect_relations(
    struct_item: &ItemStruct,
    model_attrs: &ModelAttrs,
    field_attrs: &[FieldAttrs],
) -> Vec<String> {
    let n_framework = framework_field_count(model_attrs);
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
            .map(|s| s.to_uppercase())
            .unwrap_or_else(|| "RESTRICT".to_string());
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
