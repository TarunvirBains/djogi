> [Back to README](../../README.md) | [All Specs](./index.md)

# Architecture Principles

Djogi is a public, model-first, Postgres-native data framework, not a full application framework.

Its architecture is guided by six principles:

1. Public requirements, private origins erased.
 Real applications can reveal missing capabilities, but Djogi documents them only as general systems requirements.
2. One job done well.
 Djogi owns the model-to-database derivation chain and closely adjacent tooling. It does not absorb product logic, request handling, or application-shell concerns.
3. Strong primitives, thin policy.
 Djogi should provide reusable metadata, query, migration, and audit primitives. Business rules belong in application or companion crates.
4. Postgres-native correctness over generic abstraction.
 Djogi prefers explicit Postgres features and `tokio-postgres` integration over lowest-common-denominator portability.
5. Performance-safe abstractions over ORM convenience.
 Djogi must not force slower database shapes for common production workloads. Set-based writes, explicit eager loading, row locks, typed visages, and Postgres-native query forms belong in the framework when they are needed to keep the efficient path available without dropping to raw SQL.
6. Performance-safe defaults over hidden behavior.
 No hidden lazy loads, no implicit row-by-row fallback where a set-based form is expected, and no abstraction that obscures query count, lock semantics, or write amplification. If the framework does extra work, that work must be explicit.

The design target is not "pleasant CRUD," and it is not "Rust Django." It is a model-first Postgres runtime that can stand alongside the popular Rust ORM alternatives for high-read and high-write production systems.

For the operational boundary, see [Scope & Boundaries](./scope.md).
