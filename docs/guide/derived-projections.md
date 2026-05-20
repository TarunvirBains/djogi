> [Back to Guides](./index.md) | [Visages](./visages.md)

# Derived Projections

Derived projections add fields to generated visages without adding
storage columns to the source model. Use them when an API shape needs
a value computed from existing model state, and that value belongs to
the transport projection rather than to the persistence struct.

## Declaring a derived field

Declare each derived projection as a struct-level `#[derived(...)]`
attribute on the same item that carries `#[model(...)]`:

```rust
use djogi::prelude::*;

#[derive(Model, Debug, Clone)]
#[model(table = "consignments")]
#[derived(
    name = facility_site,
    ty = Site,
    scopes = [public, admin, export],
    sql = "CASE WHEN direction = 'inbound' THEN inbound_site ELSE outbound_site END",
    rust = "match model.direction {
        Direction::Inbound => model.inbound_site.clone(),
        _ => model.outbound_site.clone(),
    }",
    doc = "Facility-side site for this consignment.",
)]
pub struct Consignment {
    #[field(expose(public, admin, export))]
    pub inbound_site: Site,

    #[field(expose(public, admin, export))]
    pub outbound_site: Site,

    #[field(expose(public, admin, export))]
    pub direction: Direction,
}
```

This emits `facility_site` on `ConsignmentPublic`,
`ConsignmentAdmin`, and `ConsignmentExport`. It does not add a
`facility_site` field to `Consignment`.

The required keys are:

| Key | Meaning |
|---|---|
| `name` | Lowercase output field name on each scoped visage |
| `ty` | Rust output type; use `Option<T>` for nullable values |
| `scopes` | One or more built-in scopes: `public`, `self_view`, `admin`, `export` |
| `sql` | Per-row Postgres expression used when fetching the visage |
| `rust` | Rust expression used when constructing the visage from `&Model` |

The optional `doc` string becomes rustdoc on the generated visage
field.

`#[derived]` is a helper consumed by `#[model]`; it is not a
standalone attribute macro.

## SQL and Rust parity

Every derived field has two implementations:

- `sql` runs in Postgres when fetching through a visage queryset.
- `rust` runs in memory when converting from a `&Model` with
  `From` or `TryFrom`.

Djogi does not translate between the two. The adopter is responsible
for keeping them equivalent. Put the generated parity helper in tests
for models where both paths matter:

```rust
let in_memory: ConsignmentPublic = (&consignment).into();
let from_db: ConsignmentPublic =
    ConsignmentPublic::filter(|f| f.id().eq(consignment.id))
        .fetch_one(&mut ctx)
        .await?;

in_memory.assert_derived_parity(&from_db)?;
```

Parity failures surface from the helper as
`djogi::testing::DerivedParityError::Drift { visage, field }`. SQL
parse and type problems that only the database can see surface when
the visage queryset is fetched, usually as `DjogiError` wrapping the
underlying database error or a `VisageError` variant.

### Async fetch + compare in one call

The `assert_derived_parity` inherent method is sync and takes two
pre-constructed visages. For the common "create the model, fetch
the visage, compare to in-memory" pattern, reach for the additive
async helper:

```rust
use djogi::testing::assert_derived_parity_fetched;

let in_memory: ConsignmentPublic = (&consignment).into();
let target_id = consignment.id;

assert_derived_parity_fetched(&in_memory, || async {
    ConsignmentPublic::filter(|f| f.id().eq(target_id))
        .fetch_one(&mut ctx)
        .await
})
.await?;
```

The helper takes the in-memory visage by reference and a closure
that returns the fetch future. It awaits the fetch, lifts any
`DjogiError` into `DerivedParityError::Fetch { source }`, and
delegates to the sync per-visage method on success. Both surfaces
share the same comparison body; pick whichever shape your test
prefers.

### Generic dispatch via `DerivedParity`

Generic helpers that need to call `assert_derived_parity` against
an unknown visage type bound on `djogi::testing::DerivedParity`:

```rust
use djogi::testing::DerivedParity;

fn compare_pair<V: DerivedParity>(a: &V, b: &V)
    -> Result<(), djogi::testing::DerivedParityError>
{
    a.assert_derived_parity(b)
}
```

The trait is sealed (only macro-emitted visages may impl it) and
its method body is identical to the per-visage inherent method.
Rust's inherent-method-first resolution means `visage.assert_derived_parity(&other)`
still resolves via the inherent method at unqualified call sites.

## Fallible Rust expressions

The macro chooses `From<&Model>` or `TryFrom<&Model>` from the syntax
of each `rust` expression. A visage becomes fallible if any derived
entry ends in one of the supported result-shaped tails:

