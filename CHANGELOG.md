# Changelog

All notable changes to this project will be documented in this
file. The format follows [Keep a Changelog][kac] loosely; once
v1.0 ships, we'll commit to strict semver.

[kac]: https://keepachangelog.com/en/1.1.0/

## [Unreleased]

### Pre-v0.1.0 publish-readiness work in progress

- Cargo.toml publish fields populated across all four publishable
  crates
- Repo hygiene files added (CONTRIBUTING, CODE_OF_CONDUCT,
  SECURITY, issue + PR templates, this changelog)
- Retro `/simplify-with-review` passes underway on Phases 1, 2,
  4 (the no-Phase-7-collision clusters not covered by the prior
  pass)
- User-facing docs audit underway across `ReadMe.MD`,
  `docs/spec/`, `docs/guide/`, `docs/roadmap/`,
  `docs/hypothesis.md`

## [0.1.0] — TBD

Initial public preview. The framework is feature-complete through
**Phase 7.5** of the implementation plan:

### Phases shipped

- **Phase 1 + 1.5** — Core Model trait, `#[derive(Model)]`,
  field injection, framework columns, descriptor + inventory
  registration.
- **Phase 2** — `QuerySet`, condition tree, `ConditionBuilder`
  with positional `$n` parameter emission.
- **Phase 3** — Relations: ForeignKey, OneToOne, explicit-through
  M2M, eager loading via `prefetch` / `select_related`.
- **Phase 4 + 4.5** — Transactions, expression IR, outbox writes,
  row locks, bulk writes; visage-typed projections, `expose(...)`
  grammar, peer `TryFrom` blanket.
- **Phase 5-Zero** — sqlx retired in favor of `tokio-postgres` +
  `deadpool-postgres`; `#[djogi_test]` harness.
- **Phase 5 + 5.5** — Tracked, version field, DjogiEnum,
  Jsonb<T>, arrays, RLS tenant_key, outbox worker, FTS;
  DjogiAuth + AuthContext + PasswordHash + auto-set_tenant +
  nested-atomic snapshot.
- **Phase 6 + 6.5** — Spatial: GeoPoint + EWKB codec +
  within_km / order_by_distance + IndexSpec policy fields;
  grouped aggregation type-state; non-point geometries (LineString,
  Polygon, MultiPoint, MultiLineString, MultiPolygon) + shape
  predicates.
- **Phase 7-Zero** — IndexSpec v3 + apps subsystem
  (`djogi::apps!`, AppRegistry, lifecycle markers, AppDiagnostic).
- **Phase 7-Zero-2** — PrimaryKey trait split + custom-PK macro
  + visage query surface (SELECT narrowing, `DjogiVisageOf<M>`
  boundary).
- **Phase 7** — Descriptor-driven migrations: full CLI
  (`compose` / `status` / `attune` / `db reset` / `db seed` /
  `docs`), T9 PK-flip, T10 sync_models.
- **Phase 7.5** — Live migrations substrate
  (`OnlineSafetyClassification`), protected-data field
  attributes, T11 bug-fix sprint, T12 integration tests,
  EXCLUSION + stored-generated descriptor extension.

### Out of scope at v0.1.0

- Phase 8 onward: hooks/composition/proxy, shell + admin polish,
  observability + scaling, distributed topology + residency
- Catalog drift detection (T13) — deferred from Phase 7.5
- The `djogi-maahi` admin console — separate carve-out crate;
  not yet stable enough for v0.1.0 inclusion
