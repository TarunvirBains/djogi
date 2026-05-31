> [Back to README](../../ReadMe.MD) | [Shipped guides](../guide/index.md)

# Djogi Roadmap

> **Status as of v0.1.0:** the bulk of the original roadmap has shipped.
> The documents in this directory remain as design history — they capture
> the framework's planning target before each subsystem landed. For the
> live, authoritative API surface always go to the corresponding **user
> guide** under [`docs/guide/`](../guide/index.md). The snippets in
> roadmap docs may not compile against today's framework.

## What shipped, where it lives now

| Roadmap doc | Status | Authoritative guide |
|---|---|---|
| [Models](./models.md) | Shipped (Phases 3–7.5) | [Models](../guide/models.md), [Relations](../guide/relations.md), [Spatial](../guide/spatial.md), [JSONB](../guide/jsonb.md) |
| [Querying](./querying.md) | Shipped (Phase 2 + 4 + 6.5) | [Queries](../guide/queries.md), [Expressions](../guide/expressions.md), [Aggregation](../guide/query-aggregation.md) |
| [Security](./security.md) | Shipped (Phases 5 + 5.5) | [Auth](../guide/auth.md), [Tenancy](../guide/tenancy.md) |
| [CLI](./cli.md) | Shipped (Phase 7) — except the `rollback` CLI dispatcher is deferred; `apply --fake` ships as a flag on the apply command and `baseline` ships as `djogi migrations baseline` | [Migrations](../guide/migrations.md) |
| [Future work](./future-work.md) | Mixed — some items shipped, some still future | Cross-reference per item; this doc still useful for not-yet-shipped scope expansions |

## Why the roadmap docs are kept

Each document captured the framework's design intent at a specific
point in time. Reading them alongside the corresponding guide is a
fast way to see *why* a subsystem looks the way it does — what the
target shape was before implementation, what got compromised, what
got expanded. They also serve as a historical record for the next
round of API discussions.

When a follow-up phase ships scope that the roadmap document
explicitly anticipated (e.g. the Phase 7 follow-up CLI dispatchers),
the corresponding bullet here will move from the "Status" column to
the authoritative guide.
