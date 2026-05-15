// Phase 8.5 Cluster 4B (#101) — typed set operations: mismatched
// model arms must NOT compile.
//
// The set-op surface enforces same-`T: Model` arms through the
// `IntoSetOpArm<T>` trait bound on every operator method. Passing a
// `QuerySet<Cat>` as the right arm of a `QuerySet<Dog>::union(...)`
// is a type error at the trait-bound site: `QuerySet<Cat>` does not
// implement `IntoSetOpArm<Dog>`, only `IntoSetOpArm<Cat>`.
//
// This fixture pins the compile-time rejection so a future widening
// of the bound (e.g. relaxing `IntoSetOpArm<T>` to `IntoSetOpArm<()>`
// or a blanket impl) cannot silently break the same-model invariant.

use djogi::prelude::*;

#[model(table = "phase8_5_c4b_mixed_dogs")]
#[derive(Debug, Clone)]
pub struct Dog {
    pub name: String,
}

#[model(table = "phase8_5_c4b_mixed_cats")]
#[derive(Debug, Clone)]
pub struct Cat {
    pub name: String,
}

fn main() {
    // Same-model union compiles; mismatched-model does not.
    let _ok: SetOpQuerySet<Dog> = Dog::objects().union(Dog::objects());

    // The next line MUST fail to compile. `Cat::objects()` is a
    // `QuerySet<Cat>` which does not satisfy `IntoSetOpArm<Dog>`.
    let _err = Dog::objects().union(Cat::objects());
}
