//! Compile-pass: Condition::__is_null_leaf is reachable from adopter-crate
//! code. The soft-delete macro emission calls this across the crate boundary;
//! if it were pub(crate) the fixture would fail to compile (E0624).
//!
//! Every lihaaf compile-fixture must have `fn main()` so the binary links and
//! the stored output does not pick up E0601 noise.

fn _reachable() -> djogi::query::internal::Condition {
    djogi::query::internal::Condition::__is_null_leaf("deleted_at")
}

fn main() {
    let _c = _reachable();
}
