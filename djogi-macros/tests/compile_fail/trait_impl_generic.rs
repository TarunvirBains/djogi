// Compile-fail fixture for `#[djogi::trait_impl]` on a
// generic impl block. v0.1.0 only handles concrete impls; generic
// impls are deferred to a future phase per
// `feedback_anchored_deferrals` because they require runtime
// parameter substitution for the `TypeId::of` lookup.
//
// Every compile-fixture has `fn main() {}` so lihaaf's per-fixture rustc invocation produces a linkable artifact.

trait Searchable {
    fn searchable_columns(&self) -> &'static [&'static str];
}

#[djogi::trait_impl]
impl<T> Searchable for Vec<T> {
    fn searchable_columns(&self) -> &'static [&'static str] {
        &[]
    }
}

fn main() {}
