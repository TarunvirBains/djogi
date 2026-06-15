// `#[derive(JsonbSchema)]` accepts narrow integers
// (closes GH issue #29).
//
// Pre-#29 the SCALAR_TYPE_PATTERNS allowlist in `djogi-macros::jsonb_schema`
// recognised `i16` / `i32` / `i64` / `u64` only. Common Rust types like
// `u16` (port numbers, small counts) were treated as nested schemas and
// the derive failed to resolve `u16: JsonbSchema`. Adopters had to
// manually upcast every narrow field to `i32`.
//
// The fix:
// 1. Extend SCALAR_TYPE_PATTERNS with `i8` / `u8` / `u16` / `u32`.
// 2. Extend `jsonb_sql_cast_for_type` (and its `#[cfg(test)]` shim
// `sql_cast_for_type`) with the same set, each widening to
// the smallest signed Postgres int that fits the full range:
// - `i8` → `int2`
// - `u8` → `int2` (u8 max 255 fits in i16)
// - `u16` → `int4` (u16 max 65535 exceeds i16)
// - `u32` → `int8` (u32 max ~4.3B exceeds i32)
// 3. Extend `IntoFilterValue` with the same widening on the binding
// side so `.eq(80u16)` etc. compile.
//
// `u64` was already present in SCALAR_TYPE_PATTERNS before #29 but lacked
// a `JsonbSqlCast` mapping; djogi#161 completed that by adding
// `u64 => JsonbSqlCast::Numeric` (`::numeric`). Boundary: #29 covers the
// new narrow types (i8 / u8 / u16 / u32); #161 wired u64 to `::numeric`.

use djogi::JsonbSchema;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(JsonbSchema, Serialize, Deserialize, Default, Debug, Clone)]
pub struct ServerProfile {
 pub port: u16,     // closes #29 — the headline narrow type
 pub priority: i8,    // signed-byte
 pub retry_count: u8,   // small unsigned
 pub bytes_received: u32,  // wider unsigned
}

#[model(table = "phase7_zero2_jsonb_narrow_ints_servers")]
#[derive(Debug, Clone)]
pub struct Server {
 pub profile: Jsonb<ServerProfile>,
}

#[allow(dead_code)]
fn _u16_path_compiles() {
 // Direct equality on a u16 JSONB field — the headline acceptance test.
 let _f1 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().port().eq(80u16);
 let _f2 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().port().gt(1024u16);
 let _f3 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().port().is_not_null();
}

#[allow(dead_code)]
fn _i8_path_compiles() {
 let _f1 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().priority().eq(-1i8);
 let _f2 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().priority().gt(0i8);
}

#[allow(dead_code)]
fn _u8_path_compiles() {
 let _f1 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().retry_count().eq(3u8);
 let _f2 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().retry_count().lt(10u8);
}

#[allow(dead_code)]
fn _u32_path_compiles() {
 let _f1 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().bytes_received().eq(1_000u32);
 let _f2 = |f: ServerFields| f.profile().explicit_pg_predicate().typed().bytes_received().gt(0u32);
}

fn main() {}
