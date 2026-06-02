// Issue #189 — opt-in HeerId / RanjId structural CHECK.
//
// Internal wrapper. The body exercises catalog assertions, round-trip
// behaviour, and OOB rejection through the raw SQL bypass — externally
// injected structurally-malformed IDs (negative BIGINT for HeerId, UUIDv4
// where UUIDv8 / RFC 4122 is required for RanjId) cannot be constructed
// through the typed Rust surface (`HeerId::from_i64`, `RanjId::from_uuid`
// reject them at the type boundary), so `raw_execute` is the only way to
// land them and verify the DB-level CHECK fires.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#189): exercises the opt-in structural CHECK for
// HeerId / RanjId columns. OOB inserts (negative BIGINT for HeerId,
// UUIDv4 for a RanjId column) are unreachable through the typed surface
// because `HeerId::from_i64` rejects negatives and `RanjId::from_uuid`
// rejects non-v8 / non-RFC4122 UUIDs. Raw INSERT SQL is the only way to
// construct these values and verify the projected CHECK rejects them at
// the DB layer. The default-off catalog assertion and the round-trip
// tests do NOT use raw_execute.
mod strict_id_check {
    include!("sources/strict_id_check.rs");
}
