// Phase 7-Zero v3 T1 — `pk = "heerid_desc"` compiles cleanly.
//
// Pins the full chain from attribute parse → `PkStrategy::HeerIdDesc` →
// `PkType::HeerIdDesc` → injected `id: HeerIdDesc` field. The ascending ↔
// descending PK migration itself lands in Phase 7; T1 only freezes that
// the declaration compiles and injects the right Rust type.
use djogi::prelude::*;

#[model(table = "posts_desc", pk = "heerid_desc")]
#[derive(Debug, Clone)]
struct Post {
    pub title: String,
}

fn _injected_id_is_heerid_desc(p: &Post) {
    let _: &::djogi::types::HeerIdDesc = &p.id;
    let _: &DateTime = &p.created_at;
    let _: &DateTime = &p.updated_at;
    let _: &str = &p.title;
}

fn _default_constructs() {
    let _p = Post::default();
}

fn main() {}
