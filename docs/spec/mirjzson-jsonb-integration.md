# `MirJzSON` JSONB Integration Spec

**Date:** 2026-05-14
**Status:** v3 Djogi-owned draft after split review — initial slice +
macro justification gate landing 2026-05-15
**Owning repo:** `/home/tarunvir/projects/djogi/`
**Issue:** https://github.com/TarunvirBains/djogi/issues/195
**Sassi contract:** `/home/tarunvir/projects/sassi/docs/specs/2026-05-14-jsahibon-portable-json-design-v3.md`

## Shipped in this slice

- `MirJzSON` and `MirJzSONError` (`djogi::jsonb::mirjzson`) — wrapper type
  over `sassi::JSahibON` with `#[repr(transparent)]` layout, no `Default`,
  no `PartialEq` / `Eq` / `Hash` / `PartialOrd`.
- Construction: `From<sassi::JSahibON>`, `TryFrom<serde_json::Value>` (typed
  `MirJzSONError`), the Postgres `FromSql` / `ToSql` codec routing through
  Sassi's `serde-json-bridge` (no JSahibON-equality reimplementation in
  Djogi).
- Projection: `as_jsahibon` / `into_jsahibon` (named cache-boundary helpers),
  `From<MirJzSON>` for `serde_json::Value`, `Jsonb<T>::to_jsahibon` for the
  full-document cache projection.
- Query builder: `DjogiField<M, MirJzSON>::jsahibon()` and
  `DjogiField<M, Option<MirJzSON>>::jsahibon()` returning Djogi-trusted
  `PortablePredicate<M>` values for every Sassi `JSahibONPredicateBody`
  variant (exists / missing / type / is-json-null / has-key family /
  scalar comparison + IN + BETWEEN / json equality / array contains /
  array length).
- SQL lowering for `LookupOp::Json` (`query::portable::emit_jsahibon_body`)
  — two-valued guarded shapes per the table in §SQL Mapping. Numeric
  operands bind through `rust_decimal::Decimal` (full `u64` range, never
  `as i64`). Path segments and keys are bound parameters; nothing is
  interpolated.
- Trusted-provenance posture: `IntoQ<T>` is sealed; raw Sassi
  `BasicPredicate<T>` cannot reach `Q::Portable(_)` from adopter code.
  `PortablePredicateError::UntrustedJsonPredicate` is the defense-in-depth
  rejection for the (currently unreachable) case where a `LookupOp::Json`
  payload fails the `JSahibONPredicateBody` downcast.
- `ExplicitPgPredicateField::mirjzson()` entry points exist with **no v1
  predicate methods** — the API shape is committed but reserved for future
  PostgreSQL-only operators.
- Tests: 30 SQL-shape + value-projection pins (`djogi::jsonb::mirjzson::*`
  + `djogi::query::mirjzson::*`).
- Adopter guide: `docs/guide/jsonb.md` §MirJzSON section.

## Implemented (Phase 8.5 issue #195) — macro gate

The `#[mirjzson(justification = "...")]` attribute described under
§Model Gating below is now emitted and enforced by `#[model]`. The macro:

- Detects every `MirJzSON` / `Option<MirJzSON>` field (last-segment
  ident match — covers bare, `djogi::`, `djogi::jsonb::`, `crate::`,
  `super::`, and `::djogi::*` path forms uniformly).
- Requires `#[mirjzson(justification = "...")]` on every such field
  and rejects missing annotations at expand time with a span-precise
  field-level diagnostic.
- Validates the justification literal: present, non-empty after trim,
  not in an ASCII case-insensitive placeholder denylist (`TODO`,
  `TBD`, `FIXME`, `?`, `none`, `external`, `see comment`, and
  similar), and at least 12 trimmed bytes.
- Rejects `#[mirjzson(...)]` on any field whose type is not
  `MirJzSON` / `Option<MirJzSON>` (including `Jsonb<T>` — the typed
  schema IS the justification).
- Consumes the attribute from the rewritten struct so rustc never
  emits `unknown attribute mirjzson`.
- Maps `MirJzSON` to `JSONB` in the descriptor pipeline through
  `rust_type_to_sql` and accepts it for `#[field(index = "gin")]`.

