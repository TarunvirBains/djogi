// v3 T1 + T2 — `pk = HeerIdDesc` compiles cleanly.
//
// Pins the full chain from attribute parse → `PkStrategy::HeerIdDesc` →
// `PkType::HeerIdDesc` → injected `id: HeerIdDesc` field. The ascending ↔
// descending PK migration itself is handled by the migration runner;
// this fixture only freezes that the declaration compiles and injects
// the right Rust type. The grammar was switched from the string literal
// (`pk = "heerid_desc"`) to the bare identifier (`pk = HeerIdDesc`).
// `pub struct Post` mirrors the macro-emitted `pub` visages —
// reconciliation pins `type Model: Model` on `DjogiVisage`,
// so the source model must be at least as visible as its visages.
use djogi::prelude::*;

#[model(table = "posts_desc", pk = HeerIdDesc)]
#[derive(Debug, Clone)]
pub struct Post {
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
