// v3 T1 + T2 — `pk = RanjIdDesc` compiles cleanly.
//
// Mirrors `phase7_zero_pk_heerid_desc.rs` for the UUID variant. Pins
// `PkStrategy::RanjIdDesc` → `PkType::RanjIdDesc` → injected
// `id: RanjIdDesc` field. 7 T2 switched the grammar from the string
// literal (`pk = "ranjid_desc"`) to the bare identifier (`pk = RanjIdDesc`).
// `pub struct Post` mirrors the macro-emitted `pub` visages — Phase
// 8.5 #231 reconciliation pins `type Model: Model` on `DjogiVisage`,
// so the source model must be at least as visible as its visages.
use djogi::prelude::*;

#[model(table = "posts_desc_u", pk = RanjIdDesc)]
#[derive(Debug, Clone)]
pub struct Post {
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
