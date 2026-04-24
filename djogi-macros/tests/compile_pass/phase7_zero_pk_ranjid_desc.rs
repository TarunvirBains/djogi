// Phase 7-Zero v3 T1 — `pk = "ranjid_desc"` compiles cleanly.
//
// Mirrors `phase7_zero_pk_heerid_desc.rs` for the UUID variant. Pins
// `PkStrategy::RanjIdDesc` → `PkType::RanjIdDesc` → injected
// `id: RanjIdDesc` field.
use djogi::prelude::*;

#[model(table = "posts_desc_u", pk = "ranjid_desc")]
#[derive(Debug, Clone)]
struct Post {
    pub title: String,
}

fn _injected_id_is_ranjid_desc(p: &Post) {
    let _: &::djogi::types::RanjIdDesc = &p.id;
    let _: &DateTime = &p.created_at;
    let _: &DateTime = &p.updated_at;
    let _: &str = &p.title;
}

fn _default_constructs() {
    let _p = Post::default();
}

fn main() {}
