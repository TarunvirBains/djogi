> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# JSONB Per-Audience Schema Projection

This spec resolves the per-audience JSONB schema gap (djogi#226). It
specifies how a single `Jsonb<T>` storage column on a `#[model]` struct
can surface different audience-shaped schemas on different generated
visages — for example, `Profile.metadata` storing the full
`ProfileMetaAdmin` shape including `stripe_customer_id`, while
`ProfilePublic.metadata` carries only the display-safe
`ProfileMetaPublic` subset.

> **Status:** Design locked for Phase 8.5 (djogi#226). Implementation
> reuses the visage-derived-field machinery shipped under djogi#231 —
> no new attribute, no new macro emission path, no new descriptor
> channel. Spec-only PR; implementation lands in a follow-up issue.

---

## Product intent

### Adopter shape

An adopter declares a JSONB column whose stored shape is the
**superset** — the union of every field every audience needs. Visages
that surface narrower audiences carry a different typed schema for the
same logical column. Example:

```rust
#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct ProfileMetaAdmin {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
    pub stripe_customer_id: String,   // admin-only
    pub analytics_id: String,         // admin-only
    pub last_referrer: Option<String>, // admin-only
}

#[derive(JsonSchema, Serialize, Deserialize, Validate)]
pub struct ProfileMetaPublic {
    pub display_name: String,
    pub bio: String,
    pub avatar_url: Option<String>,
}
```

The model carries the admin / full schema as storage truth:

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "profiles")]
pub struct Profile {
    // Full schema, persisted as-is. Exposed only to scopes that may see
    // every field. `public` is intentionally absent here.
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}
```

`ProfileSelfView`, `ProfileAdmin`, `ProfileExport` carry
`metadata: Jsonb<ProfileMetaAdmin>` (the full storage shape). The
`ProfilePublic` visage does not yet carry `metadata` at all — it is
omitted from the `expose(...)` list. The adopter then adds the narrow
projection through the existing struct-level `#[derived(...)]`
attribute:

```rust
#[derive(Model, Debug, Clone)]
#[model(table = "profiles")]
#[derived(
    name   = metadata,
    ty     = Jsonb<ProfileMetaPublic>,
    scopes = [public],
    sql    = "jsonb_build_object(\
                  'display_name', metadata->'display_name', \
                  'bio',          metadata->'bio', \
                  'avatar_url',   metadata->'avatar_url' \
              )",
    rust   = "Jsonb::new(ProfileMetaPublic { \
                  display_name: model.metadata.data.display_name.clone(), \
                  bio:          model.metadata.data.bio.clone(), \
                  avatar_url:   model.metadata.data.avatar_url.clone(), \
              })",
    doc    = " Public-audience projection of `metadata`. Excludes \
              `stripe_customer_id`, `analytics_id`, `last_referrer`.",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}
```

