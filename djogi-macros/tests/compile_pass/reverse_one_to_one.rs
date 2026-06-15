// Verifies that `djogi::reverse_one_to_one!` expands cleanly and emits
// an accessor method whose return type is `Result<Option<T>, DjogiError>`
// rather than `Result<Vec<T>, DjogiError>`. Acceptance checks:
//
// - method returns `Option<Profile>`, not `Vec<Profile>`;
// - the inventory marker carries `RelationKind::O2O`, not `FK`;
// - the fixture's source model uses `OneToOneField<Receiver>` — the
//  canonical O2O shape. The macro emits `OneToOneField::new(pk)` in
//  the closure body, so the returned-model's field type must match.
//
// The sibling reverse-FK test (`reverse_one_to_many.rs`) covers the
// `ForeignKey<Receiver>` wrapper flavor; here we pin the O2O-specific
// path. Users whose source wraps with `ForeignKey` + a `UNIQUE`
// constraint on the DB column should declare the reverse via
// `reverse_one_to_many!` and treat the `.first()` vs `.fetch_all()`
// cardinality distinction at the call site — or switch the field type
// to `OneToOneField<Receiver>` to pick up the singular accessor.
//
// No live Postgres: typecheck the signature by coercing the method's
// return type to the expected `impl Future<Output = Result<Option<T>,
// DjogiError>> + Send` shape and walk the inventory markers inside
// `fn main()` for the registration assertions.

use djogi::prelude::*;

#[model(table = "users_ro1")]
#[derive(Debug, Clone)]
pub struct User {
 pub name: String,
}

// Profile carries a `OneToOneField<User>` — the canonical O2O shape.
// The `no_default` gate is required because `OneToOneField<T>` (like
// `ForeignKey<T>`) intentionally does not implement `Default`.
#[model(table = "profiles_ro1", no_default)]
#[derive(Debug, Clone)]
pub struct Profile {
 pub bio: String,
 pub user_id: OneToOneField<User>,
}

djogi::reverse_one_to_one!(User, profile -> Profile by user_id);

fn _profile_accessor_returns_option<'a>(
 user: &'a User,
 ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Option<Profile>, DjogiError>> + Send + 'a {
 user.profile(ctx)
}

fn main() {
 use djogi::relation::registry::{RelationKind, ReverseRelationMarker};

 let mut saw_profile = false;
 for marker in djogi::__private::inventory::iter::<ReverseRelationMarker> {
  if marker.source() == "User" && marker.name() == "profile" {
   assert_eq!(marker.kind(), RelationKind::O2O);
   assert_eq!(marker.target(), "Profile");
   assert_eq!(marker.via(), "user_id");
   saw_profile = true;
  }
 }
 assert!(
  saw_profile,
  "reverse_one_to_one! did not register the `profile` accessor"
 );
}