Lihaaf compile fixtures pin the gate: `phase85_195_mirjzson_basic`,
`phase85_195_mirjzson_optional`, and `phase85_195_mirjzson_mixed_with_jsonb`
in `djogi-macros/tests/compile_pass/`; eight compile-fail fixtures cover
missing annotations, empty / placeholder / too-short justifications,
attribute-on-wrong-type, attribute-on-`Jsonb<T>`, bare `#[mirjzson]`,
unknown key, non-string value, and duplicate attributes.

## Pending follow-up (still tracked under #195)

- **Cluster portability gate explicit rejection.** Adding
  `PortablePredicateError::UntrustedJsonPredicate` rejection at the
  `try_portable` / cache-refresh boundary is wired through the existing
  `Q::Condition(_)` / `Q::Expression(_)` rejection paths — future
  SQL-only `.explicit_pg_predicate().mirjzson()` predicates will surface
  through `Condition::MirJzSON(_)` (or a successor variant) and ride
  the same cache-invalid path.
- **Live-DB SQL parity integration tests.** Unit tests pin SQL shape;
  live PostgreSQL fixture coverage of the truth tables (SQL NULL vs JSON
  null on a real `jsonb` column, `?` vs `?|` semantics on objects vs
  arrays, `jsonb_array_length` non-array safety, etc.) is a separate
  fixture file under `tests/integration/`.

This file is the Djogi-owned half of the JSON query design. Sassi owns
`JSahibON` value semantics and portable predicate truth rules. Djogi imports
that contract and owns only PostgreSQL storage, model gating, trusted Djogi
field construction, SQL lowering, cache boundaries, and the update to Djogi
issue #195.

## Goal

Djogi adds `MirJzSON` as the explicit JSONB field type for genuinely unschemed
JSON columns. Unlike `Jsonb<T>`, it has no known Rust schema. Unlike arbitrary
`serde_json::Value`, it projects to Sassi's portable `JSahibON` model so the
same raw JSON predicates can run against PostgreSQL rows and Punnu-local cache
entries.

Djogi v1 includes:

- `MirJzSON` and `Option<MirJzSON>` model field support.
- A required model-field justification attribute.
- Construction from `sassi::JSahibON` and `serde_json::Value`.
- Conversion back to `serde_json::Value`.
- Explicit projection to `sassi::JSahibON`.
- Trusted portable JSON predicate construction through
  `DjogiField<M, MirJzSON>::jsahibon()`.
- SQL lowering for Sassi `JSahibONPredicateBody` leaves with two-valued boolean
  semantics.

Djogi v1 does not add schema-derived typed JSON path trees for `MirJzSON`.
Typed schema JSON querying remains the job of existing `Jsonb<T>` /
`JsonbSchema`. `MirJzSON` v1 provides typed scalar leaves over raw paths, via
the imported Sassi contract.

## Sassi Cache Boundary

Djogi's database JSON wrappers are not automatically Sassi wire types.
`Jsonb<T>` remains a database representation for typed JSONB, including its
unknown-field handling. A backend cache model that contains `Jsonb<T>` must not
assume Sassi will downcast it during cache insertion or that a postcard
frontend can deserialize it as-is.

When a `Jsonb<T>` field is exposed through a Sassi/Punnu cache, Djogi users must
choose an explicit cache projection:

- Project to `T` when the cache/frontend needs only the typed schema content.
- Project to `sassi::JSahibON` when the cache/frontend needs the full merged
  JSON document, unknown fields, or Sassi-local JSON predicates.
- Project to a future Sassi-owned typed JSON wrapper only if Sassi later
  defines one.

Djogi should provide an explicit helper such as `Jsonb<T>::to_jsahibon()` or a
fallible conversion from `Jsonb<T>` into `sassi::JSahibON` for the full-document
case. That conversion is a cache-boundary projection, not an implicit behavior
of `Jsonb<T>` serde.

## Dependency Contract

Djogi must enable Sassi's `serde-json-bridge` feature, or an equivalent
Sassi-owned conversion helper, so all `serde_json::Value` conversion semantics
come from Sassi.

Expected dependency shape:

```toml
sassi = {
    path = "../sassi-reference/sassi",
    features = ["watermark-time", "serde-json-bridge"],
}
```

Djogi must not reimplement `JSahibON` equality, numeric matching, path
traversal, or predicate truth rules. It imports Sassi's public types and
evaluator.

## Representation

V1 is portable-only:

