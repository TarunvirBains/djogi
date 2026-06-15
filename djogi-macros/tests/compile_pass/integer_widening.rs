// djogi#190: u8 / u16 / u64 (and i8 / u32 from
// djogi#186) are valid `#[model]` field types.
//
// These five Rust types do not have `tokio_postgres::ToSql` / `FromSql`
// impls that target the correct Postgres column type, so the `#[model]`
// macro emits bind / decode shims:
//
// i8 → SMALLINT : bind as i16::from(v), decode via i8::try_from(i16)
// u8 → SMALLINT : bind as i16::from(v), decode via u8::try_from(i16)
// u16 → INTEGER : bind as i32::from(v), decode via u16::try_from(i32)
// u32 → BIGINT : bind as i64::from(v), decode via u32::try_from(i64)
// u64 → NUMERIC  : bind as Decimal::from(v), decode via to_u64()
//
// The migration projection layer additionally emits a range CHECK on each
// column (`RustSourceType` discriminator on `FieldDescriptor`).
//
// This fixture only verifies that the macro expands without compiler errors
// for all five types in their direct, Option<T>, and Tracked<T> variants.
// Round-trip correctness is exercised by the live integration test
// `tests/internal/phase8_5_c2_190_integer_widening.rs`.

use djogi::prelude::*;

// ── All five narrowed types as plain scalar fields ───────────────────────────

#[model(table = "narrow_ints", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct NarrowInts {
 pub signed_byte: i8,
 pub unsigned_byte: u8,
 pub unsigned_short: u16,
 pub unsigned_int: u32,
 pub unsigned_long: u64,
 // Direct-mapped types must keep working alongside widened ones.
 pub regular_i16: i16,
 pub regular_i32: i32,
 pub regular_i64: i64,
 pub label: String,
}

// ── Nullable variants ─────────────────────────────────────────────────────────

#[model(table = "narrow_ints_nullable", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct NarrowIntsNullable {
 pub signed_byte: Option<i8>,
 pub unsigned_byte: Option<u8>,
 pub unsigned_short: Option<u16>,
 pub unsigned_int: Option<u32>,
 pub unsigned_long: Option<u64>,
 pub label: String,
}

// ── Tracked variants ──────────────────────────────────────────────────────────

#[model(table = "narrow_ints_tracked", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct NarrowIntsTracked {
 pub signed_byte: Tracked<i8>,
 pub unsigned_byte: Tracked<u8>,
 pub unsigned_short: Tracked<u16>,
 pub unsigned_int: Tracked<u32>,
 pub unsigned_long: Tracked<u64>,
 pub label: String,
}

fn main() {}
