> [Back to README](../../README.md) | [All Specs](./index.md)

# Postgres Range Types

This note locks the #215 public range surface. It builds on the Phase 8.5 G0
codec substrate and intentionally does not cover exclusion constraints,
Postgres 18 temporal constraints, or multiranges.

## Rust Shape

Djogi exposes Postgres built-in range columns as `djogi::Range<T>`, with
`djogi::RangeBound<T>` carrying `Inclusive(T)`, `Exclusive(T)`, or `Unbounded`
endpoints plus an explicit empty-range sentinel.

The supported field mappings are:

| Rust field type | SQL column type |
|---|---|
| `Range<i32>` | `int4range` |
| `Range<i64>` | `int8range` |
| `Range<rust_decimal::Decimal>` | `numrange` |
| `Range<time::PrimitiveDateTime>` | `tsrange` |
| `Range<djogi::DateTime>` / `Range<time::OffsetDateTime>` | `tstzrange` |
| `Range<djogi::Date>` / `Range<time::Date>` | `daterange` |

`tsrange` is deliberately the one timestamp-without-timezone entry point. It
uses `time::PrimitiveDateTime` so the Rust type keeps the timezone boundary
visible instead of overloading Djogi's `DateTime` alias, which remains
timezone-aware `TIMESTAMPTZ`.

## Descriptor Shape

Range fields use `FieldSqlType::Range { subtype: RangeSubtypeKind }`.
`RangeSubtypeKind` has one value per built-in range family:
`Int4`, `Int8`, `Num`, `Ts`, `Tstz`, and `Date`. Display and schema projection
render those as `int4range`, `int8range`, `numrange`, `tsrange`, `tstzrange`,
and `daterange`.

The proc macro maps bare `Range<T>`, `djogi::Range<T>`, and
`djogi::types::Range<T>` path forms to those variants. Standard-library
`std::ops::Range` and other lookalike range types are not accepted through the
typed field mapping.

## Query Operators

Range predicates are PostgreSQL-specific and live behind
`explicit_pg_predicate()` on root model fields. That keeps them out of the
portable/Punnu predicate path, where Rust cannot reproduce Postgres range
canonicalization and operator semantics.

| Method | RHS | SQL operator | Meaning |
|---|---|---|---|
| `contains(value)` | element `T` | `@>` | range contains element |
| `contains_range(range)` | `Range<T>` | `@>` | range contains range |
| `contained_by(range)` | `Range<T>` | `<@` | range is contained by range |
| `overlaps(range)` | `Range<T>` | `&&` | ranges overlap |
| `strictly_left_of(range)` | `Range<T>` | `<<` | range is strictly left of range |
| `strictly_right_of(range)` | `Range<T>` | `>>` | range is strictly right of range |
| `not_extends_right_of(range)` | `Range<T>` | `&<` | range does not extend right of range |
| `not_extends_left_of(range)` | `Range<T>` | `&>` | range does not extend left of range |
| `adjacent_to(range)` | `Range<T>` | `-|-` | ranges are adjacent |

The range equality and ordering methods inherited through the explicit
Postgres predicate path follow Postgres SQL semantics only. `Range<T>` is not a
portable equality type because Rust structural equality is not Postgres range
canonicalization.

## Migration Projection

`FieldSqlType::Range` projects the native range SQL type. Type-derived CHECKs
are endpoint checks: `numrange` reuses Decimal's representability rule;
`tsrange`, `tstzrange`, and `daterange` reuse temporal upper-bound checks over
`lower(col)` and `upper(col)` while preserving `NULL`, empty, and unbounded
range pass-through. `int4range` and `int8range` need no additional CHECK
because their element types are identity mapped.

## Multirange Decision

Postgres multirange types are out of scope for #215. They need a separate Rust
container shape, separate wire-codec work, and a dedicated query/migration
contract. The #215 surface must not add `int4multirange`, `int8multirange`,
`nummultirange`, `tsmultirange`, `tstzmultirange`, or `datemultirange`.
