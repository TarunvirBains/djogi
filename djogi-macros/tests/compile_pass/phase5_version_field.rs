// Verify that `#[field(version)]` on `i32` and `i64` fields both compile.
// Two separate models — one with i32, one with i64 — to prove both integral
// types are accepted by the macro without a compile error.
use djogi::prelude::*;

/// Model with a version field typed as i32.
#[model(table = "versioned_i32")]
#[derive(Debug, Clone)]
pub struct VersionedI32 {
    pub title: String,
    #[field(version)]
    pub v: i32,
}

/// Model with a version field typed as i64.
#[model(table = "versioned_i64")]
#[derive(Debug, Clone)]
pub struct VersionedI64 {
    pub title: String,
    #[field(version)]
    pub v: i64,
}

fn main() {}