```rust
pub struct MirJzSON {
    portable: sassi::JSahibON,
}
```

Required APIs:

```rust
impl From<sassi::JSahibON> for MirJzSON;

impl TryFrom<serde_json::Value> for MirJzSON {
    type Error = MirJzSONError;
}

impl From<MirJzSON> for serde_json::Value;

impl MirJzSON {
    pub fn into_jsahibon(self) -> sassi::JSahibON;
    pub fn as_jsahibon(&self) -> &sassi::JSahibON;
}
```

There is intentionally no `From<MirJzSON> for JSahibON`. Projection is named so
the database-to-portable boundary is visible at call sites.

PostgreSQL JSONB read behavior:

- Djogi accepts only JSONB values that can project to `JSahibON`.
- Non-portable JSONB numbers fail decoding with a typed `MirJzSONError`.
- Because v1 stores only `JSahibON`, `MirJzSON -> serde_json::Value` is total.

Trait posture:

- `MirJzSON: Clone + Debug + Send + Sync + 'static`.
- `MirJzSON` must not implement `PartialEq`, `Eq`, `Hash`, or `PartialOrd`.
- Whole-value JSON predicates go through explicit JSON predicate methods, not
  root `DjogiField::eq`.

## Model Gating

`MirJzSON` is Djogi's raw JSONB escape hatch and requires a justification on
model fields:

```rust
#[mirjzson(justification = "payload is externally owned by partner API")]
payload: MirJzSON,
```

Required macro behavior:

- `MirJzSON` and `Option<MirJzSON>` fields without a justification fail at
  expand time.
- Empty or vague justifications fail at expand time.
- The attribute is stripped from the rewritten struct.
- `Jsonb<T>` remains the typed-schema JSONB path. `MirJzSON` is for genuinely
  unschemed JSON.

## Trusted Portable Construction

Djogi must not lower arbitrary Sassi `LookupOp::Json` predicates by trusting a
raw Sassi `field_name`. Raw Sassi fields are valid for local Sassi evaluation
but are not trusted provenance for Djogi SQL.

Djogi owns the trusted portable accessor:

```rust
impl<M: Model> DjogiField<M, MirJzSON> {
    pub fn jsahibon(self) -> DjogiJSahibONFieldRef<M>;
}

impl<M: Model> DjogiField<M, Option<MirJzSON>> {
    pub fn jsahibon(self) -> DjogiJSahibONOptionFieldRef<M>;
}
```

Implementation extends the existing `djogi::query::PortablePredicate<T>` and
`DjogiFieldProvenance` mechanism. JSON leaves stamped by
`DjogiField<M, MirJzSON>::jsahibon()` carry trusted Djogi provenance; SQL
lowering accepts `LookupOp::Json` leaves only through this trusted path. Raw
standalone `sassi::BasicPredicate<T>` JSON leaves are rejected with a typed
portability error.

The accessor:

- Is generated from Djogi model field metadata.
- Captures the physical column through a Djogi-private trusted field token, not
  through a caller-supplied string.
- Builds Sassi `JSahibONPredicateBody` payloads for semantics.
- Uses Sassi's `evaluate_jsahibon_predicate` for local/Punnu evaluation.

## Portable Predicate Shape

Portable Djogi JSON predicates resemble the existing typed JSONB path surface
while importing Sassi semantics:

```rust
Post::objects()
    .filter(|f| {
        f.payload()
            .jsahibon()
            .path("engine.cylinders")
            .value::<i64>()
            .gte(4)
    });

Post::objects()
    .filter(|f| {
        f.payload()
            .jsahibon()
            .key("content-type")
            .value::<String>()
            .eq("application/json".to_string())
    });

Post::objects()
    .filter(|f| {
        f.payload()
            .jsahibon()
            .path_segments(["a.b", "0", "cafe"])
            .exists()
    });
```

The `.path("a.b")` convenience keeps sibling resemblance with Djogi's existing
`JsonbPathRef<M, V>` shape. The `.key(...)` and `.path_segments(...)` APIs are
required for raw JSON keys that are not plain dotted identifiers.

## SQL-Only Route

Djogi reserves the PostgreSQL-specific route for future JSONB-only behavior:

