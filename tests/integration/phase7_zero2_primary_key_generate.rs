//! Phase 7-Zero-2 T1 live-DB coverage for `PrimaryKeyDbGen`.
//!
//! Confirms the single-round-trip contract for `generate_many` and the
//! ascending-monotonicity guarantee `HeerId` advertises. Uses the
//! standard `#[djogi::djogi_test]` harness so the test spins up a
//! throwaway database, runs `install_schema` + `seed_default_node`, and
//! tears down on completion.

use djogi::prelude::*;
use djogi::types::{HeerId, HeerIdDesc, RanjId, RanjIdDesc};

#[djogi::djogi_test]
async fn heerid_generate_many_returns_n_distinct_ascending_ids(mut ctx: DjogiContext) {
    let ids = <HeerId as PrimaryKeyDbGen>::generate_many(&mut ctx, 5)
        .await
        .unwrap();
    assert_eq!(ids.len(), 5);
    for win in ids.windows(2) {
        assert!(win[0] < win[1], "ascending HeerId not monotonic: {win:?}");
    }
}

#[djogi::djogi_test]
async fn heerid_generate_single(mut ctx: DjogiContext) {
    let a = <HeerId as PrimaryKeyDbGen>::generate(&mut ctx)
        .await
        .unwrap();
    let b = <HeerId as PrimaryKeyDbGen>::generate(&mut ctx)
        .await
        .unwrap();
    assert!(a < b, "generate() should produce strictly increasing ids");
}

#[djogi::djogi_test]
async fn heerid_desc_generate_many_orders_newest_first(mut ctx: DjogiContext) {
    let ids = <HeerIdDesc as PrimaryKeyDbGen>::generate_many(&mut ctx, 4)
        .await
        .unwrap();
    assert_eq!(ids.len(), 4);
    // Desc variants encode recency as a descending bit pattern: the
    // most-recent allocation sorts smallest under the default `Ord`
    // impl. The XOR-flipped batch therefore reads strictly decreasing.
    for win in ids.windows(2) {
        assert!(
            win[0] > win[1],
            "descending HeerIdDesc not monotonic (expected win[0] > win[1]): {win:?}"
        );
    }
}

#[djogi::djogi_test]
async fn ranjid_generate_many_returns_n(mut ctx: DjogiContext) {
    let ids = <RanjId as PrimaryKeyDbGen>::generate_many(&mut ctx, 3)
        .await
        .unwrap();
    assert_eq!(ids.len(), 3);
}

#[djogi::djogi_test]
async fn ranjid_desc_generate_many_returns_n(mut ctx: DjogiContext) {
    let ids = <RanjIdDesc as PrimaryKeyDbGen>::generate_many(&mut ctx, 3)
        .await
        .unwrap();
    assert_eq!(ids.len(), 3);
}

#[djogi::djogi_test]
async fn generate_many_zero_is_a_noop_without_round_trip(mut ctx: DjogiContext) {
    // count == 0 returns an empty vec without issuing a query — the
    // impl guards on `checked_count == 0` before hitting `query_all`.
    let ids = <HeerId as PrimaryKeyDbGen>::generate_many(&mut ctx, 0)
        .await
        .unwrap();
    assert!(ids.is_empty());
}
