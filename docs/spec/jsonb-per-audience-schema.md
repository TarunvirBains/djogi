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
struct, the public-visage wire JSON, or the public-visage rustdoc.

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
  boundary**, not at the Rust deserialization boundary. The SQL
  `jsonb_build_object(...)` literally specifies which keys reach the
  wire; the `Jsonb<ProfileMetaPublic>::Deserialize` then sees only
  those keys and `extra` is structurally empty. There is no path by
  which `stripe_customer_id` reaches `ProfilePublic`.
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
This spec adds **no new keys, no new attributes, no new error codes.**
The only addition is a recommended JSONB-shaped usage pattern.

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
  `metadata` (bare column reference) is **rejected by the safety
  pattern** below: it ships the full JSON to the wire and relies on
  Rust-side filtering.
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

The two declarations are independent. The framework does not detect or
forbid overlap in `scopes` between two `#[derived]` entries that share
a `name` — the existing
[E_DJG_VDF_003](./visage-derived-fields.md#error-taxonomy)
derived-name collision check fires at parse time when the same `name`
hits the same scope twice across multiple `#[derived]` entries.

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

The framework cannot inspect `ProfileMetaPublic`'s field set against
`ProfileMetaAdmin`'s field set at macro time (proc macros operate in
token space). What the framework **does** enforce:

### Mechanical guards inherited from the derived-field surface

1. **SQL-side projection is the trusted boundary.** The
   `jsonb_build_object(...)` call literally names which keys ship.
   Postgres ships exactly those keys; no admin-only key can survive
   the SELECT projection. This is the load-bearing safety invariant.
2. **`Jsonb<NarrowSchema>::extra` is structurally empty on the
   projected visage.** Because Postgres returns only the narrow keys,
   `Jsonb<ProfileMetaPublic>::Deserialize` sees no unknown keys and
   `extra` is empty. The serialize merge on the wire is therefore a
   no-op for unknown fields. The unknown-field preservation contract
   that makes `Jsonb<T>` safe for storage is **structurally inert** on
   the projected path.
3. **Column-name collision check (E_DJG_VDF_002).** A model field
   exposed to scope `S` and a `#[derived(name = <same>, scopes =
   [..., S, ...])]` declaration cannot both target `S`. This forces
   the adopter to make an explicit choice: either the storage shape
   appears on `S` or the projected shape appears on `S`, never both.
   No accidental double-exposure where the projection hides one
   audience-only key while the column entry leaks it.
4. **Derived-name collision check (E_DJG_VDF_003).** Two
   `#[derived(name = metadata)]` entries cannot share a scope. This
   prevents an adopter from accidentally declaring two narrow shapes
   for the same audience and shipping the second-declared one
   (whichever the macro happens to pick) without realising the first
   was overwritten.
5. **`name` lowercase-only (E_DJG_VDF_012).** The visage struct field
   name and the SELECT alias are byte-identical, both lowercase. No
   case-folding surprise where a `Metadata` alias silently renames to
   `metadata` server-side and breaks positional decode.

### Documented patterns (not mechanically enforced — verified in fixtures)

These are patterns the spec documents and the fixture corpus exercises
so the user-guide can recommend them; the macro does not reject the
unsafe forms because proc macros cannot prove type-shape equivalence.

1. **Use `jsonb_build_object` for narrowing, not bare column
   reference.** A `sql = "metadata"` derived entry returns the full
   JSON; even though the Rust-side deserialization narrows the typed
   `data`, the unknown keys land in `extra` and round-trip on the
   wire. **The user guide explicitly recommends `jsonb_build_object`
   for any per-audience JSONB projection** and the fixture corpus
   demonstrates this pattern. A `jsonb_path_query(...)` projection is
   also safe when the path expression is narrowing.
2. **Construct `Jsonb::new(NarrowSchema { ... })` in the `rust`
   block.** The Rust-side construction must build a fresh narrow
   value, not clone the storage `Jsonb<AdminSchema>`. This pattern
   ensures `Jsonb::extra` is empty on the projected visage —
   structurally matching the SQL-side projection.
