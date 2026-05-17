// Verifies that #[model] compiles and injects id/created_at/updated_at.
// Phase 7-Zero-2 T2 flipped the default PK to `HeerIdRecencyBiased`; this
// fixture pins the ascending-HeerId injection path via an explicit
// `pk = HeerId` so the type checks below exercise the historical shape.
//
// `pub struct Post` mirrors the macro-emitted `pub struct PostPublic` /
// `PostSelfView` / `PostAdmin` / `PostExport` visages: Phase 8.5 #231
// reconciliation pins `type Model: Model` on `DjogiVisage`, so the
// source model must be at least as visible as its visages (otherwise
// `impl DjogiVisage for PostPublic { type Model = Post; ... }` trips
// rustc's `private_interfaces` check / E0446). Every other compile_pass
// fixture in this crate follows the same `pub` convention; this one
// was a pre-#231 vestige.
use djogi::prelude::*;

#[model(table = "posts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub published: bool,
}

fn _check_fields(p: &Post) {
    let _: &HeerId = &p.id;
    let _: &DateTime = &p.created_at;
    let _: &DateTime = &p.updated_at;
    let _: &str = &p.title;
    let _: &bool = &p.published;
}

fn _check_default() {
    let _p = Post::default();
}

// Exercises the plan's primary stated Default-impl goal: struct-update syntax.
// A user can write `..Post::default()` to fill framework fields without manually
// initializing id / created_at / updated_at at every call site.
fn _check_struct_update() {
    let _p = Post {
        title: "hello".to_string(),
        published: true,
        ..Post::default()
    };
}

fn main() {}
