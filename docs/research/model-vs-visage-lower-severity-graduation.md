> [Back to README](../../ReadMe.MD) | [All Specs](../spec/index.md)

# Model-vs-visage adjacent surfaces — lower-severity graduation analysis

This note closes the analysis loop for [djogi#229][issue-229] — the
umbrella issue that catalogued seven lower-severity surfaces where the
model-vs-visage conflation pattern *might* exist on Djogi's public
API. Four HIGH-severity surfaces from the same pattern hunt earned
individual issues ([djogi#225][issue-225] `#[computed]` `expose=`
deprecation, [djogi#226][issue-226] `Jsonb<T>` per-audience schemas,
[djogi#227][issue-227] `#[field(protected(...))]` per-scope governance,
[djogi#228][issue-228] aggregate-annotation declaration-site
pre-lockdown). The remaining seven were batched for individual design
passes before any standalone issue was filed.

For each surface this note records:

- **What the umbrella claimed** — the gap as originally surfaced.
- **What the code actually does** — verified against the current
  `djogi-macros/` / `djogi/` tree at `phase85/visage-design-228-229`.
- **Decision** — one of: `phantom`, `accepted-design`,
  `file-standalone-issue`, `re-categorise-and-route`.
- **Rationale** — why this decision, applying the user-lens decision
  priorities (scalability > production stability > idiomatic Rust >
  simple to use) and the security-by-default invariant.
- **Anchor** — when the decision depends on a future phase or another
  issue, the named dependency.
- **Proposed issue title** — for surfaces that earn standalone
  tracking, the recommended title (the issue is *not* filed by this
  note; the umbrella owner files it).

The umbrella's surface numbering is preserved so cross-references
between the umbrella body and this note remain stable.

---

## Methodology

Every claim in this note that names a current code shape was verified
against the worktree at `phase85/visage-design-228-229` (branch on top
of `main` at commit `c8fe48b`, after the PR #234 merge). Verification means: `Grep` /
`Read` for the named symbol, file, or pattern; record the file:line.
The umbrella body referenced four research artefacts
(`model-vs-visage-pattern-definition.md`,
`codex-spark-adjacent-pattern-findings.md`,
`model-vs-visage-catalog-draft.md`,
`codex-spark-pattern-hunt-pass2.md`,
`haiku-pattern-hunt-pass2.md`) that do **not** exist in this worktree
— the umbrella's claims are treated as un-replicated until reproduced
against current code. This note flags every claim that did not
survive that reproduction step.

---

## Surface 1 — `#[field(generated = "...")]` per-scope projection

**Umbrella claim** (MEDIUM, Shape B). "Stored generated columns
(Postgres `GENERATED ... STORED`) are included uniformly in every
visage. No mechanism for 'admin sees `search_vector tsvector
GENERATED`, public visage omits it.'"

**Code reality.** Verified at
[`djogi-macros/src/model/visage_ctx.rs:70-109`][visage-ctx-classify].
`classify_field_for_scope` makes no special case for
`FieldAttrs::generated`. The function classifies a field's
participation in a scope purely via
`attrs.expose.{scalar_scopes, relation_scopes, suppressed}`. When
neither scalar nor relation scopes contain the current scope, the
return is `ScopeMembership::Absent` — i.e., the field is **not** in
that visage. A `#[field(generated = "<expr>")]` field without
`#[field(expose(...))]` is `Absent` from every visage by default.
A field that declares both — for example
`#[field(generated = "to_tsvector(...)", expose(admin))]` —
appears in `{Model}Admin` only.

This is consistent with the documented default at
[`docs/guide/visages.md:11-12`][guide-visages-default]: "Field
exposure is **opt-in**: fields without a `#[field(expose(...))]`
annotation do not appear in any transport visage."

**Decision — `phantom`.** The umbrella's premise is false. The
mechanism the umbrella asked for already exists in the form of
ordinary per-field `expose` annotations; generated columns participate
on equal footing with stored columns. Both attributes coexist on the
same field (`FieldAttrs` declares them as independent fields), and
the macro applies them independently.

**Rationale.** The "phantom" classification is the correct one because
re-deriving the surface (a separate
`#[field(generated, expose_generated = [...])]` axis) would *add*
declaration sites for a concern the existing surface already handles.
That would itself be a Shape B conflation in reverse — bundling the
storage shape (`generated`) with audience selection on a new
attribute slot when the existing `expose` slot already addresses it.

**Anchor.** None — no follow-up needed.

**Proposed issue title.** None.

---

## Surface 2 — `#[field(expose(...))]` per-field vs struct default

**Umbrella claim** (MEDIUM, Shape B). "Adding a new audience scope
requires touching every field's `#[field(expose)]` annotation. No
struct-level default scope set exists (e.g., `#[model(visage_default
= [public, admin])]` so fields inherit the default and only
deviations need per-field overrides)."

**Code reality.** Verified at
[`djogi-macros/src/model/visage_ctx.rs:70-109`][visage-ctx-classify]
and [`djogi-macros/src/model/attrs.rs` — `ExposeSpec` / `FieldAttrs::expose`][attrs-expose-fields].
There is no struct-level visage default; per-field `expose` is the
only declaration site. The default for a field without `expose` is
`Absent` (no visage).

**Decision — `accepted-design`.** The locked-design row added to
[`docs/spec/decisions.md`][decisions-doc] in the #229 design
lockdown commit codifies this. The decision is not "we agree the gap
exists but defer" — the decision is "the current per-field rule **is
the right design**; the proposed struct-level default would be a
regression in safety posture."

**Rationale.** A struct-level default would invert the
security-by-default direction. With the current per-field rule, a
developer who adds a new sensitive field (`password_hash`, `api_key`,
internal-only flag, business-secret pricing) must explicitly opt
**in** to each transport scope. With a struct-level default, the new
field would inherit the default scope set and ship into transport
visages unless the developer remembers to opt **out**. The forgetting
case is the dangerous one; the current design makes forgetting safe
(field omitted from transport) rather than dangerous (field leaked).
This matches Rust's per-item `pub` discipline and serde's per-field
`#[serde(rename = "...")]` pattern — both refuse a struct-level
default for the same reason. The `serde(rename_all = "camelCase")`
analogy that the umbrella implicitly invokes is a different concern:
camelCase is a transform of an *existing* visibility, not the
visibility itself. The analogy does not extend to exposure.

**Adopter-friction question, addressed honestly.** The umbrella
identified real adopter friction: when a model has many fields all
exposed to the same scope set, the per-field declarations are
repetitive. The decisions-log row leaves open one route that
preserves security-by-default: a *user-side* `pub use` of a
declarative macro like `djogi::expose_all_public_admin_export!`
applied per-field. Such a macro expands locally into per-field
annotations at parse time — the per-field decision token stays at
each field site, reviewers see it, and the umbrella's "ten fields
all exposed to the same scope set" pattern compresses to one
attribute-name per field instead of one parameter list per field.
This is incremental ergonomic work, not a redesign; it does not
require a struct-level default and does not relax the invariant.

**Anchor.** None — no follow-up needed.

**Proposed issue title.** None for the core rule. An optional future
ergonomic helper (the per-field expansion macro) belongs in adopter
feedback after Djogi has shipped, not in pre-publish lockdown.

---

## Surface 3 — Relation-form embedding `expose(scope -> Peer)` declaration site

**Umbrella claim** (MEDIUM, Shape A). "Embedding declaration ('in
this scope, replace the FK id with the peer's visage') lives on the
source model's relation field. Renaming the target's visage
(e.g., `User::public` → `User::summary`) forces touching every
source model that embeds it."

**Code reality.** Verified at [`docs/spec/visages.md:281-291`][visages-deferred-surface]
(Deferred Surface section). Visage names are derived mechanically
from `{Model}{Scope}` where `Scope ∈ {Public, SelfView, Admin,
Export}`. The four scopes are fixed; the names are not
user-renameable. Generated types `UserPublic` / `UserSelfView` /
`UserAdmin` / `UserExport` are the only names emitted, and the
macro asserts this set at [`djogi-macros/src/model/visages.rs:50-56`][visages-scopes-const].

The "rename `User::public` → `User::summary`" scenario assumed by
the umbrella is **not possible today** — visage renaming is one of
the items explicitly deferred at
[`docs/spec/visages.md:281-291`][visages-deferred-surface]:
"visage renaming rules beyond the default canonical names" is in
the "Deferred Surface" list. Until that feature lands, the
declaration churn the umbrella worries about cannot occur.

**Decision — `accepted-design (conditional)`.** The current
declaration site is sound as long as visage names remain
mechanically derived. When the deferred "custom visage names"
feature is taken up, the design must re-examine whether a
target-side `#[model(embeddable_as = [...])]` alias (or similar
indirection) is preferable to forcing the source-side declaration
to track every rename. That design decision is anchored to the
custom-visage-names feature spec.

**Rationale.** Filing a standalone issue today would track a churn
risk for a feature that does not exist. The umbrella's framing
implicitly assumed visage rename was a present-day concern; against
current code it is not. The proper artefact is a `TODO` inside the
deferred-feature spec, not a standalone tracking issue.

**Anchor.** `docs/spec/visages.md` "Deferred Surface" → "visage
renaming rules beyond the default canonical names." When that
deferred work is taken up, the spec that lands it must address the
embedding-site declaration-churn question. Adding a one-line
forward-reference at that anchor (below) makes the dependency
explicit.

**Proposed issue title.** None at this time. When the deferred
custom-visage-names work is filed as a standalone issue, it
must include "address relation-embedding declaration-site churn
under custom visage names" in its scope.

---

## Surface 4 — `proxy_for + default_filter` / `default_order`

**Umbrella claim** (MEDIUM, Shape A). "The
`#[model(proxy_for = Parent, default_filter = ...)]` attribute
bundles 'I am a proxy of Parent' (model-native) with 'every query
applies this filter' (queryset-level concern). Similarly for
`default_order`."

**Code reality.** The proxy surface **shipped in Phase 8β (v0.1.0)**.
`docs/guide/proxy.md` documents the full declaration shape and marks
the feature `Status: v0.1.0`. The parser lives at
`djogi-macros/src/model/attrs.rs`: `proxy_for` (bare-identifier form,
keyed at `path.is_ident("proxy_for")`), `default_order` (ordered tuple
list), and `default_filter` (closure predicate) all parse into
`ModelAttrs` and emit through the derive pipeline.

The **shipped shape** places identity (`proxy_for = Parent`) and query
defaults (`default_filter`, `default_order`) inside the same
`#[model(...)]` attribute — the arrangement the umbrella flagged as the
Shape A pattern in miniature. This shape was chosen during Phase 8β
implementation for adopter ergonomics: one attribute namespace to learn,
one declaration site for all proxy configuration.

**Decision — `post-shipment design question`.** The Shape A concern
(identity bundled with queryset-level semantics) is no longer
pre-implementation territory. The `#[model(proxy_for, default_filter,
default_order)]` surface is the accepted shipped v0.1.0 design. Any
reshape requires a deliberate migration story and a `docs/guide/proxy.md`
update. The candidate split paths remain:

1. **Identity** — `#[model(proxy_for = Parent)]` stays in the
   `#[model(...)]` namespace (model-level metadata, already the
   correct surface).
2. **Query semantics** — extracting `default_filter` and
   `default_order` into a companion surface:
   - **2a — Separate attribute:** `#[proxy(filter = "...", order = ["..."])]`,
     keeping the identity declaration uncluttered. Carries the same
     Shape A vs Shape B trade-off as the broader umbrella analysis.
   - **2b — Trait impl:** `impl ProxyDefaults for VehicleArchived`
     with typed `default_filter()` / `default_order()` methods.
     Matches the `ModelHooks` shape; typed closures compose with the
     queryset API without stringly-typed attribute parameters.

A standalone redesign issue may be filed when this reshape is taken up.
This note authorises the recommendation but does not schedule or promise
implementation.

**Anchor.** Proxy Models (shipped Phase 8β; `docs/guide/proxy.md`).
Any post-shipment declaration-site change must update the parser at
`djogi-macros/src/model/attrs.rs`, `docs/guide/proxy.md`, and the
relevant `#[derive(Model)]` emission paths.

**Proposed issue title.**

```
[design] proxy-model query defaults — post-shipment: split
#[model(proxy_for)] identity from default_filter / default_order
```

The issue body should document: (a) the shipped v0.1.0 shape, (b) the
Shape A concern from the original analysis, (c) candidate split shapes
2a/2b above, (d) that any reshape requires a migration story before
landing.

---

## Surface 5 — `{Model}_logs` per-audience access

**Umbrella claim** (MEDIUM, Shape B). "Audit log mirror tables are
generated unconditionally with no per-audience access gate. Admin
should read log tables; public should get 403. Today that's
adopter-side outside the framework."

**Code reality.** Verified at
[`docs/spec/logging.md`][logging-spec] and
[`docs/spec/decisions.md`][decisions-doc] (rows on CRUD log
architecture / log database lifecycle). CRUD-log mirror tables
(`{snake_case(model)}_logs`) live in a separate `myapp_crud_logs`
database; their per-audience access is **not** addressed in the
current logging spec. The umbrella's diagnosis of the gap is
correct on its face.

**Decision — `re-categorise-and-route`.** This surface is **not** a
model-vs-visage conflation. It is an access-control gap for
log-table reads. The umbrella batched it under the same banner
because the symptom looked similar (per-audience visibility on
descriptor-defined entities), but the proper categorisation is:

- **Model-vs-visage conflation** addresses *transport-shape*
  exposure: which fields appear in which audience's serialised
  output. The visage layer is a Rust struct generation concern.
- **Log-table access control** addresses *who is allowed to read
  the log table itself* — a runtime authorisation concern that
  belongs alongside the Phase 5.5 `AuthContext` + tenant-keyed RLS
  substrate, not alongside visage projection.

Conflating the two would re-introduce exactly the Shape A pattern
that djogi#225 was filed to dismantle — bundling transport-shape
projection with access-control gating in a single declaration
namespace.

**Rationale.** The correct surface is one of:

1. A new `LogsQuerySet<M>` entry point that requires an
   `AuthContext` carrying an admin-class scope, refuses by default,
   and propagates `AuthError::ScopeMissing { required }` otherwise.
   The descriptor declares which scope(s) gate access via
   `#[model(audit_log_read_scopes = [admin, internal])]` —
   distinct from the visage `expose(...)` namespace and addressed
   at the model-level, where the corresponding parent table's
   identity already lives.
2. A Phase 10 / Maahi-side access rule (the admin console is the
   primary consumer of audit-log reads), with the framework
   surfacing only the descriptor metadata for the admin console
   to read.

The umbrella's proposed `#[model(audit_log_scopes = [admin,
internal])]` syntax is roughly in the right shape; the
re-categorisation point is that the *issue* belongs in the
access-control / RBAC / Maahi domain rather than the visage
domain.

**Anchor.** Phase 5.5 (`AuthContext`, scope checks) for runtime
gating; Phase 10 / Maahi for admin-console consumption.

**Proposed issue title.**

```
[access-control] CRUD log mirror table per-audience access —
#[model(audit_log_read_scopes = [...])] descriptor + LogsQuerySet
runtime gate
```

The issue body should explicitly separate "this is NOT a visage
projection concern" — to head off a future contributor accidentally
re-bundling it with `#[field(expose(...))]`.

---

## Surface 6 — M2M through-model visage suppression

**Umbrella claim** (LOW, ergonomic). "Through-models are full
`#[derive(Model)]` structs, so they get four auto-generated visages
(`TaggedPostPublic`, `TaggedPostSelfView`, etc.) that adopters
almost never use. Pure namespace noise."

**Code reality.** Verified at
[`djogi-macros/src/model/visages.rs:50-86`][visages-scopes-const].
The macro iterates the four fixed scopes unconditionally and emits
one visage struct per `(Model, Scope)` pair for every model that
goes through `#[model]`, including M2M through-models. There is no
mechanism today to suppress visage emission per-model. Confirmed
phantom-suppression: for a through-model with no fields carrying
`#[field(expose(...))]`, the emitted visages still ship as types
in the user's crate — they are minimal (framework columns only:
`id`, `created_at`, `updated_at`) but they are present.

**Decision — `file-standalone-issue`** (LOW priority). A
`#[model(no_visages)]` opt-out attribute is genuinely simple to
add: the visage emitter checks the attribute and skips emission
when set. The umbrella's framing is correct that this is
"ergonomic," not "conflation" — namespace noise rather than
declaration-shape risk.

**Rationale.** The benefits are real but bounded:

- Through-model namespaces (`TaggedPost`, `UserTag`,
  `ProjectMember`) no longer carry four phantom types each.
- `rustdoc` for the user's crate is cleaner.
- `serde` derives compile slightly faster (one fewer struct ×
  three derives × N through-models).

The costs are also bounded: the attribute adds another
`#[model(...)]` parameter slot, increasing the per-model
declaration surface by one (low-frequency) opt-out. The visage
emitter gains one early-return branch.

The case for filing a standalone issue rather than locking it
immediately is that **adopter friction is the right driver**. If
adopters reach Phase 8.5 / v0.1.0 publish and report that
through-model namespace noise is a real issue, the attribute lands
quickly. If nobody reports it, the attribute remains
hypothetical-friction work and stays unfiled. The umbrella is the
correct artefact today; a standalone issue should be filed when
adopter feedback supports it.

**Anchor.** Post-v0.1.0 adopter feedback. Or earlier if any M2M-heavy adopter reports through-model visage emissions as an active nuisance.

**Proposed issue title.**

```
[ergonomic] #[model(no_visages)] opt-out for through-models and
other non-transport-projected models
```

Suggested-but-not-filed; orchestrator may defer pending adopter
feedback.

---

## Surface 7 — Tree queries + visage projection

**Umbrella claim** (LOW, ergonomic). "`tree_descendants` and
`tree_ancestors` return `QuerySet<Self>` (model type, not
visage-projected). Adopters who want
`tree_descendants_as_visage::<ScopePublic>(...)` must project
manually. Not a declaration-site conflation; it's a missing typed
projection terminal."

**Code reality.** Verified at
[`docs/spec/positioning.md:42`][positioning-tree] and the
reserved-identifier entry at
[`docs/spec/reserved-identifiers.md:27`][reserved-tree-ident].
`tree_descendants` / `tree_ancestors` return `QuerySet<Self>`.
The Phase 7-Zero-2 `VisageQuerySet<V>` infrastructure exists for
first-class visage queries (`Model::visage_query::<V>()` and
similar), but there is no terminal on `QuerySet<T>` to project
into a `VisageQuerySet<V>` with SELECT narrowing. The asymmetry
the umbrella names is real and uniform — it applies to every
operation that produces a `QuerySet<T>`, not just tree queries.

**Decision — `file-standalone-issue`** (LOW priority, queryset
scope). The right shape is a general queryset-level
`.as_visage::<V>()` terminal that narrows SELECT and produces a
`VisageQuerySet<V>`. The tree-queries case is one consumer of
that uniform terminal, not the load-bearing motivation.

**Rationale.** Filing a tree-specific terminal
(`tree_descendants_as_visage::<V>`) would be the wrong shape:

- It adds one new terminal per `QuerySet` producer (tree, raw
  queryset, filtered queryset, JSON-path queryset, …). The
  combinatorial explosion is the same one the umbrella correctly
  identified as "asymmetric across queryset operations."
- It does not compose: an adopter chaining
  `.filter(...).tree_descendants(...)` would lose the ability to
  reach `VisageQuerySet<V>` from anywhere in the chain.

The correct shape is a single terminal on `QuerySet<T>`:

```rust
impl<T: Model> QuerySet<T> {
    pub fn as_visage<V: Visage<Source = T>>(self) -> VisageQuerySet<V>
}
```

`V: Visage<Source = T>` ensures that the visage projects from
the queryset's source model — a typed boundary that the macro
generates per `(Model, Scope)` pair. This composes with every
producer of `QuerySet<T>`: `filter`, `order_by`,
`tree_descendants`, `select_for_update`, `raw_filter`, all of
them.

**Anchor.** Queryset / visage cluster, post-v0.1.0. Lower priority
than Surface 4 (which is a post-shipment design question) and
Surface 5 (which is access-control).

**Proposed issue title.**

```
[queryset] .as_visage::<V>() terminal — uniform conversion from
QuerySet<Model> to VisageQuerySet<V> with SELECT narrowing
```

Suggested-but-not-filed; orchestrator may defer pending
adopter feedback or sequence relative to other queryset
extensions.

---

## Summary table

| # | Surface | Decision | Standalone issue? | Anchor |
|---|---|---|---|---|
| 1 | `#[field(generated)]` per-scope projection | `phantom` | No — mechanism exists | n/a |
| 2 | Per-field `expose` vs struct default | `accepted-design` (decisions.md row added) | No — per-field is security-by-default | n/a |
| 3 | Relation-form embedding declaration site | `accepted-design` (conditional) | No — depends on deferred custom-visage-names feature | `docs/spec/visages.md` Deferred Surface |
| 4 | `proxy_for + default_filter / default_order` | `post-shipment design question` | **Yes — deferable, migration required** | Phase 8β (shipped; `docs/guide/proxy.md`) |
| 5 | `{Model}_logs` per-audience access | `re-categorise-and-route` | **Yes — but in access-control domain, not visage** | Phase 5.5 / Phase 10 / Maahi |
| 6 | M2M through-model visage suppression | `file-standalone-issue` (low priority) | **Yes — deferable** | Post-v0.1.0 adopter feedback |
| 7 | Tree queries + visage projection | `file-standalone-issue` (low priority) | **Yes — deferable, queryset-general** | Post-v0.1.0 queryset cluster |

---

## Proposed follow-up issues (titles only)

These issues are **not filed by this note**. The umbrella owner
(orchestrator) reviews this analysis and files them at the
appropriate time, with the body content derived from each surface's
section above.

1. `[design] proxy-model query defaults — post-shipment: split
   #[model(proxy_for)] identity from default_filter / default_order`
   (shipped Phase 8β; file when reshape is taken up; requires
   migration story before landing).
2. `[access-control] CRUD log mirror table per-audience access —
   #[model(audit_log_read_scopes = [...])] descriptor +
   LogsQuerySet runtime gate` (Phase 5.5 access-control domain,
   not visage; file before Maahi audit-log surfaces design).
3. `[ergonomic] #[model(no_visages)] opt-out for through-models and
   other non-transport-projected models` (low priority,
   adopter-feedback-driven; defer until reported).
4. `[queryset] .as_visage::<V>() terminal — uniform conversion from
   QuerySet<Model> to VisageQuerySet<V> with SELECT narrowing`
   (low priority, queryset cluster; sequence with other queryset
   extensions).

---

## What this note locks now

The two rows added to [`docs/spec/decisions.md`][decisions-doc] in
the same Phase 8.5 design-lockdown cluster as djogi#228:

1. **Aggregate annotation declaration site lockdown** (djogi#228)
   — covers the visage-derived-fields Shape A risk for the future
   aggregate / window-function surface.
2. **Visage exposure default — per-field opt-in, no struct-level
   default** (djogi#229 surface 2) — locks the
   security-by-default invariant against future drift.

Surfaces 1, 3, 6, and 7 do not earn decisions.md rows: surface 1 is
phantom (no decision to record), surface 3 is conditional on
deferred work (the dependency is the artefact), surfaces 6 and 7
are deferable ergonomic / queryset items where the right
documentation is the proposed-issue title, not a pre-emptive
lock.

Surfaces 4 and 5 carry standalone-issue recommendations rather
than decisions.md rows because the design choice has not yet been
made — the recommended issues are the artefact that *records the
choice* when the work is taken up.

---

## Closing the umbrella

When all four proposed issues above are either filed or explicitly
deferred (with the deferral documented in the issue tracker or
this note), djogi#229 can close. The closing condition stated in
the umbrella body — "Each of the 7 surfaces has a graduation
decision logged in the body comments below" — is satisfied by this
note's existence; the umbrella body can link to this note and
close.

---

[issue-225]: https://github.com/TarunvirBains/djogi/issues/225
[issue-226]: https://github.com/TarunvirBains/djogi/issues/226
[issue-227]: https://github.com/TarunvirBains/djogi/issues/227
[issue-228]: https://github.com/TarunvirBains/djogi/issues/228
[issue-229]: https://github.com/TarunvirBains/djogi/issues/229
[visage-ctx-classify]: ../../djogi-macros/src/model/visage_ctx.rs
[visages-scopes-const]: ../../djogi-macros/src/model/visages.rs
[attrs-expose-fields]: ../../djogi-macros/src/model/attrs.rs
[guide-visages-default]: ../guide/visages.md
[visages-deferred-surface]: ../spec/visages.md
[impl-plan-8c]: ../spec/implementation-plan.md
[logging-spec]: ../spec/logging.md
[decisions-doc]: ../spec/decisions.md
[positioning-tree]: ../spec/positioning.md
[reserved-tree-ident]: ../spec/reserved-identifiers.md
