// djogi#161 — `#[jsonb(scalar)]` escape hatch on `#[derive(JsonbSchema)]`.
//
// Adopter-defined scalar types — for example, a `primary_key!`-emitted
// custom PK newtype or a project-local newtype that wraps a built-in
// scalar — sit outside the built-in cast-matrix allowlist. The
// `#[jsonb(scalar)]` field-level annotation tells the derive to emit a
// `JsonbPathRef<M, FieldType>` leaf instead of treating the field type
// as a nested `JsonbSchema`.
//
// The annotation does NOT accept SQL cast text. Postgres cast selection
// flows through `FieldType: IntoFilterValue`'s `jsonb_sql_cast` method
// (returning a typed `JsonbSqlCast` enum variant), not adopter-supplied
// strings. Per djogi#161, `primary_key!`-emitted custom PK newtypes
// delegate their cast through to the inner SQL value type, so a
// `#[jsonb(scalar)]` field of type `MyAppId(i64)` emits the same
// `::int8` cast a bare `i64` field would emit.

use djogi::prelude::*;
use serde::{Deserialize, Serialize};

// Adopter-declared custom PK newtype. The `primary_key!` macro emits a
// transparent newtype over `i64`, plus `IntoFilterValue` delegated to
// `i64`. JSONB cast metadata flows through the same delegation chain.
djogi::primary_key! {
 pub struct OwnerId(i64);
 sql_type = "BIGINT";
 default_sql = "0";
 bulk_sql = "SELECT 0::bigint AS id FROM generate_series(1, $1)";
}

#[derive(djogi::JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct Spec {
 /// Adopter-defined scalar — opted in to leaf treatment via
 /// `#[jsonb(scalar)]`. Without the marker the derive would attempt
 /// `OwnerId: JsonbSchema` resolution and fail at compile time.
 #[jsonb(scalar)]
 pub owner_id: OwnerId,
 /// Built-in allowlist scalar — no marker needed.
 pub displacement_cc: f32,
 /// Built-in allowlist scalar — no marker needed.
 pub brand: String,
}

#[model(table = "phase85_jsonb_scalar_escape_hatch_specs")]
#[derive(Debug, Clone)]
pub struct Vehicle {
 pub spec: Jsonb<Spec>,
}

#[allow(dead_code)]
fn _scalar_escape_hatch_compiles() {
 // The marker exposes a `JsonbPathRef<Vehicle, OwnerId>` leaf with
 // the full comparison surface. The cast metadata (`::int8`) is
 // selected by `<OwnerId as IntoFilterValue>::jsonb_sql_cast` which
 // delegates to `<i64 as IntoFilterValue>::jsonb_sql_cast` =
 // `JsonbSqlCast::Int8` — NOT by adopter-supplied SQL text.
 let _eq = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .owner_id()
  .eq(OwnerId(42))
 };
 let _gt = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .owner_id()
  .gt(OwnerId(0))
 };
 let _in = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .owner_id()
  .in_list(vec![OwnerId(1), OwnerId(2), OwnerId(3)])
 };
 let _is_null = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .owner_id()
  .is_null()
 };

 // Built-in allowlist scalars continue to work without the marker.
 let _f32 = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .displacement_cc()
  .gt(1500.0_f32)
 };
 let _string = |f: VehicleFields| {
  f.spec()
  .explicit_pg_predicate()
  .typed()
  .brand()
  .eq("Acme".to_string())
 };
}

fn main() {}
