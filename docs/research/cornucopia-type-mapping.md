# Cornucopia type mapping & nullability — research notes

> **Source:** `cornucopia-reference/crates/cornucopia/src/` (local symlink to `../cornucopia-reference`, cloned 2026-05-09 at depth 50)
> **Upstream:** github.com/cornucopia-rs/cornucopia
> **License:** MIT / Apache-2.0 dual
> **Use:** Design reference for djogi Phase 13 (Runtime Query Verification, D7xx). Not a runtime or build dependency.

## Scope

These notes capture cornucopia's approach to two specific problems Phase 13 needs solved:

1. Mapping Postgres OIDs to Rust types
2. Surfacing column nullability from the prepare protocol

The rest of cornucopia (codegen, parser, validation, CLI) is irrelevant to djogi — it solves a different problem (SQL-first codegen for adopter-written queries vs. djogi's Model-first verification of macro-generated queries).

## Key Findings

### Finding 1: OID → Rust type is a flat match table, not introspection

`crates/cornucopia/src/type_registrar.rs:374-408` — the entire mapping is a single `match` on `tokio_postgres::types::Type`:

```rust
let (rust_name, is_copy) = match *ty {
    Type::BOOL => ("bool", true),
    Type::CHAR => ("i8", true),
    Type::INT2 => ("i16", true),
    Type::INT4 => ("i32", true),
    Type::INT8 => ("i64", true),
    Type::FLOAT4 => ("f32", true),
    Type::FLOAT8 => ("f64", true),
    Type::TEXT | Type::VARCHAR => ("String", false),
    Type::BYTEA => ("Vec<u8>", false),
    Type::TIMESTAMP => ("time::PrimitiveDateTime", true),
    Type::TIMESTAMPTZ => ("time::OffsetDateTime", true),
    Type::DATE => ("time::Date", true),
    Type::TIME => ("time::Time", true),
    Type::JSON | Type::JSONB => ("serde_json::Value", false),
    Type::UUID => ("uuid::Uuid", true),
    Type::INET => ("std::net::IpAddr", true),
    Type::MACADDR => ("eui48::MacAddress", true),
    Type::NUMERIC => ("rust_decimal::Decimal", true),
    _ => return Err(Error::UnsupportedPostgresType { ... })
};
```

Eighteen entries. Anything else returns a typed error with location info. No fancy introspection of `pg_type`, no schema-level catalog walks, no extensibility hook for adopter types.

### Finding 2: Type kinds dispatched recursively

`crates/cornucopia/src/type_registrar.rs:348-417` — five kinds covered, structured as recursion:

```rust
match ty.kind() {
    Kind::Enum(_)       => insert as Custom (is_copy=true, is_params=true),
    Kind::Array(inner)  => recurse inner, wrap as Array,
    Kind::Domain(inner) => recurse inner, wrap as Domain,
    Kind::Composite(fields) => recurse each field, propagate is_copy/is_params,
    Kind::Simple        => big match table from Finding 1,
    _                   => UnsupportedPostgresType error,
}
```

The recursion handles arbitrary nesting (array of domain of composite of …). The `is_copy` and `is_params` properties propagate up — an array of `Copy` simples is not `Copy` (always `Vec<T>`); a composite is `Copy` only if every field is.

### Finding 3: Nullability is USER-ANNOTATED, never inferred

This is the biggest single finding for djogi. From `crates/cornucopia/src/prepare_queries.rs:75-87`:

```rust
pub(crate) fn new(
    db_ident: String,
    ty: Rc<CornucopiaType>,
    nullity: Option<&NullableIdent>,
) -> Self {
    Self {
        ident: Ident::new(db_ident),
        ty,
        is_nullable: nullity.map_or(false, |it| it.nullable),
        is_inner_nullable: nullity.map_or(false, |it| it.inner_nullable),
    }
}
```

`nullity` is `Option<&NullableIdent>` — a parsed annotation from the user's `.sql` file. If absent, `is_nullable` defaults to `false` (NOT NULL).

The `.sql` annotation syntax (from cornucopia's parser):
```sql
--! select_user_with_email? : (email?, name)
SELECT email, name FROM users WHERE id = :id;
```
The trailing `?` after a column name marks it nullable. Absence means NOT NULL.

**Cornucopia made nullability the user's problem.** They never attempted to infer it from the prepare protocol. The reason is structural: `tokio_postgres::Statement::columns()` returns `Column { name(), type_() }` — there is no `nullable()` method. Postgres's prepare protocol does not surface nullability for derived columns (joins, coalesce, function calls, outer joins, subqueries). Even for direct column references, the protocol returns the table's NOT NULL status, but as soon as the column passes through any expression (`COALESCE(x, 0)`, `t.x` in a `LEFT JOIN`), that information is lost.

This collapses the hardest open question from the v1 stub: **djogi doesn't need nullability inference either**, because descriptors already declare it (`Option<T>` field type → nullable; bare `T` → NOT NULL). The descriptor is djogi's analogue of cornucopia's `--!` annotation.

### Finding 4: The prepare protocol surface, in full

From `crates/cornucopia/src/prepare_queries.rs:366-432`, the full extraction logic:

```rust
let stmt = client.prepare(&sql_str)?;
let stmt_params: &[Type] = stmt.params();      // parameter types
let stmt_cols:   &[Column] = stmt.columns();   // result columns
// each Column: column.name() -> &str, column.type_() -> &Type
```

That's the complete surface. The `Column` struct exposes name and type. No nullability, no constraint metadata, no source-table info. Cornucopia's "value-add" beyond this is the type registrar (Finding 1) and the user-annotation pipeline (Finding 3).

### Finding 5: Error shape for unsupported types

`crates/cornucopia/src/type_registrar.rs:451-468` — the error variant:

```rust
pub enum Error {
    UnsupportedPostgresType {
        src: NamedSource,
        query: SourceSpan,        // points at the offending query in source
        col_name: String,
        col_ty: String,
    },
}
```

Critical detail: the error carries the column name and the unsupported type's name as a string. djogi's D7xx for unknown OIDs (D704 in v1 stub) should follow the same shape — name the column and the unmapped Postgres type.

## Implications for djogi

### What we adopt

1. **Flat OID → Rust type table** — same approach, smaller table specialized to djogi's type ecosystem. Enumerate the supported types, error explicitly on anything else.

2. **Recursive Kind dispatch** — `Kind::Array(inner)`, `Kind::Domain(inner)`, `Kind::Composite(fields)` all recurse, propagating properties. djogi's table walks the same shape because Postgres's type system imposes it.

3. **Nullability from declaration, not inference** — descriptors declare expected nullability (`Option<T>` ↔ nullable). The verifier reads the descriptor's expectation, never tries to infer from Postgres. This is the single most important lesson.

4. **Error-on-unknown-type with column + type-name** — D704 carries `col_name: String, col_ty: String` like cornucopia's `UnsupportedPostgresType`. Adopters get a precise pointer.

### What we invert

Cornucopia's direction is `Postgres OID → Rust type` (codegen). djogi's direction is **`Rust type → expected OID, then verify against Postgres OID`** (verification). The mapping table is the same set of pairs, but the lookup direction reverses:

- Cornucopia: "Postgres returned `INT8`; emit a Rust field of type `i64`."
- djogi: "Descriptor declares field of type `HeerId`; expected OID is `INT8` (oid 20). Postgres returned `INT8`. ✅"

The inversion has consequences for djogi-specific wrapper types. Cornucopia maps `INT8 → i64` always — it has no notion of "this `i64` is actually a `HeerId`." djogi's reverse map has multiple Rust types pointing at the same Postgres OID:
- `HeerId → INT8`
- `i64 → INT8` (for non-PK columns)

Both are valid; the descriptor-side type is what determines which mapping is checked. The verifier doesn't need to disambiguate at the OID layer — it reads "this column's expected type is `HeerId`" from the descriptor and confirms Postgres returned `INT8`.

### What we don't adopt

- **`.sql` file annotation pipeline** — djogi doesn't have user `.sql` files; nullability comes from descriptor.
- **Codegen** — Phase 13 is verify-only, no Rust generation.
- **Parser, validator, container layers** — irrelevant to verification use case.
- **Custom enum / composite codegen as Rust types** — djogi already has its own enum and composite story (proc macro emits them); the verifier just needs to recognize them.

### Type table draft for djogi

| Postgres type (OID) | djogi expected Rust type(s) | Notes |
|---|---|---|
| `BOOL` (16) | `bool` | |
| `INT2` (21) | `i16` | |
| `INT4` (23) | `i32` | Phase 7's `Serial` PK uses this |
| `INT8` (20) | `HeerId` (PK), `i64` (non-PK) | Default PK; descriptor disambiguates |
| `FLOAT4` (700) | `f32` | |
| `FLOAT8` (701) | `f64` | |
| `TEXT` (25) | `String` | |
| `VARCHAR` (1043) | `String` | Same Rust type as TEXT |
| `BYTEA` (17) | `Vec<u8>` | |
| `TIMESTAMP` (1114) | `time::PrimitiveDateTime` | |
| `TIMESTAMPTZ` (1184) | `time::OffsetDateTime` | djogi default for `created_at`/`updated_at` |
| `DATE` (1082) | `time::Date` | |
| `TIME` (1083) | `time::Time` | |
| `JSON` (114) | `serde_json::Value` | Untyped — rare in djogi |
| `JSONB` (3802) | `Jsonb<T>` (typed), `serde_json::Value` (untyped) | Phase 5 typed wrapper |
| `UUID` (2950) | `RanjId` (PK), `uuid::Uuid` (non-PK) | Opt-in PK type |
| `NUMERIC` (1700) | `rust_decimal::Decimal` | |
| `INET` (869) | `std::net::IpAddr` | If/when adopters need it |

Plus recursive wrappers: `_` prefix OIDs (1007 = `_INT4`, etc.) → `Vec<T>`; composite OIDs → registered descriptor structs; enum OIDs → registered descriptor enums.

**Not yet decided** (defer to v2):
- Spatial types (`GeoPoint` → which OID? `geometry`/`geography` from PostGIS, custom OID per install). Phase 6 ships, but its OID story needs a v2 audit.
- Auth-substrate types (Phase 5.5 — password hash representation). Likely just `Vec<u8>` or `String`, no special handling.
- HSTORE, range types, custom domain types over base types — defer until adopters request them.

## Open questions surfaced by this research (for v1 / v2)

1. **Custom user types.** Cornucopia errors on unknown types. djogi could (a) error like cornucopia, (b) allow opt-out per-column ("trust me, this matches"), or (c) provide a registry where adopters declare custom OID mappings. Lens-walk this in v1.
2. **Array element types.** `_INT4` is OID 1007, element OID is `INT4` (23). The verifier needs to project from the array OID to the element OID and recurse — same shape as cornucopia's `Kind::Array(inner)` recursion.
3. **JSONB typed vs. untyped.** Postgres returns OID 3802 for both `Jsonb<T>` and `serde_json::Value`. The descriptor disambiguates on the djogi side, but D704 can't fire here — it's a JSONB shape question, not an OID-mapping question. If `Jsonb<T>::T` validation fails, that's a runtime concern, not Phase 13's surface.
4. **`citext` extension.** Postgres `citext` has a per-database OID (it's an extension type). The verifier would need to look up the OID at startup or treat any unknown text-shaped type as `String`. Lens-walk in v2.

## Reference paths in cornucopia

| Path | Purpose | Lines worth re-reading |
|---|---|---|
| `crates/cornucopia/src/type_registrar.rs` | OID → Rust mapping, type wrapping | 18-37, 348-417 |
| `crates/cornucopia/src/prepare_queries.rs` | Prepare protocol, nullability handling | 65-87, 366-432 |
| `crates/cornucopia/src/codegen.rs` | Rust codegen (rendering) | Reference only — djogi doesn't codegen here |
| `crates/cornucopia/src/validation.rs` | Query validation | Pattern reference for diagnostic emission |

## License notice

Cornucopia is MIT / Apache-2.0 dual-licensed. djogi does not depend on or vendor cornucopia. These notes describe cornucopia's public API behavior and design decisions — fair commentary on a public project. No source code is copied; the table in "Implications for djogi" is djogi's own type set, not lifted from cornucopia.
