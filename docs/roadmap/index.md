> [Back to README](../../ReadMe.MD) | [Shipped guides](../guide/index.md)

# Djogi Roadmap

These documents describe **planned** features — the APIs they reference do
not exist in the current release. They are committed now so the framework's
design target is visible, reviewable, and agent-readable, but none of the
code snippets here will compile today.

For features that currently ship, see [the user guides](../guide/index.md).

| Document | Phase | Covers |
|---|---|---|
| [Models (roadmap)](./models.md) | Phases 3–8 | Aspirational `#[model]` attributes, field types, relations |
| [Querying (roadmap)](./querying.md) | Phase 2 | QuerySet filter closures, terminal fetchers, programmatic filter API |
| [Security (roadmap)](./security.md) | Phase 5+ | Row-Level Security, `TenantScoped<T>`, `_insecurely()`, intent persistence |
| [CLI (roadmap)](./cli.md) | Phase 6–8 | `cargo djogi migrate / docs / check / analyze / stats / prepare` |

Each document is dated and revision-controlled; when a phase ships, the
corresponding roadmap doc merges into `docs/guide/` with phase-accurate
wording.
