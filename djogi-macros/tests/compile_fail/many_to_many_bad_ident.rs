// A `many_to_many!` invocation whose `relation = "..."` string is
// not a valid Rust identifier must fail to compile. The macro's
// parser converts the `LitStr` into a `syn::Ident` via
// `syn::parse_str::<Ident>`; a non-ident-shaped string (here: one
// containing a hyphen, which is not legal in Rust identifiers)
// triggers the parser's error branch and produces a diagnostic
// pointing at the `relation = "..."` call site.
//
// This pins the identifier-validation story for the macro: the
// `relation` argument is reused both as a method name on the source
// type (where rustc validates) and as a registry `name` slot (where
// `const_assert_plain_ident` validates at const-eval time). The
// parser-side check runs first; this fixture exercises that path
// because a stored `.stderr` line for a const-panic is finicky under
// trybuild (the panic line number shifts with every validator edit).
// The parser-side error message stays stable under downstream edits
// because it is produced at this macro's own source site.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "persons_mmb")]
#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
}

#[model(table = "groups_mmb")]
#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
}

#[model(table = "person_groups_mmb", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
}

// `"my-groups"` is not a valid Rust identifier — hyphens are not
// permitted. The macro's parser rejects it with a diagnostic pointing
// at the `relation = ...` site.
djogi::many_to_many!(
    Person, Group,
    through = PersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "my-groups"
);

fn main() {}
