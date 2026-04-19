> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Scope & Boundaries

Djogi is a public framework. Its requirements may be informed by real applications, but the repository must describe them in public, product-agnostic terms.

This document defines:

- how application-derived requirements are translated into Djogi specs
- what belongs in Djogi itself
- what should live in a separate crate owned by an application or ecosystem layer

---

## Public Translation Rule

When a requirement originates from a private or product-specific application, Djogi documents it only as a general capability requirement.

Acceptable framing:

- "multi-tenant applications need row-level tenant scoping"
- "high-write systems need transactional outbox support"
- "real-time products need append-only event logging and audit trails"
- "global applications may need read replicas, partitioning, and shard-aware metadata"

Unacceptable framing:

- naming or implying a private product, customer, roadmap, market, or internal architecture
- documenting a feature solely because one private app wants it
- embedding app-specific workflows, domain entities, or UI assumptions into framework contracts

The standard is simple:

- Djogi may be informed by private use cases
- Djogi must justify itself in public, reusable systems language

---

## Inclusion Test

A feature belongs in Djogi only if all of the following are true:

1. It strengthens the model-to-database derivation chain, query/runtime layer, migration system, or framework-owned tooling.
2. It is reusable across multiple applications in substantially the same form.
3. It can be expressed without leaking domain-specific concepts.
4. It composes cleanly with SQLx, Postgres, and any Rust web framework (Axum is the most-supported integration, opt-in via the `axum` feature flag) rather than re-owning their jobs.
5. It would be awkward, repetitive, or error-prone for every app to reimplement separately.

If a proposal fails any of those tests, it should default to an application crate or a separate companion crate.

---

## What Djogi Owns

Djogi should own features that are central to its core promise: define models once, derive the durable data layer correctly.

This includes:

- model macros, descriptors, field metadata, and relation metadata
- typed CRUD/query APIs over Postgres
- migration differ/generation/apply workflows
- typed Postgres-native field support such as JSONB, arrays, enums, spatial metadata, and advanced indexes
- transaction helpers, expressions, bulk operations, and concurrency-safe data primitives
- framework-owned shell/admin/audit tooling derived from `ModelDescriptor`
- generic observability hooks tied directly to ORM/runtime behavior

These are framework concerns because they are part of the derivation chain or are generated mechanically from it.

---

## What Djogi Does Not Own

Djogi should not absorb product logic, workflow policy, or vertical features that happen to touch the database.

This excludes:

- domain-specific decision logic, scoring formulas, policy engines, or commercial rules
- domain-specific event taxonomies
- business-specific admin screens, dashboards, or support flows
- application workflow coordination
- product-specific background jobs
- bespoke cache shapes or API payload contracts
- frontend behavior outside the generic admin/shell surface

Those belong in:

- the application crate, when they are tightly coupled to one product
- a separate companion crate, when they are reusable but not core to Djogi's model/query/migration contract

---

## Companion Crate Heuristic

Use a separate crate instead of Djogi when the feature is one of these:

- a policy layer on top of Djogi primitives
- a domain module with its own nouns and invariants
- an integration with a third-party service or deployment stack
- an operational subsystem that only some apps need
- a specialized extension whose API would clutter the core framework

Examples:

- tenant-policy enforcement helpers beyond generic descriptor and query hooks
- domain event publishers/consumers for a specific workflow
- domain scoring, recommendation, policy, or adjudication engines
- product-specific admin actions
- shard-routing implementations for one deployment topology

Djogi may expose the primitives those crates need, but should not absorb the policy crate itself.

---

## Boundary Examples

Some features sit on the boundary. Use this split:

Keep in Djogi:

- generic metadata needed for future correctness, such as `tenant_key`, `partition_by`, `has_outbox`, or index specs
- generic runtime hooks that let applications implement policy safely
- generic audit/event plumbing where the framework can derive behavior mechanically

Move out of Djogi:

- policy decisions about who can access what
- product-specific outbox consumers
- application-defined event names and business workflows
- tenant routing strategies tied to one deployment layout

The rule is "core primitive in Djogi, business policy outside Djogi."

---

## Spec Writing Rule

When adding or revising docs in this repository:

- describe the motivating pressure in generic systems terms
- avoid references to private applications or their roadmaps
- state why the feature belongs in a public ORM/data framework
- name the likely companion-crate boundary if the feature is near the edge

Every substantial new feature proposal should answer two questions:

1. Why is this a framework primitive rather than app logic?
2. If it is not core, what would the companion crate boundary be?

---

## Review Checklist

Before accepting a new Djogi feature, verify:

- the feature is product-agnostic in naming and examples
- the value survives even if the originating private app disappears
- the API surface improves the derivation chain instead of diluting it
- the feature does not smuggle business logic into core types
- a separate crate would be worse for correctness or ergonomics

If those are not clearly true, do not put it in Djogi.