- a trailing `?`
- an outermost `match` whose arms return `Ok(...)` or `Err(...)`
- an outermost `if` / `else` whose branches return result-shaped tails
- an outermost block whose tail is result-shaped
- a bare outermost `Ok(...)` or `Err(...)`

Any other expression is treated as infallible. If you call a helper
that returns `Result<T, E>` and want the error to propagate, make that
explicit with a trailing `?` or an outer result-shaped expression.
The error must convert into `VisageError`.

## Nullability

Use `Option<T>` in `ty` when the SQL expression may return `NULL`:

```rust
#[derived(
    name = nickname_or_label,
    ty = Option<String>,
    scopes = [public],
    sql = "NULLIF(nickname, '')",
    rust = "model.nickname.clone().filter(|s| !s.is_empty())",
)]
```

For `ty = T`, the generated visage field is non-optional. If Postgres
returns `NULL` for that position, fetching the visage fails with
`VisageError::DbComputedNullForNonOptional`, wrapped at the queryset
boundary.

The in-memory `rust` expression must return the same Rust shape as
`ty`: return `Option<T>` for nullable declarations and `T` for
non-nullable declarations.

## Capability tier

Derived projections currently ship as Tier 1 read-time projections:

- They appear as fields on generated visage structs.
- They are selected by `VisageQuerySet` fetches.
- They participate in in-memory `From` / `TryFrom` construction.
- They can be checked with the generated parity helper.

They are not generated as typed query accessors yet. You cannot filter
or order by a derived field through the visage field receiver, and
derived annotations are not a replacement for queryset annotations.
Those richer predicate, ordering, annotation, and descriptor surfaces
are deferred to later tiers.

## Computed properties are separate

`#[computed(sql = "...")]` is a model-side computed-property surface.
It does not emit fields on `FooPublic`, `FooAdmin`, or the other
generated visage structs.

Two rejected forms that adopters from the early Path A draft may
encounter:

```rust
// REJECTED — `expose = ...` inside `#[computed(...)]`
#[computed(sql = "base_price * 2", expose = "public")]
pub double_price: f64,

// REJECTED — `expose(...)` list form inside `#[computed(...)]`
#[computed(sql = "base_price * 2", expose(public, admin))]
pub double_price: f64,
```

Both produce a compile error pointing at the `expose` key and
redirecting to `#[derived(...)]`. The `expose` key was entertained in
an early draft that conflated model-side virtual columns with
visage-side projection entries; the two surfaces represent distinct
concepts and the key was removed before any public adoption.

Use `#[derived(...)]` when the value belongs in a visage projection.
For the model-side computed-property surface and its limitations, see
[Computed Properties](./computed.md).

## Relations and descriptor surface

The `sql` expression is opaque SQL, so it may refer to joined data only
if you write the SQL needed for that value. The `rust` expression sees
`model: &Model`; if it reads a relation through `.resolved()`, the
caller must prefetch or select that relation before constructing the
visage, and the expression should return `VisageError::UnresolvedRelation`
when the relation is absent.

Declaring derived fields inside relation-form exposure grammar is not
part of this tier. A derived field whose `scopes = [...]` overlaps a
relation-form `#[field(expose(scope -> PeerVisage))]` on the same model
is rejected with `E_DJG_VDF_010`.

### Descriptor inventory

Every `(Model, scope)` pair with at least one derived entry in scope
emits a `djogi::descriptor::VisageDescriptor` into a separate
inventory collection from `ModelDescriptor`. Documentation
generators and framework-side lints walk that collection via
`inventory::iter::<VisageDescriptor>()`. Each `VisageDescriptor`
carries the `&'static [DerivedProjection]` slice with per-entry
`ty_path` (token-string form of the `ty = ...` source spelling),
`sql`, `rust`, optional `doc`, and the originating `scopes` list.
The collection is structurally separate from `ModelDescriptor` and
`EnumDescriptor`, so migration / snapshot / `build.rs` paths never
observe derived projections — the storage-vs-projection split is
mechanical, not conventional.

## Error locations

Declaration mistakes are compile errors from the `#[model]` macro and
are covered by Djogi's lihaaf compile-fixture gate. Examples include
missing required keys, unknown scopes, duplicate derived names in a
scope, invalid identifiers, unsupported SQL statement forms, and
computed-exposure misuse.

Rust-expression mistakes surface where generated `From` / `TryFrom`
code is type-checked. SQL name and type mistakes that require database
knowledge surface when the derived visage queryset is executed.
Parity drift between the two paths surfaces from the generated
`assert_derived_parity` helper.
