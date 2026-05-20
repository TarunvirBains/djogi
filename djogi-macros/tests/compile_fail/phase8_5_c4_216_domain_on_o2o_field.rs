// Phase 8.5 Cluster 4 djogi#216 Piece A — `#[field(domain = "...")]` on
// a `OneToOneField<T>` field is rejected.
//
// The guard at `attrs.rs::3131` covers both `ForeignKey<T>` and
// `OneToOneField<T>` via `detect_relation`. This fixture pins the
// `OneToOneField<T>` path so a future refactor of the validation block
// cannot silently drop coverage for the O2O variant.

use djogi::prelude::*;

#[model(table = "users_216_o2o", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct User216O2o {
    pub name: String,
}

#[model(table = "profiles_216_o2o", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Profile216O2o {
    #[field(domain = "positive_amount")]
    pub user: OneToOneField<User216O2o>,
}

fn main() {}
