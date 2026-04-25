// Phase 7-Zero-2 T3/T4 — a custom PK newtype emitted by `djogi::primary_key!`
// is usable as an ordinary ambient field on another `#[model]` struct, not
// only in the PK slot. The macro must route such fields through the generic
// `postgres_types::ToSql` / `FromSql` codec path (emitted by `primary_key!`)
// and through `Default` (also emitted by `primary_key!`) — no PK-slot
// special-casing.

use djogi::prelude::*;

djogi::primary_key! {
    pub struct Ref(i64);
    sql_type = "BIGINT";
    default_sql = "0";
}

#[model(table = "refs_ambient")]
#[derive(Debug, Clone)]
pub struct Holder {
    pub name: String,
    // Ambient use: custom PK type outside the PK slot. The `Default` /
    // `ToSql` / `FromSql` impls emitted by `primary_key!` make this work
    // via the generic user-field path — no macro-level detection needed.
    pub other_ref: Ref,
}

fn _ambient_surface(h: &Holder) {
    let _r: &Ref = &h.other_ref;
    // The custom PK's `Default` impl must be reachable — the macro-emitted
    // `Default for Holder` assigns `Default::default()` to every user field
    // that is not a built-in PK type.
    let _zero: Ref = ::std::default::Default::default();
}

fn main() {}