3. **Pin the projection with `assert_derived_parity` in tests.** The
   parity helper compares only derived fields between two visage
   instances. Adopters add an integration test that constructs a
   profile, fetches the public visage via the queryset, and asserts
   parity against `(&profile).into()`. A regression that re-introduces
   a leaked key (e.g. through a future `sql` edit that drops the
   `jsonb_build_object` wrapper) fails this test because the
   in-memory `rust` path yields `Jsonb::new(ProfileMetaPublic { ... })`
   with empty `extra`, while the leaky DB-fetch path yields a value
   whose `extra` carries the admin keys — the `PartialEq` derive on
   `Jsonb<T>` (transitively required by the parity helper's `where
   <Ty>: PartialEq` bound, [E_DJG_VDF_016](./visage-derived-fields.md#error-taxonomy))
   compares both `data` AND `extra`, so the two `Jsonb<ProfileMetaPublic>`
   values are not equal.

The user-guide section MUST surface the `jsonb_build_object` pattern
as the canonical form; the fixture corpus MUST include a compile-pass
test that constructs both paths and runs the parity helper.

### What we don't try to enforce

- **The narrow schema is a strict subset of the storage schema.** The
  macro can't prove it. The adopter declares both schemas; a `bio:
  String` in the narrow that isn't in the storage means the SQL emits
  `'bio', metadata->'bio'` and Postgres returns SQL `NULL`, which
  `Jsonb<NarrowSchema>::FromSql` then handles per `bio`'s declared
  nullability (`String` → `DjogiError::Decode`; `Option<String>` →
  `None`). This is a runtime failure mode equivalent to any other SQL
  typo; the per-row cost is identical to the column-reference typo
  scenario already documented in
  [visage-derived-fields.md §SQL grammar and validation](./visage-derived-fields.md#sql-grammar-and-validation).
- **Adopter writes the right `sql`.** A `sql = "metadata"` (bare
  column reference) compiles. The leak is a documented unsafe pattern;
  the user-guide marks it as such, the fixture corpus demonstrates the
  parity-helper regression it produces, and review discipline catches
  it. Same trade-off as raw SQL bypass: friction is the design.

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
| `phase85_jsonb_per_audience_004_parity_helper_catches_leak.rs` | Constructs both the in-memory `ProfilePublic` (via `(&profile).into()`) and a deliberately leaky synthetic `ProfilePublic` with `extra` populated, then asserts `assert_derived_parity` returns `Err(DerivedParityError::Drift)`. Pins the parity-helper regression behavior described in [§Safety](#documented-patterns-not-mechanically-enforced--verified-in-fixtures). |
| `phase85_jsonb_per_audience_005_typed_path_filter_on_storage_field.rs` | The storage `Jsonb<ProfileMetaAdmin>` field still supports typed-path filters via the existing `JsonbSchema` typed-accessor surface. The narrower visage projections are read-only and do not participate in `{Model}Fields` typed-path filters (consistent with the Tier-1 derived-field rule excluding derived names from `{Visage}Fields`). |
| `phase85_jsonb_per_audience_006_nested_narrow_schema.rs` | The narrow schema itself contains nested `Jsonb<Sub>` — e.g. `ProfileMetaPublic` has `theme: Jsonb<ThemePublic>`. Asserts the macro accepts nested Jsonb in the derived `ty` and that the SQL projection composes (`jsonb_build_object('theme', metadata->'theme')` ships only what the adopter's SQL builds). |

### Compile-fail fixtures

These reuse existing E_DJG_VDF_* error codes. **No new error codes are
added by this spec.**

| Fixture | Rejects with | Error code |
|---|---|---|
| `phase85_jsonb_per_audience_fail_001_double_exposure.rs` | Storage field is `#[field(expose(public, admin, ...))]` AND `#[derived(name = metadata, scopes = [public], ...)]`. The `public` scope has both a column entry and a derived entry with the same `name`. | [E_DJG_VDF_002](./visage-derived-fields.md#error-taxonomy) (column-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_002_duplicate_derived_in_scope.rs` | Two `#[derived(name = metadata, scopes = [public], ...)]` entries — second one overwrites first. | [E_DJG_VDF_003](./visage-derived-fields.md#error-taxonomy) (derived-name collision in same scope) |
| `phase85_jsonb_per_audience_fail_003_uppercase_name.rs` | `#[derived(name = Metadata, ...)]` with uppercase byte. | [E_DJG_VDF_012](./visage-derived-fields.md#error-taxonomy) (uppercase byte in name) |
| `phase85_jsonb_per_audience_fail_004_aggregate_in_sql.rs` | `sql = "jsonb_agg(metadata)"` — aggregate keyword inside derived `sql` without the Shape V `aggregate = true` opt-in. | [E_DJG_VDF_009](./visage-derived-fields.md#error-taxonomy) (aggregate / window-function detection) |
| `phase85_jsonb_per_audience_fail_005_statement_separator.rs` | `sql = "metadata; DROP TABLE profiles"` — semicolon outside string literal. | [E_DJG_VDF_007](./visage-derived-fields.md#error-taxonomy) |

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
   call `assert_derived_parity` and assert `Ok(())`.
4. **`parity_helper_catches_storage_drift`.** Hand-construct a
   `ProfilePublic` with the same `data` but a populated `extra` map;
   call `assert_derived_parity`; assert
   `Err(DerivedParityError::Drift { field: "metadata", .. })`.
5. **`storage_field_still_supports_typed_path_filter`.** Build a
   `QuerySet<Profile>` that filters by
   `f.metadata().typed().display_name().eq("...")` — confirms the
   storage `JsonbSchema` typed-path surface is untouched by the
   per-audience projection work.

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
3. **The safety note.** Why `jsonb_build_object` is preferred over
   bare column reference; what `Jsonb::extra` does on the projected
   path; how the parity helper pins the absence of leaks.
4. Pointer to `docs/guide/derived-projections.md` for the derived-field
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
visage side has structurally empty `extra` because the SQL projection
ships only the narrow keys. The same `Jsonb<T>` type satisfies both
roles — the difference is entirely in which JSON bytes Postgres
delivers.

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
structurally empty because the SQL projection delivered only the
narrow keys; the wire output is identical to a freshly-constructed
`Jsonb::new(NarrowSchema { ... })`. No serde contract change is needed.

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
  carries the narrow shape (which may omit the sensitive key entirely,
  obviating the need for redaction on the narrow path).
- A `#[derived]`-projected `Jsonb<NarrowSchema>` is itself a typed
  projection; the narrow schema's individual keys can carry their own
  `#[validate]` / serde rename / nullability annotations
  independently. The narrow type's storage shape is whatever its serde
  `Serialize` / `Deserialize` impls produce.

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

This spec adds no new implementation. The implementation surface is
**exactly** the visage-derived-field machinery shipped under
djogi#231 (Phase 8.5).

When the orchestrator dispatches the implementer task for djogi#226,
the work breakdown is:

1. **Compile-pass fixtures.** Add the six fixtures listed in
   [§Compile-pass fixtures](#compile-pass-fixtures). They exercise the
   pattern against the live `#[derived]` parser, codegen, and trait
   constants without touching macro code.
2. **Compile-fail fixtures.** Add the five fixtures listed in
   [§Compile-fail fixtures](#compile-fail-fixtures). They re-assert
   existing E_DJG_VDF_* error coverage on the specific JSONB-shaped
   declarations; the `.stderr` snapshots inherit the same diagnostic
   shapes the derived-field implementation already emits.
3. **Integration tests.** Add the five tests listed in
   [§Integration tests](#integration-tests). They run against a real
   Postgres instance via `#[djogi::djogi_test(sync_models = [Profile])]`
   per the workspace pattern.
4. **User-guide section.** Edit `docs/guide/jsonb.md`,
   `docs/guide/derived-projections.md`, and `docs/guide/visages.md`
   per [§User-guide page](#user-guide-page).
5. **Decision-row entry.** Already added under this PR — see
   `docs/spec/decisions.md` "JSONB per-audience schema projection
   (djogi#226, Phase 8.5)".

The implementation issue closes when the fixture corpus is green, the
integration tests pass, the user-guide section ships, and the doc-gen
(`cargo doc --no-deps`) is clean.

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
