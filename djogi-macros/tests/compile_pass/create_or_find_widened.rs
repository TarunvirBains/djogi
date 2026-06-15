// djogi#190 create_or_find with widened field types.
//
// Verifies that `create_or_find` compiles correctly for models that contain
// narrow/unsigned integer fields (i8/u8/u16/u32/u64), including the case where
// the idempotency key field itself is a widened type.
//
// Previously, `create_or_find` built its INSERT params and fallback SELECT
// param as a direct `&[&(dyn ToSql + Sync)]` slice, bypassing the
// `create_param_tokens` shims that every other CRUD path uses. This caused a
// type-mismatch failure at bind time for widened types.
//
// This fixture exercises:
// 1. A model with widened non-key fields — the INSERT params must go through
// the appropriate widening shim (e.g. `u64 → Decimal::from(v)`).
// 2. A model where the idempotency key field itself is a widened type (u32) —
// the fallback SELECT `WHERE key = $1` param must also be widened.

use djogi::prelude::*;

// ── Model with widened non-key fields and a direct-typed key ─────────────────

#[model(table = "cof_widened_nonkey", pk = HeerId, idempotency_key = "request_id")]
#[derive(Debug, Clone)]
pub struct CofWidenedNonKey {
 /// Direct-typed idempotency key (String).
 pub request_id: String,
 /// Widened non-key fields — each exercises a different shim path.
 pub byte_count: u8,
 pub port_number: u16,
 pub file_size: u32,
 pub byte_offset: u64,
 pub score: i8,
 pub label: String,
}

// ── Model where the idempotency key itself is a widened type ─────────────────

#[model(table = "cof_widened_key", pk = HeerId, idempotency_key = "external_ref")]
#[derive(Debug, Clone)]
pub struct CofWidenedKey {
 /// Widened idempotency key — u32 binds as i64.
 pub external_ref: u32,
 /// A widened non-key field alongside the widened key.
 pub value: u64,
 pub label: String,
}

// ── Model with Option-wrapped widened fields ──────────────────────────────────

#[model(table = "cof_widened_nullable", pk = HeerId, idempotency_key = "request_id")]
#[derive(Debug, Clone)]
pub struct CofWidenedNullable {
 pub request_id: String,
 pub maybe_count: Option<u16>,
 pub maybe_size: Option<u32>,
 pub label: String,
}

fn main() {}
