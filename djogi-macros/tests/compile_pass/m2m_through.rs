//! many_to_many! visage-scoped accessor emission.
//!
//! A `djogi::many_to_many!` invocation with an `expose(scope -> PeerVisage)`
//! clause must emit an additional inherent method on the source's
//! `{scope}` visage that returns `Vec<PeerVisage>` (the peer's `{scope}`
//! visage). The baseline trait impl + model-scoped accessor stay
//! unchanged — the clause is additive.
//!
//! # Conservative three-way requirement
//!
//! The emitter requires visages to exist at the named scope on ALL
//! three participants (source, peer, through-row). This fixture
//! declares `expose(public)` on every relevant struct so the
//! visage-scoped accessor typechecks. The sibling compile-fail
//! fixture (`phase7_zero2_t9_m2m_missing_through_visage.rs`) omits
//! the through-row exposure and proves the accessor method is absent.

use djogi::prelude::*;
use djogi::relation::{ForeignKey, ManyToMany};

#[model(table = "phase7_zero2_t9_persons")]
#[derive(Debug, Clone)]
pub struct Person {
    #[field(expose(public))]
    pub name: String,
}

#[model(table = "phase7_zero2_t9_groups")]
#[derive(Debug, Clone)]
pub struct Group {
    #[field(expose(public))]
    pub name: String,
}

// Through-row needs a visage at the same scope; otherwise the
// visage-scoped accessor is guarded off. `role` is the only user
// field and `expose(public)` threads it into `PersonGroupPublic`.
#[model(table = "phase7_zero2_t9_person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
    pub person_id: ForeignKey<Person>,
    pub group_id: ForeignKey<Group>,
    #[field(expose(public))]
    pub role: String,
}

// Extended grammar — `expose(public -> GroupPublic)` piggybacks on
// the otherwise unchanged `many_to_many!` invocation and asks the
// emitter for an additional `impl PersonPublic { pub fn groups(...)
// -> Vec<GroupPublic> }` method.
djogi::many_to_many!(
    Person, Group,
    through = PersonGroup,
    this_fk = person_id,
    that_fk = group_id,
    relation = "groups",
    expose(public -> GroupPublic)
);

// The baseline trait impl and model-scoped accessor stay intact. This
// probe compiles only if `many_to_many!` still emits the unchanged
// pre-T9 code alongside the new visage-scoped method.
#[allow(dead_code)]
fn _model_scoped_accessor_preserved<'a>(
    person: &'a Person,
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<Group>, DjogiError>> + Send + 'a {
    person.groups(ctx)
}

// T9 acceptance (post-T13b) — visage-scoped accessor on `PersonPublic`
// returns a SELECT-narrowed `VisageQuerySet<GroupPublic>` that lowers
// to an `EXISTS (...)` correlated subquery. Function coercion pins the
// method signature without needing a live Postgres pool.
#[allow(dead_code)]
fn _visage_scoped_accessor_returns_peer_visage(
    person_public: &PersonPublic,
) -> djogi::query::VisageQuerySet<GroupPublic> {
    person_public.groups()
}

fn main() {
    // Trait impl unchanged — consume RELATION / this_fk / that_fk
    // through the trait at runtime to pin the baseline invariants.
    assert_eq!(<Person as ManyToMany<Group>>::RELATION, "groups");
    assert_eq!(<Person as ManyToMany<Group>>::this_fk(), "person_id");
    assert_eq!(<Person as ManyToMany<Group>>::that_fk(), "group_id");
}
