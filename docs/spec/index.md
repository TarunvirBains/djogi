> [Back to README](../../ReadMe.MD)

# Djogi Specification Index

These documents define Djogi's public contract as a performance-sensitive, Postgres-native data runtime. The spec is not only about API shape; it also defines which query, write, migration, and observability primitives must exist so common production workloads remain efficient in-framework.

## Core

- [Architecture Principles](./architecture-principles.md) — public requirements, single-responsibility, and framework boundaries
- [Models & Field System](./models.md) — `#[model]` attribute, field types, annotations, dirty tracking
- [Visages & Shared Contracts](./visages.md) — generated audience-specific transport types from one model definition
- [Query API](./queries.md) — QuerySet, conditions, programmatic filters, ConditionBuilder
- [JSONB Schema Fields](./jsonb.md) — `Jsonb<T>`, unknown field preservation, validation, subfield queries
- [Relations](./relations.md) — ForeignKey, ManyToMany, explicit through models
- [Primary Keys](./primary-keys.md) — HeeRanjId: HeerId (64-bit), RanjId (128-bit UUIDv8), generation patterns
- [Indexing](./indexing.md) — `#[model(indexes(...))]` grammar, unique-constraint vs unique-index lowering, the `concurrently = true` contract, descending PK variants

## Infrastructure

- [Migrations](./migrations.md) — build-time drift detection, schema snapshots, differ
- [Apps & Database Domains](./apps-and-database-domains.md) — app ownership, database-target subscription, and same-target relation boundaries
- [Protected Data Metadata & Field Codecs](./protected-data.md) — sensitive-field annotations, descriptor metadata, and storage transforms
- [Data Lifecycle & Governance](./data-lifecycle.md) — lifecycle classes, anonymize/archive/purge planning, legal holds
- [Logging](./logging.md) — three-database architecture, CRUD audit trail, event tracing
- [Distributed Topology & Residency](./topology.md) — read modes, placement metadata, and topology-aware migration guardrails
- [Configuration, CLI & Integration](./configuration.md) — `Djogi.toml`, `djogi`, app registration, and web framework integration (`axum` as the concrete example)

## Tools

- [Shell](./shell.md) — Rhai REPL, transactions, import/export, seed scripts
- [Maahi (Admin Console)](./maahi/index.md) — Dioxus full-stack admin with visage-driven RBAC, multi-tenancy, six-action permissions, M2M inlines, and the inline-bulk approval threshold

## Testing

- [Testing Conventions](./testing.md) — typed-surface rule, self-referential seed convention, silent nullable-FK degradation, and the raw SQL bypass harness pointer

## Reference

- [Positioning — Djogi in the Rust Data Tier](./positioning.md) — how Djogi sits relative to Cot, SeaORM, and Diesel across the Rust-first / Postgres-first design axes
- [Scope & Boundaries](./scope.md) — what belongs in Djogi vs an app or companion crate
- [Reserved Identifier Namespace](./reserved-identifiers.md) — the `__djogi_*` prefix, what lives in it, and where it's enforced
- [Research Areas](./research.md) — open implementation questions by subsystem
- [Resolved Design Decisions](./decisions.md) — full decision log with rationale

## Planning

- [Implementation Plan](./implementation-plan.md) — phased build sequence targeting production readiness without ORM-induced performance regressions
- [Adoption Readiness](./adoption-readiness.md) — per-pattern map of the earliest phase at which each production-app use case is safe to depend on
- [ORM Gap Analysis](./orm-gap-analysis.md) — Django 6.0 deep dive, functional gaps, and where Djogi can do better

## Background

- [The Agentic Shift](../hypothesis.md) — why Model-first frameworks exist
