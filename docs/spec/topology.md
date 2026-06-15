> [Back to README](../../README.md) | [All Specs](./index.md)

# Distributed Topology & Residency

Djogi remains Postgres-first and single-cluster-friendly, but some applications need descriptor-aware support for replicas, residency constraints, and shard-sensitive schema validation.

This spec defines the framework boundary for those needs.

---

## Goals

- expose explicit read-consistency modes
- carry placement metadata in descriptors
- validate topology-sensitive schema decisions at migration time
- keep deployment-specific routing implementations outside Djogi core

---

## Minimal Public Surface

Phase 12 should stabilize only these public contracts:

Read-mode API:

```rust
ReadMode::PrimaryOnly
ReadMode::ReplicaAllowed
ReadMode::ReadYourWrites
ReadMode::StaleOk
```

Descriptor metadata:

```rust
#[model(shard_key = "account_id")]
#[model(residency = "regional")]
#[field(placement_scope = "local")]
```

Migration/tooling expectations:

- topology-sensitive schema edits are flagged before apply
- unsafe cross-placement foreign keys are rejected or require explicit override
- repartition/cutover tooling can inspect placement metadata

Djogi does not need to standardize a deployment router to justify this surface.

---

## Read Consistency Modes

Djogi should support explicit read modes such as:

- `primary_only`
- `replica_allowed`
- `read_your_writes`
- `stale_ok`

These are runtime/query semantics, not HTTP semantics, so they belong below the web-framework layer (Axum under the `axum` feature, or any other Rust web framework the adopter wires in).

Djogi owns the execution contract: `QuerySet::with_read_mode(ReadMode::ReplicaAllowed)` threads the hint into the pool-selection strategy, and the pool strategy configured by the application decides which connection honors it. Djogi does not ship a router for a specific topology — it defines the hint that any router must respect.

---

## Placement Metadata

Models may need descriptor metadata for:

- shard key
- residency class
- placement scope
- partition strategy
- relation placement constraints

The framework does not need to own a full shard router to justify this metadata. The migration and query layers still need a shared understanding of placement semantics.

---

## Migration Guardrails

Once placement metadata exists, migration tooling must understand it.

Examples of topology-sensitive checks:

- shard-key changes
- residency-class changes
- unsafe cross-placement foreign keys
- repartition/cutover operations that require explicit review

These are migration-engine concerns, so they belong in Djogi rather than in ad hoc operator docs.

---

## Boundary

Djogi does **not** own:

- cloud topology selection
- geo-routing
- service-mesh policy
- app-specific shard routers
- deployment-specific replication orchestration

Djogi does own:

- metadata
- validation
- runtime consistency contracts
- operator-facing safety checks

---

## Dependency Chain

This phase assumes earlier maturity in:

- migrations
- observability
- partition analysis/repartition workflows
- protected metadata where residency or sensitivity overlap

Distributed-topology features should come after local single-cluster correctness is already strong.
