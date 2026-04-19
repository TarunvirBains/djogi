> [Back to README](../../ReadMe.MD)

# Djogi Specification Index

## Core

- [Architecture Principles](./architecture-principles.md) — public requirements, single-responsibility, and framework boundaries
- [Models & Field System](./models.md) — `#[model]` attribute, field types, annotations, dirty tracking
- [Projections & Shared Contracts](./projections.md) — generated audience-specific transport types from one model definition
- [Query API](./queries.md) — QuerySet, conditions, programmatic filters, ConditionBuilder
- [JSONB Schema Fields](./jsonb.md) — `Jsonb<T>`, unknown field preservation, validation, subfield queries
- [Relations](./relations.md) — ForeignKey, ManyToMany, explicit through models
- [Primary Keys](./primary-keys.md) — HeeRanjId: HeerId (64-bit), RanjId (128-bit UUIDv8), generation patterns

## Infrastructure

- [Migrations](./migrations.md) — build-time drift detection, schema snapshots, differ
- [Protected Data Metadata & Field Codecs](./protected-data.md) — sensitive-field annotations, descriptor metadata, and storage transforms
- [Data Lifecycle & Governance](./data-lifecycle.md) — lifecycle classes, anonymize/archive/purge planning, legal holds
- [Logging](./logging.md) — three-database architecture, CRUD audit trail, event tracing
- [Distributed Topology & Residency](./topology.md) — read modes, placement metadata, and topology-aware migration guardrails
- [Configuration, CLI & Integration](./configuration.md) — `Djogi.toml`, `cargo djogi`, app registration, web framework integration (Axum opt-in via the `axum` feature flag)

## Tools

- [Shell](./shell.md) — Rhai REPL, transactions, import/export, seed scripts
- [Admin Panel](./admin.md) — HTMX + Askama ModelForms, list views, validation, M2M inlines

## Reference

- [Scope & Boundaries](./scope.md) — what belongs in Djogi vs an app or companion crate
- [Research Areas](./research.md) — open implementation questions by subsystem
- [Resolved Design Decisions](./decisions.md) — full decision log with rationale

## Planning

- [Implementation Plan](./implementation-plan.md) — phased build sequence targeting production readiness
- [ORM Gap Analysis](./orm-gap-analysis.md) — Django 6.0 deep dive, functional gaps, and where Djogi can do better

## Background

- [The Agentic Shift](../hypothesis.md) — why Model-first frameworks exist
