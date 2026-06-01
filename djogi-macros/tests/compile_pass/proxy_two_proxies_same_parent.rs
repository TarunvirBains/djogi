// Two proxies of the same parent coexist (v3 line 261).
//
// `Vehicle` is the parent. `ActiveVehicle` and `ArchivedVehicle` both
// proxy it with different default filters. Each emits its own
// `inventory::submit!(ModelDescriptor)` with `proxy_for = Some("Vehicle")`,
// and the runtime emits per-type `default_filter_condition` overrides.
// The migration differ skips both proxy descriptors from DDL emission
// (T3.5 projection.rs schema-passthrough), so the parent's projection
// is the only one that registers a table — duplicate-table-in-bucket
// would otherwise fire on the second proxy.

use djogi::prelude::*;

#[model(table = "phase8_proxy_two_proxies_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub name: String,
    pub active: bool,
    pub archived: bool,
    pub price: i64,
}

#[model(
    table = "phase8_proxy_two_proxies_vehicles",
    proxy_for = Vehicle,
    default_filter = |f| f.active.eq(true),
)]
#[derive(Debug, Clone)]
pub struct ActiveVehicle {
    pub name: String,
    pub active: bool,
    pub archived: bool,
    pub price: i64,
}

#[model(
    table = "phase8_proxy_two_proxies_vehicles",
    proxy_for = Vehicle,
    default_filter = |f| f.archived.eq(true),
)]
#[derive(Debug, Clone)]
pub struct ArchivedVehicle {
    pub name: String,
    pub active: bool,
    pub archived: bool,
    pub price: i64,
}

fn main() {
    // Constructing all three querysets witnesses that the per-type
    // default-filter overrides do not collide on a shared symbol.
    let _parent_qs = Vehicle::objects();
    let _active_qs = ActiveVehicle::objects();
    let _archived_qs = ArchivedVehicle::objects();
}
