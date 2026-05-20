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
guard against same-name bare-column passthrough, and the wire-key contract
between the SQL builder and serde-renamed schema keys.

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
  `Deserialize` impl requires the field, as `DjogiError::Decode`). See
  [§Safety](#safety-constraints-against-accidental-leaks) below for
  the mechanical guards that DO apply.
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
  the framework adds one mechanical guard for the same-name
  bare-column shape ([E_DJG_VDF_017](#error-taxonomy-extension)) and
  documents the recursive-narrowing obligation for the cases proc
  macros cannot mechanically prove (nested `Jsonb`, container
  per-element shape).
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
JSONB-specific same-name bare-column passthrough leak; the guard runs
at `#[derived(...)]` parse time inside the existing parser entry
point. Every other check, every emission rule, every descriptor
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
  `metadata` (bare column reference) for a same-name projection over a
  storage `Jsonb<...>` column is **mechanically rejected at parse time
  by [E_DJG_VDF_017](#error-taxonomy-extension)**: it would ship the
  full JSON to the wire and rely on Rust-side filtering, which the
  storage wrapper's unknown-field preservation contract converts into a
  silent leak via `Jsonb::extra` on re-serialize.
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

### New mechanical guard: E_DJG_VDF_017 (JSONB same-name passthrough)

The reviewer-recommended JSONB-specific guard, added by this spec
(see [§Error taxonomy extension](#error-taxonomy-extension)). The
parse-time rule fires when **all four** conditions hold for a single
`#[derived(...)]` entry on a `#[model]` host:

1. The derived entry's `ty` token-string is `Jsonb<...>` (matched
   on the rightmost identifier `Jsonb` followed by `<`, so the
   absolute, `djogi::types::`, `djogi::`, and bare-prelude paths
   all converge — same dispatch shape as the
   [INTERVAL typed surface](./decisions.md) `Interval` lookup).
2. The derived entry's `name = <ident>` matches the byte-identical
   `ident` of an existing model storage field on the same host.
3. The matched model field's declared Rust type token-string is
   also `Jsonb<...>` (under the same rightmost-identifier match).
4. The derived entry's `sql` string literal, after trimming ASCII
   whitespace, is exactly the byte-identical `ident` of that
   matched model column (i.e. a bare-column passthrough).

When all four hold, the macro emits `E_DJG_VDF_017` at the `sql =
"..."` literal span. The diagnostic names the model column, the
derived `ty`, and the offending `sql` literal, and directs the
adopter to the canonical `jsonb_build_object(...)` shape. The guard
is **narrow by construction** — it does NOT fire on:

- Cross-name projection (`name = metadata_public_view, sql = "metadata"`):
  the wire alias differs from the source column, the storage `Jsonb`'s
  serde-merge contract does not apply, and the leak path the guard
  protects against does not arise.
- Non-`Jsonb` derived `ty` (e.g. `ty = String`): the unknown-field
  preservation contract this guard protects against is specific to
  `Jsonb<T>::extra`.
- Compound expressions (`sql = "(metadata)"`, `sql = "metadata ||
  '{}'::jsonb"`, `sql = "coalesce(metadata, '{}'::jsonb)"`):
  proc macros cannot prove these are equivalent to bare passthrough,
  and conservative pattern matching on the trimmed literal is the
  rule. Adopters who reach for these forms knowingly accept the
  unknown-field preservation hazard the recursive-narrowing
  obligation calls out.

The narrowness is intentional: a wider rule would either need to
parse SQL (forbidden — no Rust regex, no in-tree SQL parser) or
reject legitimate compound expressions. The guard closes the
single most common leak shape (`sql = "<col>"` matching a `Jsonb`
column with `name = <col>` and `ty = Jsonb<NarrowSchema>`) at the
parse-time boundary and leaves wider misuse to the documented
patterns, fixtures, and parity helper below.

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
   require `jsonb_agg(jsonb_build_object(...))` over
   `jsonb_array_elements(metadata->'items')`; map containers
   (`IndexMap<String, Inner>`) require `jsonb_object_agg(key,
   jsonb_build_object(...))` over `jsonb_each(metadata->'map')`. Each
   per-element shape must itself be a `jsonb_build_object(...)`
   recursive narrowing if the inner type is a `Jsonb<...>` or carries
   nested objects. The fixture corpus exercises the array and map
   shapes; the integration test corpus pins runtime parity for both.
3. **Wire-key contract.** The SQL builder's key strings (the literal
   tokens passed to `jsonb_build_object(...)`) must be byte-identical
   to the wire keys the narrow schema's `serde::Serialize` and
   `Deserialize` impls emit and accept. A `#[serde(rename = "displayName")]`
   on the narrow schema's `display_name` field requires the SQL to
   build `'displayName', metadata->'display_name'`, NOT `'display_name',
   metadata->'display_name'`. Mismatches are not a parse-time error —
   the mismatched key lands in the projected `extra` on decode and
   the narrow field is missing (handled per its declared `Option` /
   fallible contract). Wire-key drift is caught at runtime by the
   integration-test parity check (item 5 below).
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
   nested schema, or mismatches the wire-key contract) fails this
   test because the in-memory `rust` path yields `Jsonb::new(...)`
   with empty `extra` at every nesting level, while the leaky
   DB-fetch path yields a value whose `extra` carries the leaked
   admin / nested-admin / mis-mapped keys — the `PartialEq` derive
   on `Jsonb<T>` (transitively required by the parity helper's
   `where <Ty>: PartialEq` bound,
   [E_DJG_VDF_016](./visage-derived-fields.md#error-taxonomy))
   compares both `data` AND `extra` at every nesting level, so the
   two `Jsonb<NarrowSchema>` values are not equal. A
   compile-pass-only assertion that constructs both visages
   in-memory and runs parity catches `rust`-block bugs and synthetic
   `extra` population but **cannot** catch DB-fetch leaks (no DB
   round-trip happens at compile time); the integration test is the
   binding runtime guarantee.

The user-guide section MUST surface the canonical recursive
`jsonb_build_object` pattern as the only documented-safe form,
explicitly include the unsafe counterexamples (bare passthrough
caught by E_DJG_VDF_017, shallow nested `metadata->'theme'`
non-recursive form caught only at runtime by parity, wire-key
mismatch caught only at runtime by parity), and direct adopters to
the integration test corpus for the runtime parity gate. The
fixture corpus MUST include the compile-pass parity assertion (in-memory
construction of both paths plus synthetic `extra`-populated drift) AND
the new E_DJG_VDF_017 compile-fail fixture; the integration test
corpus carries the DB-fetch parity assertions across the basic,
nested, and container shapes.

### What we don't try to enforce mechanically

- **The narrow schema is a strict subset of the storage schema at
  every nesting level.** The macro can't prove it. The adopter
  declares every level of every schema; a `bio: String` in the
  narrow that isn't in the storage means the SQL emits `'bio',
  metadata->'bio'` and Postgres returns SQL `NULL`, which
  `Jsonb<NarrowSchema>::FromSql` then handles per `bio`'s declared
  nullability (`String` → `DjogiError::Decode`; `Option<String>` →
  `None`). This is a runtime failure mode equivalent to any other
  SQL typo; the per-row cost is identical to the column-reference
  typo scenario already documented in
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
  caught at runtime by integration parity.
- **Adopter writes a compound `sql` that hides bare passthrough**
  (`sql = "coalesce(metadata, '{}'::jsonb)"`, `sql = "metadata || '{}'::jsonb"`).
  E_DJG_VDF_017's conservative pattern match does not fire on these.
  Same trade-off as raw SQL bypass: the user-guide marks these as
  hazardous, the fixture corpus demonstrates the parity regression
  they produce, and review discipline catches them. Friction is the
  design.

### Error taxonomy extension

This spec adds one new error code to the
[visage-derived-fields §Error taxonomy](./visage-derived-fields.md#error-taxonomy)
table:

| Code | Condition | Span |
|---|---|---|
| `E_DJG_VDF_017` | JSONB same-name bare-column passthrough: all four conditions hold simultaneously — derived `ty` matches `Jsonb<...>`, derived `name` matches a model storage column on the same host, the matched model column's declared type matches `Jsonb<...>`, and the trimmed `sql` literal is byte-identical to the matched column's `ident`. Rejected at parse time because the storage `Jsonb<T>::Serialize` merges `data` + `extra` on the wire — under bare passthrough the projected visage would silently re-emit every admin-only key from the source column. | `sql = "..."` literal |

The diagnostic shape mirrors the existing E_DJG_VDF_* family: a
span-precise `syn::Error` at the offending `sql` literal,
including the model column name, the derived `ty`, and a
"replace with `jsonb_build_object(...)`" remediation pointer to
[§Canonical pattern](#canonical-pattern). The lihaaf compile-fail
fixture pins the `.stderr` snapshot.

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
| `phase85_jsonb_per_audience_007_array_container_recursive_narrowing.rs` | The narrow schema declares an array container `tags: Vec<TagPublic>` whose source storage shape is `tags: Vec<TagAdmin>` (each `TagAdmin` carries an admin-only `internal_owner_id` field). The fixture uses the canonical per-element narrowing: `sql = "jsonb_build_object('tags', (SELECT jsonb_agg(jsonb_build_object('name', t->>'name')) FROM jsonb_array_elements(metadata->'tags') t))"`. Asserts the macro accepts the array shape, the SQL grammar guard accepts the subquery form, and the compile-time visage struct carries `tags: Vec<TagPublic>`. Runtime DB-fetch parity for the array shape is covered in [§Integration tests](#integration-tests). |
| `phase85_jsonb_per_audience_008_map_container_recursive_narrowing.rs` | The narrow schema declares a map container `flags: IndexMap<String, FlagPublic>` whose source storage shape is `flags: IndexMap<String, FlagAdmin>` (each `FlagAdmin` carries an admin-only `set_by_internal_user` field). The fixture uses the canonical per-value narrowing: `sql = "jsonb_build_object('flags', (SELECT jsonb_object_agg(k, jsonb_build_object('enabled', v->'enabled')) FROM jsonb_each(metadata->'flags') AS e(k, v)))"`. Asserts the macro accepts the map shape, the SQL grammar guard accepts the subquery form, and the compile-time visage struct carries `flags: IndexMap<String, FlagPublic>`. Runtime DB-fetch parity for the map shape is covered in [§Integration tests](#integration-tests). |

### Compile-fail fixtures

Five fixtures re-assert existing E_DJG_VDF_* error coverage on
JSONB-shaped declarations; one new fixture pins the new
[E_DJG_VDF_017](#error-taxonomy-extension) JSONB same-name passthrough
guard.

| Fixture | Rejects with | Error code |
|---|---|---|
| `phase85_jsonb_per_audience_fail_001_double_exposure.rs` | Storage field is `#[field(expose(public, admin, ...))]` AND `#[derived(name = metadata, scopes = [public], ...)]`. The `public` scope has both a column entry and a derived entry with the same `name`. | [E_DJG_VDF_002](./visage-derived-fields.md#error-taxonomy) (column-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_002_duplicate_derived_in_scope.rs` | Two `#[derived(name = metadata, scopes = [public], ...)]` entries — second one would overwrite first; rejected at parse time the moment the same `name` hits the same scope. | [E_DJG_VDF_003](./visage-derived-fields.md#error-taxonomy) (derived-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_003_uppercase_name.rs` | `#[derived(name = Metadata, ...)]` with uppercase byte. | [E_DJG_VDF_012](./visage-derived-fields.md#error-taxonomy) (uppercase byte in name) |
| `phase85_jsonb_per_audience_fail_004_aggregate_in_sql.rs` | `sql = "jsonb_agg(metadata)"` — aggregate keyword inside derived `sql` without the Shape V `aggregate = true` opt-in. | [E_DJG_VDF_009](./visage-derived-fields.md#error-taxonomy) (aggregate / window-function detection) |
| `phase85_jsonb_per_audience_fail_005_statement_separator.rs` | `sql = "metadata; DROP TABLE profiles"` — semicolon outside string literal. | [E_DJG_VDF_007](./visage-derived-fields.md#error-taxonomy) |
| `phase85_jsonb_per_audience_fail_006_same_name_bare_passthrough.rs` | `#[derived(name = metadata, ty = Jsonb<ProfileMetaPublic>, scopes = [public], sql = "metadata", rust = "...")]` on a model with `pub metadata: Jsonb<ProfileMetaAdmin>`. All four E_DJG_VDF_017 conditions hold: derived `ty` is `Jsonb<_>`, derived `name` matches the storage column `metadata`, the storage column's declared type is `Jsonb<_>`, and `sql.trim() == "metadata"`. | [E_DJG_VDF_017](#error-taxonomy-extension) (JSONB same-name bare-column passthrough — new in this spec) |

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
   `TagAdmin` carries an admin-only `internal_owner_id`. Derived
   uses the canonical array per-element narrowing
   (`jsonb_agg(jsonb_build_object(...)) FROM jsonb_array_elements(...)`).
   Insert a row with multiple tags; fetch `ProfilePublic`; assert
   every element of `metadata.data.tags` has only the public fields
   AND no element's `extra` carries `internal_owner_id`. Pins
   container-element recursive narrowing.
9. **`profile_public_map_container_omits_admin_only_keys`.** Same
   shape as #8 with a map container (`flags: IndexMap<String,
   FlagPublic>` projected from `flags: IndexMap<String, FlagAdmin>`).
   Derived uses the canonical map per-value narrowing
   (`jsonb_object_agg(k, jsonb_build_object(...)) FROM jsonb_each(...)`).
   Insert a row with several flags; fetch `ProfilePublic`; assert
   every map value has only the public fields and no entry's `extra`
   carries the admin-only key.
10. **`profile_public_wire_key_mismatch_caught_by_parity`.** Narrow
    schema declares `#[serde(rename = "displayName")]` on its
    `display_name` field. Derived `sql` mistakenly uses the
    pre-rename key `'display_name'`. Fetch `ProfilePublic` via the
    queryset; the wire-key `display_name` lands in `extra` on
    decode and the narrow `displayName` field is missing. Call
    `assert_derived_parity` against `(&profile).into()`; assert
    `Err(DerivedParityError::Drift { field: "metadata", .. })`.
    Pins the wire-key contract at runtime — proc macros cannot
    detect the rename / key mismatch at parse time.

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
   rejects same-name bare passthrough; what `Jsonb::extra` does on the
   projected path; the wire-key contract between SQL builder keys and
   the narrow schema's serde-renamed keys; how the integration-test
   parity helper pins the absence of leaks at runtime (compile-pass
   parity is in-memory only and does not catch DB-fetch leaks).
4. **The unsafe counterexamples (mandatory).** The section MUST show
   the three documented unsafe shapes with the failure mode each
   produces:
   - `sql = "metadata"` (bare passthrough on a same-name `Jsonb`
     column) — rejected mechanically by E_DJG_VDF_017 at parse time.
   - Shallow nested projection (`jsonb_build_object('theme',
     metadata->'theme')` over a nested `Jsonb<ThemePublic>` whose
     source is `Jsonb<ThemeAdmin>`) — compiles cleanly, leaks
     admin-only keys via the nested `Jsonb<ThemePublic>::extra`,
     caught only at runtime by integration parity (test #7 in
     [§Integration tests](#integration-tests)).
   - Wire-key mismatch (SQL builder uses pre-rename key while the
     narrow schema declares `#[serde(rename)]`) — compiles cleanly,
     leaks the renamed key into `extra` and surfaces as a missing
     narrow-schema field, caught at runtime by integration parity
     (test #10).
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
decode and the narrow field is missing (handled per its declared
nullability / fallible contract); the wire JSON re-emits the
mis-mapped key from `extra` and the parity helper's runtime
integration assertion catches the drift (test #10 in
[§Integration tests](#integration-tests)).

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
collision. This pattern is unusual but legal and fully supported.

---

## Implementation plan

This spec adds **one new mechanical guard, `E_DJG_VDF_017`** (the
JSONB same-name bare-column passthrough rejector — see
[§Error taxonomy extension](#error-taxonomy-extension)) to the
existing `#[derived(...)]` parser shipped under djogi#231 (Phase 8.5);
every other surface — codegen, trait emission, descriptor channel,
parity helper, capability tiers — is reused unchanged.

When the orchestrator dispatches the implementer task for djogi#226,
the work breakdown is:

1. **Macro guard.** Add the E_DJG_VDF_017 check to the
   `#[derived(...)]` parser entry point (the same module that hosts
   the existing E_DJG_VDF_001 through E_DJG_VDF_016 checks). The
   check is the four-condition match defined in
   [§Error taxonomy extension](#error-taxonomy-extension). It runs
   per-derived-entry, scopes against the host model's storage field
   list (already collected by the macro for the existing E_DJG_VDF_002
   check), and emits a span-precise `syn::Error` at the `sql = "..."`
   literal. No new descriptor channel, no new emission rule, no new
   public surface — just one additional parse-time rejector.
2. **Compile-pass fixtures.** Add the eight fixtures listed in
   [§Compile-pass fixtures](#compile-pass-fixtures). They exercise the
   pattern against the live `#[derived]` parser, codegen, and trait
   constants; the nested / array-container / map-container fixtures
   exercise the recursive narrowing shapes the SQL grammar guard
   must accept (subquery-with-`jsonb_agg` / `jsonb_object_agg`
   variants).
3. **Compile-fail fixtures.** Add the six fixtures listed in
   [§Compile-fail fixtures](#compile-fail-fixtures). Five re-assert
   existing E_DJG_VDF_* error coverage on the specific JSONB-shaped
   declarations; the sixth pins the new E_DJG_VDF_017 `.stderr`
   snapshot.
4. **Integration tests.** Add the ten tests listed in
   [§Integration tests](#integration-tests). They run against a real
   Postgres instance via `#[djogi::djogi_test(sync_models = [Profile])]`
   per the workspace pattern; tests #6 through #10 carry the runtime
   parity / leak / wire-key drift coverage that proc macros cannot
   detect at parse time.
5. **User-guide section.** Edit `docs/guide/jsonb.md`,
   `docs/guide/derived-projections.md`, and `docs/guide/visages.md`
   per [§User-guide page](#user-guide-page), including the mandatory
   unsafe counterexamples and recursive narrowing rule.
6. **Decision-row entry.** Already added under this PR — see
   `docs/spec/decisions.md` "JSONB per-audience schema projection
   (djogi#226, Phase 8.5)".

The implementation issue closes when the fixture corpus is green
(including the new E_DJG_VDF_017 compile-fail), the ten integration
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
