// Cluster 8δ T7.2 — Codex Finding 6 — `pk = None` skips Cacheable.
//
// Models declared with `pk = None` get NO auto-emitted `impl Cacheable`
// (per `model::cacheable::expand`'s early return for `PkStrategy::None`).
// This fixture pins that contract: probing the trait's associated
// `id()` method through fully-qualified syntax MUST NOT resolve, the
// same way `pk_none_has_no_model_impl.rs` pins `Model` absence by
// probing `create`.
//
// The compile_fail expectation is "no method `id` found through
// `Cacheable` for `Custom`" — rustc's error names the trait the
// missing method is supposed to live on, which keeps the diagnostic
// stable across rustc minor versions and across changes to other
// traits in the dep graph (any trait declaring an `id()` method
// would otherwise widen the candidate-trait list).
//
// Per the lihaaf compile-fixture contract, every compile-fail fixture
// must have `fn main` so the stored `.stderr` does not pick up E0601
// noise.
//
// Spec anchor:
//   docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md
//   §3 commit T7.2 — phase amendment block (Codex Finding 6).

use djogi::prelude::*;

#[model(table = "phase8_t7_cacheable_skip_none", pk = None)]
#[derive(Debug, Clone)]
pub struct Custom {
    pub custom_id: String,
    pub value: String,
}

fn _must_not_compile() {
    // `Custom::cache_type_name` must NOT resolve — the auto-emit
    // skipped this model. Path-only probing (no method invocation)
    // produces a stable "no function or associated item" error from
    // rustc, matching the convention used by
    // `pk_none_has_no_model_impl.rs` for the parallel `Model`-trait
    // absence check.
    let _ = Custom::cache_type_name;
}

fn main() {}
