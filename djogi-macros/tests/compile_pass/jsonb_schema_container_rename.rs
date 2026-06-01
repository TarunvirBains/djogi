// Verifies that #[derive(JsonbSchema)] honours container-level
// #[serde(rename_all = "...")] — the generated accessor uses the
// case-converted JSON key, not the raw Rust field ident.
//
// Fix 2 (container rename_all): serde container-level rename support.
use djogi::prelude::*;
use djogi::JsonbSchema;
use serde::{Deserialize, Serialize};

// ── camelCase container rename ───────────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CamelSpec {
    pub engine_type: i32,
    pub weight_kg: f32,
}

// ── kebab-case container rename ──────────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct KebabStructure {
    pub first_field: i32,
    pub second_value: f64,
}

// ── SCREAMING_SNAKE_CASE container rename ────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct ScreamingSpec {
    pub engine_type: i32,
    pub max_torque: f64,
}

// ── Field-level rename takes priority over container rename ──────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MixedRenames {
    // Field-level rename wins — JSON key is "engine" not "engineField".
    #[serde(rename = "engine")]
    pub engine_field: i32,
    // No field-level rename — container rename applies.
    pub weight_kg: f32,
}

// ── No rename — container rename_all absent, no field renames ───────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct NoRename {
    pub plain_field: i32,
}

// ── Models ───────────────────────────────────────────────────────────────────

#[model(table = "camel_specs")]
#[derive(Debug, Clone)]
pub struct CamelSpecModel {
    pub spec: Jsonb<CamelSpec>,
}

#[model(table = "kebab_structures")]
#[derive(Debug, Clone)]
pub struct KebabModel {
    pub data: Option<Jsonb<KebabStructure>>,
}

#[model(table = "mixed_rename_models")]
#[derive(Debug, Clone)]
pub struct MixedModel {
    pub info: Jsonb<MixedRenames>,
}

// ── Verify typed paths compile with container-renamed fields ─────────────────

fn _check_camel_path_compiles() {
    // engine_type (Rust) → engineType (JSON key via camelCase rename_all)
    let _path = |f: CamelSpecModelFields| f.spec().explicit_pg_predicate().typed().engine_type().gt(4);
    // weight_kg (Rust) → weightKg (JSON key via camelCase rename_all)
    let _weight = |f: CamelSpecModelFields| f.spec().explicit_pg_predicate().typed().weight_kg().gt(500.0_f32);
}

fn _check_kebab_path_compiles() {
    // first_field (Rust) → first-field (JSON key via kebab-case rename_all)
    let _path = |f: KebabModelFields| f.data().explicit_pg_predicate().typed().first_field().gt(0);
}

fn _check_mixed_path_compiles() {
    // engine_field (Rust) → "engine" (field-level rename wins over container rename)
    let _path = |f: MixedModelFields| f.info().explicit_pg_predicate().typed().engine_field().gt(0);
    // weight_kg (Rust) → "weightKg" (container camelCase applies)
    let _weight = |f: MixedModelFields| f.info().explicit_pg_predicate().typed().weight_kg().gt(0.0_f32);
}

fn main() {}
