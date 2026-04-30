# elephant-tracker

A runnable example showcasing Djogi's core and esoteric features through a
plausible domain: tracking African elephant herds across borders.

This example is intentionally tightly scoped — six models, one binary —
but it exercises a wide cross-section of the framework so a learner can
see real features in real combinations rather than reading isolated
snippets.

## What it demonstrates

| Feature                          | Where you see it                                       |
|----------------------------------|--------------------------------------------------------|
| `#[model]` macro                 | every file in `src/models/`                            |
| Foreign keys with `ForeignKey<T>`| `Elephant::herd`, `Sighting::elephant`                 |
| M2M with explicit through model  | `Herd ↔ Country` via `HerdRange` (cross-border ranges) |
| `Jsonb<T>` typed JSONB           | `Elephant::tags`                                       |
| `GeoPoint` + EWKB                | `Sighting::location`                                   |
| Spatial cluster grouping (DBSCAN)| `cluster_sightings` query in `src/main.rs`             |
| `within_km` / `order_by_distance`| `nearby_sightings` query                               |
| Full-text search (FTS)           | `Researcher` notes column                              |
| Self-referential FK (lineage)    | `Elephant::parent` — recursive-CTE escape hatch demo   |
| Visages with side-query trait    | `HerdSummary` reports `herd_size` via aggregate, not row|
| Transactional outbox             | `Sighting::create` enqueues `SightingRecorded` event   |
| RLS via `tenant_key`             | Researchers scoped per-organization                    |
| Optimistic locking (`version`)   | `Elephant::tags` updates                               |
| Tracked field changes            | `Elephant::name` audit                                 |

## The domain

Six models, organized to make cross-border movement and population
density the load-bearing story:

- **Country** — reference table (Kenya, Tanzania, Uganda, Botswana,
  Zimbabwe). Serial PK, no audit trail.
- **Researcher** — per-organization. Scoped by `tenant_key = "org_id"`
  so each org sees only its own field staff and their notes.
- **Herd** — a named family group. M2M to `Country` through `HerdRange`
  with a `season: TEXT` payload, because herds genuinely cross borders
  seasonally.
- **HerdRange** — the through model. Holds the season payload and the
  composite uniqueness constraint.
- **Elephant** — individual elephants. `parent: Option<ForeignKey<Self>>`
  for matriarchal lineage. `tags: Jsonb<ElephantTags>` for typed extra
  fields. `version: i32` for optimistic locking on tag updates.
- **Sighting** — observation events. `location: GeoPoint`, `observed_by:
  ForeignKey<Researcher>`, `notes: TEXT` (FTS-indexed). Records on
  `Sighting::create` enqueue a `SightingRecorded` outbox event.

## Why this design

We considered a wider model graph (separate `Sanctuary`, `Patrol`,
`PoachingIncident`, etc.) but trimmed to six. The bar each model has
to clear: it must be the **simplest** model that demonstrates a
distinct framework feature. Adding more models past that just dilutes
attention.

Specifically:

- We kept `HerdRange` even though it's a bare-payload through model,
  because explicit-through M2M is a deliberate Djogi choice (no
  implicit M2M fields) and the cross-border story makes the season
  payload feel earned rather than synthetic.
- We kept `Elephant::parent` as a self-FK because it gives us a place
  to demonstrate the raw recursive-CTE escape hatch — Djogi doesn't
  ship a tree-query API, and the example is honest about that.
- We chose visages with a side-query trait (rather than embedding
  `herd_size` in `HerdRange`) because that's the realistic shape:
  aggregates that are too expensive to denormalize into rows but cheap
  to compute on demand belong in projections.

## Running it

```bash
# 1. Bring up Postgres 18 (Djogi targets PG 18+ exclusively)
docker compose up -d postgres

# 2. Apply migrations
DATABASE_URL=postgres://djogi:djogi@localhost:5432/elephant_tracker \
  cargo run -p elephant-tracker -- migrate

# 3. Seed countries + a few herds + 200 sightings
cargo run -p elephant-tracker -- seed

# 4. Try the demos
cargo run -p elephant-tracker -- demo cluster-sightings
cargo run -p elephant-tracker -- demo cross-border-herds
cargo run -p elephant-tracker -- demo lineage --matriarch=Wema
cargo run -p elephant-tracker -- demo herd-summaries
```

## Status

This example is part of pre-v0.1.0 publish prep. The model definitions
target Djogi `0.1.0` and depend on the API surface as it stands after
the Stage 1 cluster simplify-with-review passes merge to `main`. Until
those PRs land, the example may not build against `main` — it builds
against the post-merge state.

## Layout

```
elephant-tracker/
├── Cargo.toml
├── Djogi.toml                  # adopter config (DATABASE_URL, log targets)
├── README.md                   # this file
├── migrations/                 # generated by `djogi compose`
├── seeds/
│   ├── countries.sql           # five countries, hand-written
│   └── herds_and_sightings.rs  # programmatic seed (uses GeoPoint, JSONB)
└── src/
    ├── main.rs                 # CLI dispatch — migrate / seed / demo subcommand
    ├── lib.rs                  # re-exports for tests
    ├── models/
    │   ├── mod.rs
    │   ├── country.rs          # serial PK, lookup table
    │   ├── researcher.rs       # tenant_key = "org_id"; FTS on notes
    │   ├── herd.rs             # M2M to Country via HerdRange
    │   ├── herd_range.rs       # explicit through model with season payload
    │   ├── elephant.rs         # self-FK lineage; Jsonb<ElephantTags>; version
    │   └── sighting.rs         # GeoPoint; outbox on create; FTS on notes
    ├── visages/
    │   ├── mod.rs
    │   └── herd_summary.rs     # side-query trait pattern for herd_size
    └── demos/
        ├── mod.rs
        ├── cluster_sightings.rs   # DBSCAN over GeoPoint column
        ├── cross_border_herds.rs  # M2M traversal + season filter
        ├── lineage.rs             # recursive-CTE escape hatch
        └── herd_summaries.rs      # visage + side-query trait
```
