// Cluster 4B (#101) — typed set operations: public-API
// compile-pass.
//
// Locks the user-facing surface of [`QuerySet::union`] /
// `.union_all(...)` / `.intersect(...)` / `.except(...)` and the
// chaining methods on [`SetOpQuerySet`]:
//
// 1. Both arms must share the same `T: Model` — enforced at the type
//    signature level via `IntoSetOpArm<T>`.
// 2. The four operators each return `SetOpQuerySet<T>`.
// 3. Chaining a set-op back into another set-op (`a.union(b).intersect(c)`)
//    type-checks because `SetOpQuerySet<T>: IntoSetOpArm<T>`.
// 4. Outer modifiers (`order_by` / `limit` / `offset`) compose without
//    requiring `T: FromPgRow` (terminals do that bound separately).
// 5. `SetOpKind` round-trips through `op()`.
//
// Every lihaaf compile-pass fixture needs `fn main` to link as a
// standalone binary.

use djogi::prelude::*;

#[model(table = "phase8_5_c4b_set_op_dogs")]
#[derive(Debug, Clone)]
pub struct Dog {
    pub name: String,
    pub adopted: bool,
}

#[model(table = "phase8_5_c4b_set_op_cats")]
#[derive(Debug, Clone)]
pub struct Cat {
    pub name: String,
    pub adopted: bool,
}

fn main() {
    // 1. Same-model arms compose through every operator. Each call
    //    consumes `self` (so we reconstruct fresh querysets for each
    //    test) and returns `SetOpQuerySet<Dog>`.
    let _union: SetOpQuerySet<Dog> = Dog::objects().union(Dog::objects());
    let _union_all: SetOpQuerySet<Dog> = Dog::objects().union_all(Dog::objects());
    let _intersect: SetOpQuerySet<Dog> = Dog::objects().intersect(Dog::objects());
    let _except: SetOpQuerySet<Dog> = Dog::objects().except(Dog::objects());

    // 2. `SetOpQuerySet<T>: IntoSetOpArm<T>` enables chaining a
    //    set-op result back as either arm of a fresh set-op. The
    //    nested case is left-associative through the chained
    //    builder methods on `SetOpQuerySet` itself.
    let chained: SetOpQuerySet<Dog> = Dog::objects()
        .union(Dog::objects())
        .intersect(Dog::objects())
        .except(Dog::objects())
        .union_all(Dog::objects());
    // The chained value is still a `SetOpQuerySet<Dog>` — set-op
    // composition preserves the model parameter.
    let _: SetOpQuerySet<Dog> = chained;

    // 3. Passing a set-op as the right arm of a fresh QuerySet union
    //    works through the `IntoSetOpArm` impl on `SetOpQuerySet<T>`.
    let inner: SetOpQuerySet<Dog> = Dog::objects().intersect(Dog::objects());
    let _nested_right: SetOpQuerySet<Dog> = Dog::objects().union(inner);

    // 4. Outer modifiers compose: `order_by` returns `Self`, so it
    //    chains naturally with `.limit` and `.offset`.
    let _windowed: SetOpQuerySet<Dog> = Dog::objects()
        .union(Dog::objects())
        .order_by(|f| f.name().asc())
        .limit(20)
        .offset(5);

    // 5. `SetOpKind::Union` / `UnionAll` / `Intersect` / `Except`
    //    round-trip through the `op()` accessor.
    let kind: SetOpKind = Dog::objects().union(Dog::objects()).op();
    let _ = matches!(kind, SetOpKind::Union);
    let _ = matches!(
        Dog::objects().intersect(Dog::objects()).op(),
        SetOpKind::Intersect
    );

    // 6. The two model types are unrelated — `Dog::objects()` cannot
    //    be unioned with `Cat::objects()`. The compile_fail sibling
    //    fixture (`phase8_5_c4b_set_op_mixed_model.rs`) proves the
    //    rejection.
    let _: SetOpQuerySet<Cat> = Cat::objects().union(Cat::objects());
}
