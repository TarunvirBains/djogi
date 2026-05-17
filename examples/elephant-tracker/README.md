# elephant-tracker

A runnable example showcasing Djogi's core and esoteric features through a
plausible domain: tracking African elephant herds across borders.

This example is intentionally tightly scoped — seven models, one
binary — but it exercises a wide cross-section of the framework so
a learner can see real features in real combinations rather than
reading isolated snippets.

## What it demonstrates

| Feature                          | Where you see it                                       |
|----------------------------------|--------------------------------------------------------|
| `#[model]` macro                 | every file in `src/models/`                            |
| Foreign keys with `ForeignKey<T>`| `Elephant::herd_id`, `Sighting::elephant_id`           |
| M2M with explicit through model  | `Herd ↔ Country` via `HerdRange` (cross-border ranges) |
| `Jsonb<T>` typed JSONB           | `Elephant::tags`                                       |
| `GeoPoint` + EWKB                | `Sighting::location`                                   |
| Spatial cluster grouping (DBSCAN)| `cluster_sightings` demo                               |
| Full-text search (FTS)           | `Researcher::notes` and `Sighting::notes`              |
| Multi-edge self-FKs (pedigree)   | `Elephant::mother_id` + `father_id` — single-edge typed `tree_descendants` for matrilineal lineage; materialized `ElephantAncestry` closure (via `Model::materialize_closure`) for indexed Wright kinship lookup in the `mating-pairs` demo |
| Visages with side-query trait    | `HerdSummary` reports `herd_size` via aggregate, not row|
| Transactional outbox             | `Sighting::create` enqueues an event in `sightings_outbox` |
| RLS via `tenant_key`             | Researchers scoped per organisation                    |
| Optimistic locking (`version`)   | `Elephant::tags` updates                               |
| Tracked field changes            | `Elephant::name`, `Researcher::name` audit             |

## The domain

Seven models, organized to make cross-border movement, population
density, and pedigree-driven mating-pair selection the load-bearing
story:

- **Country** — reference table (Kenya, Tanzania, Uganda, Botswana,
  Zimbabwe). Serial PK, no audit trail.
- **Researcher** — per-organization. Scoped by `tenant_key = "org_id"`
  so each org sees only its own field staff and their notes.
- **Herd** — a named family group. M2M to `Country` through `HerdRange`
  with a `season: TEXT` payload, because herds genuinely cross borders
  seasonally.
- **HerdRange** — the through model. Holds the season payload and the
  composite uniqueness constraint.
- **Elephant** — individual elephants. `mother_id` + `father_id`,
  both `Option<ForeignKey<Self>>`, model biological pedigree (each
  individual has at most one of each, either potentially unknown).
  Matrilineal lineage walks `mother_id` only (single-edge
  `tree_descendants` / raw recursive CTE in the `lineage` demo);
  Wright kinship reads from a materialized `ElephantAncestry`
  closure (populated at seed time via
  `Model::materialize_closure::<ElephantAncestry>`) which the
  `mating-pairs` demo joins to itself on `ancestor_id` to find
  shared ancestors per candidate pair.
  `tags: Jsonb<ElephantTags>` for typed extra fields including sex.
  `version: i32` for optimistic locking on tag updates.
- **ElephantAncestry** — materialized transitive closure of the
  pedigree graph. `(elephant_id, ancestor_id, depth, path_count)`
  with the framework-injected `id` / `created_at` / `updated_at`.
  Populated post-seed (and re-runnable to refresh) by a single
  `Elephant::materialize_closure::<ElephantAncestry>(ctx, opts)`
  call; the helper walks both self-FK edges in one recursive CTE
  and upserts via `ON CONFLICT (...) DO UPDATE`. Indexed lookup at
  query time means every Wright F computation against this closure
  is a cheap join, not a re-walk of the recursive CTE per pair.
- **Sighting** — observation events. `location: GeoPoint`, FK to
  `Researcher` (`observed_by_id`), `notes: TEXT` (FTS-indexed). Records
  on `Sighting::create` enqueue a row into `sightings_outbox` inside the
  same transaction as the data write.

