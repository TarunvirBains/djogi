// Verifies that #[derive(JsonbSchema)] compiles for the canonical use cases:
// 1. A nested struct with mixed scalar + nested fields.
// 2. A flat struct with only scalar fields.
// 3. An empty named struct (zero fields).
// 4. The.explicit_pg_predicate().typed() bridge on FieldRef<M, Jsonb<T>> compiles.
//
// Field accesses on {T}Path<M> use method-call syntax with `()` — the
// derive emits methods, not struct fields.
use djogi::prelude::*;
use djogi::JsonbSchema;
use serde::{Deserialize, Serialize};

// ── Case 1: nested struct with mixed scalar + nested fields ──────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct EngineSpecs {
 pub cylinders: i32,
 pub displacement_cc: f32,
 pub turbo: bool,
}

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct VehicleSpecs {
 pub engine: EngineSpecs,
 pub weight_kg: f32,
 pub brand: String,
}

// ── Case 2: flat scalar-only struct ─────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct PostMeta {
 pub view_count: i64,
 pub rating: f64,
 pub published: bool,
 pub slug: String,
}

// ── Case 3: empty named struct ───────────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct EmptySchema {}

// ── Case 4: model with Jsonb<T: JsonbSchema> uses.explicit_pg_predicate().typed() ──────────────────

#[model(table = "schema_cars")]
#[derive(Debug, Clone)]
pub struct Car {
 pub specs: Jsonb<VehicleSpecs>,
 pub meta: Option<Jsonb<PostMeta>>,
}

fn _check_typed_path_compiles() {
 //.explicit_pg_predicate().typed() returns VehicleSpecsPath<Car> — the tree is navigable.
 // Field accesses use method-call syntax with `()`.

 // Depth-2 scalar: specs.engine().cylinders()
 let _path_fn = |f: CarFields| {
  f.specs()
  .explicit_pg_predicate().typed()
  .engine()
  .cylinders()
  .gt(4)
 };

 // Depth-1 scalar: specs.weight_kg()
 let _path_fn2 = |f: CarFields| f.specs().explicit_pg_predicate().typed().weight_kg().gt(1000.0_f32);

 // Optional JSONB also has.explicit_pg_predicate().typed()
 let _path_fn3 = |f: CarFields| f.meta().explicit_pg_predicate().typed().view_count().gt(100_i64);

 // Depth-1 string
 let _path_fn4 = |f: CarFields| f.specs().explicit_pg_predicate().typed().brand().eq("Acme".to_string());

 // bool
 let _path_fn5 = |f: CarFields| f.specs().explicit_pg_predicate().typed().engine().turbo().eq(true);
}

fn main() {}
