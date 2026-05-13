//! Cluster 8δ T7.1 — `djogi::cache` re-export module tests.
//!
//! Pins the surface at the type level: every test compiles a value of
//! the re-exported type to prove the symbol is reachable through
//! `djogi::cache::*` without an explicit `sassi` dep on the consumer
//! side. The auto-emitted `Cacheable` impl from `#[derive(Model)]`
//! (T7.2) does NOT exist yet in this commit, so the tests hand-roll a
//! minimal `Cacheable` impl on a tiny test type instead of leaning on
//! `#[model]`.
//!
//! Spec anchor: `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 commit T7.1 — "Test names + assertions" bullet.
//!
//! # Why these tests live in `djogi/tests/`
//!
//! Per the cluster plan, the surface check is an integration-level
//! consumer test: it verifies that someone writing
//! `use djogi::cache::*;` (and only `djogi` in their `Cargo.toml`)
//! can actually reach `Punnu`, `BasicPredicate`, etc. Putting it in
//! the `djogi` crate's `tests/` directory lets cargo build the test
//! binary against the public `djogi` crate API exactly as an adopter
//! would. The lihaaf compile-pass fixture in
//! `djogi-macros/tests/compile_pass` covers the same surface from the
//! macro-consumer angle (a model definition that imports through
//! `djogi::cache::*`).

use djogi::cache::{BasicPredicate, Cacheable, Punnu};

/// Minimal `Cacheable` implementor for the surface tests below.
///
/// Hand-rolled because T7.2 (auto-emit `Cacheable` from
/// `#[derive(Model)]`) lands in the next commit. The fields are
/// trivial — `id` only — and the `Fields` companion struct is unit-
/// shaped so we do not need to import sassi's `Field` accessor type
/// directly. Hand impls of `Cacheable::fields()` are explicitly
/// supported per `sassi::cacheable` — the doc says "hand impls are
/// supported when the macro doesn't fit", and an empty companion
/// struct is the smallest valid shape.
#[derive(Clone)]
struct TestModel {
    id: i64,
}

#[derive(Default)]
struct TestModelFields;

impl Cacheable for TestModel {
    type Id = i64;
    type Fields = TestModelFields;

    fn id(&self) -> i64 {
        self.id
    }

    fn fields() -> TestModelFields {
        TestModelFields
    }
}

/// Pins `djogi::cache::Punnu` as a reachable type — adopters can
/// construct an empty pool with the default config through the
/// re-exported builder. The test compiles when the symbol is
/// reachable; runtime behaviour is sassi's responsibility.
#[test]
fn djogi_cache_module_exports_sassi_punnu() {
    let _: Punnu<TestModel> = Punnu::<TestModel>::builder().build();
}

/// Pins `djogi::cache::BasicPredicate` as a reachable type. Uses the
/// `True` no-op variant rather than a `Field::eq` lift because the
/// `Field` accessor is not part of T7.1's re-export set (only the
/// trait + algebra surface is). Constructing the `True` variant is
/// sufficient to lock the type-level visibility per the cluster
/// plan's "BasicPredicate::True (sassi's no-op variant)" fallback.
/// The full `Field::eq → BasicPredicate` lift is exercised by the
/// `sassi` crate's own tests; T7.1 only verifies adopter
/// reachability of the type.
#[test]
fn djogi_cache_basic_predicate_lift_compiles() {
    let _: BasicPredicate<TestModel> = BasicPredicate::True;
}
