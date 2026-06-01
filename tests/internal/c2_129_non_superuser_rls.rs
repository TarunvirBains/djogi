// Cluster 2 issue #129 — non-superuser test pool for RLS-backed
// integration tests.
//
// Internal wrapper. The body issues raw DDL for `ALTER TABLE ... ENABLE /
// FORCE ROW LEVEL SECURITY` and `CREATE POLICY`, which are schema-fixture
// operations with no ordinary typed Djogi surface. Keep this target under
// `tests/internal` so adopter-shaped integration tests remain raw-free.

#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#129): non-superuser RLS test needs raw DDL for
// ALTER TABLE ... ENABLE/FORCE ROW LEVEL SECURITY and CREATE POLICY; this is
// framework-owned policy setup, not ordinary adopter-shaped integration code.
mod c2_129_non_superuser_rls {
    include!("sources/c2_129_non_superuser_rls.rs");
}
