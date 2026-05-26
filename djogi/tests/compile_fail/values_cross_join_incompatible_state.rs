//! Verify that `cross_join_values` does not accept an ON predicate closure.
//!
//! Cross joins are unconditional Cartesian products — there is no `ON`
//! predicate to supply.  Passing a closure (as one would to `join_values`)
//! is a type error because `cross_join_values` takes only the `InlineValues`
//! argument.  This fixture documents that guarantee at the type level.
use djogi::prelude::*;

#[model(table = "animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Animal {
    pub name: String,
}

fn main() {
    let labels: InlineValues<(String,)> = InlineValues::new(
        vec![("a".to_string(),)],
        "lbl",
        ("tag",),
    )
    .unwrap();

    // Passing an ON predicate closure to cross_join_values must fail —
    // cross_join_values is an unconditional Cartesian join and takes only
    // `InlineValues` as its argument.  The extra closure argument has no
    // parameter slot.
    let _bad = Animal::objects().cross_join_values(labels, |_a: AnimalFields, _v: ValuesFields<(String,)>| ());
}
