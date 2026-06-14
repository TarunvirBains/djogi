use djogi::prelude::*;

#[model(table = "wp_none_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WpNoneTarget {
    pub published: bool,
}

fn main() {
    let _t =
        ConflictTarget::<WpNoneTarget>::none().where_predicate(|t| t.published().conflict_is_true());
}
