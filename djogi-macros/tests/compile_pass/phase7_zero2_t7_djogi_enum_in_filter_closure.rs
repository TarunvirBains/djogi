//! Phase 7-Zero-2 T7 — `DjogiEnum`-derived types compose with filter
//! closures via the emitted `IntoFilterValue` impl.
//!
//! The T7 `derive_djogi_enum` emitter now folds an `IntoFilterValue`
//! impl into its expansion — encoding the variant as its Postgres wire
//! string, matching the `ToSql` encoding path. This fixture pins the
//! surface: `.eq`, `.neq`, `.in_`, and `.not_in` must all type-
//! check when the column's declared Rust type is a `DjogiEnum`.
use djogi::prelude::*;

#[derive(DjogiEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[djogi_enum(name = "vehicle_status_t7_filter", rename_all = "snake_case")]
pub enum VehicleStatus {
    Active,
    InMaintenance,
    Retired,
}

#[model(table = "vehicles_t7_enum_filter")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub status: VehicleStatus,
    pub label: String,
}

impl Default for VehicleStatus {
    fn default() -> Self {
        VehicleStatus::Active
    }
}

fn main() {
    // `.eq` / `.neq` — scalar lookup through `IntoFilterValue`.
    let _eq = Vehicle::objects().filter(|f| f.status().eq(VehicleStatus::Active));
    let _neq = Vehicle::objects().filter(|f| f.status().neq(VehicleStatus::Retired));

    // `.in_` / `.not_in` — IN / NOT IN with a variant list. Each
    // variant converts via the emitted `IntoFilterValue` impl into a
    // `FilterValue::String(<wire>)`, matching the ToSql encoding path.
    let _in = Vehicle::objects().filter(|f| {
        f.status()
            .in_(vec![VehicleStatus::Active, VehicleStatus::InMaintenance])
    });
    let _nin = Vehicle::objects().filter(|f| f.status().not_in(vec![VehicleStatus::Retired]));
}