The macro now emits a `ProfilePublic` visage whose `metadata` field is
typed `Jsonb<ProfileMetaPublic>` and is populated via the SQL
`jsonb_build_object(...)` projection at fetch time and the
`Jsonb::new(ProfileMetaPublic { ... })` Rust expression at in-memory
construction time. `stripe_customer_id` never reaches the public-visage
struct, the public-visage wire JSON, or the public-visage rustdoc **when
the canonical recursive object-narrowing pattern is followed** — every
nesting level (including nested `Jsonb<...>` sub-schemas and the per-element
shape of array / map containers) is itself shaped with `jsonb_build_object(...)`
(or a comparable shape-narrowing builder); see
[§Safety](#safety-constraints-against-accidental-leaks) for the recursive
requirement, the new [E_DJG_VDF_017](#error-taxonomy-extension) mechanical
guard against bare-column passthrough from any same-host `Jsonb` storage
column (regardless of whether the derived field's `name` matches the source
column name), and the wire-key contract between the SQL builder and
serde-renamed schema keys.

### Non-goals

- **Automatic SQL→Rust translation.** Adopters write both `sql` and
  `rust`. Same trade-off as ordinary derived fields (see
  [visage-derived-fields.md §SQL grammar and validation](./visage-derived-fields.md#sql-grammar-and-validation)).
- **Automatic narrow-schema generation.** Adopters declare both
  `ProfileMetaAdmin` and `ProfileMetaPublic`. The framework does not
  generate the narrow struct from a subset list (rationale: adopters
  attach their own `#[validate]` / `#[serde(rename)]` / docs to the
  narrow schema; generating the type would forfeit that surface).
- **Subset-must-be-strict-subset compile check.** Proc macros operate
  in token space; the macro cannot prove `ProfileMetaPublic`'s field
  set is a subset of `ProfileMetaAdmin`'s field set at expansion time.
  A field declared in the narrow shape that doesn't exist in storage
  fails at query time as a Postgres `jsonb_build_object` referencing a
  missing path returning `null`, surfaced through normal
  `Jsonb<T>::FromSql` deserialization (or, if the narrow schema's
  `Deserialize` impl requires the field, the inner decode error is
  propagated through
  [`decode_derived_at`](../../djogi/src/pg/decode.rs) as
  `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
  — the derived-field-specific error variant; see
  [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
  item 5 for the full derived-vs-direct decode-path distinction).
  See [§Safety](#safety-constraints-against-accidental-leaks) below
  for the mechanical guards that DO apply.
- **Per-audience JSONB *value* redaction (hashing, masking,
  tokenisation).** That is the protected-data axis (djogi#227 — a
  sibling spec). This spec covers per-audience *schema shape*; the
  protected-data spec covers per-audience *value rendering*. The two
  axes compose orthogonally (see [§Interactions](#interactions)).
- **Tier-2 filter / order-by on the projected JSONB shape.** Inherits
  the existing derived-field tier deferral
  (see [visage-derived-fields.md §Capability tiers](./visage-derived-fields.md#capability-tiers)).

---

## Recommended design

**Candidate B — reuse `#[derived(...)]`.** The audience-shaped JSONB
projection is one application of the existing visage-derived-field
surface. No new attribute, no new descriptor channel, no new macro
emission path.

The contract:

1. **Storage truth lives on the model field.** The `Jsonb<T>` field
   carries the union schema; that schema is what migrations / snapshot
   / `build.rs` reflect. The model's `#[field(expose(...))]` list
   names every scope that should see the **full** stored shape.
2. **Narrower scopes get explicit struct-level
   `#[derived(name, ty = Jsonb<NarrowSchema>, scopes, sql, rust)]`
   declarations** that project the narrow shape on the listed scopes.
   The `name` matches the model column name when the narrow projection
   *replaces* the full column on those scopes; the `name` differs when
   the narrow projection lives **alongside** the full column in some
   composite shape (rare but valid — see
   [§Adjacent projection patterns](#adjacent-projection-patterns)).
3. **The two declarations must not both expose the same scope.** If
   the model field's `expose(...)` includes a scope and a
   `#[derived(name = <same-name>, scopes = [<same-scope>])]` also
   targets that scope, the existing
   [E_DJG_VDF_002](./visage-derived-fields.md#error-taxonomy)
   (column-name collision) fires at parse time. This is the forcing
   function: every transport-shape inclusion is an explicit decision
   at one site or the other, never both.

The pattern in one sentence: **the model carries the storage shape;
the visage carries the audience shape; `#[derived]` is the bridge.**

---

## Rejected alternatives

### Candidate A — field-level `#[field(jsonb_scope(public = X, admin = Y))]`

**Premise.** Add a new key under `#[field(...)]` that maps each
visage scope to a different `Jsonb<T>` schema. The macro then emits
each visage's field with the scope's narrow type, and at row-decode
time deserializes the JSONB bytes into the narrow type.

**Why rejected.**

1. **`Jsonb<T>::Deserialize` preserves unknown keys in `extra`.** The
   `Jsonb<T>` contract is load-bearing for unknown-field preservation:
   keys present in the database object but absent from `T`'s
   `Deserialize` impl land in `extra: IndexMap<String, UnknownField>`
   and round-trip through every `save()`. That contract is correct for
   the storage path — it prevents a running service from silently
   deleting a forward-compatible key written by a newer service.

   It is **incorrect** for the audience-projection path. If
   `ProfilePublic::metadata: Jsonb<ProfileMetaPublic>` is constructed
   by deserializing the full admin-shaped database row, the typed
   `data: ProfileMetaPublic` gets the three public-safe fields, **and
   the `extra` map captures `stripe_customer_id`, `analytics_id`,
   `last_referrer` verbatim.** `Jsonb<T>::Serialize` merges `data` +
   `extra` into one flat object on the wire — leaking the admin-only
   keys to the public audience. **The "Rust-side type narrowing"
   approach is unsafe by default under the existing `Jsonb<T>` serde
   contract.**

2. **Postponing #1 by introducing a `JsonbStrict<T>` variant that
   drops unknowns** is a worse trade. It (a) adds a new public type
   in the JSONB surface every adopter must learn, (b) splits the
   storage / projection paths around the same wrapper type — meaning
   the descriptor channel, the row-decode path, the admin form
   renderer, and the audit-log differ all need to know which variant
   they're walking, and (c) still doesn't address the model-side
   declaration-of-projection anti-pattern that Path B of
   visage-derived-fields explicitly moved away from.

3. **Declaration site mismatch.** Visage-derived-fields established
   visages-as-projections: derived projections live at the visage
   definition site (`scopes = [...]` addressing), not on model fields
   as virtual columns (the prior Shape A bundling that took ten review
   rounds to catch). A field-level `jsonb_scope(...)` re-introduces
   Shape A: storage shape and per-audience projection shape would
   share one declaration site on the model field, conflating two
   concerns. The reshape's whole point was eliminating that bundling;
   `jsonb_scope` undoes it for one type only.

4. **Compositional ceiling.** Candidate A handles narrowing only.
   Per-audience JSONB needs more than narrowing — adopters also want
   to (a) rename keys for export shape (`stripe_customer_id` →
   `payment_provider_id`), (b) collapse a sub-object into a scalar
   (e.g. `last_login.timestamp` → `last_seen_iso`), (c) compute a
   derived sub-shape from multiple top-level fields. The
   `#[derived(sql, rust)]` pair handles every one of these because the
   adopter writes the SQL and the Rust verbatim. Candidate A handles
   case (a) only.

### Candidate C — `#[field(public_subset = [...])]`

**Premise.** Declare a per-field, per-scope subset list of key names.
The macro auto-generates a narrow struct for each scope-field pair
(e.g. `ProfileMetadataPublicAuto`) and surfaces it on the visage.

**Why rejected.**

1. **Inherits Candidate A's #1 unsafe-by-default problem.** Even with
   an auto-generated narrow struct, the macro must decide whether the
   row-decode path consults the storage shape or the narrow shape. If
   it consults the narrow shape, `Jsonb::extra` captures the dropped
   admin-only keys and merges them back on serialize — same leak.
2. **No place for `#[validate]` / `#[serde(rename)]` / rustdoc.** The
   auto-generated narrow struct is named by the framework; adopters
   cannot attach validators, custom serde attributes, or doc comments
   to it. Real schemas need all three.
3. **Shared narrow shapes are unreachable.** Adopters with multiple
   models that need the same narrow JSONB shape (e.g.
   `Profile.metadata` and `OrgProfile.metadata` both projecting the
   same public subset) cannot share one `ProfileMetaPublic` type
   because each model auto-generates its own
   `<Model>MetadataPublicAuto`.
4. **Same declaration-site anti-pattern as A.** Subset list on the
   model field is the Shape A bundling Path B eliminated.

### Why Candidate B wins

- **Architectural cohesion.** Reuses fully-specified machinery
  (visage-derived-fields, djogi#231) instead of inventing a parallel
  surface. The error taxonomy
  ([E_DJG_VDF_001 through E_DJG_VDF_016](./visage-derived-fields.md#error-taxonomy)),
  the identifier rules, the SQL grammar guard, the descriptor inventory,
  the parity helper — all of it applies unchanged.
- **Safety story.** The narrowing happens at the **SQL projection
  boundary**, not at the Rust deserialization boundary — *but only when
  the SQL is shaped as canonical recursive object narrowing*. A
  top-level `jsonb_build_object(...)` that names exactly the narrow
  schema's wire keys, with every nested `Jsonb<...>` sub-schema
  similarly wrapped in its own `jsonb_build_object(...)` and every
  container element (array, map value) similarly shaped, narrows every
  reachable key path. Under that shape, `Jsonb<ProfileMetaPublic>::Deserialize`
  sees only the narrow keys and `extra` is structurally empty at every
  nesting level, so no admin-only key can reach `ProfilePublic`. Bare
  column passthrough (`sql = "metadata"`) and shallow projections that
  ship full sub-objects (`jsonb_build_object('theme', metadata->'theme')`
  where the source `theme` carries admin-only keys) **do not narrow**;
  the framework adds one mechanical guard for the bare-column shape
  against any same-host `Jsonb` storage column
  ([E_DJG_VDF_017](#error-taxonomy-extension)) — the guard fires
  regardless of whether the derived `name` matches the source column
  name, because the leak path runs through the projected
  `Jsonb<NarrowSchema>`'s `extra`-merge behavior, which is independent
  of the visage field alias — and documents the recursive-narrowing
  obligation for the cases proc macros cannot mechanically prove
  (nested `Jsonb`, container per-element shape).
- **Single source of truth on the model.** Storage shape lives in
  `Jsonb<ProfileMetaAdmin>`. Audience shapes live on the visage via
  `#[derived(...)]`. Migrations / snapshot / `build.rs` walk only the
  model's storage shape; rustdoc / `djogi docs` walk both via the
  separate descriptor channels (`ModelDescriptor` and
  `VisageDescriptor` per djogi#231 §Stage 2).
- **Compositional with the protected-data axis (#227).** A field can
  carry `protected(redaction = "hash_id", ...)` on the model while the
  per-audience JSONB schema is projected through `#[derived]`. The two
  axes compose without confusion because they live on different
  declaration sites (`#[field(protected)]` on the model column;
  `#[derived(...)]` on the struct).
- **Tier-2 / Tier-3 deferral story is consistent.** Filter / order-by
  on a derived JSONB projection inherits the same tier deferral as
  every other derived field. No bespoke deferred path.

---

## Macro / API syntax

The full grammar is unchanged from
[visage-derived-fields.md §Declaration](./visage-derived-fields.md#declaration).
This spec adds **no new keys and no new attributes**. It adds **one
new mechanical guard, `E_DJG_VDF_017`** (see
[§Error taxonomy extension](#error-taxonomy-extension)), targeting the
JSONB-specific bare-column passthrough leak from any same-host `Jsonb`
storage column (regardless of whether the derived `name` matches the
source column name); the guard runs at `#[derived(...)]` parse time
inside the existing parser entry point. Every other check, every emission rule, every descriptor
channel, every capability tier, and the parity helper remain unchanged
from djogi#231.

### Canonical pattern

```rust
#[derive(Model, Debug, Clone, PartialEq)]
#[model(table = "profiles")]
#[derived(
    name   = metadata,
    ty     = Jsonb<ProfileMetaPublic>,
    scopes = [public],
    sql    = "jsonb_build_object(\
                  'display_name', metadata->'display_name', \
                  'bio',          metadata->'bio', \
                  'avatar_url',   metadata->'avatar_url' \
              )",
    rust   = "Jsonb::new(ProfileMetaPublic { \
                  display_name: model.metadata.data.display_name.clone(), \
                  bio:          model.metadata.data.bio.clone(), \
                  avatar_url:   model.metadata.data.avatar_url.clone(), \
              })",
)]
pub struct Profile {
    #[field(expose(self_view, admin, export))]
    pub metadata: Jsonb<ProfileMetaAdmin>,
}
```

Notes on this pattern:

- The `#[derived]` `name = metadata` matches the source column name,
  so the same identifier appears on every generated visage (the
  full-shape visages carry the model's column entry; `ProfilePublic`
  carries the derived entry). Because the model's
  `#[field(expose(...))]` list omits `public`, the
  [E_DJG_VDF_002](./visage-derived-fields.md#error-taxonomy)
  column-name collision check passes for the `public` scope.
- The `sql` uses `jsonb_build_object(...)` — the canonical Postgres
  builder for constructing a narrow JSONB value. The alternative
  `metadata` (bare column reference) over any same-host storage
  `Jsonb<...>` column is **mechanically rejected at parse time by
  [E_DJG_VDF_017](#error-taxonomy-extension)** — regardless of whether
  the derived `name` matches the source column name. It would ship the
  full JSON to the wire and rely on Rust-side filtering, which the
  storage wrapper's unknown-field preservation contract converts into a
  silent leak via `Jsonb::extra` on re-serialize; the visage field
  alias does not change the projected `Jsonb<NarrowSchema>`'s decode /
  serialize behavior, so cross-name passthrough leaks the same way
  same-name passthrough does.
- The SQL builder's key strings ("`display_name`", "`bio`", "`avatar_url`")
  are wire-key tokens. They must match the wire keys the narrow
  schema's `serde::Serialize` / `Deserialize` impls emit and accept —
  i.e., the field names after any `#[serde(rename = "...")]` or
  `#[serde(rename_all = "...")]` adjustments. A mismatch causes the
  emitted key to land in `extra` on decode and the narrow field to be
  missing (handled per the field's declared `Option` / fallible
  contract); the parity helper's runtime DB-fetch assertion catches
  the drift. See [§Interactions with serde](#with-serde) for the
  full wire-key contract.
- The `rust` constructs a fresh `Jsonb::new(...)` with a typed
  `ProfileMetaPublic` value pulled from `model.metadata.data`. No
  `extra` is preserved on the projected visage — the typed struct
  surface and the empty unknown map are both correct semantics for a
  *projection*, where unknown-field preservation is incorrect.

### Multi-audience pattern

When more than one audience needs a narrow projection (e.g. `public`
and `export` see different narrow shapes), declare one `#[derived]`
per audience-shape pair:

```rust
#[derived(
    name   = metadata,
    ty     = Jsonb<ProfileMetaPublic>,
    scopes = [public],
    sql    = "jsonb_build_object(...)",
    rust   = "Jsonb::new(ProfileMetaPublic { ... })",
)]
#[derived(
    name   = metadata,
    ty     = Jsonb<ProfileMetaExport>,
    scopes = [export],
    sql    = "jsonb_build_object(...)",
    rust   = "Jsonb::new(ProfileMetaExport { ... })",
)]
```

The two declarations are independent — each carries its own `scopes`
list and its own `ty`. Two `#[derived]` entries that share a `name`
**cannot share any scope**: the existing
[E_DJG_VDF_003](./visage-derived-fields.md#error-taxonomy)
derived-name collision check fires at parse time the moment the same
`name` hits the same scope across multiple `#[derived]` entries. The
forcing function is mechanical: an adopter accidentally writing
`scopes = [public]` on two `name = metadata` entries (e.g. expecting
the second to win for `public`) gets a parse-time rejection naming the
overlapping scope, not a silent overwrite.

### When the source field is not exposed at all

If the source storage field is `#[field(expose(self_view))]` (only
self-view sees the full shape), and `public` / `admin` / `export` need
their own narrow projections, declare three `#[derived(name =
metadata, ty = Jsonb<...>, scopes = [...])]` entries. The model field
itself never appears in the other three visages.

### When the storage shape is hidden entirely

The model field can also be `#[field(expose(none))]` (or simply have
no `expose` annotation — the per-field opt-in default per
[djogi#229 surface 2](./decisions.md)). The storage shape is then
invisible to every transport visage; every audience that needs the
column receives it via `#[derived]` only. This is the strictest form:
no visage ever carries the union schema.

---

## Safety constraints against accidental leaks

The safety story is layered: the framework enforces what proc macros
can mechanically detect at parse time; the spec, fixture corpus,
parity helper, and user guide carry the recursive-narrowing
obligation that proc macros cannot mechanically prove (since macros
operate in token space and cannot compare `ProfileMetaPublic`'s field
set against `ProfileMetaAdmin`'s, especially across nested `Jsonb<...>`
sub-schemas or array / map container element shapes).

### Mechanical guards inherited from the derived-field surface

1. **Column-name collision check (E_DJG_VDF_002).** A model field
   exposed to scope `S` and a `#[derived(name = <same>, scopes =
   [..., S, ...])]` declaration cannot both target `S`. This forces
   the adopter to make an explicit choice: either the storage shape
   appears on `S` or the projected shape appears on `S`, never both.
   No accidental double-exposure where the projection hides one
   audience-only key while the column entry leaks it.
2. **Derived-name collision check (E_DJG_VDF_003).** Two
   `#[derived(name = metadata)]` entries cannot share a scope. This
   prevents an adopter from accidentally declaring two narrow shapes
   for the same audience and shipping the second-declared one
   (whichever the macro happens to pick) without realising the first
   was overwritten.
3. **`name` lowercase-only (E_DJG_VDF_012).** The visage struct field
   name and the SELECT alias are byte-identical, both lowercase. No
   case-folding surprise where a `Metadata` alias silently renames to
   `metadata` server-side and breaks positional decode.

### New mechanical guard: E_DJG_VDF_017 (JSONB simple-column passthrough)

The reviewer-recommended JSONB-specific guard, added by this spec
(see [§Error taxonomy extension](#error-taxonomy-extension)). The
parse-time rule fires when **both** conditions hold for a single
`#[derived(...)]` entry on a `#[model]` host:

1. The derived entry's `sql` string literal, after trimming ASCII
   whitespace, is a simple reference to **any** existing model storage
   field on the same host. Two spellings are simple references:
   unquoted bare identifier (`metadata`) and simple double-quoted
   identifier (`"metadata"`). The quoted form is accepted only when
   the trimmed literal begins and ends with `"`, contains no embedded
   double quote or SQL-escaped quote, and the unquoted body is
   byte-identical to the storage field `ident`. The derived entry's
   `name` field is **not consulted** — the guard fires whether the
   derived `name` matches the source column name (`name = metadata,
   sql = "metadata"` or `sql = "\"metadata\""`) or differs
   (`name = metadata_public_view, sql = "metadata"`).
2. The matched model field's declared Rust type token-string contains
   the rightmost identifier `Jsonb` followed by `<` (so the absolute,
   `djogi::types::`, `djogi::`, and bare-prelude paths all converge —
   same dispatch shape as the [INTERVAL typed surface](./decisions.md)
   `Interval` lookup).

When both hold, the macro emits `E_DJG_VDF_017` at the `sql = "..."`
literal span. The diagnostic names the source model column (matched by
the trimmed / unquoted `sql` literal), the derived `ty`, and the
offending `sql` literal verbatim, and directs the adopter to the
canonical `jsonb_build_object(...)` shape.

**Why the cross-name shape is also rejected.** A visage field alias
`metadata_public_view: Jsonb<ProfileMetaPublic>` populated from
`sql = "metadata"` (or `sql = "\"metadata\""`) decodes the same
admin-shaped JSON bytes Postgres returns from the bare `metadata`
column. The decode path is `Jsonb<ProfileMetaPublic>::Deserialize`,
which puts every admin-only key (`stripe_customer_id`, `analytics_id`,
`last_referrer`) into the projected value's
`extra: IndexMap<String, UnknownField>`. On serialize via
`Jsonb<T>::Serialize`'s `data + extra` merge, those admin-only keys
re-emit on the wire under the `metadata_public_view` field name.
Changing the alias does not change the wrapper's serde behavior; the
storage column's identity is what determines whether the bytes carry
admin-only keys, and the projected wrapper's `extra` merge is what
leaks them back out.

**Type aliases are not an escape hatch from passthrough.** The macro
does not need Rust name resolution to reject the dangerous derived-side
alias shape: if the source storage column is declared directly as
`Jsonb<...>` and the `sql` literal is a simple unquoted or quoted
reference to that same-host storage column, E_DJG_VDF_017 fires even
when the derived `ty` is written through an alias such as
`type PublicMeta = Jsonb<ProfileMetaPublic>; ty = PublicMeta`. The
same alias is allowed when paired with a real narrowing SQL expression
such as the canonical `jsonb_build_object(...)` shape; the alias
changes only Rust type spelling, not the SQL safety boundary. Adopters
should still prefer spelling `ty = Jsonb<ProfileMetaPublic>` directly
for clearer diagnostics and reviewability.

The guard is **narrow in SQL shape** but **comprehensive in source
column and visage-alias coverage**. It does NOT fire on:

- Bare-column passthrough from a non-`Jsonb` storage column (`sql =
  "string_col"` where `string_col: String`): condition 2 fails.
- Non-passthrough JSONB expressions, including the canonical
  `jsonb_build_object(...)` recursive narrowing shape.
- Any compound SQL expression — anything other than the simple
  bare-ident or simple quoted-ident spellings condition 1 matches —
  including `sql = "(metadata)"`, `sql = "coalesce(metadata,
  '{}'::jsonb)"`, `sql = "metadata || '{}'::jsonb"`,
  `sql = "jsonb_set(metadata, ...)"`, and `sql = "(SELECT metadata)"`.

### Compound passthrough: precise specification

E_DJG_VDF_017's conservative pattern match deliberately targets only
the simple bare-ident (`metadata`) and simple quoted-ident
(`"metadata"`) spellings of a same-host `Jsonb` storage column. Any
other SQL shape is **adopter-owned compound territory** and the
parse-time guard does not fire; the runtime parity gate is the binding
catch.

**Allowed — canonical narrowing shapes.** The documented-safe forms
are closed over these patterns: top-level `jsonb_build_object(...)`,
recursive nested `jsonb_build_object(...)` for every nested
`Jsonb<...>` sub-schema, scalar `jsonb_agg(jsonb_build_object(...)
ORDER BY ord)` subqueries wrapped in `COALESCE(..., '[]'::jsonb)` for
array containers, and scalar `jsonb_object_agg(k,
jsonb_build_object(...))` subqueries wrapped in `COALESCE(...,
'{}'::jsonb)` for map containers. The user guide MUST present these
canonical forms as the only documented-safe shapes. A derived-side type
alias such as `type PublicMeta = Jsonb<ProfileMetaPublic>; ty =
PublicMeta` is accepted when the `sql` is one of these real narrowing
forms; the acceptance fixture pins that aliases are a Rust spelling
choice, not a SQL passthrough exception.

**Mechanically rejected — simple-column passthrough spellings.** The
fixture corpus pins four E_DJG_VDF_017 shapes:

| `name` / `ty` shape | `sql` literal | Fixture |
|---|---|---|
| `name = metadata`, `ty = Jsonb<ProfileMetaPublic>` | `"metadata"` | `phase85_jsonb_per_audience_fail_006_same_name_bare_passthrough.rs` |
| `name = metadata_public_view`, `ty = Jsonb<ProfileMetaPublic>` | `"metadata"` | `phase85_jsonb_per_audience_fail_007_cross_name_bare_passthrough.rs` |
| `name = metadata`, `ty = Jsonb<ProfileMetaPublic>` | `""metadata""` | `phase85_jsonb_per_audience_fail_008_quoted_bare_passthrough.rs` |
| `name = metadata`, `ty = PublicMeta` where `type PublicMeta = Jsonb<ProfileMetaPublic>` | `"metadata"` | `phase85_jsonb_per_audience_fail_009_type_alias_bare_passthrough.rs` |

**Adopter-owned compound — not mechanically rejected; runtime parity
gate is the binding catch.** Worked examples of compound shapes that
semantically hide passthrough include `sql = "(metadata)"`,
`sql = "coalesce(metadata, '{}'::jsonb)"`,
`sql = "metadata || '{}'::jsonb"`, `sql = "jsonb_set(metadata, ...)"`,
and `sql = "(SELECT metadata)"`. The integration test corpus pins this
with test #11, `profile_public_compound_coalesce_passthrough_caught_by_parity`:
it uses `sql = "coalesce(metadata, '{}'::jsonb)"` against a storage row
containing admin-only keys and asserts `assert_derived_parity` returns
`Err(DerivedParityError::Drift { field: "metadata", .. })` because the
fetched projection's `extra` carries the admin-only keys while the
in-memory `(&model).into()` construction produces `Jsonb::new(...)`
with empty `extra`.

**Why the spec does not widen the parse-time guard to cover compound
shapes.** A wider rule would need either a SQL parser (forbidden — no
Rust regex, no in-tree SQL parser) or heuristic substring matching
that would produce false positives and false negatives. The narrow
simple-identifier guard catches the common accidental leak shapes;
compound expressions require deliberate SQL composition and are pinned
by user-guide counterexamples plus runtime parity.

### Documented patterns (not mechanically enforced — verified in fixtures + integration tests)

These are patterns the spec documents and the fixture / integration
test corpus exercises so the user-guide can recommend them; proc
macros cannot prove type-shape equivalence, recursive narrowing
across nested `Jsonb<...>`, or per-element container narrowing.

1. **Use `jsonb_build_object(...)` for top-level narrowing**, and
   nest a `jsonb_build_object(...)` (or a comparable shape-narrowing
   builder) inside it for **every** nested `Jsonb<...>` sub-schema and
   for every element shape inside an array / map container the narrow
   schema declares. A shallow projection that ships a full sub-object
   (`jsonb_build_object('theme', metadata->'theme')` where the source
   `theme` carries admin-only keys) **leaks recursively**: the inner
   `Jsonb<ThemePublic>::Deserialize` puts the admin-only nested keys
   into its own `extra`, and the outer `Jsonb::Serialize` merges them
   back on the wire. The canonical recursive form is:
   ```sql
   jsonb_build_object(
     'theme', jsonb_build_object(
       'color',       metadata->'theme'->'color',
       'font_family', metadata->'theme'->'font_family'
     )
   )
   ```
   The user guide explicitly recommends the recursive form for every
   per-audience JSONB projection that involves nested `Jsonb<...>` or
   container element shapes; the fixture corpus exercises the
   recursive form on a nested narrow schema; the integration test
   corpus pins runtime parity against a synthetic non-recursive leak.
   The unsafe `jsonb_path_query(...)` shortcut is **not** documented
   as safe for narrowing — its semantics return the source shape
   verbatim at the matched path and it does not compose with the
   recursive narrowing contract.
2. **Container element narrowing.** Array containers (`Vec<Inner>`)
   require `jsonb_agg(jsonb_build_object(...) ORDER BY ord)` over
   `jsonb_array_elements(metadata->'items') WITH ORDINALITY AS e(t, ord)`
   inside a scalar subquery, with the whole subquery wrapped in
   `COALESCE(..., '[]'::jsonb)`; map containers (`IndexMap<String, Inner>`)
   require `jsonb_object_agg(key, jsonb_build_object(...))` over
   `jsonb_each(metadata->'map')` inside a scalar subquery, with the
   whole subquery wrapped in `COALESCE(..., '{}'::jsonb)`. The
   canonical shapes are:
   ```sql
   -- Vec<Inner>
   jsonb_build_object(
     'items',
     COALESCE(
       (SELECT jsonb_agg(jsonb_build_object('name', t->>'name') ORDER BY ord)
        FROM jsonb_array_elements(metadata->'items') WITH ORDINALITY AS e(t, ord)),
       '[]'::jsonb
     )
   )
   -- IndexMap<String, Inner>
   jsonb_build_object(
     'map',
     COALESCE(
       (SELECT jsonb_object_agg(k, jsonb_build_object('enabled', v->'enabled'))
        FROM jsonb_each(metadata->'map') AS e(k, v)),
       '{}'::jsonb
     )
   )
   ```
   Each of the three sub-clauses is mandatory for correct container
   reconstruction:
   - **`WITH ORDINALITY` + `ORDER BY ord` on the array shape** preserves
     `Vec` semantics. `jsonb_agg` without an `ORDER BY` aggregates rows in
     undefined order; without `WITH ORDINALITY` the source insertion order
     is unrecoverable. `Vec<Inner>` decode treats array position as
     semantically meaningful, so the projected order must match storage
     order. The map shape carries no ordering obligation because
     `IndexMap<String, Inner>` is keyed and JSONB object ordering is not
     part of the wire contract.
   - **`COALESCE(..., '[]'::jsonb)` and `COALESCE(..., '{}'::jsonb)`**
     preserve empty containers. `jsonb_agg` / `jsonb_object_agg` return
     SQL `NULL` over zero rows (not an empty array / empty object). A
     required `Vec<Inner>` / `IndexMap<String, Inner>` field on the narrow
     schema cannot decode SQL `NULL` — `serde_json::from_value` would
     surface the missing key as an inner decode error, propagated through
     [`decode_derived_at`](../../djogi/src/pg/decode.rs) as
     `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
     (or, for `Option<Vec<_>>`, silently coerce to `None`, which loses the
     "container-was-present-but-empty" signal). Wrapping the subquery in
     `COALESCE` returns the canonical empty container so empty-list / empty-map
     source rows project to empty-list / empty-map narrow rows without
     decode failure or signal loss.

   Each per-element shape must itself be a `jsonb_build_object(...)`
   recursive narrowing if the inner type is a `Jsonb<...>` or carries
   nested objects; **when the inner type is a plain serde struct**
   (`Vec<TagPublic>` where `TagPublic` is `#[derive(Serialize,
   Deserialize)]` without a `Jsonb<...>` wrapping), `serde`'s default
   "ignore unknown fields" decode behavior provides part of the
   safety story even when the per-element SQL is non-narrow — the
   admin-only keys are dropped at the serde deserialize boundary
   rather than captured in an `extra` map. The canonical
   per-element `jsonb_build_object(...)` form is still recommended
   for plain-serde elements to keep the wire bytes narrow and to
   future-proof the shape against an inner-type change to
   `Jsonb<...>`. Container-narrowing derived entries MUST opt into
   Shape V `aggregate = true` to bypass E_DJG_VDF_009 — see
   [§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries).
   The fixture corpus exercises the array and map shapes (compile-pass
   007 / 008, both with `aggregate = true`, both wrapped in
   `COALESCE(..., '[]'::jsonb)` / `COALESCE(..., '{}'::jsonb)`, and the
   array fixture using `WITH ORDINALITY` + `ORDER BY ord`); the
   integration test corpus pins runtime parity for both (test #8
   phases 8a/8b/8c and test #9 phases 9a/9b in
   [§Integration tests](#integration-tests)), including empty-container
   preservation (test #8 phase 8c, test #9 phase 9b) and array-order
   preservation (test #8 phase 8a assertion (e)).
3. **Wire-key contract.** The SQL builder's key strings (the literal
   tokens passed to `jsonb_build_object(...)`) must be byte-identical
   to the wire keys the narrow schema's `serde::Serialize` and
   `Deserialize` impls emit and accept. A `#[serde(rename = "displayName")]`
   on the narrow schema's `display_name` field requires the SQL to
   build `'displayName', metadata->'display_name'`, NOT `'display_name',
   metadata->'display_name'`. Mismatches are not a parse-time error —
   the mismatched key lands in the projected `extra` on decode. The
   downstream failure mode then depends on the narrow field's declared
   shape:
   - **Required** narrow field (`display_name: String`,
     `count: u32`, etc.): `Jsonb<NarrowSchema>::FromSql` calls the
     inner `serde_json::from_value::<NarrowSchema>(...)` after stripping
     the projected JSONB into `data` + `extra`; the missing required
     key surfaces as a missing-field decode error inside the inner
     `serde_json::Error`, which `Jsonb<NarrowSchema>::FromSql`
     propagates back to `tokio_postgres` and which
     [`decode_derived_at`](../../djogi/src/pg/decode.rs) maps via
     `map_derived_decode_failure` into
     `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
     (NOT `DjogiError::Decode`, which is reserved for direct
     model-column decode failures via
     [`decode_at`](../../djogi/src/pg/decode.rs)). The queryset
     fetch fails outright before parity can run. This is the failure
     mode test #10 pins.
   - **Optional or defaulted** narrow field (`Option<String>`,
     `#[serde(default)]`, etc.): the typed `data` deserializes with
     the field absent / defaulted while the mismatched key sits in
     `extra`. `Jsonb<T>::Serialize` then merges `data` + `extra` on
     the wire under the original mismatched key, producing wire output
     that differs from the in-memory `Jsonb::new(NarrowSchema { ... })`
     construction. The integration parity check then catches the drift
     via the populated `extra` on the fetched value vs the empty
     `extra` on the in-memory construction.

   Wire-key drift is therefore caught at runtime by **decode failure**
   on required-field shapes and by **parity drift** on optional /
   defaulted shapes; both are documented and the required-field
   decode-failure shape is the binding test (test #10, item 5 below
   covers the parity-drift path for the optional shape).
4. **Construct `Jsonb::new(NarrowSchema { ... })` in the `rust`
   block.** The Rust-side construction must build a fresh narrow
   value, not clone the storage `Jsonb<AdminSchema>`. This pattern
   ensures `Jsonb::extra` is empty on the in-memory projected visage
   — structurally matching the canonical SQL-side projection.
5. **Pin DB-fetch parity at runtime with `assert_derived_parity` in
   an integration test, NOT a compile-pass fixture.** The parity
   helper compares only derived fields between two visage instances.
   Adopters add an integration test that constructs a profile,
   fetches the public visage via the queryset, and asserts parity
   against `(&profile).into()`. A regression that re-introduces a
   leaked key (e.g. through a future `sql` edit that drops the
   `jsonb_build_object` wrapper, fails recursive narrowing on a
   nested schema, mismatches the wire-key contract on an
   optional / defaulted narrow field, or swaps in a compound shape
   that hides bare passthrough per
   [§Compound passthrough: precise specification](#compound-passthrough-precise-specification))
   fails this test because the in-memory `rust` path yields
   `Jsonb::new(...)` with empty `extra` at every nesting level,
   while the leaky DB-fetch path yields a value whose `extra`
   carries the leaked admin / nested-admin / mis-mapped keys.

   **Implementation prerequisite — `Jsonb<T>: PartialEq where T:
   PartialEq`.** The parity helper's emitted `where <Ty>: PartialEq`
   bound ([E_DJG_VDF_016](./visage-derived-fields.md#error-taxonomy))
   makes derived `Jsonb<NarrowSchema>` fields require
   `Jsonb<NarrowSchema>: PartialEq`. `Jsonb<T>` currently lacks
   `PartialEq`; the implementation acceptance for djogi#226 MUST add
   `impl<T: PartialEq> PartialEq for Jsonb<T>` that compares both
   `data` and `extra`. If the contained unknown-field type does not
   already implement `PartialEq`, add the matching structural impl
   there as part of this prerequisite. The resulting `PartialEq` impl
   compares both `data` AND `extra`
   at every nesting level, so a regression that re-introduces a
   leak through `extra` (compound passthrough, non-recursive nested
   projection, optional-field wire-key drift) fails parity at runtime
   against the empty
   `extra` produced by `Jsonb::new(NarrowSchema { ... })`. Without
   this prerequisite, the derived `Jsonb<NarrowSchema>` field
   cannot satisfy the parity helper's bound and the macro emits
   E_DJG_VDF_016 at the `assert_derived_parity` impl block. The
   prerequisite is named in
   [§Implementation plan](#implementation-plan) step 1 and
   `docs/spec/decisions.md` notes the dependency.

   A compile-pass-only assertion that constructs both visages
   in-memory and runs parity catches `rust`-block bugs and synthetic
   `extra` population (fixture
   `phase85_jsonb_per_audience_004_parity_helper_catches_synthetic_drift.rs`)
   but **cannot** catch DB-fetch leaks (no DB round-trip happens at
   compile time); the integration test is the binding runtime
   guarantee.

   The parity gate **does not** catch wire-key mismatches against
   **required** narrow fields: the queryset fetch surfaces
   `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
   from the
   [`decode_derived_at`](../../djogi/src/pg/decode.rs) helper
   before any parity check runs (the inner decode error from
   `serde_json::from_value` propagates through
   `Jsonb<NarrowSchema>::FromSql` and is mapped by
   `map_derived_decode_failure` into the `Visage` arm of
   `DjogiError`), so the binding runtime gate for that shape is the
   decode-failure assertion (test #10), not parity. The
   `DbComputedTypeMismatch` arm is the derived-field-specific error
   variant exposed on `VisageError` and is distinct from
   `DjogiError::Decode` (which fires on **direct model columns**
   via [`decode_at`](../../djogi/src/pg/decode.rs)); the per-audience
   JSONB spec ALWAYS routes through `decode_derived_at` because the
   narrow `Jsonb<NarrowSchema>` field is a derived projection on the
   visage, never a direct model column.

The user-guide section MUST surface the canonical recursive
`jsonb_build_object` pattern as the only documented-safe form,
explicitly include the unsafe counterexamples (simple passthrough
caught by E_DJG_VDF_017 regardless of derived `name` alias, quoted
identifier spelling, or unresolved projected-`ty` alias; shallow
nested `metadata->'theme'` non-recursive form caught only at runtime
by parity; compound passthrough such as `coalesce(metadata,
'{}'::jsonb)` caught by integration parity; wire-key mismatch caught
at runtime either by decode failure on required-narrow-field shapes or
by parity drift on optional / defaulted shapes — see
[§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
item 3 for the failure-mode breakdown), and direct adopters to the
integration test corpus for the runtime decode / parity gate. The fixture corpus MUST include the compile-pass parity
assertion (in-memory construction of both paths plus synthetic
`extra`-populated drift), the type-alias canonical-builder compile-pass
fixture, AND the four E_DJG_VDF_017 compile-fail
fixtures (`phase85_jsonb_per_audience_fail_006_same_name_bare_passthrough.rs`,
`phase85_jsonb_per_audience_fail_007_cross_name_bare_passthrough.rs`,
`phase85_jsonb_per_audience_fail_008_quoted_bare_passthrough.rs`, and
`phase85_jsonb_per_audience_fail_009_type_alias_bare_passthrough.rs`);
the integration test corpus carries the DB-fetch parity assertions
across the basic, nested, container, and compound-passthrough shapes.

### What we don't try to enforce mechanically

- **The narrow schema is a strict subset of the storage schema at
  every nesting level.** The macro can't prove it. The adopter
  declares every level of every schema; a `bio: String` in the
  narrow that isn't in the storage means the SQL emits `'bio',
  metadata->'bio'` and Postgres returns SQL `NULL`, which
  `Jsonb<NarrowSchema>::FromSql` then handles per `bio`'s declared
  nullability (`String` → inner decode error propagated by
  [`decode_derived_at`](../../djogi/src/pg/decode.rs) as
  `DjogiError::Visage(VisageError::DbComputedTypeMismatch { ... })`;
  `Option<String>` → `None`). This is a runtime failure mode
  equivalent to any other SQL typo; the per-row cost is identical
  to the column-reference typo scenario already documented in
  [visage-derived-fields.md §SQL grammar and validation](./visage-derived-fields.md#sql-grammar-and-validation).
- **Recursive narrowing inside nested `Jsonb<...>` sub-schemas.** The
  outer `name`-equal-to-`name` same-`Jsonb` shape is caught by
  E_DJG_VDF_017; the recursive obligation inside a nested narrow
  schema cannot be mechanically detected (proc macros can't compare
  `Jsonb<ThemePublic>` against the source `Jsonb<ThemeAdmin>` at
  expansion time). Recursive narrowing is the adopter's
  responsibility; the integration test corpus enforces it at
  runtime; the user-guide surfaces the unsafe shallow counterexample.
- **Per-element narrowing inside array / map containers.** Same
  recursive obligation; same proc-macro limitation. Documented and
  fixture-covered (compile-pass for the canonical recursive
  per-element form; integration-test parity for the runtime check).
- **Wire-key matches across `#[serde(rename)]` adjustments.** The
  narrow schema's serde keys may differ from its Rust field names;
  the SQL builder must follow the serde keys. No parse-time check;
  caught at runtime by **decode failure** on required narrow-field
  shapes (`DjogiError::Visage(VisageError::DbComputedTypeMismatch { ... })`
  from the queryset fetch, surfaced by
  [`decode_derived_at`](../../djogi/src/pg/decode.rs) before parity
  can run) and by **parity drift** on optional / defaulted
  narrow-field shapes (the mismatched key lands in `extra` on
  decode and the parity helper catches the drift between the leaky
  fetched value and the in-memory `(&model).into()` construction).
  The required-field decode-failure path is the binding test
  (test #10 in [§Integration tests](#integration-tests)).
- **Adopter writes a compound `sql` that hides bare passthrough**
  (`sql = "coalesce(metadata, '{}'::jsonb)"`, `sql = "metadata || '{}'::jsonb"`,
  `sql = "(metadata)"`, `sql = "jsonb_set(metadata, ...)"`,
  `sql = "(SELECT metadata)"`, `sql = "\"\"metadata\"\""`).
  E_DJG_VDF_017's conservative pattern match does not fire on these
  — the complete taxonomy and the runtime-gate proof are documented
  in [§Compound passthrough: precise specification](#compound-passthrough-precise-specification).
  Same trade-off as raw SQL bypass: the user-guide marks these as
  hazardous, the fixture corpus demonstrates the parity regression
  they produce ([§Integration tests](#integration-tests) test #11),
  and review discipline catches them. Friction is the design.
- **Type-alias paths on the projected JSONB `ty`.** Proc macros do
  not perform Rust name resolution for arbitrary aliases. The
  E_DJG_VDF_017 condition pair (storage-column ident match + storage
  column's Rust type token contains `Jsonb<`) is designed so the
  guard fires whenever the storage column is spelled directly as
  `Jsonb<...>`, regardless of how the derived `ty` is spelled — a
  declaration such as `type PublicMeta = Jsonb<ProfileMetaPublic>;
  #[derived(ty = PublicMeta, sql = "metadata", ...)]` against
  `pub metadata: Jsonb<ProfileMetaAdmin>` is rejected by the
  E_DJG_VDF_017 fixture corpus (fail_009), not deferred to runtime
  parity. Adopters must spell `Jsonb<NarrowSchema>` directly on
  projected JSONB fields so the diagnostic that fires names the
  right type; the guard does not depend on it.
- **Type-alias paths on the STORAGE Rust type.** The same proc-macro
  token-space limitation also makes condition 2 of E_DJG_VDF_017
  miss the case where the storage column itself is spelled through
  a type alias: `type AdminMeta = Jsonb<ProfileMetaAdmin>;
  pub metadata: AdminMeta;`. The macro sees the storage column's
  Rust type token-string as `AdminMeta`, not `Jsonb<...>`, so
  condition 2 fails and the guard does not fire even when the
  derived `sql = "metadata"`. This case is **adopter-owned** and
  pinned at runtime by integration test #12
  `profile_public_storage_side_alias_passthrough_caught_by_parity`
  ([§Integration tests](#integration-tests)). The user guide
  recommends spelling `Jsonb<...>` directly on storage columns for
  the same reason it recommends directness on the derived `ty`:
  parse-time guards engage on direct spellings, not aliased ones.
  See [§OQ-5](#oq-5--should-a-future-spec-extension-resolve-type-aliases-for-e_djg_vdf_017)
  for the open question on whether a future macro extension should
  syntactically resolve aliases.

### Aggregate token discipline for container subqueries

Container-element narrowing for `Vec<Inner>` and `IndexMap<String,
Inner>` shapes (see [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
item 2) reconstructs the per-element shape with `jsonb_agg(...)` or
`jsonb_object_agg(...)` inside a scalar subquery driven by
`jsonb_array_elements(...)` / `jsonb_each(...)`. The aggregate
operates **within the subquery's per-outer-row scope** — Postgres
evaluates the subquery once per outer row and the aggregate folds
across that row's container elements; this is **same-row container
reconstruction**, not cross-row row aggregation. Conceptually, this
shape is distinct from top-level row aggregation (which folds across
the visage's result rows).

The existing
[E_DJG_VDF_009](./visage-derived-fields.md#error-taxonomy)
aggregate / window-function guard is implemented as a **token-level
case-insensitive scan** for a recognised set including `JSONB_AGG`,
`JSON_AGG`, `JSONB_OBJECT_AGG`, `JSON_OBJECT_AGG`, `ARRAY_AGG`,
`STRING_AGG`, `COUNT`, `SUM`, `AVG`, etc. (full set in
visage-derived-fields.md). Tokens inside single-quoted strings and
dollar-quoted bodies are skipped, but tokens **inside scalar
subqueries are not skipped** — the guard cannot distinguish
subquery-scoped aggregates from top-level row aggregates without a
SQL parser (forbidden — no Rust regex, no in-tree SQL parser).

Container-narrowing derived entries that use `jsonb_agg(...)` /
`jsonb_object_agg(...)` therefore **MUST opt into Shape V
`aggregate = true`** to bypass E_DJG_VDF_009. The Shape V opt-in is
the adopter's explicit acknowledgment that they are invoking an
aggregate function (locked in
[`docs/spec/decisions.md` §Aggregate annotation declaration site](./decisions.md#aggregate-annotation-declaration-site));
Postgres evaluates the subquery as a scalar regardless of whether the
marker is set, so runtime behavior is unaffected by the opt-in. The
Shape V marker is a parser-level acknowledgment, not a runtime
behavior switch.

Top-level row aggregation across visage result rows (e.g. an
unconditioned `sql = "jsonb_agg(metadata)"` outside any subquery
wrapping) also requires the Shape V `aggregate = true` opt-in;
without it, the same E_DJG_VDF_009 token-scan fires. The compile-fail
fixture `phase85_jsonb_per_audience_fail_004_top_level_aggregate_without_shape_v.rs`
pins the rejection at parse time when Shape V is absent. The
container subqueries in compile-pass fixtures
`phase85_jsonb_per_audience_007_array_container_recursive_narrowing.rs`
and `phase85_jsonb_per_audience_008_map_container_recursive_narrowing.rs`
include `aggregate = true` for the same reason.

The conceptual distinction (subquery-scoped vs top-level) is
documented because it informs adopter intuition about what the
derived SQL is doing; the parser-level opt-in is uniform because
E_DJG_VDF_009's token scan cannot tell the two apart. See
[§OQ-4](#oq-4--should-e_djg_vdf_009-recognise-subquery-scoped-aggregates)
for the open question on whether a future E_DJG_VDF_009 extension
should recognise the `(SELECT jsonb_agg(...) FROM jsonb_array_elements(...))`
and `(SELECT jsonb_object_agg(...) FROM jsonb_each(...))` shapes
specifically and elide the Shape V opt-in for them.

### Error taxonomy extension

This spec adds one new error code to the
[visage-derived-fields §Error taxonomy](./visage-derived-fields.md#error-taxonomy)
table:

| Code | Condition | Span |
|---|---|---|
| `E_DJG_VDF_017` | JSONB simple-column passthrough from a same-host `Jsonb` storage column: the trimmed `sql` literal is either byte-identical to the `ident` of a same-host model storage column (`metadata`) or a simple quoted spelling of that ident (`"metadata"`), and that matched column's declared Rust type matches `Jsonb<...>`. The derived `name` and derived `ty` alias spelling are **not** escape hatches — the guard fires for same-name, cross-name, quoted, and unresolved type-alias passthrough shapes. Rejected at parse time because a projected `Jsonb<NarrowSchema>` would deserialize admin-only keys into `extra` regardless of the visage field alias, then `Jsonb<T>::Serialize` would merge `data + extra` on the wire. Derived-side type aliases remain allowed with real narrowing SQL such as `jsonb_build_object(...)`; the guard is about the storage-column passthrough, not about Rust type spelling by itself. | `sql = "..."` literal |

The diagnostic shape mirrors the existing E_DJG_VDF_* family: a
span-precise `syn::Error` at the offending `sql` literal,
including the matched source model column name (recovered by the
trimmed-and-optionally-unquoted `sql` literal lookup, independent
of the derived `name`), the derived `ty`, and a "replace with
`jsonb_build_object(...)`" remediation pointer to
[§Canonical pattern](#canonical-pattern). Four lihaaf
compile-fail fixtures pin the `.stderr` snapshots — one for the
same-name bare-ident alias shape
(`phase85_jsonb_per_audience_fail_006_same_name_bare_passthrough.rs`),
one for the cross-name bare-ident alias shape
(`phase85_jsonb_per_audience_fail_007_cross_name_bare_passthrough.rs`),
one for the lowercase-quoted-ident shape covering both
same-name and cross-name aliases through the single normalised
match
(`phase85_jsonb_per_audience_fail_008_quoted_bare_passthrough.rs`),
and one for the derived-side type-alias passthrough shape
(`phase85_jsonb_per_audience_fail_009_type_alias_bare_passthrough.rs`).

---

## Compile-fail and compile-pass fixture plan

Every fixture lives under the existing lihaaf-managed corpus:
- `djogi-macros/tests/compile_pass/` — passing fixtures.
- `djogi-macros/tests/compile_fail/` — rejecting fixtures with paired
  `.stderr` snapshots.

The lihaaf harness (which replaced trybuild per Phase 8.5) drives both
directories. Snapshot blessing follows the existing
`cargo lihaaf --filter compile_fail --bless -j 4` workflow per
`CLAUDE.md`.

### Compile-pass fixtures

| Fixture | Asserts |
|---|---|
| `phase85_jsonb_per_audience_001_basic_narrowing.rs` | Storage `Jsonb<ProfileMetaAdmin>` exposed to `[self_view, admin, export]`; `#[derived(name = metadata, ty = Jsonb<ProfileMetaPublic>, scopes = [public], sql = "jsonb_build_object(...)", rust = "Jsonb::new(ProfileMetaPublic { ... })")]`. Assert: `ProfilePublic` has `metadata: Jsonb<ProfileMetaPublic>`; `ProfileAdmin` has `metadata: Jsonb<ProfileMetaAdmin>`; `<ProfilePublic as DjogiVisage>::PROJECTION_LIST` contains `jsonb_build_object` and `AS metadata`; `<ProfileAdmin as DjogiVisage>::COLUMNS` ends in `metadata`. |
| `phase85_jsonb_per_audience_002_two_narrow_audiences.rs` | Two `#[derived]` entries on the same source column with different `ty` and different `scopes` — e.g. `Jsonb<ProfileMetaPublic>` for `[public]` and `Jsonb<ProfileMetaExport>` for `[export]`. Assert both visages compile with the narrower types and that the two derived entries register independently in `VisageDescriptor`. |
| `phase85_jsonb_per_audience_003_storage_hidden_from_every_scope.rs` | Model field carries `#[field(expose(none))]`; three `#[derived]` entries supply the three audience-shaped projections (`Jsonb<X>` for `public`, `Jsonb<Y>` for `self_view`, `Jsonb<Z>` for `admin`). Assert no visage carries the storage schema; every projected visage carries the narrow type. |
| `phase85_jsonb_per_audience_004_parity_helper_catches_synthetic_drift.rs` | In-memory only (no DB): constructs both the in-memory `ProfilePublic` (via `(&profile).into()`) and a deliberately leaky synthetic `ProfilePublic` with `extra` populated, then asserts `assert_derived_parity` returns `Err(DerivedParityError::Drift)`. Pins the parity-helper regression behavior at compile-pass scope only. **Runtime DB-fetch parity is exercised by the integration tests** in [§Integration tests](#integration-tests); a compile-pass fixture cannot connect to a DB so cannot assert DB-fetch parity. |
| `phase85_jsonb_per_audience_005_typed_path_filter_on_storage_field.rs` | The storage `Jsonb<ProfileMetaAdmin>` field still supports typed-path filters via the existing `JsonbSchema` typed-accessor surface. The narrower visage projections are read-only and do not participate in `{Model}Fields` typed-path filters (consistent with the Tier-1 derived-field rule excluding derived names from `{Visage}Fields`). |
| `phase85_jsonb_per_audience_006_nested_recursive_narrow_schema.rs` | The narrow schema itself contains nested `Jsonb<Sub>` — e.g. `ProfileMetaPublic` has `theme: Jsonb<ThemePublic>`. The fixture uses the canonical **recursive** narrowing: `sql = "jsonb_build_object('theme', jsonb_build_object('color', metadata->'theme'->'color'))"`. Asserts the macro accepts nested Jsonb in the derived `ty` and that the recursive `jsonb_build_object` shape composes through the SQL grammar guard. The non-recursive shallow `jsonb_build_object('theme', metadata->'theme')` form is exercised as the unsafe counterexample by the integration test `profile_public_non_recursive_nested_projection_leaks_caught_by_parity` (see [§Integration tests](#integration-tests)). |
| `phase85_jsonb_per_audience_007_array_container_recursive_narrowing.rs` | The narrow schema declares an array container `tags: Vec<TagPublic>` whose source storage shape is `tags: Vec<TagAdmin>` (each `TagAdmin` carries an admin-only `internal_owner_id` field). The fixture uses the canonical per-element narrowing inside a scalar subquery with order preservation and empty-array preservation: `aggregate = true`, `sql = "jsonb_build_object('tags', COALESCE((SELECT jsonb_agg(jsonb_build_object('name', t->>'name') ORDER BY ord) FROM jsonb_array_elements(metadata->'tags') WITH ORDINALITY AS e(t, ord)), '[]'::jsonb))"`. The `WITH ORDINALITY` + `ORDER BY ord` combination preserves source array order across the aggregate fold (mandatory for `Vec<Inner>` semantics — see [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests) item 2); the `COALESCE(..., '[]'::jsonb)` wrap preserves empty arrays (the inner `jsonb_agg` returns SQL `NULL` over zero rows, which would surface as `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, .. })` on a required `Vec<TagPublic>` field). The `aggregate = true` Shape V opt-in is required by [§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries) — E_DJG_VDF_009's token scan would otherwise fire on `jsonb_agg`. Asserts the macro accepts the array shape with the Shape V opt-in and the compile-time visage struct carries `tags: Vec<TagPublic>`. Runtime DB-fetch parity for the array shape (including empty-array and array-order preservation) is covered in [§Integration tests](#integration-tests). |
| `phase85_jsonb_per_audience_008_map_container_recursive_narrowing.rs` | The narrow schema declares a map container `flags: IndexMap<String, FlagPublic>` whose source storage shape is `flags: IndexMap<String, FlagAdmin>` (each `FlagAdmin` carries an admin-only `set_by_internal_user` field). The fixture uses the canonical per-value narrowing inside a scalar subquery with empty-map preservation: `aggregate = true`, `sql = "jsonb_build_object('flags', COALESCE((SELECT jsonb_object_agg(k, jsonb_build_object('enabled', v->'enabled')) FROM jsonb_each(metadata->'flags') AS e(k, v)), '{}'::jsonb))"`. The `COALESCE(..., '{}'::jsonb)` wrap preserves empty maps (the inner `jsonb_object_agg` returns SQL `NULL` over zero rows, which would surface as `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, .. })` on a required `IndexMap<String, FlagPublic>` field). The `aggregate = true` Shape V opt-in is required by [§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries) — E_DJG_VDF_009's token scan would otherwise fire on `jsonb_object_agg`. Asserts the macro accepts the map shape with the Shape V opt-in and the compile-time visage struct carries `flags: IndexMap<String, FlagPublic>`. Runtime DB-fetch parity for the map shape (including empty-map preservation) is covered in [§Integration tests](#integration-tests). |
| `phase85_jsonb_per_audience_009_type_alias_canonical_narrowing.rs` | `type PublicMeta = Jsonb<ProfileMetaPublic>; #[derived(name = metadata, ty = PublicMeta, scopes = [public], sql = "jsonb_build_object('display_name', metadata->'display_name')", rust = "Jsonb::new(ProfileMetaPublic { ... })")]` on a model whose storage column is declared directly as `Jsonb<ProfileMetaAdmin>`. Asserts a derived-side alias is accepted when paired with a real narrowing expression, and that E_DJG_VDF_017's alias coverage is limited to simple same-host JSONB passthrough rather than rejecting Rust type aliases by themselves. |

### Compile-fail fixtures

Five fixtures re-assert existing E_DJG_VDF_* error coverage on
JSONB-shaped declarations; four new fixtures pin the new
[E_DJG_VDF_017](#error-taxonomy-extension) JSONB bare-column
passthrough guard across the bare-ident same-name alias, the
bare-ident cross-name alias, and the lowercase-quoted-ident
spellings condition 2 normalises to the same matchable form, plus the
derived-side type-alias passthrough shape.

| Fixture | Rejects with | Error code |
|---|---|---|
| `phase85_jsonb_per_audience_fail_001_double_exposure.rs` | Storage field is `#[field(expose(public, admin, ...))]` AND `#[derived(name = metadata, scopes = [public], ...)]`. The `public` scope has both a column entry and a derived entry with the same `name`. | [E_DJG_VDF_002](./visage-derived-fields.md#error-taxonomy) (column-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_002_duplicate_derived_in_scope.rs` | Two `#[derived(name = metadata, scopes = [public], ...)]` entries — second one would overwrite first; rejected at parse time the moment the same `name` hits the same scope. | [E_DJG_VDF_003](./visage-derived-fields.md#error-taxonomy) (derived-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_003_uppercase_name.rs` | `#[derived(name = Metadata, ...)]` with uppercase byte. | [E_DJG_VDF_012](./visage-derived-fields.md#error-taxonomy) (uppercase byte in name) |
| `phase85_jsonb_per_audience_fail_004_top_level_aggregate_without_shape_v.rs` | `sql = "jsonb_agg(metadata)"` — recognised aggregate token (`jsonb_agg`) present in the derived `sql` without the Shape V `aggregate = true` opt-in. The E_DJG_VDF_009 token-scan is context-blind (see [§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries)); the fixture's `sql` is the simplest top-level shape — neither wrapped in a scalar subquery nor opted into Shape V — so the guard fires. | [E_DJG_VDF_009](./visage-derived-fields.md#error-taxonomy) (aggregate / window-function token detection) |
| `phase85_jsonb_per_audience_fail_005_statement_separator.rs` | `sql = "metadata; DROP TABLE profiles"` — semicolon outside string literal. | [E_DJG_VDF_007](./visage-derived-fields.md#error-taxonomy) |
| `phase85_jsonb_per_audience_fail_006_same_name_bare_passthrough.rs` | `#[derived(name = metadata, ty = Jsonb<ProfileMetaPublic>, scopes = [public], sql = "metadata", rust = "...")]` on a model with `pub metadata: Jsonb<ProfileMetaAdmin>`. E_DJG_VDF_017 rejects the unquoted same-name simple passthrough from a same-host `Jsonb<_>` storage column. | [E_DJG_VDF_017](#error-taxonomy-extension) (JSONB simple-column passthrough — new in this spec) |
| `phase85_jsonb_per_audience_fail_007_cross_name_bare_passthrough.rs` | `#[derived(name = metadata_public_view, ty = Jsonb<ProfileMetaPublic>, scopes = [admin], sql = "metadata", rust = "...")]` on a model with `pub metadata: Jsonb<ProfileMetaAdmin>`. The derived `name` differs from the source column ident, but E_DJG_VDF_017 still fires because the storage-column passthrough and source `Jsonb<_>` type are what matter. | [E_DJG_VDF_017](#error-taxonomy-extension) (JSONB simple-column passthrough — new in this spec) |
| `phase85_jsonb_per_audience_fail_008_quoted_bare_passthrough.rs` | `#[derived(name = metadata, ty = Jsonb<ProfileMetaPublic>, scopes = [public], sql = "\"metadata\"", rust = "...")]` on a model with `pub metadata: Jsonb<ProfileMetaAdmin>`. The trimmed `sql` literal is a simple quoted identifier whose unquoted body is byte-identical to the storage column ident, so E_DJG_VDF_017 rejects it the same way it rejects `sql = "metadata"`. | [E_DJG_VDF_017](#error-taxonomy-extension) (JSONB simple-column passthrough — new in this spec) |
| `phase85_jsonb_per_audience_fail_009_type_alias_bare_passthrough.rs` | `type PublicMeta = Jsonb<ProfileMetaPublic>; #[derived(name = metadata, ty = PublicMeta, scopes = [public], sql = "metadata", rust = "...")]` on a model with `pub metadata: Jsonb<ProfileMetaAdmin>`. E_DJG_VDF_017 rejects the same-host JSONB passthrough even though the projected `ty` token string is `PublicMeta`; derived-side aliases are not escape hatches from the storage-column passthrough guard. The paired compile-pass fixture `phase85_jsonb_per_audience_009_type_alias_canonical_narrowing.rs` confirms the same alias is accepted with canonical `jsonb_build_object(...)` narrowing. | [E_DJG_VDF_017](#error-taxonomy-extension) (JSONB simple-column passthrough / unresolved alias — new in this spec) |

The lihaaf fixture corpus is the SOLE compile-time gate; trybuild was
removed in Phase 8.5 per the [`Spatial cfg flag in djogi-macros (Phase 7.5)`](./decisions.md)
row and the workspace `CLAUDE.md`.

---

## Integration / rustdoc / user-guide acceptance plan

Closing condition follows the workspace's standard policy for new
public API: every adopter-visible surface lands with rustdoc on every
new item, a doctest where the surface is callable in isolation, and a
spec update that names where the new behavior is documented. The
per-audience JSONB pattern adds no new Rust items (it reuses
`#[derived]` and `Jsonb<T>`), so the rustdoc surface change is
limited to cross-linking from existing items (see [§Rustdoc](#rustdoc)
below).

### Integration tests

`djogi/tests/integration/phase85_jsonb_per_audience.rs` — at least:

1. **`profile_public_omits_admin_only_keys`.** Insert a profile with a
   `metadata: Jsonb<ProfileMetaAdmin>` carrying every admin-only key.
   Fetch `ProfilePublic` via the queryset. Assert the returned
   `metadata` value's typed `data` has only the three public fields
   AND `metadata.extra().is_empty()` is true. Assert serialization to
   JSON via `serde_json::to_string(&visage)` produces a string that
   does NOT contain `stripe_customer_id` / `analytics_id` /
   `last_referrer` substrings.
2. **`profile_admin_carries_full_schema`.** Same insert; fetch
   `ProfileAdmin`; assert all six fields are present in the typed
   `data`.
3. **`parity_helper_passes_for_correct_projection`.** Construct the
   `ProfilePublic` in-memory via `(&profile).into()` and via DB fetch;
   call `assert_derived_parity` and assert `Ok(())`. This is the
   binding runtime parity assertion the spec promises (compile-pass
   parity fixture `004` cannot connect to a DB).
4. **`parity_helper_catches_storage_drift`.** Hand-construct a
   `ProfilePublic` with the same `data` but a populated `extra` map;
   call `assert_derived_parity`; assert
   `Err(DerivedParityError::Drift { field: "metadata", .. })`.
5. **`storage_field_still_supports_typed_path_filter`.** Build a
   `QuerySet<Profile>` that filters by
   `f.metadata().typed().display_name().eq("...")` — confirms the
   storage `JsonbSchema` typed-path surface is untouched by the
   per-audience projection work.
6. **`profile_public_recursive_nested_jsonb_omits_admin_only_keys`.**
   The model declares `metadata: Jsonb<ProfileMetaAdmin>` where
   `ProfileMetaAdmin` carries `theme: Jsonb<ThemeAdmin>` and
   `ThemeAdmin` carries an admin-only `internal_palette_id` field.
   `#[derived(name = metadata, ty = Jsonb<ProfileMetaPublic>, scopes = [public], sql = "jsonb_build_object('theme', jsonb_build_object('color', metadata->'theme'->'color'))", rust = ...)]`.
   Insert a row with a populated nested `theme`; fetch `ProfilePublic`;
   assert the projected `metadata.data.theme.data` has only `color`
   AND `metadata.data.theme.extra().is_empty()` AND
   `metadata.extra().is_empty()`. Assert wire JSON contains no
   `internal_palette_id` substring. Pins the recursive narrowing
   invariant at the inner `Jsonb<ThemePublic>` level.
7. **`profile_public_non_recursive_nested_projection_leaks_caught_by_parity`.**
   Same model shape as #6, but the derived `sql` deliberately uses
   the shallow `jsonb_build_object('theme', metadata->'theme')` form
   (ships the full `theme` sub-object including `internal_palette_id`).
   Fetch `ProfilePublic` via the queryset; build the in-memory
   `ProfilePublic` via `(&profile).into()` (whose `rust` block
   constructs `Jsonb::new(ThemePublic { color: ... })` with empty
   nested `extra`); call `assert_derived_parity`; assert
   `Err(DerivedParityError::Drift { field: "metadata", .. })`. Pins
   the runtime parity gate that catches the shallow non-recursive
   leak shape the macro cannot mechanically detect.
8. **`profile_public_array_container_omits_admin_only_keys`.** Model
   declares `metadata: Jsonb<ProfileMetaAdmin>` where
   `ProfileMetaAdmin` carries `tags: Vec<TagAdmin>` and each
   `TagAdmin` carries an admin-only `internal_owner_id`. The narrow
   schema `ProfileMetaPublic` carries `tags: Vec<TagPublic>` where
   `TagPublic` is a plain `#[derive(Serialize, Deserialize)]` struct
   (no `Jsonb<...>` wrapping), so `TagPublic` instances do **not**
   carry an `extra` map — the per-element safety assertion runs
   against typed fields and serialized output rather than against
   `extra`. Derived uses the canonical array per-element narrowing
   inside a scalar subquery with order preservation and empty-array
   preservation under the Shape V `aggregate = true` opt-in
   (`COALESCE((SELECT jsonb_agg(jsonb_build_object(...) ORDER BY ord)
   FROM jsonb_array_elements(...) WITH ORDINALITY AS e(t, ord)),
   '[]'::jsonb)` — see [§Aggregate token discipline for container
   subqueries](#aggregate-token-discipline-for-container-subqueries)
   and [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests) item 2).

   The test runs in three phases that share one `Profile` model and
   the same derived projection:
   - **Phase 8a (non-empty).** Insert a row with three tags whose
     `name` values are deliberately ordered to make a sorted-order
     bug observable: `["zeta", "alpha", "mike"]` (alphabetic order
     `["alpha", "mike", "zeta"]` differs from insertion order). Fetch
     `ProfilePublic`; assert:
     (a) `metadata.data.tags.len() == 3` matches the inserted count;
     (b) for every `tag` in `metadata.data.tags`, `tag.name` carries
     the expected public value (typed-field check);
     (c) `serde_json::to_string(&visage).unwrap()` does NOT contain
     the substring `internal_owner_id`;
     (d) the outer wrapper's `metadata.extra().is_empty()` is true
     (the outer `Jsonb` wrapper IS a `Jsonb<ProfileMetaPublic>` and
     DOES carry an `extra` map — assert it is empty to pin the
     outer-level narrowing);
     (e) **array-order preservation.** Assert
     `metadata.data.tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>() == vec!["zeta", "alpha", "mike"]`
     — the projected order must match the stored insertion order, not
     any other order. Without `WITH ORDINALITY` + `ORDER BY ord` on
     the derived `sql`, `jsonb_agg` aggregates in undefined order and
     this assertion fails non-deterministically.
   - **Phase 8b (single-element).** Insert a row with exactly one tag.
     Fetch `ProfilePublic`; assert `metadata.data.tags.len() == 1`
     and the typed `name` matches the inserted value. Pins the
     boundary between empty-array and multi-element shape.
   - **Phase 8c (empty-array preservation).** Insert a row with an
     empty `tags: Vec<TagAdmin>` in storage. Fetch `ProfilePublic`;
     assert `metadata.data.tags.is_empty()` and that the queryset
     fetch succeeded — without the `COALESCE(..., '[]'::jsonb)` wrap
     on the derived `sql`, the inner `jsonb_agg` would return SQL
     `NULL` over the zero rows yielded by `jsonb_array_elements(...)`
     and the required `tags: Vec<TagPublic>` decode would surface as
     `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
     via [`decode_derived_at`](../../djogi/src/pg/decode.rs) (the
     derived-field error variant; see
     [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
     item 5 for the derived-vs-direct decode-path distinction).
     This phase pins the COALESCE wrap.

   Pins container-element narrowing at runtime for the plain-serde
   element case across the non-empty, single-element, and empty-array
   shapes, plus array-order preservation.
9. **`profile_public_map_container_omits_admin_only_keys`.** Same
   shape as #8 with a map container (`flags: IndexMap<String,
   FlagPublic>` projected from `flags: IndexMap<String, FlagAdmin>`).
   `FlagPublic` is also a plain `#[derive(Serialize, Deserialize)]`
   struct (no `Jsonb<...>` wrapping), so map values do **not** carry
   an `extra` map. Derived uses the canonical map per-value
   narrowing inside a scalar subquery with empty-map preservation
   under the Shape V `aggregate = true` opt-in
   (`COALESCE((SELECT jsonb_object_agg(k, jsonb_build_object(...))
   FROM jsonb_each(...) AS e(k, v)), '{}'::jsonb)`).

   The test runs in two phases that share one `Profile` model and
   the same derived projection:
   - **Phase 9a (non-empty).** Insert a row with several flags. Fetch
     `ProfilePublic`; assert:
     (a) `metadata.data.flags.len() == n` matches the inserted count;
     (b) for every `(key, flag)` in `metadata.data.flags`,
     `flag.enabled` carries the expected public value (typed-field
     check);
     (c) `serde_json::to_string(&visage).unwrap()` does NOT contain
     the substring `set_by_internal_user`;
     (d) the outer wrapper's `metadata.extra().is_empty()` is true.
     Maps have no semantic ordering obligation — `IndexMap<String, _>`
     preserves insertion order in-memory but JSONB object key order is
     not part of the wire contract; the typed-field check above
     compares per-key, not in order.
   - **Phase 9b (empty-map preservation).** Insert a row with an
     empty `flags: IndexMap<String, FlagAdmin>` in storage. Fetch
     `ProfilePublic`; assert `metadata.data.flags.is_empty()` and
     that the queryset fetch succeeded — without the
     `COALESCE(..., '{}'::jsonb)` wrap on the derived `sql`, the
     inner `jsonb_object_agg` would return SQL `NULL` over the zero
     rows yielded by `jsonb_each(...)` and the required
     `flags: IndexMap<String, FlagPublic>` decode would surface as
     `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
     via [`decode_derived_at`](../../djogi/src/pg/decode.rs). This
     phase pins the COALESCE wrap.
10. **`profile_public_wire_key_mismatch_required_field_fails_decode`.**
    Narrow schema declares `#[serde(rename = "displayName")]` on its
    **required** `display_name: String` field (the field is plain
    `String`, not `Option<String>`, and carries no
    `#[serde(default)]`). Derived `sql` mistakenly uses the
    pre-rename key `'display_name'` —
    `sql = "jsonb_build_object('display_name', metadata->'display_name', ...)"`
    instead of the correct
    `sql = "jsonb_build_object('displayName', metadata->'display_name', ...)"`.

    Insert a profile with a populated `display_name`. Attempt to fetch
    `ProfilePublic` via the queryset; assert the fetch returns
    `Err(DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage: "ProfilePublic", field: "metadata", expected, actual }))`
    where `expected` carries `Jsonb<ProfileMetaPublic>`'s
    `std::any::type_name` and `actual` is `"JSONB"`. The
    decode-failure mechanism: Postgres returns the projected JSONB
    object `{"display_name": "<value>", ...}`;
    `Jsonb<ProfileMetaPublic>::FromSql` splits the object into `data`
    and `extra` by consulting `ProfileMetaPublic`'s serde keys;
    because `display_name` does not match the renamed serde key
    `displayName`, it lands in `extra`; the required `displayName`
    field is missing from the `data`-shaped subset and
    `serde_json::from_value::<ProfileMetaPublic>(...)` returns a
    `missing field "displayName"` error. The error propagates back
    through `Jsonb<ProfileMetaPublic>::FromSql` to `tokio_postgres`'s
    `Row::try_get`; the derived projection routes decoding through
    [`decode_derived_at`](../../djogi/src/pg/decode.rs), whose
    `map_derived_decode_failure` helper maps every non-`WasNull`
    `tokio_postgres` decode error to
    `DjogiError::Visage(VisageError::DbComputedTypeMismatch { ... })`
    — **not** `DjogiError::Decode`, which is reserved for direct
    model-column decode failures via
    [`decode_at`](../../djogi/src/pg/decode.rs). The queryset fetch
    fails before `assert_derived_parity` can run.

    Pins the wire-key contract at runtime via the harder
    decode-failure path. For required renamed fields, wire-key
    mismatch fails outright before parity can run, which better
    matches Djogi's "fail loudly" safety model than silently
    defaulting the missing field. Proc macros cannot detect the
    rename / key mismatch at parse time (the narrow schema's serde
    attributes are not visible from the `#[derived]` attribute's
    declaration site); the queryset fetch is the runtime gate. The
    error variant assertion (`VisageError::DbComputedTypeMismatch`
    rather than `DjogiError::Decode`) is itself part of the contract
    this test pins, because it documents the derived-vs-direct
    decode-path distinction at the wire.

    The companion optional / defaulted shape (where the renamed field
    is declared `Option<String>` or `#[serde(default)]`) is the
    parity-drift path documented in
    [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
    item 3 and item 5 — that shape's drift is caught by
    `assert_derived_parity` because the mismatched key sits in `extra`
    on the fetched value but is absent on the in-memory `(&profile).into()`
    construction. The required-field decode-failure path (test #10) is
    the binding test; the optional-field parity-drift path is not
    pinned by an additional integration test because the parity helper
    is already exercised by tests #3, #4, and #7 and the optional-field
    shape is structurally identical from the parity helper's
    perspective.
11. **`profile_public_compound_coalesce_passthrough_caught_by_parity`.**
    Same model shape as #1, but the derived `sql` deliberately uses
    `sql = "coalesce(metadata, '{}'::jsonb)"` instead of recursive
    `jsonb_build_object(...)`. E_DJG_VDF_017 intentionally does not
    fire because the string is not a simple unquoted or quoted
    identifier and the macro does not parse SQL. Insert a row carrying
    admin-only keys, fetch `ProfilePublic`, build the in-memory
    `ProfilePublic` via `(&profile).into()`, call
    `assert_derived_parity`, and assert
    `Err(DerivedParityError::Drift { field: "metadata", .. })`.
    Pins the runtime parity gate for compound passthrough hazards and
    backs the user-guide counterexample rather than leaving the claim
    to review discipline only.
12. **`profile_public_storage_side_alias_passthrough_caught_by_parity`.**
    Pins the runtime parity gate for the storage-side type-alias hole
    in E_DJG_VDF_017's condition 2 (see
    [§What we don't try to enforce mechanically](#what-we-dont-try-to-enforce-mechanically)
    type-alias bullets). The model declares the storage column
    through a type alias:
    ```rust
    pub type AdminMeta = Jsonb<ProfileMetaAdmin>;
    #[derive(Model, Debug, Clone, PartialEq)]
    #[model(table = "profiles_storage_alias")]
    #[derived(
        name   = metadata,
        ty     = Jsonb<ProfileMetaPublic>,
        scopes = [public],
        sql    = "metadata",
        rust   = "Jsonb::new(ProfileMetaPublic { display_name: model.metadata.data.display_name.clone(), bio: model.metadata.data.bio.clone(), avatar_url: model.metadata.data.avatar_url.clone() })",
    )]
    pub struct ProfileStorageAlias {
        #[field(expose(self_view, admin, export))]
        pub metadata: AdminMeta,
    }
    ```
    The storage column's Rust type token-string is `AdminMeta`, not
    `Jsonb<...>`; E_DJG_VDF_017 condition 2 fails so the macro accepts
    the declaration at compile time despite the `sql = "metadata"`
    simple-passthrough shape against an effectively-`Jsonb<...>`
    storage column.

    Insert a row with admin-only keys populated. Fetch
    `ProfileStorageAliasPublic` via the queryset; build the in-memory
    visage via `(&profile).into()` (the `rust` block constructs
    `Jsonb::new(ProfileMetaPublic { ... })` with empty `extra`). Call
    `assert_derived_parity`; assert
    `Err(DerivedParityError::Drift { field: "metadata", .. })`. The
    fetched projection's `Jsonb<ProfileMetaPublic>::Deserialize`
    populates `extra` with `stripe_customer_id`, `analytics_id`,
    `last_referrer` (the bare `sql = "metadata"` ships the full
    admin-shaped JSON bytes); the in-memory construction's `extra`
    is empty; the `PartialEq` impl on `Jsonb<ProfileMetaPublic>`
    (implementation prerequisite per
    [§Documented patterns](#documented-patterns-not-mechanically-enforced--verified-in-fixtures--integration-tests)
    item 5 and [§Implementation plan](#implementation-plan) step 1)
    catches the `extra`-map difference and parity fails.

    Pins the runtime gate for storage-side alias passthrough: the
    parity helper IS the binding catch when adopters spell the
    storage column's Rust type through a type alias. Distinct from
    test #11 (compound passthrough) and from fail_009 (derived-side
    alias rejected at parse time): this test specifically exercises
    the case where the macro's parse-time guard misses the leak
    because the storage column's Rust type is aliased.

### Rustdoc

- `Jsonb<T>` rustdoc gains a "Per-audience schema projection" section
  pointing to this spec and the user-guide page.
- `VisageDescriptor` / `DerivedProjection` rustdoc already covers the
  inventory channel; no edit needed.
- Every fixture's `//!` module-level doc cites this spec.

### User-guide page

**Edit `docs/guide/jsonb.md`.** Add a "Per-audience JSONB schema" section
positioned after the existing "Subfield Query Filters" section. The
section contains:

1. The product scenario (`Profile.metadata` with `stripe_customer_id`).
2. The canonical pattern (storage `Jsonb<AdminSchema>` +
   `#[derived(ty = Jsonb<PublicSchema>, sql = "jsonb_build_object(...)",
   rust = "Jsonb::new(...)")]`).
3. **The safety note.** Why `jsonb_build_object` is required (not
   merely preferred) over bare column reference, including the new
   [E_DJG_VDF_017](#error-taxonomy-extension) parse-time guard that
   rejects bare passthrough from any same-host `Jsonb` storage column
   regardless of the derived `name` (so both `name = metadata, sql =
   "metadata"` and `name = metadata_public_view, sql = "metadata"`
   fail at parse time, as do `sql = "\"metadata\""` and unresolved
   `ty = PublicMeta` alias pass-through); that the same `PublicMeta`
   alias is acceptable with canonical `jsonb_build_object(...)`
   narrowing; what `Jsonb::extra` does on the projected
   path; the wire-key contract between SQL builder keys and the
   narrow schema's serde-renamed keys (covering both failure modes —
   required-field decode failure surfacing as `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, .. })`
   at queryset fetch time, and optional-field parity drift caught
   by the integration parity helper); the container reconstruction
   contract (`WITH ORDINALITY` + `ORDER BY ord` for `Vec<Inner>`
   order preservation, `COALESCE(..., '[]'::jsonb)` /
   `COALESCE(..., '{}'::jsonb)` for empty-container preservation);
   how the integration-test runtime gates pin the absence of leaks
   (parity helper for shape-drift, decode failure for required-field
   wire-key mismatch — compile-pass parity is in-memory only and does
   not catch DB-fetch leaks).
4. **The unsafe counterexamples (mandatory).** The section MUST show
   the eight documented unsafe shapes with the failure mode each
   produces:
   - **Same-name simple passthrough.** `sql = "metadata"` with
     `name = metadata` on a `Jsonb` storage column — rejected
     mechanically by E_DJG_VDF_017 at parse time (fixture `fail_006`).
   - **Cross-name simple passthrough.** `sql = "metadata"` (or quoted
     `sql = "\"metadata\""`) with `name = metadata_public_view` on a
     `Jsonb` storage column — rejected mechanically by E_DJG_VDF_017 at
     parse time (fixture `fail_007`; the quoted spelling shares the
     same code path). The visage alias does not change the projected
     `Jsonb<NarrowSchema>::Deserialize` / `Serialize` leak path; the
     guard fires on the storage-column / sql pair regardless of the
     visage field alias.
   - **Quoted simple passthrough.** `sql = "\"metadata\""` with any
     derived `name` on a same-host `Jsonb` storage column — rejected
     mechanically by E_DJG_VDF_017 at parse time (fixture `fail_008`).
     Quoting the identifier does not make it a narrowing expression;
     the parser normalises this simple quoted form to the same storage
     column ident for the passthrough check.
   - **Type-alias on the projected `ty`.** `type PublicMeta =
     Jsonb<ProfileMetaPublic>;` with `#[derived(ty = PublicMeta,
     sql = "metadata", ...)]` on a `Jsonb` storage column — rejected
     mechanically by E_DJG_VDF_017 at parse time (fixture `fail_009`).
     The macro does not check the derived `ty` for `Jsonb<...>`; the
     guard fires because the storage column is spelled directly as
     `Jsonb<...>` and the `sql` is a simple ident match. Adopters
     should still spell `Jsonb<NarrowSchema>` directly on the derived
     `ty` so the diagnostic that fires names the right type.
   - **Type-alias on the STORAGE Rust type.** `type AdminMeta =
     Jsonb<ProfileMetaAdmin>;` with `pub metadata: AdminMeta;` —
     compiles cleanly because E_DJG_VDF_017 condition 2 fails (the
     storage column's Rust type token-string is `AdminMeta`, not
     `Jsonb<...>`). Leaks the full storage JSONB through the projected
     `Jsonb<NarrowSchema>::extra`; caught at runtime by integration
     parity (test #12). Adopters should spell `Jsonb<...>` directly on
     storage columns so the parse-time guard stays engaged.
   - **Shallow nested projection.** `jsonb_build_object('theme',
     metadata->'theme')` over a nested `Jsonb<ThemePublic>` whose
     source is `Jsonb<ThemeAdmin>` — compiles cleanly, leaks
     admin-only keys via the nested `Jsonb<ThemePublic>::extra`,
     caught only at runtime by integration parity (test #7).
   - **Compound passthrough.** `sql = "coalesce(metadata, '{}'::jsonb)"`,
     `sql = "metadata || '{}'::jsonb"`, `sql = "(metadata)"`,
     `sql = "jsonb_set(metadata, ...)"`, `sql = "(SELECT metadata)"`,
     or `sql = "\"\"metadata\"\""` (doubly-quoted) — compiles cleanly
     because E_DJG_VDF_017 is deliberately simple-identifier-only; leaks
     the full source JSONB through `extra`, caught at runtime by
     integration parity (test #11). See
     [§Compound passthrough: precise specification](#compound-passthrough-precise-specification)
     for the full taxonomy.
   - **Wire-key mismatch.** SQL builder uses pre-rename key while the
     narrow schema declares `#[serde(rename)]` — compiles cleanly;
     downstream failure mode splits on field shape: **required**
     renamed fields surface
     `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, expected, actual })`
     at queryset fetch time (the missing required field cannot be
     filled from `extra` and the queryset fetch fails outright —
     test #10); **optional or `#[serde(default)]`** renamed fields
     silently default the field and let the mismatched key sit in
     `extra`, caught at runtime by integration parity drift (covered
     structurally by the parity helper exercised in tests #3, #4, #7).
     The required-field path is the binding integration assertion.
5. **The recursive narrowing rule.** Every nesting level of a
   per-audience JSONB projection (nested `Jsonb<...>` sub-schemas,
   each element of an array container, each value of a map container)
   must itself be shaped by `jsonb_build_object(...)` (or a
   comparable shape-narrowing builder). The user guide includes
   one worked example each for nested `Jsonb`, `Vec<Inner>` array,
   and `IndexMap<String, Inner>` map shapes — the same shapes the
   fixture and integration corpora exercise.
6. Pointer to `docs/guide/derived-projections.md` for the derived-field
   surface; pointer to `docs/guide/visages.md` for the visage / scope
   semantics; pointer to `docs/guide/protected-data.md` (when it lands
   under #227) for the per-audience value-rendering axis.

**Cross-reference from `docs/guide/derived-projections.md`** — add a
short "Per-audience JSONB schemas" subsection that links to the new
`docs/guide/jsonb.md` section as a worked example.

**Cross-reference from `docs/guide/visages.md`** — update the
"Typed JSON Fields" section (mirrors `docs/spec/visages.md §Typed JSON
Fields`) to note that the historical "include the whole field or
exclude it" baseline now has a third option: project a narrower shape
via `#[derived]`, with a link to this spec.

---

## Interactions

### with `Jsonb<T>` (`docs/spec/jsonb.md`)

The storage `Jsonb<T>` type is unchanged. The unknown-field
preservation contract on the storage path remains load-bearing for
forward compatibility. The projected `Jsonb<NarrowSchema>` on the
visage side has structurally empty `extra` at the outer level **when
the SQL projection follows the canonical recursive object-narrowing
pattern** (top-level `jsonb_build_object(...)` naming exactly the
narrow schema's wire keys, with every nested `Jsonb<...>` sub-schema
shape wrapped in its own `jsonb_build_object(...)`, and every array /
map container element / value similarly shape-narrowed at every
nesting level). When the SQL is non-recursive — a shallow
`jsonb_build_object('theme', metadata->'theme')` over a nested
`Jsonb<ThemePublic>` whose source is `Jsonb<ThemeAdmin>`, for
example — the OUTER `extra` is still empty but the INNER nested
`Jsonb<ThemePublic>::extra` captures the source's admin-only keys
and re-emits them via the storage `Jsonb` serde-merge on the wire.
The recursive narrowing requirement is the adopter's responsibility;
integration test parity catches non-recursive drift at runtime. The
same `Jsonb<T>` type satisfies both roles — the difference is
entirely in which JSON bytes Postgres delivers and whether every
nesting level was shape-narrowed by the adopter-written SQL.

### with `JsonbSchema` typed-path API (`djogi/src/jsonb/schema.rs`)

Typed-path filters on the **storage** field
(`f.metadata().typed().display_name().eq(...)`) work exactly as before
— they consult the storage column directly via Postgres JSONB path
operators, independent of any visage. The narrow projected shapes on
visages do NOT participate in `{Model}Fields` typed-path filters
because derived fields are excluded from `{Model}Fields` (Tier-1
contract per
[visage-derived-fields.md §Capability tiers](./visage-derived-fields.md#tier-1--read-time-projection-v010)).
Tier-2 widening (filter on derived fields including JSONB projections)
inherits the same deferral.

### with visages (`docs/spec/visages.md`)

- The existing per-field opt-in default (djogi#229 surface 2) is
  preserved. The model field's `expose(...)` list says which scopes
  see the storage shape; absent annotation means no scope sees it.
- The "Typed JSON Fields" section's historical baseline — "include the
  whole field or exclude it" — is now formally amended: a third
  option, project a narrower shape via `#[derived]`, is available.
  The spec doc lists this as the canonical resolution to djogi#226.

### with visage-derived fields (`docs/spec/visage-derived-fields.md`)

This spec is a **pattern** layered on the existing derived-field
surface, not a parallel feature. Every constraint, error code,
descriptor channel, capability tier, parity helper, and emission rule
described in `visage-derived-fields.md` applies unchanged. The only
addition is the documented JSONB-shaped usage pattern (above) and the
fixture corpus that exercises it.

### with serde

`Jsonb<T>::Serialize` merges `data` + `extra` into one flat JSON
object on the wire. On the **storage** path this is correct (round-trip
forward-compatible keys). On the **projected** path, `extra` is
structurally empty **at every nesting level** when the SQL projection
follows the canonical recursive object-narrowing pattern (the
"`with Jsonb<T>`" interaction section above details the recursive
requirement);
under that shape the wire output is identical to a freshly-constructed
`Jsonb::new(NarrowSchema { ... })`. No serde contract change is needed.

**Wire-key contract** (cross-referenced from the canonical pattern
notes, the safety section, and the `Jsonb<T>` interaction section
above). The string keys passed to `jsonb_build_object(...)` in the
derived `sql` are wire-key tokens. They MUST be byte-identical to
the keys the narrow schema's `serde::Serialize` and `Deserialize`
impls emit and accept — i.e., the field names after any
`#[serde(rename = "...")]` or `#[serde(rename_all = "camelCase")]` /
`PascalCase` / `kebab-case` / similar adjustments declared on the
narrow schema. Examples:

- Narrow schema: `pub struct ProfileMetaPublic { #[serde(rename =
  "displayName")] pub display_name: String, ... }`. Derived SQL must
  build `'displayName', metadata->'display_name'` (the wire-key
  `displayName` paired with the source's storage-key path
  `metadata->'display_name'`).
- Narrow schema: `#[serde(rename_all = "camelCase")] pub struct
  ProfileMetaPublic { pub display_name: String, ... }`. Derived SQL
  must build `'displayName', metadata->'display_name'` for the same
  reason.

A mismatch is **not** a parse-time error — proc macros cannot read the
narrow schema's serde attributes from the derived attribute's
declaration site. A mismatched key lands in the projected `extra` on
decode; the downstream failure mode depends on the narrow field's
declared shape:

- **Required** narrow field (`display_name: String`): the
  required-field decode failure surfaces as `DjogiError::Visage(VisageError::DbComputedTypeMismatch { visage, field, .. })` at
  queryset fetch time, before any parity check can run. The inner
  `serde_json::from_value::<NarrowSchema>(...)` cannot fill the
  missing required field from `extra` and propagates the error up
  through `Jsonb<NarrowSchema>::FromSql` and through the visage
  fetch. This is the failure mode test #10 pins (see
  [§Integration tests](#integration-tests)).
- **Optional / defaulted** narrow field (`Option<String>`,
  `#[serde(default)]`): the typed `data` deserializes with the field
  absent / defaulted; the mismatched key sits in `extra` and re-emits
  on serialize, producing wire output that differs from the in-memory
  `Jsonb::new(NarrowSchema { ... })`. The parity helper catches the
  drift via the populated `extra` map at the runtime integration
  boundary.

Both shapes are runtime gates; the required-field decode-failure path
is the binding integration test, and the optional-field parity-drift
path is covered structurally by the parity helper already exercised
by tests #3, #4, and #7.

### with query / projection surfaces

- `VisageQuerySet<ProfilePublic>::fetch_all(...)` walks
  `<ProfilePublic as DjogiVisage>::PROJECTION_LIST`, which the macro
  rendered once at compile time. For the canonical pattern that string
  contains `(jsonb_build_object('display_name', metadata->'display_name', ...)) AS metadata`
  at the derived position.
- `VisageQuerySet<ProfileAdmin>::fetch_all(...)` walks the unmodified
  `PROJECTION_LIST` that includes the bare `metadata` column. The two
  visages' queries are independent; no SELECT-list collision because
  they target different visage types.
- Predicate / order-by reuse on derived JSONB projections is
  **Tier-2 deferral** (see
  [visage-derived-fields.md §Tier 2](./visage-derived-fields.md#tier-2--predicate-use-deferred-to-a-named-phase)).

### with `#[field(protected(...))]` (djogi#227 — sibling spec)

`protected(...)` describes how a stored value should be **redacted /
hashed / masked** when shown in a redaction-aware surface (admin form,
audit log, export bundle). It operates on the same storage column,
across every audience that sees the column. djogi#227 will extend that
axis with per-scope redaction rules.

`#[derived(ty = Jsonb<NarrowSchema>, scopes = [...], sql, rust)]`
describes a **different audience-shaped schema** for the same logical
column. The two compose orthogonally:

- A storage `Jsonb<X>` field can carry
  `protected(sensitivity = "pii", redaction = "drop", ...)` on the
  model column — meaning every audience that sees the full shape sees
  it redacted. The `#[derived]` projection on a different audience
  carries the narrow shape; redaction on the narrow path is obviated
  ONLY when the narrow schema **recursively omits** the sensitive key
  at every nesting level (top-level absence is not enough if the
  sensitive key is reachable through a nested `Jsonb<...>` sub-schema,
  a `Vec<Inner>` element, or a `IndexMap<String, Inner>` value whose
  per-element shape carries the key). Adopters who claim "the narrow
  shape omits the sensitive key" must verify the omission at every
  nesting level reachable from the projected `Jsonb<NarrowSchema>` —
  the integration test parity helper (test #7 in
  [§Integration tests](#integration-tests)) is the binding runtime
  check.
- A `#[derived]`-projected `Jsonb<NarrowSchema>` is itself a typed
  projection; the narrow schema's individual keys can carry their own
  `#[validate]` / `#[serde(rename = "...")]` / `#[serde(rename_all =
  "...")]` / nullability annotations independently. The narrow type's
  storage shape is whatever its serde `Serialize` / `Deserialize`
  impls produce, and the derived `sql`'s `jsonb_build_object` keys
  must match those wire-keys per the
  [§Wire-key contract](#with-serde).

Neither spec depends on the other; they can land in any order. This
spec does not block on djogi#227, and djogi#227 does not block on this.

---

## Adjacent projection patterns

Some adopters want both the full and narrow shape on the same visage
(rare — typical when an internal-audit visage shows the redacted
public shape alongside the un-redacted full shape for comparison):

```rust
#[derived(
    name   = metadata_public_view,
    ty     = Jsonb<ProfileMetaPublic>,
    scopes = [admin],
    sql    = "jsonb_build_object(...)",
    rust   = "Jsonb::new(ProfileMetaPublic { ... })",
)]
pub struct Profile {
    #[field(expose(admin, ...))]
    pub metadata: Jsonb<ProfileMetaAdmin>,  // appears as `metadata`
                                            // on `ProfileAdmin`
}
```

`ProfileAdmin` then carries **both** `metadata: Jsonb<ProfileMetaAdmin>`
(the column entry) AND `metadata_public_view: Jsonb<ProfileMetaPublic>`
(the derived entry). Different `name`s avoid the E_DJG_VDF_002
collision. This pattern is unusual but legal and fully supported **as
long as the derived `sql` is a real narrowing expression** —
`sql = "jsonb_build_object(...)"` in the example above. Substituting
the bare-column shortcut `sql = "metadata"` here would be rejected at
parse time by the widened [E_DJG_VDF_017](#error-taxonomy-extension):
the derived `name` differing from the source column ident is **not**
an escape hatch from the guard, because the projected
`Jsonb<ProfileMetaPublic>::Deserialize` / `Serialize` leak path runs
regardless of the visage field alias (see the [§E_DJG_VDF_017](#new-mechanical-guard-e_djg_vdf_017-jsonb-bare-column-passthrough)
discussion for the full rationale and the
`phase85_jsonb_per_audience_fail_007_cross_name_bare_passthrough.rs`
compile-fail fixture).

---

## Implementation plan

This spec adds **one new mechanical guard, `E_DJG_VDF_017`** (the
JSONB simple-column passthrough rejector covering same-name,
cross-name, quoted-identifier, and unresolved type-alias passthrough shapes — see
[§Error taxonomy extension](#error-taxonomy-extension)) to the
existing `#[derived(...)]` parser shipped under djogi#231 (Phase 8.5);
every other surface — codegen, trait emission, descriptor channel,
parity helper, capability tiers — is reused unchanged.

When the orchestrator dispatches the implementer task for djogi#226,
the work breakdown is:

1. **`Jsonb<T>` PartialEq prerequisite.** Add
   `impl<T: PartialEq> PartialEq for Jsonb<T>` before relying on the
   parity fixtures. The implementation compares both `data` and
   `extra`; `extra` comparison is mandatory because the #226 runtime
   parity gate detects leaks by observing preserved unknown fields on
   fetched projected values. If `extra`'s contained unknown value type
   does not yet implement `PartialEq`, add the matching structural
   `PartialEq` there as part of this prerequisite.
2. **Macro guard.** Add the E_DJG_VDF_017 check to the
   `#[derived(...)]` parser entry point (the same module that hosts
   the existing E_DJG_VDF_001 through E_DJG_VDF_016 checks). The
   check is the two-condition match defined in
   [§Error taxonomy extension](#error-taxonomy-extension): the trimmed
   `sql` literal is either the byte-identical ident of some same-host
   model storage column or a simple quoted identifier whose unquoted
   body is byte-identical to that ident, and the matched column's
   declared Rust type is `Jsonb<...>`. The derived `name` field and
   unresolved `ty` aliases are not consulted as escape hatches. The
   check runs per-derived-entry, scopes against the host model's
   storage field list (already collected by the macro for the existing
   E_DJG_VDF_002 check), and emits a span-precise `syn::Error` at the
   `sql = "..."` literal. No new descriptor channel, no new emission
   rule, no new public surface — just one additional parse-time
   rejector.
3. **Compile-pass fixtures.** Add the nine fixtures listed in
   [§Compile-pass fixtures](#compile-pass-fixtures). They exercise the
   pattern against the live `#[derived]` parser, codegen, and trait
   constants; the nested / array-container / map-container fixtures
   exercise the recursive narrowing shapes the SQL grammar guard
   must accept (subquery-with-`jsonb_agg` / `jsonb_object_agg`
   variants under the Shape V `aggregate = true` opt-in described in
   [§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries)).
4. **Compile-fail fixtures.** Add the nine fixtures listed in
   [§Compile-fail fixtures](#compile-fail-fixtures). Five re-assert
   existing E_DJG_VDF_* error coverage on the specific JSONB-shaped
   declarations; four pin the new E_DJG_VDF_017 `.stderr` snapshots
   (same-name, cross-name, quoted-identifier, and unresolved
   type-alias shapes of the simple-column passthrough).
5. **Integration tests.** Add the twelve tests listed in
   [§Integration tests](#integration-tests). They run against a real
   Postgres instance via `#[djogi::djogi_test(sync_models = [Profile])]`
   per the workspace pattern; tests #6 through #9 carry the runtime
   parity / leak / container-shape (empty + order) coverage, test
   #10 carries the wire-key required-field decode-failure coverage,
   test #11 carries the compound-passthrough parity coverage, and
   test #12 carries the storage-side type-alias parity coverage —
   all of which proc macros cannot detect at parse time.
6. **User-guide section.** Edit `docs/guide/jsonb.md`,
   `docs/guide/derived-projections.md`, and `docs/guide/visages.md`
   per [§User-guide page](#user-guide-page), including the mandatory
   unsafe counterexamples and recursive narrowing rule.
7. **Decision-row entry.** Already added under this PR — see
   `docs/spec/decisions.md` "JSONB per-audience schema projection
   (djogi#226, Phase 8.5)".

The implementation issue closes when the fixture corpus is green
(including the new E_DJG_VDF_017 compile-fail), the twelve integration
tests pass against the real Postgres instance, the user-guide section
ships, and the doc-gen (`cargo doc --no-deps`) is clean.

---

## Open questions

This spec resolves none of these — they remain open for follow-up
phases. They are filed here so future readers find them anchored to the
correct surface.

### OQ-1 — Should the user-guide ship a `protected`-aware example?

The interaction between this spec (per-audience JSONB *schema*) and
djogi#227 (per-audience JSONB *value* redaction) is described above as
orthogonal. The user-guide section described in
[§User-guide page](#user-guide-page) covers this spec only. When
djogi#227 lands, a worked example combining the two axes (e.g.
"`Jsonb<ProfileMetaAdmin>` with `protected(redaction = "drop")` on
self-view PLUS a narrow `Jsonb<ProfileMetaPublic>` projection on
public") may belong in either spec's guide section. Tracking issue:
follow-up after #227 ships.

### OQ-2 — Should `#[derived]` gain a `jsonb_only = true` helper?

A future ergonomic improvement could detect the common
"narrow-by-key-selection" pattern and synthesise both the `sql` and
the `rust` from a key list. The current design rejects the sugar
(Candidate C above) because (a) it cannot generate the narrow schema's
custom annotations, and (b) the explicit `sql` / `rust` pair lets
adopters compose non-trivial projections (renames, sub-object
collapses, computed values). If sustained adopter friction surfaces
post-v0.1.0 publish, a `jsonb_only = true` opt-in could narrow the
common case without re-opening Candidate C's safety problem (because
the macro would emit the safe `jsonb_build_object` form, not the
unsafe bare column reference). Tracking: post-publish adopter
feedback.

### OQ-3 — Tier-2 filter / order-by on derived JSONB projections

Filtering `ProfilePublic` by `metadata.display_name` from the queryset
side is the same Tier-2 deferral that affects every derived field. No
JSONB-specific deferral is needed; the underlying mechanism
(per-entry SQL re-rendering from `V::PROJECTIONS`) handles the JSONB
case the same way it handles any other derived expression. Tracking:
the Tier-2 work named in
[visage-derived-fields.md §Tier 2](./visage-derived-fields.md#tier-2--predicate-use-deferred-to-a-named-phase).

### OQ-4 — Should E_DJG_VDF_009 recognise subquery-scoped aggregates?

Container-element narrowing for `Vec<Inner>` / `IndexMap<String, Inner>`
requires `jsonb_agg(...)` / `jsonb_object_agg(...)` inside a scalar
subquery. Conceptually this is **same-row container reconstruction**
(the aggregate folds across one outer row's container elements), not
cross-row row aggregation. The current spec requires the Shape V
`aggregate = true` opt-in for these subquery shapes because
E_DJG_VDF_009 is implemented as a token-level scan that cannot
distinguish subquery context from top-level context (see
[§Aggregate token discipline for container subqueries](#aggregate-token-discipline-for-container-subqueries)).

A future extension to E_DJG_VDF_009 could recognise a narrow set of
canonical container-reconstruction subquery shapes — specifically the
`(SELECT jsonb_agg(jsonb_build_object(...)) FROM jsonb_array_elements(...))`
and `(SELECT jsonb_object_agg(key, jsonb_build_object(...)) FROM jsonb_each(...))`
patterns — and elide the Shape V opt-in for them, on the grounds that
the per-element narrowing they express is structurally
same-row-scoped. The extension is the spec amendment that would
realise the GPT-5.5-recommended "preferred spec" cited in the
phase-86 review thread. Tracking: post-v0.1.0 adopter feedback — if
container narrowing surfaces sustained friction, the
visage-derived-fields.md spec amendment lands as a follow-up issue;
this spec's compile-pass fixtures 007 / 008 lose the `aggregate =
true` opt-in at that point. Until then, the Shape V opt-in is the
uniform mechanism.

### OQ-5 — Should a future spec extension resolve type aliases for E_DJG_VDF_017?

E_DJG_VDF_017 condition 2 fires when the storage column's declared
Rust type token-string contains the rightmost identifier `Jsonb`
followed by `<`; proc macros operate in token space and cannot
resolve type aliases at expansion time, so storage-side type aliases
(`type AdminMeta = Jsonb<ProfileMetaAdmin>; pub metadata: AdminMeta;`)
bypass the guard. This spec routes the storage-side alias case
through the runtime parity gate (integration test #12) and
documents the limitation; adopters who spell `Jsonb<...>` directly
on the storage column never encounter it.

A future macro extension could syntactically resolve a narrow set of
canonical alias patterns within the same module / `use` graph the
macro can see at expansion time (single-file alias chains where
`type Foo = Jsonb<Bar>;` lives in the same `mod`/file as the model
struct) and extend condition 2 to follow those aliases. The
trade-off: parse-time alias resolution adds complexity to the guard
and shifts a documented runtime gate (test #12) into a parse-time
rejector, but the resolution can only ever be partial — cross-crate
aliases, generic alias chains, and `pub use` re-exports cannot be
resolved in token space. The runtime parity gate remains the
authoritative catch for the unresolvable cases. Tracking:
post-v0.1.0 adopter feedback — if storage-side alias usage surfaces
as a common adopter footgun, a same-module alias-resolution pass
lands as a follow-up issue on
`docs/spec/visage-derived-fields.md`; until then, integration
test #12 is the binding runtime gate.

---

## References

- Sibling spec: [`docs/spec/visage-derived-fields.md`](./visage-derived-fields.md) (djogi#231) — the machinery this spec reuses.
- Sibling spec: [`docs/spec/visages.md`](./visages.md) — `expose(...)` axis and visage emission contract.
- Sibling spec: [`docs/spec/jsonb.md`](./jsonb.md) — `Jsonb<T>` storage contract and unknown-field preservation.
- Sibling spec: [`docs/spec/protected-data.md`](./protected-data.md) — protected-data metadata axis (djogi#227 extends this for per-scope redaction).
- Decisions index: [`docs/spec/decisions.md`](./decisions.md) — the locked decision row for this spec.
- Research: [`docs/research/model-vs-visage-lower-severity-graduation.md`](../research/model-vs-visage-lower-severity-graduation.md) — surface 1 / surface 2 analysis that informs the per-field declaration-site discipline this spec inherits.
- User guide: [`docs/guide/jsonb.md`](../guide/jsonb.md) — where the per-audience pattern lands for adopters.
- User guide: [`docs/guide/derived-projections.md`](../guide/derived-projections.md) — the derived-field guide; gets a JSONB cross-reference.
- User guide: [`docs/guide/visages.md`](../guide/visages.md) — the visage guide; gets a "per-audience JSONB" note in the Typed JSON Fields section.
- Issue: [djogi#226](https://github.com/TarunvirBains/djogi/issues/226).
