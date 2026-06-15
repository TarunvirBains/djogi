//! T9 compile-fail — M2M visage accessor when the through
//! model does NOT declare a visage at the exposure scope.
//!
//! The plan requires the visage-scoped M2M emission to fire only when
//! all three participants (source, peer, through-row) declare a visage
//! at the same scope. Here `PersonGroup` carries no `#[field(expose(...))]`,
//! so `PersonGroupPublic` has no user-scoped fields — but the through
//! model's visage still exists (framework cols default into every
//! scope per Q13). To force a genuine absence, we use a through struct
//! decorated `#[model(pk = none)]` which suppresses `{Visage}Fields` /
//! `DjogiVisageOf<M>` / visage-convertible surface.
//!
//! Actually, the simpler way: the visage-scoped body inside
//! `many_to_many!` references `<{Through}{Suffix} as TryFrom<&Through>>`.
//! If `{Through}{Suffix}` doesn't exist as a type, rustc errors out
//! with "cannot find type" at the `many_to_many!` call site. We trigger
//! that by pointing at a Through struct that is NOT `#[model]`-decorated
//! at all — no visage is emitted, the type-level probe in the body
//! fails.
//!
//! Because `many_to_many!` REQUIRES a `through = <Model>` argument and
//! expects that model to impl `djogi::model::Model`, a non-`#[model]`
//! struct would also break the baseline M2M emission. Instead the
//! cleanest compile-fail witness is: declare `expose(admin ->...)` on
//! a set of models that only have `public` visages visibly populated —
//! the emitted `PersonGroupAdmin` exists (it's emitted for every scope)
//! but `GroupAdmin` exists too. The real absence to prove is: omit the
//! `-> PeerVisage` entirely from the caller and let rustc fail.
//!
//! Since all four scopes always emit visage structs, the strongest
//! compile-fail witness here is simpler: call the `groups` method on
//! a visage that does NOT have the `expose(scope ->...)` declared.

use djogi::prelude::*;
use djogi::relation::ForeignKey;

#[model(table = "phase7_zero2_t9_negm_persons")]
#[derive(Debug, Clone)]
pub struct Person {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "phase7_zero2_t9_negm_groups")]
#[derive(Debug, Clone)]
pub struct Group {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "phase7_zero2_t9_negm_person_groups", through, no_default)]
#[derive(Debug, Clone)]
pub struct PersonGroup {
 pub person_id: ForeignKey<Person>,
 pub group_id: ForeignKey<Group>,
}

// Declare the M2M with a `public` exposure ONLY. No `admin` clause, so
// calling `.groups(...)` on `PersonAdmin` must be absent.
djogi::many_to_many!(
 Person, Group,
 through = PersonGroup,
 this_fk = person_id,
 that_fk = group_id,
 relation = "groups",
 expose(public -> GroupPublic)
);

fn _does_not_compile<'a>(
 person_admin: &'a PersonAdmin,
 ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<GroupAdmin>, DjogiError>> + Send + 'a {
 // Must fail: the M2M only exposed at `public`, so `PersonAdmin::groups`
 // does not exist.
 person_admin.groups(ctx)
}

fn main() {}
