> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Architecture Principles

Djogi is a public, model-first, Postgres-native data framework.

Its architecture is guided by four principles:

1. Public requirements, private origins erased.
   Real applications can reveal missing capabilities, but Djogi documents them only as general systems requirements.
2. One job done well.
   Djogi owns the model-to-database derivation chain and closely adjacent tooling. It does not absorb product logic.
3. Strong primitives, thin policy.
   Djogi should provide reusable metadata, query, migration, and audit primitives. Business rules belong in application or companion crates.
4. Postgres-native correctness over generic abstraction.
   Djogi prefers explicit Postgres features and SQLx integration over lowest-common-denominator portability.

For the operational boundary, see [Scope & Boundaries](./scope.md).
