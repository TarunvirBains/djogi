> [Back to README](../../ReadMe.MD)

# Djogi Specification Index

## Core

- [Models & Field System](./models.md) — `#[derive(Model)]`, field types, annotations, dirty tracking
- [Query API](./queries.md) — QuerySet, conditions, programmatic filters, ConditionBuilder
- [JSONB Schema Fields](./jsonb.md) — `Jsonb<T>`, unknown field preservation, validation, subfield queries
- [Relations](./relations.md) — ForeignKey, ManyToMany, explicit through models
- [Primary Keys](./primary-keys.md) — HeeRanjId: HeerId (64-bit), RanjId (128-bit UUIDv8), generation patterns

## Infrastructure

- [Migrations](./migrations.md) — build-time drift detection, schema snapshots, differ
- [Logging](./logging.md) — three-database architecture, CRUD audit trail, event tracing
- [Configuration, CLI & Integration](./configuration.md) — `Djogi.toml`, `cargo djogi`, app registration, Axum integration

## Tools

- [Shell](./shell.md) — Rhai REPL, transactions, import/export, seed scripts
- [Admin Panel](./admin.md) — HTMX + Askama ModelForms, list views, validation, M2M inlines

## Reference

- [Research Areas](./research.md) — open implementation questions by subsystem
- [Resolved Design Decisions](./decisions.md) — full decision log with rationale

## Planning

- [Implementation Plan](./implementation-plan.md) — phased build sequence targeting production readiness
- [ORM Gap Analysis](./orm-gap-analysis.md) — Django 6.0 deep dive, functional gaps, and where Djogi can do better

## Background

- [The Agentic Shift](../hypothesis.md) — why Model-first frameworks exist
