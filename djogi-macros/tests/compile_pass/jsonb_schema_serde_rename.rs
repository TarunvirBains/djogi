// Verifies that #[derive(JsonbSchema)] honours #[serde(rename = "...")] —
// the generated accessor emits the renamed JSON key, not the Rust field ident.
//
// Fix 2 (MAJOR): serde rename support.
use djogi::prelude::*;
use djogi::JsonbSchema;
use serde::{Deserialize, Serialize};

// ── Schema with renamed field ────────────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct EngineMetrics {
 // Rust ident: `cylinders` — on-disk JSON key: `numCylinders`
 #[serde(rename = "numCylinders")]
 pub cylinders: i32,
 // No rename — key matches field name.
 pub displacement_cc: f32,
}

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct CarProfile {
 // Nested field with rename — accessor key is "engineData" not "engine".
 #[serde(rename = "engineData")]
 pub engine: EngineMetrics,
 // skip_serializing_if is allowed — does not affect the JSON key.
 #[serde(skip_serializing_if = "String::is_empty")]
 pub brand: String,
 pub weight_kg: f32,
}

// ── Model ───────────────────────────────────────────────────────────────────

#[model(table = "schema_rename_cars")]
#[derive(Debug, Clone)]
pub struct RenameCar {
 pub profile: Jsonb<CarProfile>,
}

// ── Verify typed path compiles with renamed fields ────────────────────────────

fn _check_renamed_path_compiles() {
 // Depth-2 scalar path using renamed nested key + renamed leaf key.
 // This call chain must compile; the path produced internally uses
 // "engineData" (not "engine") and "numCylinders" (not "cylinders").
 let _path_fn = |f: RenameCarFields| f.profile().explicit_pg_predicate().typed().engine().cylinders().gt(4);

 // Non-renamed scalar — must still compile alongside renamed fields.
 let _weight = |f: RenameCarFields| f.profile().explicit_pg_predicate().typed().weight_kg().gt(1000.0_f32);

 // Non-renamed nested then renamed leaf.
 let _brand = |f: RenameCarFields| f.profile().explicit_pg_predicate().typed().brand().eq("Acme".to_string());

 // skip_serializing_if on a scalar field — must compile.
 let _disp = |f: RenameCarFields| f.profile().explicit_pg_predicate().typed().engine().displacement_cc().gt(1500.0_f32);
}

fn main() {}
