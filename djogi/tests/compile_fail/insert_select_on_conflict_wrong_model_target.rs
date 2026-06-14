use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "wrong_tgt_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongTgtSource {
    pub label: String,
}

#[model(table = "wrong_tgt_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct WrongTgtTarget {
    pub label: String,
}

fn main() {
    let _stmt = WrongTgtSource::objects()
        .insert_into::<WrongTgtTarget, _, _>(|target, source| {
            vec![target.label().copy_from(source.label().as_insert_source())]
        })
        .on_conflict_do_nothing(ConflictTarget::columns([WrongTgtSource::fields().label()]));
}
