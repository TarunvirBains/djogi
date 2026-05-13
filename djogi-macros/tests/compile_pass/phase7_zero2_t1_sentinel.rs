// Phase 7-Zero-2 T1 — `PrimaryKey::sentinel()` produces a zero-valued
// instance for every built-in PK type and `i32` (Serial).
//
// NOTE: the original Phase 7-Zero-2 plan called for an inherent
// `const SENTINEL: Self` on each built-in. That form is double-blocked:
// (1) the orphan rule forbids inherent impls on foreign types (HeerId
// lives in `heeranjid`), and (2) heeranjid 0.3 ships no `const fn`
// constructor for the built-ins, so even if the impl were legal there
// would be nothing to evaluate at compile time. The shipped design
// routes sentinels through the `PrimaryKey::sentinel()` trait method;
// this fixture exercises the runtime form across all five PK shapes.
//
// `fn main() {}` is intentional — compile-pass fixtures must compile as
// normal binaries, not as library artifacts; see
// `lihaaf compile-fixture contract`.

use djogi::prelude::*;

fn main() {
    let _zero_heerid: HeerId = <HeerId as PrimaryKey>::sentinel();
    let _zero_heerid_desc: djogi::types::HeerIdDesc =
        <djogi::types::HeerIdDesc as PrimaryKey>::sentinel();
    let _zero_heerid_recency_biased: HeerIdRecencyBiased =
        <HeerIdRecencyBiased as PrimaryKey>::sentinel();
    let _zero_ranjid: RanjId = <RanjId as PrimaryKey>::sentinel();
    let _zero_ranjid_desc: djogi::types::RanjIdDesc =
        <djogi::types::RanjIdDesc as PrimaryKey>::sentinel();
    let _zero_ranjid_recency_biased: RanjIdRecencyBiased =
        <RanjIdRecencyBiased as PrimaryKey>::sentinel();
    let _zero_serial: i32 = <i32 as PrimaryKey>::sentinel();
}