## Why this design

We considered a wider model graph (separate `Sanctuary`, `Patrol`,
`PoachingIncident`, etc.) but trimmed to seven (six core models +
`ElephantAncestry` as the materialized closure). The bar each model has
to clear: it must be the **simplest** model that demonstrates a
distinct framework feature. Adding more models past that just dilutes
attention.

Specifically:

- We kept `HerdRange` even though it's a bare-payload through model,
  because explicit-through M2M is a deliberate Djogi choice (no
  implicit M2M fields) and the cross-border story makes the season
  payload feel earned rather than synthetic.
- We split `Elephant.parent_id` into `mother_id` + `father_id`
  because biological pedigree has two edges, both potentially
  unknown, and elephant-research data captures matrilineal and
  patrilineal kinship distinctly. The split unlocks Wright
  kinship-coefficient calculation across the population: the
  framework's `Model::materialize_closure` helper walks both edges
  with path-multiplicity preservation in a single recursive CTE
  and writes the result into the `ElephantAncestry` table; the
  `mating-pairs` demo joins that closure to itself on `ancestor_id`
  for indexed shared-ancestor lookup per candidate pair. We kept
  the raw recursive-CTE form in the `lineage` demo for matrilineal
  descent because that path is naturally single-edge and the
  inline SQL is currently a documented raw-SQL escape hatch; it is not yet
  the baseline, and it should stay gated by the Phase 8.5 raw-SQL debt policy.
- We chose visages with a side-query trait (rather than embedding
  `herd_size` in `HerdRange`) because that's the realistic shape:
  aggregates that are too expensive to denormalize into rows but cheap
  to compute on demand belong in projections.

## Running it

You need a running Postgres 18 with the PostGIS 3.x extension installed
(Djogi targets PG 18+ exclusively). The connecting role does not need
to own the target database just to configure HeeRanjID node GUCs: the
migrate step uses HeeRanjID's no-`ALTER DATABASE` phase-zero bootstrap
SQL (`phase_zero_sql_without_database_guc()`) through `batch_execute`.
Each physical pool connection then sets the single-node example GUCs in
`post_connect`:

```sql
SET heer.node_id = '1';
SET heer.ranj_node_id = '1';
```

The role still needs whatever DDL/extension privileges your Postgres
installation requires for this example's schema bootstrap and PostGIS /
HeeRanjID installation, but database ownership is no longer part of the
GUC contract.

For deployments with multiple writers, register and provision each node in
`heer_nodes` first, then start each service with its selected `NODE_ID`
so its pool `post_connect` hook applies the matching `heer.node_id` and
`heer.ranj_node_id` values on every physical connection. Do not copy/paste
the hard-coded `1` assumption. See
https://github.com/TarunvirBains/heeranjid-sql/blob/main/README.md and
https://github.com/TarunvirBains/HeeRanjID/issues/49.

