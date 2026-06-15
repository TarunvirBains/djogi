> [Back to README](../../README.md) | [All Specs](./index.md)

# Visage-Derived Fields

A **visage-derived field** is a projection entry on a visage that does not
correspond to a model column — it is computed from one or more model
columns by a paired SQL expression (evaluated server-side at fetch
time) and Rust expression (evaluated in-memory when constructing the
visage from a `&Model` reference).

Derived fields exist so audience-shaped transport types can synthesise
fields that the model itself does not carry. The model holds storage
truth (raw columns); the visage holds the audience-shaped projection,
including any derivations that belong to that audience and nowhere
else.

---

## Mental model: visages are projections

A visage is not a "narrowed model" with optional add-ons grafted on
afterwards. A visage **is** a projection — a deterministic mapping from
model state (and optionally resolved relation state) to an
audience-shaped output. Every visage equals one projection; the
projection equals the visage.

Under this framing, a visage's projection is a list of *entries*. Each
entry produces one output field. Two kinds of entry exist:

- **Column reference** — copies a field directly from the source
 model. The SQL projects the column; the in-memory conversion reads
 it from `model.<field>`.
- **Derived expression** — evaluates a SQL expression server-side and
 a paired Rust expression in-memory. The two expressions are
 adopter-provided; the framework does not translate between them.

Both kinds compose into one ordered list at the trait level. The kind
discriminant is an internal implementation detail of the SQL emitter
and is sealed off the public surface. A "column-only visage" is a
*degenerate case* of a computed visage (zero derived entries) — not a
separate concept.

This framing matters because it puts derived-field declarations at
the projection definition site (the visage, addressed by `scopes =
[...]`) instead of on the source model as a virtual column. The
model struct stays pure storage; derivations live where they belong.

---

## Motivating scenario

A shipping app records every consignment with a receiver and a shipper
plus a direction flag describing which party is the facility itself.
In the simplest form, `Site` is a scalar Rust type (an enum or
newtype around a text column) and `Direction` is an enum:

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "consignments")]
pub struct Consignment {
 #[field(expose(public, admin, export))]
 pub inbound_site: Site,
 #[field(expose(public, admin, export))]
 pub outbound_site: Site,
 #[field(expose(public, admin, export))]
 pub direction: Direction,
}
```

(A real-world shipping schema might store sites as foreign-keyed
references to a `sites` table; see [Relation references](#relation-references)
for the FK case. The motivating scenario uses scalar `Site` to focus
on the simplest derived-field surface.)

Public consumers want a single `facility_site` field — the side of the
shipment that is the facility itself — without re-deriving the rule in
every consumer:

```sql
CASE WHEN direction = 'inbound' THEN inbound_site ELSE outbound_site END
```

The derivation lives on the transport shape, not on the storage shape.
The model still records both raw sites; each visage projects the
unified single value.

 makes this constructible: visage emission projects both
fields that carry `#[field(expose(...))]` on real model columns and
struct-level `#[derived(...)]` entries scoped to generated visages.
The existing model-side `#[computed(sql = "...")]` remains a
different concept (virtual column on the model, not a projection
entry on a visage) and does not produce visage struct fields.

---

## Declaration

### `#[derived]` is a helper attribute, not an attribute macro

`#[derived(...)]` is a **helper attribute**. It is **not** an
independent attribute macro: there is no
`#[proc_macro_attribute] pub fn derived(...)` entry point, and
adopters never invoke `#[derived]` independently of `Model`.

Ownership is split cleanly between the two existing macro entry
points:

1. **`#[derive(Model)]` REGISTERS `derived` as a helper.** The
 proc-macro derive declaration registers
 `#[proc_macro_derive(Model, attributes(field, derived))]`. This
 registration is purely a rustc-syntax-acceptance contract — it
 tells the compiler "this attribute is legal on a struct that
 `#[derive(Model)]`." The derive itself remains a no-op stub; it
 does no parsing.
2. **`#[model(...)]` OWNS the parsing, validation, and stripping.**
 The `#[model(...)]` attribute macro at the entry point is the
 single site where `#[derived(...)]` attributes are walked,
 parsed, validated, and stripped from `item_struct.attrs` before
 the struct is re-emitted. The emitted struct therefore contains
 neither per-field `#[field(...)]` helper attributes nor
 struct-level `#[derived(...)]` helper attributes. Without this
 stripping, a surviving `#[derived(...)]` attribute would reach the
 user crate's compiled output and trigger an "unknown attribute"
 error downstream — rustc only recognises helper attributes within
 the macro's expansion scope, not on the post-expansion item.

In practice every Djogi model carries both `#[derive(Model)]` and
`#[model(...)]`, so this split is invisible to adopters: the
combination accepts and consumes `#[derived]` correctly. The split
matters at the implementation level — it answers "which macro file
owns the parser?" unambiguously (the answer: `#[model]`).

The "consumed helper, not independent attribute macro" framing matters
for both the implementation (the touch points above) and the user
guide (adopters cannot put `#[derived]` on a struct that does not also
carry `#[derive(Model)]` or `#[model(...)]`).

### `#[derived]` struct-level attribute

A derived field is declared on the model struct as a top-level
`#[derived(...)]` attribute. Each `#[derived]` attribute declares one
projection entry scoped to one or more visages:

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "consignments")]
#[derived(
 name = facility_site,
 ty = Site,
 scopes = [public, admin, export],
 sql = "CASE WHEN direction = 'inbound' \
  THEN inbound_site \
  ELSE outbound_site END",
 rust = "match model.direction { \
  Direction::Inbound => model.inbound_site.clone(), \
  _  => model.outbound_site.clone(), \
 }",
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

Multiple `#[derived(...)]` attributes may decorate one struct; each
adds one entry to the visages named in its `scopes` list. The
attribute lives on the struct rather than on a field because a derived
projection has no field counterpart on the model — putting the
attribute on a (non-existent) field would force the awkward
"phantom field" pattern the earlier spec carried.

### Attribute keys

