// `that_fk` must pass the same const-time plain-identifier validator
// as `relation` and `this_fk`. A raw Rust identifier can still spell a
// reserved Postgres keyword (`r#select` -> `"select"` once stringified),
// so parse-time `syn::Ident` validation alone is not sufficient.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "persons_mmbtfk")]
#[derive(Debug, Clone)]
pub struct Person {
 pub name: String,
}

#[model(table = "groups_mmbtfk")]
#[derive(Debug, Clone)]
pub struct Group {
 pub name: String,
}

#[model(table = "person_groups_mmbtfk", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
 pub person_id: ForeignKey<Person>,
 pub group_id: ForeignKey<Group>,
}

djogi::many_to_many!(
 Person, Group,
 through = PersonGroup,
 this_fk = person_id,
 that_fk = r#select,
 relation = "groups"
);

fn main() {}