```bash
# 1. Postgres + PostGIS — for example via docker. We publish the port to
# `127.0.0.1` only so the weak local credentials cannot be reached from
# off-host (the bare `-p 5432:5432` shorthand would bind to `0.0.0.0`).
# djogi-allow-secret: local-dev example, intentionally weak.
docker run --rm -d --name elephant-pg \
  -e POSTGRES_PASSWORD=djogi -e POSTGRES_USER=djogi \
  -e POSTGRES_DB=djogi_test -p 127.0.0.1:5432:5432 \
  postgis/postgis:18-3.6

# 2. Apply schema (drop + recreate; idempotent).
export DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test
cargo run -p elephant-tracker -- migrate

# 3. Seed countries + 4 herds + 120 elephants + 200 sightings.
cargo run -p elephant-tracker -- seed

# 4. Try the demos — each accepts `--format` and `--out`.
cargo run -p elephant-tracker -- demo herd-summaries
cargo run -p elephant-tracker -- demo herd-summaries --format markdown

cargo run -p elephant-tracker -- demo cross-border-herds
cargo run -p elephant-tracker -- demo cross-border-herds --format mermaid
cargo run -p elephant-tracker -- demo cross-border-herds --format markdown

cargo run -p elephant-tracker -- demo lineage --matriarch Wema
cargo run -p elephant-tracker -- demo lineage --matriarch Wema --format mermaid
cargo run -p elephant-tracker -- demo lineage --matriarch Wema --format markdown

# Preferred typed-builder lineage mode. The CLI still exposes the
# legacy raw recursive-CTE path above as Phase 8.5 debt; use --typed
# to exercise `Elephant::objects().tree_descendants(...)` and compose
# --order=bfs|dfs for SEARCH BREADTH/DEPTH FIRST.
cargo run -p elephant-tracker -- demo lineage --matriarch Wema --typed
cargo run -p elephant-tracker -- demo lineage --matriarch Wema --typed --order bfs --format mermaid
cargo run -p elephant-tracker -- demo lineage --matriarch Wema --typed --order dfs --format markdown

cargo run -p elephant-tracker -- demo cluster-sightings
cargo run -p elephant-tracker -- demo cluster-sightings --format markdown

cargo run -p elephant-tracker -- demo mating-pairs
cargo run -p elephant-tracker -- demo mating-pairs --format markdown
cargo run -p elephant-tracker -- demo mating-pairs --format mermaid
```

Available format matrix:

| Demo                | json | mermaid | markdown |
|---------------------|:---:|:-------:|:--------:|
| `herd-summaries`    |  ✓  |   —     |    ✓     |
| `cross-border-herds`|  ✓  |   ✓     |    ✓     |
| `lineage`           |  ✓  |   ✓     |    ✓     |
| `cluster-sightings` |  ✓  |   —     |    ✓     |
| `mating-pairs`      |  ✓  |   ✓     |    ✓     |

## Status

This example is part of pre-v0.1.0 publish prep. The model definitions
target Djogi `0.1.0`. The example pre-dates the Phase 7 migration
runner integration that adopters will eventually use; the `migrate`
subcommand applies hand-written DDL via `ctx.raw_ddl` and
`ctx.raw_execute` rather than the descriptor-driven differ. This is
documented as current-state raw-SQL bypass debt and not a claim that
the typed escape surface is complete.

## Layout

```
elephant-tracker/
├── Cargo.toml
├── Djogi.toml                  # adopter config
├── README.md                   # this file
├── seeds/
│   └── countries.sql           # five countries, hand-written
└── src/
    ├── main.rs                 # CLI dispatch — migrate / seed / demo
    ├── migrate.rs              # drop+recreate tables, install HeeRanjID + PostGIS
    ├── seed.rs                 # programmatic seed wrapped in a single atomic()
    ├── output.rs               # Format enum + JSON / Mermaid / Markdown writers
    ├── models/
    │   ├── mod.rs              # re-exports + the `many_to_many!` invocation
    │   ├── country.rs          # Serial PK, lookup table
    │   ├── researcher.rs       # tenant_key = "org_id"; FTS on notes
    │   ├── herd.rs             # source side of the M2M
    │   ├── herd_range.rs       # explicit through model with season payload
    │   ├── elephant.rs         # self-FK lineage; Jsonb<ElephantTags>; version
    │   └── sighting.rs         # GeoPoint; events; FTS on notes
    ├── visages/
    │   ├── mod.rs
    │   └── herd_summary.rs     # hand-rolled visage + side-query trait
    └── demos/
        ├── mod.rs
        ├── cluster_sightings.rs   # cluster_by_proximity + centroid (typed)
        ├── cross_border_herds.rs  # M2M traversal + season filter
        ├── lineage.rs             # recursive-CTE escape hatch
        ├── herd_summaries.rs      # visage + side-query trait
        └── mating_pairs.rs        # Wright kinship + window-fn ranking (raw SQL override path; typed path is the preferred end state)
```
