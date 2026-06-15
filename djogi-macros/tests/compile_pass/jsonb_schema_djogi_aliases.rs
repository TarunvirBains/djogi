// / GH issue #40 — `djogi::DateTime` / `djogi::Date` aliases.
//
// PR 5 of added the alias spellings (`DateTime`, `djogi::DateTime`,
// `djogi::types::DateTime`, `::djogi::DateTime`, `::djogi::types::DateTime`,
// and the same four for `Date`) to the `JsonbSchema` derive's scalar
// allowlist so authors who have already run `use djogi::prelude::*` can
// declare datetime-typed JSONB fields without dropping down to the `time`
// crate.
//
// PR 5 covers the unit-level matcher with parser-only tests in
// `djogi-macros/src/jsonb_schema.rs`. This fixture covers the *end-to-end*
// derive: a struct that mixes the eight alias forms compiles cleanly,
// produces a `{T}Path<M>` whose every alias-typed field exposes a leaf
// `JsonbPathRef`, and slots into a `Jsonb<T>` field on a real `#[model]`.
//
// If any alias spelling regresses, the derive will fall back to the
// "assume nested `JsonbSchema` struct" branch, the path method will try
// to descend into a non-existent `impl JsonbSchema for OffsetDateTime`,
// and the fixture stops compiling.
//
// Uses only `djogi::*` paths so this fixture doesn't need `time` as a
// direct dep — `djogi` already re-exports it via `djogi::types`.
//
// `Default` is intentionally omitted from every derive list because the
// alias targets (`time::OffsetDateTime` / `time::Date`) do not implement
// `Default`. The same reason `no_default_model.rs` exists. The path tree
// the derive emits is unaffected by `Default`'s presence — what we care
// about here is that each alias spelling routes to a `JsonbPathRef` leaf.
use djogi::prelude::*;
use djogi::JsonbSchema;
use serde::{Deserialize, Serialize};

// ── Unqualified prelude aliases ──────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct UnqualifiedAliases {
 pub created_at: DateTime,
 pub published_on: Date,
}

// ── Qualified `djogi::` aliases ──────────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct DjogiQualifiedAliases {
 pub created_at: djogi::DateTime,
 pub published_on: djogi::Date,
}

// ── Crate-relative `djogi::types::` aliases ──────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct DjogiTypesAliases {
 pub created_at: djogi::types::DateTime,
 pub published_on: djogi::types::Date,
}

// ── Absolute-path `::djogi::` aliases ────────────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct AbsoluteDjogiAliases {
 pub created_at: ::djogi::DateTime,
 pub published_on: ::djogi::Date,
}

// ── Absolute-path `::djogi::types::` aliases ─────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct AbsoluteDjogiTypesAliases {
 pub created_at: ::djogi::types::DateTime,
 pub published_on: ::djogi::types::Date,
}

// ── Mixed: every alias spelling in one struct ────────────────────────────

#[derive(JsonbSchema, Serialize, Deserialize, Debug, Clone)]
pub struct MixedAliasSpellings {
 pub bare_dt: DateTime,
 pub bare_d: Date,
 pub djogi_dt: djogi::DateTime,
 pub djogi_d: djogi::Date,
 pub djogi_types_dt: djogi::types::DateTime,
 pub djogi_types_d: djogi::types::Date,
 pub abs_djogi_dt: ::djogi::DateTime,
 pub abs_djogi_d: ::djogi::Date,
 pub abs_djogi_types_dt: ::djogi::types::DateTime,
 pub abs_djogi_types_d: ::djogi::types::Date,
}

// ── Use the schema in a real model so the path tree builds ──────────────
//
// `no_default` because `MixedAliasSpellings` / `DjogiQualifiedAliases`
// transitively contain `time::OffsetDateTime` / `time::Date`, neither of
// which implement `Default`. Same rationale as `no_default_model.rs`.

#[model(table = "alias_audit_log", no_default)]
#[derive(Debug, Clone)]
pub struct AuditEntry {
 pub payload: Jsonb<MixedAliasSpellings>,
 pub legacy: Option<Jsonb<DjogiQualifiedAliases>>,
}

fn _every_alias_field_is_a_scalar_leaf() {
 // Each accessor on `MixedAliasSpellingsPath<AuditEntry>` returns a
 // `JsonbPathRef<AuditEntry, V>` where V is `DateTime` or `Date`. If
 // any alias spelling regressed to the nested-schema branch, the
 // derive would emit a method returning `<V as JsonbSchema>::Path<M>`
 // — and the alias targets (`OffsetDateTime` / `Date`) do not
 // implement `JsonbSchema`, so this binding would not compile.
 let _f1 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().bare_dt();
 let _f2 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().bare_d();
 let _f3 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().djogi_dt();
 let _f4 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().djogi_d();
 let _f5 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().djogi_types_dt();
 let _f6 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().djogi_types_d();
 let _f7 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().abs_djogi_dt();
 let _f8 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().abs_djogi_d();
 let _f9 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().abs_djogi_types_dt();
 let _f10 = |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().abs_djogi_types_d();

 // Optional Jsonb<T> with the qualified spellings descends through
 //.explicit_pg_predicate().typed() too — exercises the Option<Jsonb<T>> path.
 let _f11 = |f: AuditEntryFields| f.legacy().explicit_pg_predicate().typed().created_at();
 let _f12 = |f: AuditEntryFields| f.legacy().explicit_pg_predicate().typed().published_on();

 // Once we have a concrete value (`DateTime` is `time::OffsetDateTime`,
 // re-exported through `djogi::types`), `.eq(value)` must accept it —
 // i.e. the leaf is typed as `JsonbPathRef<_, DateTime>`, not
 // `JsonbPathRef<_, OffsetDateTime>` (those are the same type via
 // alias, but the call-site only sees the alias). Uses the inherent
 // `UNIX_EPOCH` const sentinel rather than `now_utc()` to avoid the
 // runtime call in a compile-only fixture.
 let epoch: djogi::DateTime = DateTime::UNIX_EPOCH;
 let _f13 = move |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().bare_dt().eq(epoch);
 let _f14 = move |f: AuditEntryFields| f.payload().explicit_pg_predicate().typed().djogi_types_dt().eq(epoch);
}

fn main() {}