```rust
impl<M: Model> ExplicitPgPredicateField<M, MirJzSON> {
    pub fn mirjzson(self) -> MirJzSONFieldRef<M>;
}

impl<M: Model> ExplicitPgPredicateField<M, Option<MirJzSON>> {
    pub fn mirjzson(self) -> MirJzSONOptionFieldRef<M>;
}
```

In v1, `.explicit_pg_predicate().mirjzson()` exposes no duplicate portable
predicate methods. All v1 JSON querying flows through `.jsahibon()` so it is
both SQL-lowerable and Punnu-evaluable. The SQL-only route is reserved for
future PostgreSQL-specific operators such as JSONPath `@?` / `@@` or
GIN-specific shapes that have no Sassi-local contract.

If future SQL-only methods are added, they emit `Condition::MirJzSON(_)` and are
cache-rejected.

## SQL Parity

Djogi SQL must match Sassi local evaluation exactly. Every JSON predicate leaf
must be a two-valued SQL boolean (`TRUE` or `FALSE`) before it is composed under
`NOT`, `XOR`, `AND`, or `OR`. SQL `NULL` must not leak out of a JSON predicate
leaf except as an internal value that is converted to `FALSE` or handled by
`missing()`.

For every JSON expression `j` (root column or `column #> $path_text_array`):

- Missing path or SQL NULL yields SQL NULL for `j`; value predicates return
  `FALSE` unless the predicate is `missing()`.
- Key predicates guard `jsonb_typeof(j) = 'object'`; PostgreSQL `?`, `?|`, and
  `?&` must not match arrays for the portable Sassi key contract.
- Array length predicates guard `jsonb_typeof(j) = 'array'` before calling
  `jsonb_array_length`; non-arrays return `FALSE`.
- Array containment guards `jsonb_typeof(j) = 'array'`.
- Scalar string predicates guard `jsonb_typeof(j) = 'string'`.
- Scalar boolean predicates guard `jsonb_typeof(j) = 'boolean'`.
- Scalar numeric predicates guard `jsonb_typeof(j) = 'number'`.
- Numeric casts occur only inside a `CASE` expression or equivalent safe
  preflight shape.
- JSON null predicates compare against JSONB `null`, not SQL `NULL`.

Required safe numeric shape:

```sql
CASE
  WHEN jsonb_typeof(j) = 'number'
  THEN (j #>> '{}'::text[])::numeric <op> $operand_numeric
  ELSE FALSE
END
```

`u64` operands bind through a numeric-safe path and support the full `u64`
range. Concretely, Djogi binds `u64` via `rust_decimal::Decimal::from(value)` or
an equivalent unlimited-precision numeric representation. It must never cast
through `as i64`. Tests include `u64::MAX`.

String ordering is absent from portable `JSahibON` predicates, so Djogi must not
lower `value::<String>().lt/gte/between`.

## SQL Mapping

Path mapping:

- `JPath::root()` maps to an empty text-array path.
- Non-root paths bind the exact UTF-8 segments as `text[]`.
- No path segment is interpolated into SQL.
- Segment `"0"` is a key, not an array index.

Use one uniform JSON expression for root and path:

```sql
j := column #> $path_text_array
```

At root, `$path_text_array` is `'{}'::text[]`; this makes `exists()` and
`missing()` uniform for `MirJzSON` and `Option<MirJzSON>`.

Predicate mapping requirements:

| Sassi body | Djogi SQL requirement |
|---|---|
| `Exists` | `(column #> $path_text_array) IS NOT NULL` |
| `Missing` | `(column #> $path_text_array) IS NULL` |
| `IsJsonNull` | `COALESCE(j = 'null'::jsonb, FALSE)` |
| `IsNotJsonNull` | `COALESCE(j <> 'null'::jsonb, FALSE)` |
| `HasKey` | `COALESCE(jsonb_typeof(j) = 'object' AND j ? $key, FALSE)` |
| `HasAnyKey` | `COALESCE(jsonb_typeof(j) = 'object' AND j ?| $keys_text_array, FALSE)` |
| `HasAllKeys` | `COALESCE(jsonb_typeof(j) = 'object' AND j ?& $keys_text_array, FALSE)` |
| `ScalarCompare` | guarded two-valued type-specific comparison; numeric via safe `numeric`, string/bool equality-family only |
| `ScalarIn` | guarded two-valued membership; missing/type mismatch false before empty-list identities |
| `ScalarBetween` | numeric only, guarded two-valued safe `numeric BETWEEN` |
| `JsonEq` | `COALESCE(j = $jsonb_value, FALSE)` |
| `JsonNeq` | `COALESCE(j <> $jsonb_value, FALSE)` |
| `ArrayContains` | `COALESCE(jsonb_typeof(j) = 'array' AND j @> $single_element_jsonb_array, FALSE)` |
| `ArrayLen` | `CASE WHEN jsonb_typeof(j) = 'array' THEN jsonb_array_length(j) <op> $len ELSE FALSE END` |

