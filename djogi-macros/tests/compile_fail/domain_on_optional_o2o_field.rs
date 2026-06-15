// djogi#216 — `#[field(domain = "...")]` on
// an `Option<OneToOneField<T>>` field is rejected.
//
// `detect_relation` unwraps one layer of `Option<…>` before classifying
// the inner type. This fixture pins the optional-O2O path through the
// guard at `attrs.rs::3131` so a future refactor of the validation
// block cannot silently drop coverage for the optional-wrapped O2O variant.

use djogi::prelude::*;

#[model(table = "users_216_opt_o2o", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct User216OptO2o {
 pub name: String,
}

#[model(table = "profiles_216_opt_o2o", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Profile216OptO2o {
 #[field(domain = "positive_amount")]
 pub user: Option<OneToOneField<User216OptO2o>>,
}

fn main() {}
