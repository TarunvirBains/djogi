// A `reverse_one_to_many!` invocation that names a via-column which does
// not exist on the returned model must fail to compile with a clear
// type-system error. The macro does not (and cannot, without pulling a
// full descriptor table into the expansion) validate the via-column
// name against the returned model's declared columns; instead, the
// closure body inside the expansion calls `f.nonexistent_col()`, which
// fails to resolve against the macro-generated `VehicleFields` impl.
//
// The error message must point at the invocation site, which rustc
// reports as the macro call because the generated method is synthetic.
// Pinning the `.stderr` keeps this contract stable: future macro
// refactors that move the check or change the emission must keep the
// error localised.
use djogi::prelude::*;

#[model(table = "owners_cf")]
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
}

#[model(table = "vehicles_cf", no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub make: String,
    pub owner_id: ForeignKey<Owner>,
}

// `nonexistent_col` is not a column on `Vehicle`. The macro's closure
// body is `|f| f.nonexistent_col().eq(...)` which cannot resolve.
djogi::reverse_one_to_many!(Owner, cars -> Vehicle by nonexistent_col);

fn main() {}