| Key | Required | Type | Meaning |
|---|---|---|---|
| `name` | yes | bare identifier | Output field name on each scoped visage struct. Must satisfy [identifier rules](#identifier-rules). |
| `ty` | yes | Rust type | Rust type of the output field. Nullability is encoded by wrapping in `Option<_>`. |
| `scopes` | yes | bracketed list of scope idents | One or more of `public`, `self_view`, `admin`, `export`. Empty list rejected at parse time. |
| `sql` | yes | string literal | Postgres SQL expression evaluated server-side. Treated as opaque per-row scalar; see [SQL grammar](#sql-grammar-and-validation). |
| `rust` | yes | string literal | Rust expression evaluated in-memory with `model: &{Model}` bound in scope. See [in-memory derivation](#in-memory-derivation). |
| `doc` | no | string literal | Rustdoc attached to the generated field on every scoped visage. |

All five required keys must be present. Missing any required key is
[E_DJG_VDF_001](#error-taxonomy).

### Identifier rules

The `name` value must be:

1. A bare ASCII lowercase identifier. The first byte must be a `_`
 byte or an ASCII lowercase letter byte (`'a'..='z'`); every
 subsequent byte must be a `_` byte, an ASCII lowercase letter
 byte, or an ASCII digit byte. Total length at most 63 bytes.
 Rejecting uppercase bytes at parse time is what keeps the alias
 stable through Postgres's unquoted-identifier case-folding — see
 [Alias case-folding](#alias-case-folding-and-quoting). General
 shape violations (length, leading-byte class, body-byte class)
 surface as [E_DJG_VDF_004](#error-taxonomy); the uppercase-byte
 case has its own code at [E_DJG_VDF_012](#error-taxonomy) so the
 more precise diagnostic helps adopters who reach for camelCase.
2. Not a Postgres reserved keyword (rejected at parse time using the
 sorted const slice in `djogi-macros/src/ident.rs::RESERVED_KEYWORDS`;
 the lookup is byte-level via `binary_search`, never via regex).
 Violations surface as [E_DJG_VDF_014](#error-taxonomy) — a
 separate code from the general shape rule (E_DJG_VDF_004) so the
 diagnostic can point at the keyword conflict directly.
3. Not prefixed by `__djogi_` (ASCII case-insensitive byte compare) —
 [E_DJG_VDF_005](#error-taxonomy).
4. Not collide with any column exposed in the same scope on the same
 model — [E_DJG_VDF_002](#error-taxonomy).
5. Not collide with any other derived field in any of its `scopes` —
 [E_DJG_VDF_003](#error-taxonomy).

Identifier violations are therefore split across five codes:
[E_DJG_VDF_002](#error-taxonomy) (column collision),
[E_DJG_VDF_003](#error-taxonomy) (derived collision),
[E_DJG_VDF_004](#error-taxonomy) (general shape),
[E_DJG_VDF_005](#error-taxonomy) (`__djogi_` prefix),
[E_DJG_VDF_012](#error-taxonomy) (uppercase byte specifically),
[E_DJG_VDF_014](#error-taxonomy) (reserved keyword).

### Alias case-folding and quoting

The macro emits `(<sql>) AS <name>` into `PROJECTION_LIST` with
`<name>` as a bare unquoted identifier. The lowercase-only rule
above (E_DJG_VDF_012) is what makes the wire alias stable: the
visage struct's `pub <name>: <ty>` field, the row column at the
derived position, and the `COLUMNS[i]` entry all share the same
lowercase byte sequence. Quoting the alias with `"..."` would let
the adopter use `FacilitySite` but would force every downstream
consumer (other SQL that joins or unions the visage query)
to double-quote the column too. Rejecting uppercase at parse time
is the smaller surface.

### Scope rules

Each scope listed in `scopes` must:

1. Be one of the four supported names: `public`, `self_view`, `admin`,
 `export`. Unknown scope identifiers are rejected with
 [E_DJG_VDF_006](#error-taxonomy) at parse time, with the diagnostic
 span anchored at the offending identifier.
2. Not be a scope that elsewhere declares relation-form embedding —
 see [Relation-form visages](#relation-form-visages-deferred).

Additionally, the `scopes = [...]` list itself must be a **set**:
duplicate scope identifiers in the same list — `scopes = [public,
public]` — are rejected with [E_DJG_VDF_013](#error-taxonomy) at parse
time, with the diagnostic span anchored at the second occurrence.
The duplicate would otherwise be a silent no-op (the post-parse
collation deduplicates), which masks a real adopter mistake — most
commonly a copy-paste of the wrong scope. Rejecting at parse time
keeps the declaration honest. The check is per-attribute: a
`#[derived]` with `scopes = [public]` and a separate `#[derived]`
with `scopes = [public]` on the same struct are not duplicates here
(they are independent declarations); duplicates *within one*
`scopes = [...]` list are.

Note: the spec does not require that the scope have at least one
column also exposed. The `#[model(...)]` attribute macro
auto-exposes `id`, `created_at`, `updated_at` to every emitted
scope (see `djogi-macros/src/model/visages.rs::framework_field_decls`),
so every visage carries at least the framework's identity / audit
triple regardless of explicit `#[field(expose(...))]` annotations.
A visage whose only adopter-declared projection is a derived entry
still hydrates from a real row identified by the auto-exposed
primary key.

For the model-level structural constraint that primary keys must be
present, see [Structural constraints](#structural-constraints) below
— it is filed there rather than here because it is a property of the
model carrying `#[derived(...)]`, not of the `scopes = [...]` array
this section validates.

### Structural constraints

The `#[derived(...)]` attribute imposes one model-level structural
requirement that is independent of the `scopes` list it carries:

**The model on which `#[derived(...)]` appears must have a primary
key.** `#[model(pk = None)]` is incompatible with any
`#[derived(...)]` declaration. Rejection is
[E_DJG_VDF_015](#error-taxonomy) at parse time, with the diagnostic
span anchored at the `#[derived]` attribute.

The rationale: visages hydrate per-row identified by primary key,
and adopter test patterns (notably the documented
`assert_derived_parity` workflow) construct the visage by fetching
`V::filter(|f| f.id().eq(model.id))`. A `pk = None` model has no
`id` field, no `Model::Pk` associated type, and no visage queryset
to filter against. The framework rejects the combination
structurally rather than silently emitting a broken visage surface.

This is a *model-level* constraint — a property of where the
attribute is placed (the struct), not of the `scopes` list each
`#[derived(...)]` carries. It belongs here under structural
constraints rather than under [Scope rules](#scope-rules), which
validates the `scopes = [...]` array (duplicates, relation-form
overlaps, unknown scope identifiers).

---

## Semantics

### SQL grammar and validation

The `sql` string is treated as **opaque per-row scalar SQL** at this
phase. The parser performs only the validations enumerated below; the
remainder of the expression is rendered verbatim into the SELECT
projection with surrounding parentheses and an alias matching `name`.

Validations performed at parse time:

1. **No statement separators.** The string must not contain `;`
 outside string-literal or dollar-quoted context (rejected with
 [E_DJG_VDF_007](#error-taxonomy)). This guards accidental
 sub-statements.
2. **No DDL / DML keywords as a leading token.** Strings whose first
 non-whitespace, non-comment token (case-insensitive byte compare)
 matches `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `CREATE`, `DROP`,
 `ALTER`, `GRANT`, `REVOKE`, `TRUNCATE`, `COPY`, or `WITH`
 (top-level) are rejected. Postgres is case-insensitive for
 unquoted identifiers and keywords; the parser matches in either
 case via byte comparison against a sorted const slice — no
 regex (see `feedback_no_regex_in_djogi.md`).
3. **Reserved `$N` placeholders.** Any token consisting of a literal
 `$` byte followed by one or more ASCII digit bytes is reserved
 for future cross-model references and rejects at parse time with
 [E_DJG_VDF_008](#error-taxonomy). The grammar locks now even
 though execution is deferred; see
 [Reserved syntax: `$N`](#reserved-syntax-n).
4. **Aggregate / window function rejection (best-effort guard).**
 The parser performs a token-level scan for a recognised set of
 common SQL aggregate function names — `COUNT`, `SUM`, `AVG`,
 `MIN`, `MAX`, `ARRAY_AGG`, `STRING_AGG`, `JSONB_AGG`, `JSON_AGG`,
 `JSONB_OBJECT_AGG`, `JSON_OBJECT_AGG`, `RANGE_AGG`,
 `MULTIRANGE_AGG`, `XMLAGG`, `BOOL_AND`, `BOOL_OR`, `EVERY`,
 `BIT_AND`, `BIT_OR` — and for the `OVER` keyword (with optional
 whitespace before `(`). Match is case-insensitive. Detection is
 **best-effort against a recognised set**, not exhaustive: custom
 aggregates, user-defined aggregates, and other built-in
 aggregates outside this list slip through and surface at query
 time as Postgres errors. The guard exists to catch the foot-gun
 where an adopter accidentally tries to scope an aggregate to a
 per-row projection; aggregates and window functions must route
 through **Shape Q** (QuerySet-side `.annotate(...)`) or **Shape V**
 (`#[derived(..., aggregate = true)]` with the explicit opt-in marker
 — **Shape V is not yet accepted by the parser; see djogi#226-container**)
 — both locked in [`docs/spec/decisions.md`](./decisions.md#aggregate-annotation-declaration-site)
 (see [Non-goals](#non-goals) item 2).
 Tokens inside single-quoted strings and dollar-quoted bodies are
 skipped so `'COUNT'` does not false-positive.

Validations the parser **does not** perform:

- **Column reference checking.** Identifiers inside `sql` are not
 cross-validated against the model's `{Model}Fields` set. Typos
 surface at the database as a `tokio_postgres::Error` wrapped into
 `DjogiError` via the existing `From<tokio_postgres::Error> for
 DjogiError` conversion (`djogi/src/error.rs:690`). The trade-off:
 compile-time identifier validation would require lexing a closed
 SQL grammar inside the proc macro — a substantial dependency
 surface incompatible with the no-regex discipline. The runtime
 cost is one round trip on the first execution of a broken derived
 field, with a precise error citing the SQL fragment.
- **Type inference.** The Postgres type of the expression is not
 inferred. Row decode at runtime relies on the `ty` declaration; a
 mismatch surfaces as
 [VisageError::DbComputedTypeMismatch](#runtime-errors).
- **Volatility classification.** Derived SQL may be `STABLE`,
 `VOLATILE`, or `IMMUTABLE`; the framework does not annotate or
 enforce. Adopters writing `now()` or `random()` accept that the
 in-memory `rust` path will diverge unless they reproduce the
 same source of nondeterminism. The
 [parity helper](#test-helper-assert_derived_parity) flags drift.

### Rust expression rules

The `rust` string is an expression evaluated at in-memory
visage construction. The macro splices the expression into the
existing `From<&Source>` / `TryFrom<&Source>` body (which today binds
the source as `src: &Source`) inside a block that first rebinds the
source to `model` so the adopter writes against a stable
nomenclature:

```rust
fn from(src: &Consignment) -> Self {
 Self {
 //... column entries via `src.<field>`...
 facility_site: {
 let model: &Consignment = src;
 // <rust expression>
 },
 }
}
```

The `let model = src` rebind happens once per derived entry's init
block. The existing emitter (`djogi-macros/src/model/visages.rs`) is
not retouched — the parameter name stays `src` and the rebind is
local to the derived-entry init expression. This avoids cascading
churn through the existing relation-embed emission, which already
uses `src` extensively.

The expression must be a valid Rust expression that:

1. Refers to model fields via `model.<field>` syntax. The `model`
 binding is `&{Model}` in scope.
2. Evaluates either to the declared `ty` (infallible shape) or to a
 `Result<ty, E>` where `VisageError: From<E>` (fallible shape).
 The `VisageError: From<E>` bound is what the `?` operator desugars
 to (`Err(From::from(e))`); the equivalent trait-bound vocabulary
 `E: Into<VisageError>` is interchangeable via the blanket
 `impl<T, U: From<T>> Into<U> for T`. The spec uses `From<E>` when
 describing what `?` requires at the call site, and
 `Into<VisageError>` when describing trait-bound declarations.
 Fallibility is selected from the expression's syntactic tail —
 see [Fallibility detection](#fallibility-detection-syntactic-tail-not-type)
 for the closed pattern set the macro recognises.
3. Does not perform `async` operations, `await`, or I/O. The
 in-memory path is synchronous and infallible-except-for-explicit-
 `Result`.
4. Does not depend on borrowed data outside `model` — the function
 body sees only `model: &{Model}` as input, where `{Model}` is the
 source model on which `#[derived(...)]` appears.

### Fallibility detection (syntactic tail, not type)

Proc macros operate in token space, not type space; the macro
cannot inspect the actual `rust` expression's type. The macro
recognises **exactly** the following syntactic shapes as fallible
(the closed set), and the emission shape depends on which one matches:

1. **Shape 1 — trailing `?`.** Expression `<expr>?` at the outermost
 tail (after stripping outer parentheses). The inner `?` propagates
 from inside the splice block; the macro emits the splice **without**
 an outer `?`.
2. **Shape 2 — outermost `match`.** A `match` whose every arm body
 ends in `Ok(...)` or `Err(...)` (or itself satisfies this rule
 recursively). The whole block evaluates to `Result<T, E>`; the
 macro emits the splice **with** an outer `?` to unwrap.
3. **Shape 3 — outermost `if`/`else`.** Every branch's tail body
 satisfies this rule. Same emission as Shape 2 — block evaluates to
 `Result<T, E>`; outer `?` unwraps.
4. **Shape 4 — outermost block `{... ; <tail> }`.** `<tail>`
 satisfies this rule. Block evaluates to `Result<T, E>`; outer `?`
 unwraps.
5. **Shape 5 — bare `Ok(...)` or `Err(...)` call at the outermost
 tail.** Expression evaluates to `Result<T, E>`; outer `?` unwraps.

The split matters because Shape 1 already contains the `?` operator;
re-wrapping it with `<expr>?` would double-apply. Shapes 2–5
evaluate to `Result<T, E>` and need the outer `?` to unwrap. The
macro records which shape matched per derived entry and emits the
corresponding init block (see [In-memory derivation](#in-memory-derivation)
for the two emission forms).

In every fallible shape, `?` desugars to `Err(From::from(e))`, so the
operative bound is `VisageError: From<E>`. The error type `E` may be
any type satisfying that bound; in practice adopters either return
`VisageError` directly (`From` is reflexive) or rely on one of the
ecosystem `From<X> for VisageError` impls (which today covers
`Infallible` and will grow as adopter needs surface). See
[Rust expression rules](#rust-expression-rules) for the trait-bound
vocabulary distinction (`From<E>` vs `Into<VisageError>`).

Any other shape is **infallible** — including expressions that use
`?` inside a nested block whose value is unwrapped before reaching
the outer tail. An adopter who writes ambiguous code (e.g., a
function call `compute(model)` whose return type is `Result<T, E>`
but is not syntactically wrapped) receives a compile error from the
surrounding `From<&Model>` impl's type check. The remediation
depends on the intent:

- **If the adopter wants the fallibility to propagate** (the common
 case): rewrite to `compute(model)?` — a Shape 1 trailing `?`. The
 inner `?` propagates from the surrounding `try_from`, the macro
 emits the Shape 1 splice without an outer `?`, and the visage
 lifts to `TryFrom<&Model>`.
- **If the adopter wants to handle the Result inline and return the
 unwrapped value**: rewrite to `compute(model).unwrap_or(default)`
 or an explicit `match`. The expression then evaluates to `T` and
 the visage stays on the `From<&Model>` infallible path.

Wrapping in `Ok(compute(model))` is **incorrect** — it produces
`Ok(Result<T, E>)`, double-wrapping the `Result` and breaking the
generated code. The Round 1 remediation note that suggested
`Ok(...)` wrapping was a documentation error caught in Round 2.

An explicit `fallible = true` key is not provided in v0.1.0: the
syntactic-tail rule covers the common cases, and an explicit key
would force the adopter to keep two declarations in sync (the
expression's behavior and the key). See
[Open Question 2](#open-questions-for--dual-review) for the
discussion.

Mixed fallibility within a single visage:

- All-infallible derived entries → `impl From<&Model> for V`.
- Any fallible derived entry → `impl TryFrom<&Model, Error = VisageError>
 for V`; infallible entries lift to the fallible path via the
 existing `impl From<Infallible> for VisageError` in `djogi::visage`.

### Nullability

The output column's nullability is determined by the Rust `ty`:

- `ty = Site` → NOT NULL at the visage struct level; the row decoder
 errors with `VisageError::DbComputedNullForNonOptional` (wrapped in
 `DjogiError::Visage(...)` at the fetch boundary — see
 [Runtime errors](#runtime-errors)) if Postgres returns NULL.
- `ty = Option<Site>` → nullable; the row decoder yields `None` on
 NULL.

The `sql` expression's nullability is not statically known to the
framework. Adopters are responsible for matching declared `ty` to the
expected SQL output. The mismatch surfaces only at runtime; no
compile-time check exists for this gap.

### Relation references

The `rust` expression may reference resolved relations on `model`
via the existing relation-resolution surface. Note that this section
assumes a different example model in which the relation field is an
actual `ForeignKey<Site>` (the [motivating scenario](#motivating-scenario)
above keeps `inbound_site` / `outbound_site` as scalar `Site` values
to focus on the simplest case; the example below shows the relation
case explicitly):

```rust
// Hypothetical alternative shape with FK-typed relation fields.
#[derive(Model, Debug, Clone)]
#[model(table = "consignments")]
#[derived(
 name = shipper_country,
 ty = String,
 scopes = [admin],
 sql = "(SELECT country FROM sites WHERE id = consignments.outbound_site_id)",
 rust = "Ok::<_, ::djogi::VisageError>(
 model.outbound_site
 .resolved()
 .ok_or(::djogi::VisageError::UnresolvedRelation {
  model: \"Consignment\",
  field: \"outbound_site\",
  scope: \"admin\",
  })?
 .country
 .clone()
 )",
)]
pub struct Consignment {
 #[field(expose(admin))]
 pub outbound_site: ForeignKey<Site>,
 //...
}
```

`ForeignKey<T>::resolved()` returns `Option<&T>` (per
`djogi/src/relation/foreign_key.rs:149`), so the adopter explicitly
threads the `Option` through `.ok_or(...)?` to surface
`VisageError::UnresolvedRelation` when the relation was not
pre-loaded via `.prefetch(...)` / `.select_related(...)`. The tail
`Ok::<_, ::djogi::VisageError>(...)` is **Shape 5** (bare `Ok(...)`
at the outermost tail) under the [fallibility detection](#fallibility-detection-syntactic-tail-not-type)
rules — the surrounding `TryFrom<&Consignment>` impl is emitted with
an outer `?` after the splice block, and the inner `.ok_or(...)?`
propagates through the outer block's `Result`. `VisageError:
From<VisageError>` is reflexively held, so the `?` desugaring
type-checks.

The SQL path independently handles the join via the adopter's
expression and is not subject to the same Rust-side
relation-resolution restriction.

Relation references in `rust` are *not* validated at macro time
against the model's known relation set; a missing relation surfaces
as a Rust compile error at `model.<rel>.resolved()`.

---

## Generated visage shape

For each scope `S` named in any `#[derived(scopes = [S,...],...)]`
attribute on a model, the macro emits:

1. The visage struct, with fields in this order:
 - Framework columns (`id`, `created_at`, `updated_at`) — always
 present at the head of every visage regardless of explicit
 `#[field(expose(...))]` annotations (see
 `djogi-macros/src/model/visages.rs::framework_field_decls`).
 - All user columns marked `#[field(expose(S,...))]`, in model
 struct-declaration order.
 - All derived entries with `S` in their `scopes`, in attribute
 declaration order.
2. `impl ::djogi::__private::DjogiVisageSealed for <VisageName> {}` — the
 metadata seal required by `DjogiVisage`; macro-emitted visages satisfy
 it via `::djogi::__private::DjogiVisageSealed`, which is outside the
 public API surface. Ordinary adopter code cannot satisfy it without
 naming `__private` paths; deliberate hand-impls through those paths are
 outside the supported public contract rather than compiler-impossible (see
 [Trait surface](#trait-surface)).
3. The visage's `DjogiVisage` trait impl, with the trait surface
 defined in [Trait surface](#trait-surface).
4. A `FromPgRow` impl matching the visage struct's field order
 exactly (one positional decode per field).
5. Either `impl From<&Model> for V` (all derived entries
 infallible) or `impl TryFrom<&Model> for V` (any derived
 entry fallible) — see [In-memory derivation](#in-memory-derivation).
6. The existing `DjogiVisageOf<Model>` pairing impl.

Example expansion for `ConsignmentPublic`:

```rust
pub struct ConsignmentPublic {
 pub id: HeerIdRecencyBiased,
 pub created_at: DateTime,
 pub updated_at: DateTime,
 pub inbound_site: Site,
 pub outbound_site: Site,
 pub direction: Direction,
 pub facility_site: Site, // derived
}

impl djogi::__private::DjogiVisageSealed for ConsignmentPublic {}

impl djogi::DjogiVisage for ConsignmentPublic {
 type Model = Consignment;
 const SCOPE: &'static str = "public";
 const COLUMNS: &'static [&'static str] = &[
 "id", "created_at", "updated_at",
 "inbound_site", "outbound_site", "direction",
 "facility_site",
 ];
 const PROJECTIONS: &'static [djogi::__private::ProjectionEntry] = &[ /*... */ ];
 const PROJECTION_LIST: &'static str =
 "id, created_at, updated_at, inbound_site, outbound_site, direction, \
 (CASE WHEN direction = 'inbound' THEN inbound_site ELSE outbound_site END) \
 AS facility_site";
}

impl From<&Consignment> for ConsignmentPublic {
 fn from(src: &Consignment) -> Self {
 Self {
 id: src.id,
 created_at: src.created_at,
 updated_at: src.updated_at,
 inbound_site: src.inbound_site.clone(),
 outbound_site: src.outbound_site.clone(),
 direction: src.direction,
 facility_site: {
 let model: &Consignment = src;
 match model.direction {
  Direction::Inbound => model.inbound_site.clone(),
  _  => model.outbound_site.clone(),
 }
 },
 }
 }
}
```

### Visage struct field order is the projection order

The visage struct's declared field order **is** the projection order
across both column references and derived entries. `VisageQuerySet`
emits `PROJECTION_LIST` in struct-field order; `FromPgRow` decodes
positionally in the same order. No per-entry ordinal field is needed
because the macro's projection-entry collection order is the
struct-field order.

This is a behavior shift from the earlier draft that introduced
`source_ordinal: u16`: under Path B (visages-as-projections), there
is no model-side ordering to reconcile because derived entries do not
appear in the model's `FieldDescriptor` list at all.

---

## SQL emission

### `VisageQuerySet` projection emission

The `VisageQuerySet<V>::columns` slice (currently `&'static
[&'static str]`) is replaced by a single `projection_list: &'static
str` field carrying the macro-time rendering. At query time the
queryset accumulates SQL via the existing `SqlAccumulator` and
**splices `V::PROJECTION_LIST` directly into the SELECT slot** —
there is no runtime walk over `V::PROJECTIONS`.

The macro renders `PROJECTION_LIST` once at compile time by walking
`V::PROJECTIONS` (the static list of `ProjectionEntry` values
constructed during macro expansion). The rendering rules are:

- For each **column entry**: push the bare column identifier.
- For each **derived entry**: push `(<sql>) AS <alias>` — outer
 parentheses around the adopter's SQL, then `AS <alias>` where
 `<alias>` is the entry's `name`.

Entries are joined with `", "` to produce the final
`PROJECTION_LIST` string. This rendering happens **at macro
expansion**, not at query time; the queryset's hot path never
walks `PROJECTIONS`.

`V::PROJECTIONS` is **metadata-only** at this phase: it is the
typed shape of the projection consumed by **framework-internal**
walkers — framework-side lints, debug formatters, and the future
Tier-2 per-entry SQL renderer. It is **not** the surface
documentation generators read: `ProjectionEntry::Derived` carries
only `alias` + `sql`, which is insufficient to render a derived
field's rustdoc (no `ty_path`, `rust`, or `doc`). The richer public
descriptor/inventory surface for documentation generators ships
alongside this trait in — see
[Stage 2](#stage-2--visage-side-descriptor-inventory) for the
`VisageDescriptor` / `DerivedProjection` shapes and their
inventory-collection guarantees.
The parity helper does **not** read `PROJECTIONS` either: it is
emitted as an inherent method per visage with the derived-field set
hard-coded at macro-expansion time — see [Test helper:
`assert_derived_parity`](#test-helper-assert_derived_parity) for
why the macro emits per-visage rather than walking metadata at
runtime. The queryset's read path uses `PROJECTION_LIST`
exclusively. The two trait constants are kept in lockstep at macro
time — `PROJECTION_LIST` is the textual rendering of
`PROJECTIONS` — but they serve different consumers:

- `PROJECTION_LIST`: SELECT-slot emission. Single-string splice.
- `PROJECTIONS`: metadata. Walked by **framework-internal**
 consumers only — framework-side lints, debug formatters, and
 future Tier-2 per-entry SQL renderers; never on the queryset hot
 path. It is not the public documentation descriptor surface; that
 richer descriptor/inventory channel ships via `VisageDescriptor`
 / `DerivedProjection` in
 [Stage 2](#stage-2--visage-side-descriptor-inventory).

A future feature (per-call SQL variation, e.g., a queryset method
that disables a derived entry per request) would either require a
new rendered string per variation or fall back to walking
`PROJECTIONS`. Neither is in Tier 1 scope — see
[Open Question 9](#open-questions-for--dual-review).

### Column-list constants: `COLUMNS` vs `PROJECTION_LIST`

The visage trait carries two related projection constants:

- `const COLUMNS: &'static [&'static str]` — the names that appear
 at each ordinal position of the visage's SELECT row, in
 struct-field order. For column entries this is the raw column
 name; for derived entries this is the entry's alias. This is the
 slice that drives `FromPgRow`'s positional decode and its
 debug-build name guard.
- `const PROJECTION_LIST: &'static str` — the full projection string
 (columns + derived expressions with aliases) used by
 `VisageQuerySet` to emit the SELECT clause.

`FromPgRow::COLUMN_LIST` for a visage equals `PROJECTION_LIST` so
the wire shape matches the positional decode shape exactly. The
historical model-side invariant `FromPgRow::COLUMN_LIST ==
COLUMNS.join(", ")` continues to hold for models, where every
position in `COLUMNS` is a bare column name and `COLUMN_LIST` is
their comma-join. Visages with derived entries break that
identity (the alias position renders as `(<sql>) AS <alias>` in
`COLUMN_LIST`, just `<alias>` in `COLUMNS`); the visage's
`FromPgRow` impl therefore sets `COLUMN_LIST = PROJECTION_LIST`,
which `decode_at` does not consult (it consults the per-ordinal
`COLUMNS[i]` string and the row's wire column name, both of which
are the alias for derived positions).

If a visage has zero derived entries, `PROJECTION_LIST ==
COLUMNS.join(", ")` and the column-only invariant degenerates to
the model-side one.

### Outer parentheses

The macro wraps every derived `sql` expression in a single pair of
outer parentheses before splicing into `PROJECTION_LIST`. The wrapping
is unconditional — the adopter may not have parenthesised, and even
parenthesised expressions tolerate a redundant outer pair. This
prevents precedence interactions with the surrounding comma-separated
list.

### Alias collision

Within one visage, every projection entry's name (column name or
derived `name`) must be unique. The
[identifier collision check](#identifier-rules) above enforces this at
macro time; no two entries with the same name reach SQL emission.

### Read-only surface preservation

`VisageQuerySet` continues to omit write terminals (`bulk_create`,
`save`, `delete`). Derived fields are not addressable through any
write path because they are not model columns; this falls out of the
existing read-only surface for free.

---

## In-memory derivation

### `From<&Model>` and `TryFrom<&Model>` emission

The macro emits exactly one of two conversion impls per visage. The
decision composes with the existing scalar-vs-relation-form rule in
`djogi-macros/src/model/visages.rs`:

| Visage shape | Derived fallibility | Emitted impl |
|---|---|---|
| Scalar-only, no derived | n/a | `impl From<&Model>` (existing) |
| Scalar-only, all-infallible derived | infallible | `impl From<&Model>` |
| Scalar-only, any fallible derived | fallible | `impl TryFrom<&Model, Error = VisageError>` |
| Relation-nesting (any `expose(scope -> Peer)`), no derived | n/a | `impl TryFrom<&Model, Error = VisageError>` (existing) |
| Relation-nesting, any derived (fallible or not) | irrelevant | `impl TryFrom<&Model, Error = VisageError>` |

In other words, `TryFrom` wins if either:

- the visage embeds a peer visage via `#[field(expose(scope -> Peer))]`
 (existing rule), or
- at least one derived entry is fallible (new rule).

A scalar-only visage with all-infallible derived entries continues
to enjoy `impl From<&Model>` — the new feature does not regress the
infallible path for adopters who keep their derived expressions
total.

**Infallible body shape** (`impl From<&Model>`):

```rust
impl From<&{Model}> for {Visage} {
 fn from(src: &{Model}) -> Self {
 Self {
 <col>: src.<col>.clone(), // for each column entry
 <name>: {  // for each derived entry
 let model: &{Model} = src;
 <rust>
 },
 }
 }
}
```

**Fallible body shape** (`impl TryFrom<&Model>`):

```rust
impl TryFrom<&{Model}> for {Visage} {
 type Error = djogi::VisageError;
 fn try_from(src: &{Model}) -> Result<Self, Self::Error> {
 Ok(Self {
 <col>: src.<col>.clone(),

 // Shape 1 — trailing `?`. Inner `?` propagates from the
 // surrounding `try_from` body; no outer `?`.
 <name_shape_1>: { let model: &{Model} = src; <rust> },

 // Shapes 2–5 — block evaluates to `Result<T, E>`. Outer
 // `?` unwraps, calling `Err(From::from(e))` per `?`
 // desugaring; the bound `VisageError: From<E>` is what
 // makes the propagation type-check.
 <name_shapes_2_5>: { let model: &{Model} = src; <rust> }?,

 // Infallible entry inside an otherwise-fallible visage —
 // lifted via `impl From<Infallible> for VisageError`;
 // no outer `?` because the block returns `T`, not
 // `Result<T, _>`.
 <name_infallible>: { let model: &{Model} = src; <rust> },
 })
 }
}
```

The macro records the matched shape per derived entry at parse time
and selects the corresponding init block at codegen — there is no
single "fallible body shape" that fits all five fallible shapes; the
outer `?` is shape-dependent. See
[Fallibility detection](#fallibility-detection-syntactic-tail-not-type)
for the shape catalogue and the rationale behind the split.

The per-derived-entry `let model = src;` rebind is what makes the
adopter's `model.<field>` syntax work without retouching the
existing model-side emitter (which binds the source as `src`). The
rebind is zero-cost — the compiler inlines it through the borrow
binding.

Mixed fallibility within a single fallible visage works because the
existing `impl From<Infallible> for VisageError` glue in
`djogi/src/visage.rs` lets infallible entries propagate through `?`
without an explicit error type lift.

### Relation between SQL and Rust paths

The `sql` and `rust` expressions are adopter-provided in parallel.
The framework does not translate between them, does not infer one
from the other, and does not enforce equivalence at compile time.

**Drift risk.** Nothing prevents an adopter from writing a SQL
expression and a Rust expression that compute different values. Drift
is detectable at runtime via the
[parity helper](#test-helper-assert_derived_parity), and visible to
users only as inconsistency between fetched and in-memory-constructed
visages. This trade-off is deliberate: automatic SQL→Rust translation
would require a closed SQL grammar inside the proc macro
(incompatible with the no-regex discipline), and a closed grammar's
cliff would look arbitrary to adopters.

**When to use which.** Adopters who only ever fetch derived visages
through `VisageQuerySet` never exercise the in-memory path; in that
case the `rust` expression exists for the parity helper and for any
caller that constructs a visage from a `&Model` reference (e.g.,
test fixtures, mock servers, in-memory caches). Adopters who do both
should write the parity helper into their test suite as a guard.

### Why `model: &{Model}`, not owned `{Model}`

The expression sees `model: &{Model}` rather than an owned `{Model}`
so the conversion is non-consuming. Visages are derived projections;
nothing should require destructive conversion. Cloning is the
adopter's responsibility inside the `rust` body for fields whose
types are not `Copy`.

---

## Trait surface

This work adds a `DjogiVisage` trait to `djogi/src/visage.rs`,
alongside the existing `VisageError` enum. Before this surface,
visages only had the `DjogiVisageOf<M>` marker trait in
`djogi/src/visage_boundary.rs` (which carries the model-to-visage
pairing, no associated items) and per-visage inherent methods emitted
by the `#[model]` macro.

`DjogiVisage` centralises projection metadata so generic
**framework-internal** code — lints, debug formatters, and the future
Tier-2 predicate-rendering path — can read it through a single bound.
It carries the source-model pairing through an associated `type Model:
Model` so a generic `V: DjogiVisage` consumer reaches the source table
via `<V::Model as Model>::table_name()` without threading the source
model in as a separate type parameter at every call site.

The trait has two supertraits: `DjogiVisageOf<Self::Model>` carries
the visage ↔ source-model pairing for generic code that names only
`V`; a separate metadata-only `private::Sealed` (no reflexive
blanket, re-exported as `::djogi::__private::DjogiVisageSealed` for
macro emission) is the closed-world gate on `DjogiVisage` itself.
`DjogiVisageOf<Self::Model>` alone is not a sufficient seal because
`visage_boundary::private::Sealed<M>` carries a reflexive
`impl<M: Model> Sealed<M> for M` blanket — every model already
satisfies `DjogiVisageOf<M>` trivially, so a hand-rolled
`impl DjogiVisage for MyModel { type Model = Self;... }` would pass
the pairing supertrait unchallenged. The metadata seal has no reflexive
blanket; the single emitter is the `#[model]` macro. The macro emits
`impl ::djogi::__private::DjogiVisageSealed for {Visage}`,
`impl DjogiVisageOf<{Source}> for {Visage}`, and
`impl DjogiVisage for {Visage}` in one pass. The seal is at the
existing `__private` convention boundary — downstream code naming
`djogi::__private::DjogiVisageSealed` is outside the public contract
and the framework reserves the right to break it without notice.

When a model is declared less-public than its generated visage (the
unusual but legal case of `pub(crate) struct Inner` paired with `pub
struct InnerPublic`), rustc's `private_interfaces` lint fires on the
macro-emitted impl. Mirror the visage visibility on the model
(`pub struct Inner`) or `#[allow(private_interfaces)]` on the source
if the model must stay private — Rust has no way to expose
`<V::Model as Model>::table_name()` without making the binding
nameable. This is a conscious accepted tradeoff: the spec's
`V::Model` ergonomics outweigh the visibility-asymmetry edge case.

Documentation generators (rustdoc reference tables, the `djogi docs`
CLI) are **not** `DjogiVisage` consumers — they consume the richer
[`VisageDescriptor`] / [`DerivedProjection`] inventory channel
described in [Stage 2](#stage-2--visage-side-descriptor-inventory).
`ProjectionEntry::Derived` carries only `alias` + `sql` on the
runtime trait constant (the queryset hot path needs just those two
fields); the richer per-entry metadata (`ty_path`, `rust`, `doc`,
`scopes`) lives on `DerivedProjection` records collected through
`inventory::iter::<VisageDescriptor>()`. The parity helper is also
**not** a `DjogiVisage` consumer; it is emitted as an inherent
method per visage (plus a parallel `DerivedParity` trait impl for
generic dispatch) with the derived-field set hard-coded at macro
time.

The `ProjectionEntry` discriminant is sealed off the public surface.

[`VisageDescriptor`]:../../djogi/src/descriptor.rs
[`DerivedProjection`]:../../djogi/src/descriptor.rs

### Trait shape

```rust
pub trait DjogiVisage:
 crate::visage_boundary::DjogiVisageOf<<Self as DjogiVisage>::Model>
 + private::Sealed
{
 /// Source model the visage is a projection of. Every macro-emitted
 /// `impl DjogiVisage for {Visage}` sets `type Model = {Source}`.
 /// Generic code reaches the source table via
 /// `<V::Model as Model>::table_name()` — the canonical entry
 /// point already established by `Model`.
 type Model: crate::model::Model;

 /// Stable scope key (`"public"` / `"self_view"` / `"admin"` /
 /// `"export"`). A `&'static str` rather than a typed enum to
 /// match the existing `SCOPES` tuple shape in
 /// `djogi-macros/src/model/visages.rs` and to avoid introducing
 /// a sibling enum to `VisageError`'s string-typed `scope` field.
 /// A future phase may swap this to a sealed `enum djogi::Scope`
 /// once the surrounding surface justifies the migration.
 const SCOPE: &'static str;

 // Note: there is intentionally no `TABLE` constant on this trait.
 // Source-table access goes through
 // `<V::Model as Model>::table_name()` — the supertrait bound
 // makes that callable from generic code, and a parallel `TABLE`
 // const would duplicate state the supertrait already pins.

 /// Names that appear at each ordinal position of the visage's
 /// SELECT row, in struct-field order. For column entries this is
 /// the raw column name; for derived entries this is the entry's
 /// `name` (which equals the alias emitted into the SELECT —
 /// see [Alias case-folding](#alias-case-folding-and-quoting)).
 /// `COLUMNS[i]` is what `decode_at` will compare against the
 /// `i`-th row column's name in the debug-build name guard.
 ///
 /// This **is** the visage's `FromPgRow::COLUMNS` (the visage's
 /// `FromPgRow` impl re-exports the same slice). The historical
 /// `FromPgRow::COLUMN_LIST == COLUMNS.join(", ")` invariant
 /// becomes `FromPgRow::COLUMN_LIST == PROJECTION_LIST` for
 /// visages — the only callers that interpolated `COLUMN_LIST`
 /// directly were the visage queryset builders, which now route
 /// through `PROJECTION_LIST` instead.
 const COLUMNS: &'static [&'static str];

 /// Full projection (columns and derived expressions) in
 /// struct-field order. **Metadata-only** at this phase: walked
 /// by **framework-internal** consumers only — framework-side
 /// lints, debug formatters, and the future Tier-2 per-entry SQL
 /// renderer. **Not** the surface documentation generators
 /// consume: `ProjectionEntry::Derived` carries only `alias` +
 /// `sql`, lacking the `ty_path` / `rust` / `doc` fields rustdoc
 /// reference tables and the `djogi docs` CLI need. The richer
 /// `VisageDescriptor` / `DerivedProjection` inventory channel
 /// (see [Stage 2](#stage-2--visage-side-descriptor-inventory))
 /// ships in alongside this trait — documentation
 /// generators reach the richer shape through
 /// `inventory::iter::<VisageDescriptor>()`.
 /// The parity helper does not read this — it is emitted as an
 /// inherent method per visage (plus a parallel `DerivedParity`
 /// trait impl) with derived fields hard-coded at macro time.
 /// The queryset hot path uses `PROJECTION_LIST` instead.
 /// Adopters do not name `ProjectionEntry` — the type is `pub` to
 /// satisfy the trait constant's type, but lives under
 /// `__private` and carries the "do-not-construct" convention
 /// warning matching `__private::VisageSealed` and
 /// `__private::pk_seal`.
 const PROJECTIONS: &'static [crate::__private::ProjectionEntry];

 /// Rendered SQL projection list rendered once at macro time:
 /// `"id, name, (CASE... END) AS facility_site"`. `VisageQuerySet`
 /// splices this single string into the SELECT slot at query time —
 /// no runtime walk over `PROJECTIONS`. Equal to
 /// `COLUMNS.join(", ")` when there are no derived entries
 /// (because the column entry's name and its alias coincide).
 const PROJECTION_LIST: &'static str;
}
```

`DjogiVisage::COLUMNS` and the per-visage `FromPgRow::COLUMNS` are the
**same slice** — the macro emits them from one source of truth. The
spec's earlier framing of `COLUMNS` as "column-only subset" was wrong:
the row coming back from the SELECT carries one column per ordinal
position (whether storage column or derived alias), so the positional
decoder needs the alias at the derived field's position too.

### Sealed `ProjectionEntry`

The `ProjectionEntry` type is `pub` (the trait surface requires it
to be nameable in the `PROJECTIONS: &'static [ProjectionEntry]`
constant declaration) but lives behind `crate::__private` and
carries a **convention-only seal** — the same precedent as the
existing `__private::VisageSealed` and `__private::pk_seal`
surfaces (see `djogi/src/lib.rs:186-213`):

```rust
#[doc(hidden)]
pub mod __private {
 pub use crate::visage::projection::ProjectionEntry;
}

pub(crate) mod visage {
 pub mod projection {
 /// Sealed discriminant — **do not construct or match on this
 /// type from downstream code**. The variants are public only
 /// because the `DjogiVisage::PROJECTIONS` trait constant
 /// requires the enum to be nameable through
 /// `::djogi::__private::ProjectionEntry`; reaching this type
 /// from outside the macro-emitted path is breaking the
 /// framework boundary, and the framework reserves the right
 /// to change the variants in any future release without
 /// notice. Same convention as `__private::VisageSealed` and
 /// `__private::pk_seal` — the warning is the seal.
 #[non_exhaustive]
 pub enum ProjectionEntry {
 #[doc(hidden)]
 Column(&'static str),
 #[doc(hidden)]
 Derived {
 alias: &'static str,
 sql: &'static str,
 },
 }
 }
}
```

The `pub` visibility is required for the trait constant; the
`#[non_exhaustive]` attribute prevents exhaustive `match`
construction across the crate boundary even when adopters do reach
in. The `__private` module hiding plus the `#[doc(hidden)]` on
variants removes the type from the rustdoc surface. The
"do-not-construct" warning is the seal at the language-of-conduct
level — it does not mechanically prevent construction, but it
matches the precedent the framework already establishes for its
other internal-boundary types. The macro emits cross-crate code
through `::djogi::__private::ProjectionEntry`, matching the existing
`macro_path_routing` convention.

A stronger seal (an opaque struct wrapping a private enum) was
considered and rejected: opaque-struct sealing would make the
declaration of the `PROJECTIONS` constant in macro-emitted code
noticeably uglier (the macro would need to call a private
constructor function for each variant) without buying real
protection against adopters who have already decided to reach into
`__private`. Convention is the contract here.

### Relation to existing `DjogiVisageOf<M>`

The existing `DjogiVisageOf<M>` (marker trait, no associated items)
continues to carry the visage ↔ source-model pairing through its
`visage_boundary::private::Sealed<M>` supertrait. `DjogiVisage`
supertypes **both** `DjogiVisageOf<Self::Model>` (pairing) **and** a
separate metadata-only `private::Sealed` re-exported as
`::djogi::__private::DjogiVisageSealed`. The two supertraits serve
distinct roles and are **not** the same seal:
`DjogiVisageOf<Self::Model>` says "V is a projection of M" and is
useful for generic code that names only `V`; `private::Sealed` is the
closed-world gate. They must be separate because
`visage_boundary::private::Sealed<M>` carries a reflexive
`impl<M: Model> Sealed<M> for M` blanket — every model satisfies
`DjogiVisageOf<M>` already, so `DjogiVisageOf<Self::Model>` alone
would not prevent a hand-rolled `impl DjogiVisage for MyModel`.
The metadata seal has no reflexive blanket. Every `impl DjogiVisage
for V` therefore implicitly demands both supertraits (emitted by the
same macro pass): `DjogiVisageOf<M>` says "V is a visage of M";
`private::Sealed` says "V was registered by the macro".

The reflexive `impl<M: Model> DjogiVisageOf<M> for M` blanket
continues to hold for the marker trait; **no blanket
`impl<M: Model> DjogiVisage for M` is provided**. Generic code that
needs to operate on "the model itself as a degenerate visage" works
against `DjogiVisageOf<M>` only; code that needs projection metadata
goes through `DjogiVisage`. Models are not visages; they have a
descriptor instead of a projection.

---

## Capability tiers

The full visage-derived-field surface lands in three tiers. Only
Tier 1 ships in v0.1.0; Tiers 2 and 3 land as anchored deferrals to
named future phases.

### Tier 1 — read-time projection (v0.1.0)

Adopters can:

- Declare derived fields with `#[derived(...)]`.
- Fetch derived visages via `VisageQuerySet<V>::fetch_all` /
 `fetch_one` / `first`.
- Construct derived visages in-memory via `From<&Model>` /
 `TryFrom<&Model>`.
- Run the parity helper in tests.

Adopters cannot (Tier 2 / Tier 3 scope; see below):

- Reference a derived field in a filter expression.
- Order by a derived field.
- Annotate a queryset with a derived expression bound to a name.

The "cannot reference in a filter expression" prohibition is
mechanically enforced: derived fields are **excluded** from the
generated `{Visage}Fields` typed-accessor type. The accessor surface
exposes `f.<column>()` methods only for column entries; no
`f.<derived>()` accessor is generated. An adopter writing
`V::filter(|f| f.facility_site().eq(...))` for a derived
`facility_site` therefore fails at compile time (with a "no method
named `facility_site`" error from rustc) rather than at SQL-emit time
with a less precise framework error.

When Tier 2 ships, the accessor surface widens to include derived
fields; until then, the exclusion is part of the Tier-1 contract.
Stage 9 carries a `compile_fail` fixture that asserts this: a visage
with a derived `facility_site` and a test that calls
`.filter(|f| f.facility_site()...)` must produce a `no method named
"facility_site"` diagnostic. The fixture pins the Tier-1 enforcement
so the eventual Tier-2 widening is a deliberate spec amendment, not
an accidental regression.

### Tier 2 — predicate use (deferred to a named phase)

Filtering on a derived field requires the visage queryset to emit the
derived expression in both the SELECT projection (already done in
Tier 1) and the WHERE clause. The two emissions must use the same
SQL tokens to avoid Postgres planning the expression twice (no
common-subexpression elimination across SELECT and WHERE for
non-deterministic expressions). This is non-trivial when the
expression contains positional binds (which it cannot in Tier 1 due
to the [`$N` reservation](#reserved-syntax-n), but will in a future
phase).

Deferral anchor: the predicate-pushdown work in the visage queryset
cluster. The contract this spec leaves for that cluster:

- Tier 2 must reuse **per-entry SQL renderers from `V::PROJECTIONS`**,
 not the rendered `PROJECTION_LIST` string. `PROJECTION_LIST` is a
 comma-joined SELECT list (with aliases of the form `(<sql>) AS
 <alias>`); a WHERE clause needs a single predicate expression, so
 Tier 2 walks `PROJECTIONS` looking up the matching entry by name
 and emits its `sql` fragment alone — with the same outer
 parenthesisation discipline used at SELECT time, but without the
 `AS <alias>` suffix and without the surrounding commas. The macro
 may consolidate the per-entry rendering into a helper shared
 between SELECT and WHERE paths; the point is that the rendered
 textual `PROJECTION_LIST` is the wrong source.
- Tier 2 must reject filters that reference an unresolved relation in
 the derived `sql` at predicate-emit time, surfacing
 `VisageError::UnresolvedRelationInPredicate` (new variant; not
 added until Tier 2 lands).

### Tier 3 — ordering and annotation (deferred to a named phase)

`ORDER BY <derived>` and `.annotate(name = <expression>)` (which
binds a queryset-scope alias) build on Tier 2's expression-reuse
machinery. They are out of scope for v0.1.0 and tracked as a
dependent phase on Tier 2. See [Out of scope](#out-of-scope-named-future-work).

---

## Relation-form visages (deferred)

A relation-form visage embeds peer visages of related models (e.g.,
`ConsignmentAdmin` embedding a `SiteAdmin` for its shipper). In the
current `VisageQuerySet` design, relation-form visages dispatch to a
separate projector path that handles eager loading and peer
hydration.

The intersection of "derived field" and "relation-form embedding"
within one scope is **rejected at parse time** with
[E_DJG_VDF_010](#error-taxonomy). The rejection is rationale-anchored,
not a permanent constraint:

**Why rejected today.** The relation-form projector does not yet
emit derived expressions. Adding it requires:

1. The peer-projection path renders each derived entry alongside its
 column entries in the outer SELECT.
2. The peer-projection path validates that any column the derived
 `sql` references is either a column on the parent table or a
 column on a prefetched / select-related relation; otherwise it
 rejects with `E_DJG_VDF_011` at predicate-emit time.
3. The hydration path materialises derived columns into the visage
 struct via the existing `FromPgRow` machinery (already correct
 for derived once the SELECT shape is correct).

**Unblock contract.** When the peer-projection cluster ships, it
must satisfy the three points above. This spec leaves no Tier-1
machinery that depends on relation-form support — derived fields and
relation-form embedding remain independently functional, just not
combinable until the cluster lands.

**Workaround for adopters today.** If you need both a derived field
and a relation-form peer for the same audience, split the visage
into two: one scope for the derived projection, a sibling scope (or
manually authored DTO) for the relation embedding. Tracked as
"deferred mixed-projection visages" in the peer-projection cluster
issue.

---

## Reserved syntax: `$N`

Any token consisting of a literal `$` byte followed by one or more
ASCII digit bytes inside a derived `sql` string is **reserved syntax**
at this phase and rejects at parse time with
[E_DJG_VDF_008](#error-taxonomy).

The reservation locks the grammar for a future cross-model
reference feature that has not been designed yet. Reserving the
syntax now prevents an adopter from writing a literal `$1` (e.g., as
part of a JSONB path) and expecting the framework to render it
verbatim; when the feature lands, that expectation would break.

The error message points adopters at the tracking issue and
suggests escape patterns (use `chr(36) || '1'` if a literal `$1`
must appear in the output, until proper escaping lands).

Cross-model references, when designed:

- Will likely take the form `$<ref>` where `<ref>` resolves a
 prefetched / select-related relation.
- Will require co-design with the Tier 2 predicate work because
 cross-model derived predicates need both SELECT and WHERE
 emission to align.

This grammar work is out of scope for this spec.

---

## Test helper: `assert_derived_parity`

The framework catches SQL/Rust drift in derived fields through a
**macro-emitted inherent method** on each generated visage struct.
The method is **synchronous and IO-free**: it takes two
pre-constructed visages of the same type and asserts that **only
their derived fields** are equal. The caller is responsible for the
INSERT, the DB fetch, and the in-memory `From<&Model>` construction.

### Macro-emitted signature

For every generated visage struct that has at least one derived
field in its scope, the macro emits:

```rust
impl ConsignmentPublic {
 /// Compare derived fields between two visage instances and
 /// return `Err(DerivedParityError::Drift {... })` on first
 /// mismatch. Framework columns (`id`, `created_at`,
 /// `updated_at`) and storage columns are NEVER compared — only
 /// fields populated from `#[derived(...)]` declarations whose
 /// `scopes = [...]` includes this visage's scope.
 pub fn assert_derived_parity(
 &self,
 other: &Self,
 ) -> Result<(), djogi::testing::DerivedParityError> {
 if self.facility_site != other.facility_site {
 return Err(djogi::testing::DerivedParityError::Drift {
 visage: "ConsignmentPublic",
 field: "facility_site",
 });
 }
 //... one such check per derived field exposed in this scope...
 Ok(())
 }
}
```

### Emission rule

The macro emits the inherent method on **every visage struct that
has at least one derived field in its scope**. Visages with zero
derived fields do **not** receive the method (it would be a no-op —
adopters who want full-struct equality can derive `PartialEq` on
their own visage and call `==` directly).

### Comparison surface

The method compares **only** the fields populated from
`#[derived(...)]` declarations whose `scopes = [...]` list includes
the visage's scope.

- **Framework columns** (`id`, `created_at`, `updated_at`) — never
 compared. These are populated identically on both sides (the
 in-memory side reads them from `&Model`; the from-DB side reads
 them from the row), but `tokio-postgres` round-trips truncate
 `DateTime` nanoseconds to microseconds. A naive whole-struct
 equality would false-positive `Drift` on any high-precision
 timestamp even when the derived fields are byte-identical. By
 comparing only the derived fields, the helper never observes the
 truncation.
- **Storage columns** exposed via `#[field(expose(...))]` — never
 compared. These pass through the same `&Model → from_pg_row`
 round-trip and could in principle suffer similar lossy
 conversions; more importantly, parity is a property of the *SQL/
 Rust* expression pair, not of column transport. Comparing column
 entries would test the framework's transport layer, not the
 adopter's derivation logic.
- **Derived entries scoped to this visage** — every such entry is
 compared with a per-field `!=` check, in attribute declaration
 order. The method short-circuits at the first mismatch.

### Equality bound on derived `ty`

The macro emits direct `!=` checks per derived field. This requires
each derived field's `ty` to implement `PartialEq`. If `ty` does
not implement `PartialEq`, the macro rejects the declaration at
parse time with [E_DJG_VDF_016](#error-taxonomy).

Note: the macro cannot inspect `ty`'s trait impls at parse time
(proc macros operate in token space, not type space). The
"rejection" is enforced indirectly — rustc's type check on the
emitted impl surfaces an `E0277` ("the trait bound `<Ty>: PartialEq`
is not satisfied") error. To anchor that diagnostic cleanly at the
`assert_derived_parity` site rather than at the inner `!=` token,
the macro **must** emit a `where <Ty>: PartialEq` bound (one bound
per distinct derived `ty`) on the inherent impl block. The `where`
bound is mandatory, not optional — see
[Stage 7](#stage-7--assert_derived_parity-emission) for the exact
emission shape.

### Why the macro emits per-visage instead of a generic helper

An earlier draft proposed a generic
`pub fn assert_derived_parity<V>(in_memory: &V, from_db: &V) ->
Result<(), DerivedParityError> where V: PartialEq` helper. Three
problems made that shape unworkable:

1. **Visages don't auto-derive `PartialEq`.** Generated visages
 carry `Debug, Clone, Serialize, Deserialize` only (see
 `djogi-macros/src/model/visages.rs::derive_path`). A
 `V: PartialEq` bound on the helper would force every adopter to
 manually add `#[derive(PartialEq)]` to their visage shapes,
 which the macro does not control.
2. **Round-trip lossy framework columns.** Even with `PartialEq`
 added, `tokio-postgres` truncates `DateTime` nanoseconds to
 microseconds on round-trip. `in_memory.created_at !=
 from_db.created_at` for high-precision timestamps, false-
 positiving `Drift` even when the derived fields are perfectly
 identical.
3. **Wrong contract surface.** A helper that claims to test
 *derived-field* parity but actually tests *whole-struct*
 equality is misnamed. The new emission limits the comparison to
 the derived surface, matching the name.

The macro knows exactly which derived fields are in scope for each
visage; emitting per-field comparisons hard-codes the right surface
at expansion time and side-steps all three problems.

### Recommended usage

```rust
#[djogi::djogi_test(sync_models = [Consignment])]
async fn consignment_facility_site_parity(mut ctx: DjogiContext) {
 let inbound = Consignment::create(
 &mut ctx,
 Consignment {
 inbound_site: facility(),
 outbound_site: warehouse(),
 direction: Direction::Inbound,
..Default::default()
 },
 )
.await
.unwrap();

 let in_memory: ConsignmentPublic = (&inbound).into();
 let from_db: ConsignmentPublic =
 ConsignmentPublic::filter(|f| f.id().eq(inbound.id))
.fetch_one(&mut ctx)
.await
.unwrap();

 in_memory.assert_derived_parity(&from_db).unwrap();
}
```

The helper is sync; the fetch is async. The helper does not
re-introduce IO.

### Two surfaces: inherent method + sealed trait

The `#[model]` macro emits two parallel parity-comparison surfaces
for every generated visage that has at least one derived entry in
its scope:

1. **Inherent `assert_derived_parity` method** — `visage.assert_derived_parity(&other)`
 resolves via Rust's inherent-method-first method resolution.
 This is the ergonomic shape integration tests use; no trait
 import required at the call site.
2. **`impl djogi::testing::DerivedParity for {Visage}`** — same
 body, reachable from generic code that bounds
 `where V: DerivedParity`. The trait is sealed
 (`djogi::testing::private::DerivedParitySealed` is the empty
 supertrait); only macro-emitted visages may satisfy it.

Both surfaces share the same body and the same `where <Ty>: PartialEq`
bound per distinct derived type. Method resolution prefers the
inherent method for unqualified calls; the trait method is reachable
through `<V as DerivedParity>::assert_derived_parity(...)` or
generic bounds.

### Async convenience: `assert_derived_parity_fetched`

`djogi::testing::assert_derived_parity_fetched<V, Fetch, Fut>`
wraps the **fetch + compare** workflow in one call:

```rust
use djogi::testing::{DerivedParity, assert_derived_parity_fetched};

#[djogi::djogi_test(sync_models = [Consignment])]
async fn parity_via_async_helper(mut ctx: DjogiContext) {
 let inbound = Consignment::create(&mut ctx, /*... */).await.unwrap();
 let in_memory: ConsignmentPublic = (&inbound).into();
 let target_id = inbound.id;

 assert_derived_parity_fetched(&in_memory, || async {
 ConsignmentPublic::filter(|f| f.id().eq(target_id))
.fetch_one(&mut ctx)
.await
 })
.await
.unwrap();
}
```

The helper takes the in-memory visage by reference and a fetch
closure that returns `Future<Output = djogi::Result<V>>`. It awaits
the fetch, lifts any `DjogiError` into
`DerivedParityError::Fetch { source }`, and delegates to the
`DerivedParity::assert_derived_parity` trait method on success. The
closure parameter (rather than a fixed PK + queryset shape) keeps
the helper independent of the per-visage filter entry points
(which are inherent methods on each visage, not trait methods on
`DjogiVisage`).

The async helper is **additive** — the sync per-visage inherent
method shape is unchanged. Tests that prefer the explicit
two-step shape (`let from_db = …fetch_one…; in_memory.assert_derived_parity(&from_db)`)
continue to work; the async helper is for adopters who prefer the
single-call form.

### Why opt-in

A blanket parity gate at test time would slow CI for adopters who
don't care, would force every derived field into a deterministic
shape (no `now()`, no `random()`), and would require generating
arbitrary model instances (not currently in the framework's surface).
The helper instead lets adopters opt in for the visages they
genuinely want to keep in sync.

### `DerivedParityError`

The error type is named `DerivedParityError` (not
`DbComputedParityError`) because parity is a property of *both*
sides — the SQL-evaluated value and the Rust-evaluated value
together — rather than a Postgres-side action. The `DbComputed*`
prefix used on the runtime variants
(`VisageError::DbComputedNullForNonOptional`,
`VisageError::DbComputedTypeMismatch`) marks errors describing
Postgres-side outputs specifically; the parity-helper error
straddles both sides, so it lands under `Derived*`.

```rust
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum DerivedParityError {
 #[error(
 "derived field parity drift in `{visage}` at field `{field}`"
 )]
 Drift {
 /// The visage type name (e.g. `"ConsignmentPublic"`).
 visage: &'static str,
 /// The derived field name that mismatched (e.g.
 /// `"facility_site"`).
 field: &'static str,
 },

 /// Produced only by the async `assert_derived_parity_fetched`
 /// convenience helper when the fetch closure's future yields
 /// `Err`. The sync per-visage inherent method (and the
 /// `DerivedParity` trait impl) never returns this variant — they
 /// re-introduce no IO.
 #[error("derived parity fetch failed: {source}")]
 Fetch {
 #[source]
 source: djogi::DjogiError,
 },
}
```

The diagnostic carries the visage and field names as `&'static
str` rather than `Debug`-rendered struct dumps. The macro-emitted
inherent method short-circuits at the first mismatch, so a single
`Drift` variant suffices for the sync surface — the adopter doesn't
need to see *every* mismatched field to act, and the first-mismatch
report keeps the error surface narrow. Adopters who want the full
diff can call the method, observe the field name in the error, and
`dbg!(&in_memory, &from_db)` to inspect.

The `Fetch` variant is produced exclusively by the additive async
`assert_derived_parity_fetched` helper described above. It wraps a
`DjogiError` (typically `DjogiError::Db` from the underlying
`fetch_one` call or `DjogiError::Visage` from a derived row-decode
failure) and surfaces it via `#[source]` so test runners that walk
`Error::source()` get the full cause chain.

---

## Interactions with existing surfaces

### `#[field(expose(...))]`

The existing field-level exposure attribute is unchanged. A column
exposed to scope `S` appears as a column entry in the projection of
every visage scoped to `S`. Derived fields stack on top of exposed
columns without modifying column-side semantics.

A column may not share a name with a derived entry in any scope
where both appear; this is the
[identifier collision rule](#identifier-rules).

### `#[computed(sql =...)]` (model-side virtual columns)

The pre-existing `#[computed]` attribute (model-side virtual / stored
generated columns) is **a different surface**:

- `#[computed]` is a *field-level* attribute on the model struct,
 representing a virtual or stored generated column that lives on
 the table.
- `#[derived]` (this spec) is a *struct-level* attribute, representing
 a projection entry that lives on one or more visages and has no
 storage footprint.

The two surfaces compose naturally:

- `#[computed]` is model-side only. It does not create visage struct
 fields and cannot project onto visages through
 `#[computed(... expose(...))]` or `#[computed(expose =...)]`; both
 forms are rejected by the macro.
- A model field cannot combine `#[computed(...)]` with
 `#[field(expose(...))]` to publish the computed value through a
 visage. Computed properties remain model-side virtual / stored
 columns, not visage projection entries.
- A `#[derived]` entry never appears on the model struct or in the
 table schema; it exists only on visages. `#[derived(...)]` is the
 shipped surface for adding computed projection fields to
 generated visages.

The current spec deliberately does not unify the two surfaces under
one attribute name — the storage / projection distinction is real and
naming them differently keeps the mental model clean.

### `#[field(generated = "<expr>")]`

Stored generated columns (introduced in an earlier phase) are
table-level features and do not interact with derived fields. A
stored generated column produces a real column the model carries;
derived entries produce projection-only fields on visages. Both may
coexist on the same model without conflict.

### Foreign-key and one-to-one relation traversal

A derived entry's `rust` expression may reference resolved relations
on `model` (see [Relation references](#relation-references)). The
SQL expression may reference the relation's columns via a subquery
or join; the framework does not parse the SQL to validate this and
relies on Postgres to surface any reference errors at query time.

Relation-form *visage embedding* (a peer visage projected inline)
is the deferred case covered in
[Relation-form visages](#relation-form-visages-deferred).

### M2M through-models

A derived field declared on the through-model of an M2M relation
behaves as on any model: it produces a projection entry on the
through-model's visages and is fetched via the through-model's
`VisageQuerySet`. M2M traversal helpers (`m.related.through()`) do
not project derived fields through to the owning side; the adopter
must filter on the through-model directly to access derived
projections from M2M context.

### JSONB fields

A derived `sql` expression may reference JSONB columns or paths.
The output `ty` must be a JSONB-compatible Rust type (e.g.,
`Jsonb<MySchema>`, `serde_json::Value`, or any type with a
`tokio_postgres::FromSql` impl for the JSONB OID).

### Raw SQL bypass

Derived fields do not require any of the `__bypass::*` raw SQL
escapes. The `sql` string travels through the visage projection
machinery, which is fully typed at the framework boundary; adopters
never need `raw_execute` to fetch a visage with derived fields.

---

## Error taxonomy

### Macro-time errors

All macro-time errors are emitted with span-precise diagnostics
attached to the offending token. The codes below are for
documentation reference; the user-facing diagnostic uses prose.

| Code | Condition | Span anchor |
|---|---|---|
| `E_DJG_VDF_001` | Missing required attribute key (`name`, `ty`, `scopes`, `sql`, or `rust`) | `#[derived(...)]` invocation |
| `E_DJG_VDF_002` | `name` collides with an exposed column on the same model in any of its `scopes` | `name = <ident>` |
| `E_DJG_VDF_003` | `name` collides with another derived entry's `name` in any overlapping scope | `name = <ident>` |
| `E_DJG_VDF_004` | `name` violates the general identifier-shape rules (length > 63 bytes, leading byte not `_` / ASCII lowercase letter, or contains a byte that is not `_` / ASCII lowercase letter / ASCII digit). The uppercase-byte case has its own code at E_DJG_VDF_012; the reserved-keyword case has its own code at E_DJG_VDF_014. | `name = <ident>` |
| `E_DJG_VDF_005` | `name` is prefixed by `__djogi_` (ASCII case-insensitive) | `name = <ident>` |
| `E_DJG_VDF_006` | `scopes` contains an unknown scope identifier (not one of `public`, `self_view`, `admin`, `export`) | offending identifier inside `scopes = [...]` |
| `E_DJG_VDF_007` | `sql` contains a `;` statement separator or leading DDL/DML keyword | `sql = "..."` |
| `E_DJG_VDF_008` | `sql` contains a reserved `$N` placeholder token | offending `$N` position inside `sql` literal |
| `E_DJG_VDF_009` | `sql` contains a token from the recognised aggregate-name set (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `ARRAY_AGG`, `STRING_AGG`, `JSONB_AGG`, `JSON_AGG`, `JSONB_OBJECT_AGG`, `JSON_OBJECT_AGG`, `RANGE_AGG`, `MULTIRANGE_AGG`, `XMLAGG`, `BOOL_AND`, `BOOL_OR`, `EVERY`, `BIT_AND`, `BIT_OR`) or the `OVER` keyword, detected by a **context-blind case-insensitive token scan**. The scan skips tokens inside single-quoted string literals and dollar-quoted bodies (`$$...$$` / `$tag$...$tag$`), but does **not** skip tokens inside scalar subqueries — the scan cannot distinguish subquery-scoped aggregates (same-row container reconstruction) from top-level row aggregates without a SQL parser (forbidden by the no-regex, no-in-tree-parser rule). The guard therefore fires unconditionally whenever a recognised aggregate token appears anywhere in the `sql` value outside of single-quoted or dollar-quoted bodies, including inside a `(SELECT jsonb_agg(...) FROM...)` scalar subquery. Until the Shape V `aggregate = true` marker ships (see `docs/spec/decisions.md` §Aggregate annotation declaration site), this check is unconditional. Detection is best-effort: aggregates outside this set or custom user-defined aggregates pass macro parsing and surface as Postgres errors at query time. | offending function call position |
| `E_DJG_VDF_010` | A scope listed in `scopes` is also declared as a relation-form embedding scope elsewhere on the same model | `scopes = [...]` |
| `E_DJG_VDF_011` | (Reserved for Tier 2 — emitted by the peer-projection cluster when it lands) | n/a |
| `E_DJG_VDF_012` | `name` contains an uppercase ASCII byte (Postgres unquoted-identifier case folding would silently rename the alias) | `name = <ident>` |
| `E_DJG_VDF_013` | `scopes` list contains a duplicate scope identifier (e.g., `scopes = [public, public]`) | second occurrence of the duplicate identifier inside `scopes = [...]` |
| `E_DJG_VDF_014` | `name` is a Postgres reserved keyword (rejected at parse time using the sorted const slice in `djogi-macros/src/ident.rs::RESERVED_KEYWORDS`). Distinct from E_DJG_VDF_004 (general shape) so the diagnostic can point adopters at the keyword conflict rather than at a generic shape rule. | `name = <ident>` |
| `E_DJG_VDF_015` | `#[derived(...)]` declared on a model with `#[model(pk = None)]`. Derived visages require a primary key for row hydration and for the documented parity-test workflow; the framework rejects the combination at parse time rather than silently emitting a broken visage surface. | `#[derived(...)]` attribute span |
| `E_DJG_VDF_016` | A derived entry's `ty` does not implement `PartialEq`, surfaced via the macro-emitted `assert_derived_parity` inherent method's `!=` comparison. Macros operate in token space and cannot inspect trait impls; the rejection comes from rustc's E0277 check on the emitted impl block. The macro **must** emit a `where <Ty>: PartialEq` bound (one per distinct derived `ty`) on the inherent impl, so the diagnostic anchors at the `assert_derived_parity` impl site rather than at the inner `!=` token. | `ty = <type>` |
| `E_DJG_VDF_017` | JSONB simple-column passthrough from a same-host `Jsonb` storage column: the trimmed `sql` literal is either byte-identical to the `ident` of a same-host model storage column (e.g. `metadata`) or a simple quoted spelling of that ident (e.g. `"metadata"`), and the matched column's declared Rust type token-string contains `Jsonb<`. The derived `name` and derived `ty` alias spelling are **not** escape hatches — the guard fires for same-name, cross-name, quoted-identifier, and unresolved type-alias passthrough shapes. Rejected at parse time because a projected `Jsonb<NarrowSchema>` would deserialize admin-only keys into `extra` and `Jsonb<T>::Serialize` would merge them back on the wire regardless of the visage field alias. See [`docs/spec/jsonb-per-audience-schema.md §Error taxonomy extension`](./jsonb-per-audience-schema.md#error-taxonomy-extension) for the full condition pair and fixture corpus. | `sql = "..."` literal |

### Runtime errors

`djogi::VisageError` gains two new variants (the enum is already
`#[non_exhaustive]`, so this is a non-breaking extension):

```rust
#[non_exhaustive]
pub enum VisageError {
 UnresolvedRelation { /* existing */ },

 /// A derived field declared as NOT NULL (`ty = T`) decoded NULL
 /// from the database row.
 DbComputedNullForNonOptional {
 visage: &'static str,
 field: &'static str,
 },

 /// A derived field's runtime type did not match `ty`.
 /// Surfaces from `FromPgRow` after Postgres returns a value the
 /// declared Rust type cannot accept.
 DbComputedTypeMismatch {
 visage: &'static str,
 field: &'static str,
 expected: &'static str,
 actual: &'static str,
 },
}
```

The pre-existing `From<Infallible> for VisageError` glue continues to
let mixed-fallibility codegen propagate errors uniformly.

#### Wrapping at the row-decode boundary

`FromPgRow::from_pg_row(...)` returns `Result<Self, DjogiError>` (per
`djogi/src/pg/decode.rs`), and `VisageQuerySet::fetch_all` /
`fetch_one` / `first` return `Result<V, DjogiError>` /
`Result<Vec<V>, DjogiError>`. The two new variants therefore surface
to callers **wrapped in `DjogiError::Visage(VisageError)`**, not as
bare `VisageError`:

- The generated `FromPgRow` impl for a visage with derived entries
 constructs a `VisageError::DbComputedNullForNonOptional {... }` or
 `VisageError::DbComputedTypeMismatch {... }` at the offending
 ordinal position, then wraps it via the existing
 `impl From<VisageError> for DjogiError` (at
 `djogi/src/error.rs:351`) before returning.
- Callers fetching through `VisageQuerySet` see a `DjogiError::Visage(
 VisageError::DbComputedNullForNonOptional {.. })` — the outer
 `DjogiError` is what propagates through `?` from a `fetch_all`
 call; the inner `VisageError` carries the variant detail.
- Callers using `<V as TryFrom<&Model>>::try_from(...)` directly (the
 in-memory path) see `VisageError` un-wrapped, because the
 `TryFrom::Error` is exactly `VisageError`. The row-decode boundary
 is the only place the wrapping happens.

The wrapping is intentional symmetry with the existing
`VisageError::UnresolvedRelation` variant, which the visage emitter
already surfaces through the same `DjogiError::Visage` path for
relation-nesting visages ( T9, per
`djogi/src/error.rs:339`).

---

## Non-goals

These are deliberate omissions for this phase. Each has a tracking
issue or named future phase.

1. **SQL-to-Rust automatic translation.** Adopters write both `sql`
 and `rust`. The framework does not infer one from the other.
 Tracking: future spec round if adopter friction proves serious.
2. **Aggregates and window functions.** Out of scope for `#[derived]`
 Tier 1. The declaration site for any future aggregate / window-function
 surface is **locked** by [the aggregate-annotation declaration-site
 decision in `docs/spec/decisions.md`](./decisions.md#aggregate-annotation-declaration-site): per-query
 group-by aggregates land as a typed `.annotate(...)` on
 `QuerySet<T>` / `VisageQuerySet<V>` (**Shape Q**), and per-row
 window expressions / correlated-subquery scalars land on the
 existing `#[derived]` helper attribute with an explicit
 `aggregate = true` opt-in marker (**Shape V**). No future surface
 places aggregate declarations on **model fields**; the locked rule
 exists pre-emptively because the same Shape A bundling that the
 Path B reshape eliminated for `#[computed(sql, expose)]` would
 silently re-emerge if a model-field-level `#[annotation(sql, expose)]`
 attribute landed. The `aggregate = true` marker is also the
 deliberate opt-in that relaxes Tier 1's
 [E_DJG_VDF_009](#error-taxonomy) aggregate rejection — without it,
 aggregates inside `#[derived]` `sql` continue to fail at parse
 time. Tracking: future phase (no earlier scheduling commitment,
 ahead of which a `docs/spec/aggregate-annotations.md` spec ships).
3. **Filter / order-by on derived fields from `VisageQuerySet`.**
 Tier 2 / Tier 3 work; see [Capability tiers](#capability-tiers).
4. **Cross-model `$N` references.** Reserved syntax only;
 [Reserved syntax: `$N`](#reserved-syntax-n) holds the grammar.
5. **Multi-visage shared derived declarations.** Each `#[derived]`
 attribute declares one entry; if the same logical derivation
 belongs in two unrelated scopes with different visibility shapes,
 declare twice. A shared-registry surface is tracked separately.
6. **Compile-time SQL column-reference validation.** Identifier
 typos in `sql` surface at query time, not macro time. See
 [SQL grammar and validation](#sql-grammar-and-validation).
7. **Automatic parity gating in tests.** The parity helper is
 opt-in; no `#[djogi_test]` integration auto-runs it.
8. **Derived-field migrations.** `#[derived]` entries are
 projection-only and never appear in `target/djogi_models.json` /
 the `ModelDescriptor` inventory channel that feeds `build.rs`.
 The richer `VisageDescriptor` / `DerivedProjection` inventory
 surface that documentation generators consume ships in 
 (see [Stage 2](#stage-2--visage-side-descriptor-inventory)) but
 registers against its own `inventory::collect!(VisageDescriptor)`
 collection — structurally separate from the `ModelDescriptor` /
 `EnumDescriptor` collections the migration differ walks. The
 storage-vs-projection split is preserved: migration / snapshot /
 `build.rs` paths never observe `VisageDescriptor` entries.

---

## Implementation plan

The implementation lands as a single phase with the stages below.
Each stage is gated on its predecessor; staging is internal to the
phase and not visible to adopters.

### Stage 1 — attribute parser and helper-attribute registration

- **Register `derived` as a `Model`-derive helper attribute.** The
 shipped proc-macro declaration is
 `#[proc_macro_derive(Model, attributes(field, derived))]`. This
 keeps `#[derived(...)]` legal during rustc syntax checking before
 the macro expands. See
 [§Declaration](#derived-is-a-helper-attribute-not-an-attribute-macro).
- **Strip `#[derived(...)]` from the re-emitted struct attributes.**
 The macro expansion filters every outer attribute whose `path()` is
 `derived` out of `item_struct.attrs` before re-emitting the struct.
 This mirrors the existing per-field `#[field(...)]` helper
 stripping and prevents helper attributes from surviving into the
 user crate's compiled output.
- Add `#[derived(...)]` parser to `djogi-macros/src/model/attrs.rs`.
 The parser is invoked from the `#[model(...)]` attribute-macro
 expansion path (walking `item_struct.attrs` for outer attributes
 whose `path()` is `derived`); it is **not** wired into the
 `#[derive(Model)]` proc-macro derive as a separate entry point,
 because `Model`'s derive is a no-op stub at
 `djogi-macros/src/lib.rs:111`. The two paths share one parser
 module; the `#[model]` attribute-macro is the routine call site.
- Parse the six keys (`name`, `ty`, `scopes`, `sql`, `rust`, `doc`)
 into a `DerivedAttr` struct. Five are required; `doc` is optional.
- Validation pass: identifier rules
 ([E_DJG_VDF_002–005](#error-taxonomy), [E_DJG_VDF_012](#error-taxonomy),
 [E_DJG_VDF_014](#error-taxonomy)),
 scope rules ([E_DJG_VDF_006](#error-taxonomy),
 [E_DJG_VDF_013](#error-taxonomy)).
- SQL token-level scan (no full parse): statement-separator check,
 leading-keyword check, `$N` rejection, aggregate detection. Tokeniser
 shape: byte-walk handling single-quoted strings, dollar-quoted
 strings, and bare identifiers. See the no-regex discipline in
 `feedback_no_regex_in_djogi.md`.

### Stage 2 — visage-side descriptor inventory

A separate visage-side descriptor inventory ships in 
alongside the runtime trait surface. Derived metadata MUST NOT appear
on `ModelDescriptor` or in the `target/djogi_models.json` channel
that feeds `build.rs` migrations (see [Non-goals item 8](#non-goals));
the storage / projection split is preserved by registering visage
metadata against a SEPARATE inventory collection that the migration
differ does not iterate.

- **Shipped: `VisageDescriptor`.** Lives at
 `djogi::descriptor::VisageDescriptor` — one descriptor per
 `(Model, scope)` pair the macro emits that has at least one
 derived entry in scope. Fields:
 - `model_name: &'static str` — source model type name.
 - `scope: &'static str` — visage scope key
 (`"public"` / `"self_view"` / `"admin"` / `"export"`).
 - `visage_name: &'static str` — visage struct type name
 (`"ConsignmentPublic"`).
 - `derived: &'static [DerivedProjection]` — derived entries in
 struct-field order.

 The `derived` field is a `&'static` slice — not a `Vec` — because
 `inventory::submit!` requires fully-static data, matching the
 existing descriptors in `djogi/src/descriptor.rs`. Registers
 against its own `inventory::collect!(VisageDescriptor)` collection,
 structurally separate from `ModelDescriptor` /
 `EnumDescriptor` / `AppDescriptor`; migration / snapshot /
 `build.rs` paths walk only their respective collections and never
 observe `VisageDescriptor` entries.
- **Shipped: `DerivedProjection`.** Per-entry metadata for downstream
 consumers (documentation generation, framework-side lints, debug
 formatting, future Tier-2 predicate rendering):

 ```rust
 pub struct DerivedProjection {
 /// Output field name (the entry's `name =...`).
 pub name: &'static str,
 /// Fully-qualified Rust type path for the output field, captured
 /// as a token-string (the entry's `ty =...`). Kept as a
 /// `&'static str` rather than a structured representation because
 /// downstream consumers (rustdoc reference tables, the `djogi
 /// docs` CLI) want the source spelling, not a re-parsed form.
 pub ty_path: &'static str,
 /// The adopter's Postgres SQL expression (the entry's `sql =
 /// "..."`). Verbatim — the same string spliced into
 /// `PROJECTION_LIST` (with outer parentheses added at SELECT
 /// emission time, not here).
 pub sql: &'static str,
 /// The adopter's Rust expression (the entry's `rust = "..."`),
 /// verbatim. Surfaced for documentation; not re-parsed.
 pub rust: &'static str,
 /// The optional `doc = "..."` rustdoc attached to the generated
 /// field. `None` when the entry did not declare `doc`.
 pub doc: Option<&'static str>,
 /// Scopes the entry was declared against, in source order. The
 /// per-`(Model, scope)` `VisageDescriptor` already keys on
 /// scope, but carrying the original set here lets consumers
 /// that walk across visages reconcile multi-scope declarations
 /// without re-walking the model.
 pub scopes: &'static [&'static str],
 }
 ```

 **Const-construction contract.** Every field of `DerivedProjection`
 is a type with a `const` constructor usable in static contexts —
 `&'static str`, `Option<&'static str>` (`Some("...")` is `const`
 on every supported toolchain), and `&'static [&'static str]`. The
 macro emits the entire `&'static [DerivedProjection]` slice as a
 static-context expression at the `inventory::submit!` site without
 runtime allocation:

 ```rust
 inventory::submit! {
 djogi::descriptor::VisageDescriptor {
 model_name: "Consignment",
 scope: "public",
 visage_name: "ConsignmentPublic",
 derived: &[
 djogi::descriptor::DerivedProjection {
  name: "facility_site",
  ty_path: "Site",
  sql: "CASE WHEN direction = 'inbound' \
  THEN inbound_site ELSE outbound_site END",
  rust: "match model.direction { /*... */ }",
  doc: None,
  scopes: &["public", "admin", "export"],
 },
 ],
 }
 }
 ```

 Owned types (`String`, `Vec<T>`) are forbidden anywhere in
 `DerivedProjection`'s field list because they would force a runtime
 allocator and break `inventory::submit!`'s static-data
 requirement. The same constraint binds `FieldDescriptor` (every
 field is `&'static str` / primitive / `Option<&'static str>` /
 `Option<RelationKind>` etc., per `djogi/src/descriptor.rs:1491`);
 `DerivedProjection` inherits the convention rather than
 introducing a new pattern.
- **Per-scope descriptor emission.** The macro emits one
 `inventory::submit!` block per `(Source, Scope)` pair for which
 `scope_derived()` (the iterator over derived attributes whose
 `scopes = [...]` includes `self.scope`) returns at least one
 entry. Visages with no derived entries in scope do not get a
 `VisageDescriptor` — there is nothing for the descriptor to
 describe. `pk = None` source models are skipped (they have no
 `Model::table_name()`, hence no SELECT projection).
- **`ModelDescriptor` stays pure storage.** No `derived` field is
 added to `ModelDescriptor`. Migration / snapshot / `build.rs` code
 paths see only storage-side metadata; derived entries are
 structurally invisible to them.

The reason for a separate `VisageDescriptor` (rather than adding
`#[serde(skip)] derived: Vec<...>` to `ModelDescriptor`): the
descriptor split mirrors the storage-vs-projection separation the
whole reshape establishes. A `#[serde(skip)]` field would compile but
would leave a trap for any future descriptor consumer that walks the
struct without `#[serde]` (e.g., a `Debug` printer or a hand-rolled
walker). The separate inventory channel keeps the boundary
mechanical.

### Stage 3 — codegen: visage struct + trait

- Emit derived fields as struct fields on each scoped visage.
- Generate the new trait constants: `COLUMNS`, `PROJECTIONS`,
 `PROJECTION_LIST`.
- Emit the sealed `ProjectionEntry` re-export and the
 `__private::ProjectionEntry` path in macro output.

### Stage 4 — `VisageQuerySet` projection emission

- Replace the `columns: &'static [&'static str]` field on
 `VisageQuerySet<V>` with `projection_list: &'static str`. The
 queryset constructor (`new_for_visage`) takes the table name and
 the projection list directly; the macro emits the visage's
 `PROJECTION_LIST` constant into the constructor call.
- The `COLUMNS` slice is no longer carried on `VisageQuerySet` —
 callers that need the column-only view reach it through the visage
 trait constant (`<V as DjogiVisage>::COLUMNS`) on demand.
- Update `build_visage_select` / `build_visage_count` /
 `build_visage_exists` to splice `qs.projection_list` into the
 SELECT projection slot. (Count and exists builders still emit
 `COUNT(*)` and `EXISTS (SELECT 1...)`, so they ignore the
 projection list.)
- Preserve byte-level test surface (`__sql_for_test`) so existing
 pin tests on column ordering continue to assert; the new emission
 produces strictly more text (added derived expressions with
 aliases) but the existing column ordering is unchanged.

### Stage 5 — codegen: `From<&Model>` / `TryFrom<&Model>`

- Walk derived entries' `rust` expressions; detect fallibility via
 the [syntactic-tail rule](#fallibility-detection-syntactic-tail-not-type).
 Record the matched shape per entry (Shape 1 vs Shapes 2–5 vs
 infallible) — the emission shape depends on it.
- Splice each derived expression inside a `{ let model: &{Model} =
 src; <rust> }` block so the adopter's `model.<field>` syntax binds
 to the existing emitter's `src` parameter. The outer `?` is
 shape-dependent:
 - Shape 1 (trailing `?` in adopter expression): no outer `?` —
 inner `?` propagates from the splice block to the surrounding
 `try_from` body.
 - Shapes 2–5 (block evaluates to `Result<T, E>`): outer `?` —
 `?` desugars to `Err(From::from(e))`, requiring `VisageError:
 From<E>` (held by all error types adopters return today).
 - Infallible entry inside a fallible visage: no outer `?` — block
 returns `T`, not `Result<T, _>`; the visage's `TryFrom` body
 accepts the value directly.
- Emit `From<&Model>` if all-infallible.
- Emit `TryFrom<&Model>` if any-fallible; mixed entries lift via the
 existing `Infallible → VisageError` blanket.
- Do **not** retouch the existing model-side parameter name (`src`)
 in `djogi-macros/src/model/visages.rs`; the per-entry rebind is
 the entire surface change.

### Stage 6 — error taxonomy extensions

- Extend `VisageError` with `DbComputedNullForNonOptional` and
 `DbComputedTypeMismatch` variants. Surface them from `FromPgRow`
 wrapped in `DjogiError::Visage(...)` at the row-decode boundary —
 see [Wrapping at the row-decode boundary](#wrapping-at-the-row-decode-boundary).
- Update `FromPgRow` for each derived visage to surface these on
 decode failure.

### Stage 7 — `assert_derived_parity` emission

- Emit an **inherent method** `pub fn assert_derived_parity(&self,
 other: &Self) -> Result<(), djogi::testing::DerivedParityError>`
 on every generated visage struct that has at least one derived
 field in its scope. Visages with zero derived fields do not
 receive the method.
- The method body emits one `if self.<field> != other.<field> {
 return Err(...); }` block per derived field in declaration order,
 followed by a final `Ok(())`. Framework columns (`id`,
 `created_at`, `updated_at`) and exposed storage columns are not
 compared — only derived fields are walked. See
 [Comparison surface](#comparison-surface) for the rationale.
- Emit a mandatory `where <Ty>: PartialEq` bound (one bound per
 distinct derived `ty`) on the inherent impl block, so a `ty` that
 does not implement `PartialEq` surfaces as a cleaner E0277
 diagnostic anchored at the `assert_derived_parity` impl site
 rather than at the inner `!=` token. Macro-time tracking code:
 [E_DJG_VDF_016](#error-taxonomy).
- Add `DerivedParityError` enum to `djogi::testing` with a single
 `Drift { visage: &'static str, field: &'static str }` variant. The
 enum is shared across all visages; the macro-emitted methods all
 return the same error type.
- Doctest the emission against the consignment scenario.
- The helper is **synchronous and IO-free**. No transaction
 wrapping is needed because no IO happens. The earlier draft
 wrapped a fetch in `djogi::transaction::atomic(...)` based on
 incorrect Postgres semantics claims (caught in Round 2 dual
 review); a Round 3 follow-on removed the fetch and the wrapping
 together. The Round 3 generic-helper shape was further removed
 in favour of per-visage macro emission to side-step missing
 `PartialEq` derives on visages and to keep framework-column
 round-trip lossiness from false-positiving `Drift` — see
 [Why the macro emits per-visage instead of a generic helper](#why-the-macro-emits-per-visage-instead-of-a-generic-helper).

### Stage 8 — documentation (HARD CLOSING CONDITION)

Per `feedback_issue_docs_required_for_public_api.md`, this feature
does not land without the full documentation chain. Every item below
is a hard closing condition; the issue does not close while any item
remains unchecked.

- **User-guide page** at `docs/guide/visages.md` (extend) or
 `docs/guide/derived-projections.md` (new) — adopter-facing prose
 covering: the `#[derived(...)]` attribute, the consignment scenario
 end-to-end, the SQL/Rust parity rule, the fallibility detection,
 the parity helper, the capability tiers (Tier 1 ships here, Tier 2/3
 deferred — name the deferral anchors), and the relation-form
 interaction (deferred and why). The user-guide page is the primary
 artifact adopters reach for — rustdoc supplements it but does not
 replace it.
- **Rustdoc** on every new public surface:
 - `#[derived]` helper attribute (documented on the `#[derive(Model)]`
 derive macro's rustdoc, since `#[derived]` is a helper attribute
 consumed by `Model` — not an independent attribute-macro entry
 point; see [§Declaration](#derived-is-a-helper-attribute-not-an-attribute-macro)).
 - `DjogiVisage` trait + its `type Model` associated type + its
 three associated constants (`COLUMNS`, `PROJECTIONS`,
 `PROJECTION_LIST`) + the `SCOPE` associated constant. There is
 no `TABLE` const: source-table access goes through
 `<V::Model as Model>::table_name()` — the `type Model: Model`
 supertrait bound makes that callable from generic code, and a
 parallel `TABLE` const would duplicate state the supertrait
 already pins. `DjogiVisage` carries two supertraits:
 `DjogiVisageOf<Self::Model>` for the source-model pairing and a
 separate metadata-only `private::Sealed` (re-exported as
 `__private::DjogiVisageSealed`) as the closed-world gate. The
 macro emits both the pairing impl and the metadata seal impl; the
 two supertraits serve distinct roles and neither alone covers
 both.
 - The `__private::ProjectionEntry` sealed type (with the standard
 "do not name this" warning matching the existing
 `__private::VisageSealed` precedent).
 - The two new `VisageError` variants
 (`DbComputedNullForNonOptional`, `DbComputedTypeMismatch`).
 - The macro-emitted `assert_derived_parity` inherent method on
 each derived visage (documented as part of the
 `#[derive(Model)]` rustdoc surface, since the method is emitted
 by the macro — not as a standalone item in
 `djogi::testing`) + the `djogi::testing::DerivedParityError`
 enum it returns.
- **Doctest** parity: every documented example compiles and runs.
 Specifically the consignment scenario is doctested end to end (one
 pass through `From<&Model>`, one pass through `VisageQuerySet`, one
 pass through the parity helper).
- **Spec index**: update `docs/spec/index.md` to link this spec.
- **Cross-references**: any existing user-guide page that mentions
 visages (`docs/guide/models.md`, `docs/guide/visages.md` if it
 exists) gets a one-line "see also: derived projections" pointer
 with a link to the new user-guide page.

The user-guide update is non-negotiable: shipping the feature with
only rustdoc would force adopters to assemble the workflow from
fragmented surface docs, which is exactly the kind of documentation
debt the framework's docs convention exists to prevent.

### Stage 9 — lihaaf fixtures

- `compile_pass`: consignment-shaped fixture, single-derived-field
 visage, multi-scope shared derived
 (`phase85_derived_fields.rs`); fallibility-shape variant
 exercising the syntactic-tail `?` lift
 (`phase85_derived_fields_fallible.rs`); restored
 `DjogiVisage::Model` associated-type contract
 (`phase85_derived_visage_model_assoc.rs`) — pins the spec's
 `<V::Model as Model>::table_name()` ergonomics across every
 emitted visage scope.
- `compile_fail`: one fixture per `E_DJG_VDF_*` macro-time error,
 with span-anchored expected diagnostic. The shipped inventory:
 - `phase85_derived_001_missing_required_key.rs` — E_DJG_VDF_001
 (missing required `name` key, anchored at attribute span).
 - `phase85_derived_002_column_collision.rs` — E_DJG_VDF_002
 (collision against an exposed model column in any overlapping
 scope; anchored at `name =...` token).
 - `phase85_derived_003_derived_collision.rs` — E_DJG_VDF_003
 (collision between two derived `name`s in an overlapping scope;
 anchored at the second declaration).
 - `phase85_derived_004_name_shape_too_long.rs` — E_DJG_VDF_004
 (length cap; complement of the parser-side unit tests covering
 leading byte / body byte rules).
 - `phase85_derived_005_djogi_prefix.rs` — E_DJG_VDF_005
 (framework-reserved `__djogi_` prefix).
 - `phase85_derived_006_unknown_scope.rs` — E_DJG_VDF_006
 (unknown scope identifier in `scopes = [...]`).
 - `phase85_derived_007_sql_statement_separator.rs` —
 E_DJG_VDF_007 (statement-separator arm; the leading
 DDL/DML-keyword arm shares the same diagnostic surface and is
 covered by parser-side unit tests).
 - `phase85_derived_008_sql_dollar_placeholder.rs` —
 E_DJG_VDF_008 (`$N` placeholder reservation).
 - `phase85_derived_009_sql_aggregate_keyword.rs` —
 E_DJG_VDF_009 (aggregate / window guard).
 - `phase85_derived_relation_form_overlap.rs` — E_DJG_VDF_010
 (relation-form / derived overlap in the same scope).
 - `phase85_derived_012_name_uppercase_byte.rs` —
 E_DJG_VDF_012 (uppercase byte in `name`).
 - `phase85_derived_013_duplicate_scope.rs` — E_DJG_VDF_013
 (per-list duplicate scope identifier).
 - `phase85_derived_014_reserved_keyword_name.rs` —
 E_DJG_VDF_014 (Postgres reserved keyword as `name`).
 - `phase85_derived_015_pk_none_host.rs` — E_DJG_VDF_015
 (`#[derived(...)]` on a `pk = None` host model).
 - `phase85_derived_partial_eq_required.rs` — E_DJG_VDF_016
 (derived `ty` lacks `PartialEq`; diagnostic anchored at the
 macro-emitted impl block, not the inner `!=` token).
- `compile_fail`: **Tier-1 accessor exclusion pin.** Visage with a
 derived field; test calls `V::filter(|f| f.<derived>()...)`.
 Expected diagnostic: rustc's "no method named `<derived>`" error
 surfacing from `{Visage}Fields`. Pin the message stem; the rest
 may drift across rustc versions
 (`phase85_derived_tier1_accessor_excluded.rs`).
- E_DJG_VDF_011 is reserved for Tier 2 (peer-projection cluster);
 no Tier-1 fixture exists because the diagnostic is not yet
 emitted by the macro. The fixture lands alongside the
 predicate-pushdown work that owns the code's emission site.

### Stage 10 — integration tests

- Per [feedback_no_raw_execute_in_tests.md], every integration test
 uses `#[djogi_test(sync_models = [...])]` and the typed surface;
 no `raw_*` escapes. One pin test per new raw API only — there are
 no new raw APIs in this work.
- Parity helper exercised in one integration test.

---

## Open questions for Round 1 dual review

These are the substantive design choices the spec commits to. Each is
called out so reviewers can challenge the framing rather than only
catching downstream errors.

1. **Attribute name (`#[derived]`).** Alternatives considered:
 `#[projection_entry]`, `#[visage_field]`, `#[computed_field]`.
 `#[derived]` is short and aligns with prior internal terminology
 ("visage-derived field"). Reviewers: does this collide
 conceptually with anything in adopter codebases or the wider Rust
 ecosystem? `serde(rename = "...")` and the like are field-level,
 not struct-level, so direct collision is unlikely.
2. **Fallibility detection via syntactic tail.** The macro
 recognises a closed set of syntactic shapes as fallible — see
 [Fallibility detection](#fallibility-detection-syntactic-tail-not-type).
 The spec rejects the alternative of an explicit `fallible = true`
 key because the key would force the adopter to keep two
 declarations in sync (the expression's behavior and the key); a
 single syntactic-tail rule makes the expression itself the
 source of truth. Reviewers: are the five recognised tail shapes
 the right closed set, or is there a real-world expression that
 falls in the gap?
3. **Mixed fallibility lifts the whole visage to `TryFrom`.**
 Alternative: emit one helper per entry (`fn project_<name>(model)`),
 let the caller chain. Spec rejects this because it doubles the
 public surface per visage. Reviewers: is the lift surprising
 when only one entry is fallible?
4. **Sealed `ProjectionEntry` vs two parallel slices.** Spec uses
 a sealed enum so the framework's emitter walks one ordered list.
 Alternative: `COLUMNS` + `DERIVED` as two parallel slices and let
 the emitter zip-merge them. Reviewers: is the seal worth the
 slight surface-area cost?
5. **`$N` reservation is parse-time, not codegen-time.** Spec
 rejects `$N` tokens during attribute parsing rather than letting
 them through to SQL emission. Reviewers: is this the right
 pre-emption, or should we allow `$N` through and surface a
 runtime error from Postgres?
6. **Aggregate detection is keyword-based, not semantic.** The
 tokeniser flags `COUNT(`, `SUM(`, etc. Adopters with a function
 named `count` in their schema (case-sensitive) would trip the
 detection. Spec says: this is acceptable because Postgres
 convention is uppercase aggregate names and lowercase identifiers.
 Reviewers: any real-world cases where this breaks?
7. **No column-reference validation inside `sql`.** Spec
 deliberately does not parse `sql` for identifiers. Reviewers: is
 the runtime-error trade-off acceptable, or should the macro
 attempt best-effort identifier extraction (find tokens, match
 against `{Model}Fields`, warn on unknown)?
8. **Relation-form rejection is per-scope, not per-derived.** Spec
 rejects a `scopes` list that overlaps any relation-form scope.
 Alternative: allow the overlap but reject only the specific
 derived entry whose SQL doesn't fit the relation projector. Spec
 rejects the alternative because it requires the relation projector
 to exist; the simpler per-scope rejection lets Tier 1 ship before
 the cluster.
9. **`PROJECTION_LIST` vs runtime walk.** Spec emits
 `PROJECTION_LIST` as a static `&'static str` at macro time.
 Alternative: build the string at query time by walking
 `PROJECTIONS`. Spec prefers static for SQL caching and emitted-SQL
 pin tests. Reviewers: does any future feature (per-call SQL
 variation) make this a bad bet?

(A tenth open question on parity-helper Postgres dependency
appeared in earlier rounds; it was removed when the helper was
reshaped to be sync and IO-free. The helper itself no longer touches
the database — it compares two pre-constructed visages. The
*workflow* that surrounds the helper still relies on Postgres for
the from-DB visage, but that is the adopter's fetch site, not the
helper's surface. See [Test helper:
`assert_derived_parity`](#test-helper-assert_derived_parity).)

---

## Out of scope (named future work)

The following ship as anchored deferrals to named future phases.

- **Tier 2: predicate use of derived fields.** Filtering on derived
 expressions through `VisageQuerySet`. Anchored to the visage
 queryset predicate-pushdown cluster.
- **Tier 3: ordering and annotation.** `ORDER BY <derived>` and
 `.annotate(...)`. Anchored to Tier 2's completion.
- **Aggregate and window-function projections.** Declaration site
 locked pre-implementation per [the aggregate-annotation
 declaration-site decision in `docs/spec/decisions.md`](./decisions.md#aggregate-annotation-declaration-site):
 per-query aggregates land on `QuerySet` / `VisageQuerySet` via
 `.annotate(...)` (Shape Q); per-row aggregates land on the
 `#[derived]` helper attribute with an explicit `aggregate = true`
 marker (Shape V); never on a model field. Implementation spec
 (`docs/spec/aggregate-annotations.md`) ships when this work is
 scheduled and amends the locked rules rather than supplanting them.
- **Cross-model `$N` references.** Locked grammar; design pending.
- **Relation-form derived intersection.** Anchored to the
 peer-projection cluster's contract; this spec specifies the contract.
- **Shared derived registry.** Multi-model derivations. Out of scope
 until adopter demand surfaces.

Each deferral is a *named* dependency on a future phase, not a vague
"someday." When the dependency lands, the relevant section of this
spec amends in place rather than being rewritten.

---

## Appendix: comparison with prior draft

This spec replaces an earlier ten-round draft that declared derived
fields as virtual model fields under an extended `#[computed]`
attribute. That shape conflated three concepts:

- Model-side virtual / stored generated columns (existing
 `#[computed]` semantics).
- Visage-side projection entries (the actual new feature).
- A `VisageProjection` enum at the trait surface that discriminated
 column from computed entries.

The conflation forced compile cliffs in the consignment scenario
(see the audit at `docs/spec/visage-derived-fields-ux-audit.md` for
the four-cliff walkthrough). The reshape to a struct-level
`#[derived]` attribute and a sealed `ProjectionEntry` collapses the
naming and locates derivations at the projection definition site.

The prior draft's contributions that survive into this spec:

- The error-code taxonomy (`E_DJG_VDF_*`).
- The reserved `$N` grammar.
- The capability-tier framing.
- The parity-helper concept.
- The identifier-validation rules (reserved keywords,
 `__djogi_` prefix).

The prior draft's contributions that do not survive:

- `#[computed(sql, expose)]` declaration site → replaced by
 `#[derived(scopes, sql, rust)]`.
- `source_ordinal: u16` on descriptors → no longer needed
 (struct-field order is the projection-entry collection order).
- `pub enum VisageProjection` in the public surface → sealed under
 `__private` as `ProjectionEntry`.
- "Column-only `COLUMNS` slice with `PROJECTION_LIST` as an additive
 sibling" framing → corrected: `COLUMNS` carries every ordinal
 position's name (column name or derived alias), so the visage's
 `FromPgRow::COLUMN_LIST` equals `PROJECTION_LIST`. The model-side
 `FromPgRow::COLUMN_LIST == COLUMNS.join(", ")` invariant survives
 unchanged for the model-side path; the visage-side path simply
 has a different relationship between the two constants when
 derived entries are present.

This spec is the artifact the next dual-review round dispatches
against. Prior rounds 1–10 are historical; the next round is Round 1
of the Path B series.