All keys, key arrays, path segment arrays, scalar operands, JSON operands, array
elements, and lengths are bound parameters.

## Cache Boundary

Djogi has two JSON predicate routes:

- `f.payload().jsahibon()...`: portable Sassi semantics, trusted Djogi
  provenance, lowerable to SQL and evaluable in Punnu.
- `f.payload().explicit_pg_predicate().mirjzson()...`: reserved for future
  SQL-only methods; those methods emit Djogi conditions and are rejected by
  cache/refresh portability gates.

If/when `Condition::MirJzSON(_)` exists, `try_portable`, cache refresh, and
Punnu-boundary checks reject it with a typed cache-invalid error.

## Tests

Compile-pass/fail:

- `MirJzSON` and `Option<MirJzSON>` require non-empty, non-vague
  `#[mirjzson(justification = "...")]`.
- `f.payload().eq(...)` does not compile for `MirJzSON`.
- `f.payload().jsahibon().path("a").value::<u64>().gte(...)` compiles.
- `f.payload().jsahibon().key("content-type").exists()` compiles.
- `value::<String>().lt(...)` does not compile.
- `.explicit_pg_predicate().mirjzson()` exposes no v1 portable-shaped duplicate
  predicate methods.
- Existing `Jsonb<T>` typed path APIs remain unchanged.

SQL/unit/integration:

- Every Sassi body variant lowers to SQL with bound path/key/value parameters.
- Every JSON leaf emits a two-valued boolean under `NOT`, `XOR`, `AND`, and
  `OR`.
- Arbitrary keys `content-type`, `a.b`, `0`, empty string, and non-ASCII keys
  bind as data and match exact JSON keys.
- Non-object key tests return false on arrays, strings, numbers, booleans, JSON
  null, SQL NULL, and missing paths.
- Non-array length tests return false and never call `jsonb_array_length` on
  non-arrays.
- Numeric comparisons never error on strings, booleans, objects, arrays, JSON
  null, SQL NULL, or missing paths.
- Full-range `u64` values, including `u64::MAX`, compare correctly.
- JSON null and SQL NULL are distinct.
- `JsonEq` object equality is order-insensitive and matches Sassi.
- `array_contains` uses Sassi value equality, including numeric softening.
- Portable `jsahibon()` predicates and local Sassi evaluation return identical
  id sets over a mixed fixture dataset.
- Forged standalone Sassi `LookupOp::Json` predicates without Djogi provenance
  are rejected by Djogi lowering.
- Future SQL-only `Condition::MirJzSON` predicates are cache-rejected.

Parity drift between Sassi and Djogi for portable `JSahibON` predicates is a
blocking bug.

## Djogi #195 Update Body

Djogi issue #195 should summarize only the Djogi-owned half:

- Rename the draft concept from `RawJsonb` to `MirJzSON`.
- State that `MirJzSON` is portable-only over Sassi `JSahibON` in v1.
- Link the Sassi `JSahibON` spec for value and predicate semantics.
- Add the Sassi dependency requirement for `serde-json-bridge`.
- Keep the justification requirement.
- Keep the no-root-`PartialEq` rule.
- Describe the v1 query route:
  `f.payload().jsahibon()` for trusted portable predicates.
- State that `.explicit_pg_predicate().mirjzson()` is reserved in v1 for future
  PostgreSQL-only JSONB operators.
- Include the provenance boundary: Djogi extends `PortablePredicate<T>` /
  `DjogiFieldProvenance` and must not lower forged raw Sassi field names.
- Include the SQL parity rule: every portable JSON leaf is two-valued and
  guarded so mismatches return `FALSE`, not SQL errors.
- Include cache rejection for future SQL-only `Condition::MirJzSON`.

Do not paste the full Sassi value model into Djogi #195. Link the Sassi issue or
spec instead.
