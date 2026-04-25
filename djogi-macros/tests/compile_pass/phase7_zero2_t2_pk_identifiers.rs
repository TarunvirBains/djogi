// Phase 7-Zero-2 T2 — exhaustive smoke test that every built-in `pk = X`
// identifier compiles. One model per variant; the `_pin_*` helpers pin
// the injected `id` type so a future change that flips the lowering for
// one of these identifiers without updating the attribute parser fails
// this fixture (not just the stored expansion snapshot, which is easy
// to regenerate without scrutiny).
use djogi::prelude::*;

#[model(table = "t2_heerid", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AscHeer {
    pub data: String,
}

#[model(table = "t2_ranjid", pk = RanjId)]
#[derive(Debug, Clone)]
pub struct AscRanj {
    pub data: String,
}

#[model(table = "t2_heerid_desc", pk = HeerIdDesc)]
#[derive(Debug, Clone)]
pub struct DescHeerInternal {
    pub data: String,
}

#[model(table = "t2_heerid_rb", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct DescHeerPublic {
    pub data: String,
}

#[model(table = "t2_ranjid_desc", pk = RanjIdDesc)]
#[derive(Debug, Clone)]
pub struct DescRanjInternal {
    pub data: String,
}

#[model(table = "t2_ranjid_rb", pk = RanjIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct DescRanjPublic {
    pub data: String,
}

#[model(table = "t2_serial", pk = Serial)]
#[derive(Debug, Clone)]
pub struct Lookup {
    pub data: String,
}

fn _pin_asc_heer(x: &AscHeer) {
    let _: &::djogi::types::HeerId = &x.id;
}
fn _pin_asc_ranj(x: &AscRanj) {
    let _: &::djogi::types::RanjId = &x.id;
}
fn _pin_desc_heer(x: &DescHeerInternal, y: &DescHeerPublic) {
    let _: &::djogi::types::HeerIdDesc = &x.id;
    let _: &::djogi::types::HeerIdDesc = &y.id;
}
fn _pin_desc_ranj(x: &DescRanjInternal, y: &DescRanjPublic) {
    let _: &::djogi::types::RanjIdDesc = &x.id;
    let _: &::djogi::types::RanjIdDesc = &y.id;
}
fn _pin_serial(x: &Lookup) {
    let _: &i32 = &x.id;
}

fn main() {}
