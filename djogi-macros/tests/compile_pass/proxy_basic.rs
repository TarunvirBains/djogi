// Minimal compile-pass fixture for proxy models.
//
// Declares a parent `Vehicle` model and a proxy `ActiveVehicle` that
// shares the parent's table and applies a default filter. Proves the
// `#[model(proxy_for, default_filter)]` attribute set parses and
// expands cleanly, the lowered SQL fragment reaches the descriptor,
// and the runtime composition path (`QuerySet::new()` seeding) is
// reachable from adopter code.
//
// Every lihaaf compile-fixture must
// have `fn main() {}` so the stored `.stderr` does not pick up
// E0601 noise. Compile-pass fixtures need `fn main()` for the same
// reason — the binary still has to link.

use djogi::prelude::*;

#[model(table = "phase8_proxy_basic_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub name: String,
    pub active: bool,
    pub price: i64,
}

#[model(
    table = "phase8_proxy_basic_vehicles",
    proxy_for = Vehicle,
    default_filter = |f| f.active.eq(true),
)]
#[derive(Debug, Clone)]
pub struct ActiveVehicle {
    pub name: String,
    pub active: bool,
    pub price: i64,
}

fn main() {
    // Constructing the queryset is the only thing this fixture
    // exercises — proving the `Model::default_filter_condition`
    // override is wired and the seeded condition tree compiles
    // through `QuerySet::new()`. No DB I/O.
    let _qs = ActiveVehicle::objects();
}
