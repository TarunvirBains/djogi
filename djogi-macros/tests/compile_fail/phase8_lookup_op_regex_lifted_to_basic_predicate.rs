// Phase 8γ T6.10 — Regex MUST NOT lift to `sassi::BasicPredicate`.
//
// The §660 split (spec `docs/spec/implementation-plan.md:660`) lifts 15
// Rust-evaluable lookup operators into `sassi::BasicPredicate` and keeps
// 2 SQL-only operators (`Regex`, `IRegex`, the Postgres POSIX `~` /
// `~*` operators) djogi-side as `Q::Regex`. Lifting `Regex` to sassi
// would require linking a Rust regex engine, which the framework
// forbids per `decisions.md` rows 107 + 108.
//
// This fixture verifies the rule at the type level: attempting to
// construct a `sassi::LookupOp::Regex` value MUST fail because sassi's
// `LookupOp` enum does not (and must not) carry a `Regex` variant.
// The build error pins the contract — if a future sassi version
// silently adds a `Regex` variant, this fixture starts compiling and
// the build fails the no-regex invariant.
//
// Every lihaaf compile-fixture must have
// `fn main` so the stored `.stderr` does not pick up `E0601 (main
// not found)` noise alongside the real diagnostic.

fn main() {
    // Attempt to construct a `Regex` variant on sassi's `LookupOp`.
    // Sassi's `LookupOp` enum (`sassi/src/predicate/field_predicate.rs:42`)
    // exposes Eq, Neq, Gt, Gte, Lt, Lte, In, NotIn, IsNull, IsNotNull,
    // Between, Contains, IContains, StartsWith, IStartsWith, EndsWith,
    // IEndsWith, IExact — but not Regex/IRegex. The build MUST fail
    // with `error[E0599]: no variant or associated item named `Regex`
    // found for enum `sassi::LookupOp``.
    let _ = sassi::LookupOp::Regex;
}
