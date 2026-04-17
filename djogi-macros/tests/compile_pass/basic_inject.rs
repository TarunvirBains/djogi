// Verifies that #[model] compiles and injects id/created_at/updated_at.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
struct Post {
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

fn main() {}
