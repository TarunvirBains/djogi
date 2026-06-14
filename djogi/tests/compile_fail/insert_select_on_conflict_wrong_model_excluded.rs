use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "wrong_excl_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongExclSource {
    pub label: String,
}

#[model(table = "wrong_excl_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongExclTarget {
    pub label: String,
}

fn main() {
    let _stmt = WrongExclSource::objects()
        .insert_into::<WrongExclTarget, _, _>(|target, source| {
            vec![target.label().copy_from(source.label().as_insert_source())]
        })
        .on_conflict_do_update(
            ConflictTarget::columns([WrongExclTarget::fields().label()]),
            |t| vec![t.label().conflict_set(WrongExclSource::fields().label().excluded())],
        );
}
