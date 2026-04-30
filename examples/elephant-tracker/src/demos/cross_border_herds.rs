//! `cross-border-herds` demo — M2M traversal with payload filtering.
//!
//! Walks the `Herd ↔ Country` M2M through `HerdRange`, surfacing herds
//! that range across two or more countries within the same season.
//! The cross-border story is the load-bearing reason for `HerdRange`
//! to carry the `season` payload at all.

use anyhow::Result;
use djogi::prelude::*;
use crate::models::{Herd, HerdRange};

pub async fn run(ctx: &DjogiContext) -> Result<()> {
    // Sketch — wired against real APIs once cluster PRs land.
    //
    //     let cross_border = Herd::objects()
    //         .annotate(country_count_in_season,
    //                   HerdRange::objects()
    //                       .filter(herd = OuterRef("id"), season = "wet")
    //                       .count_distinct("country_id"))
    //         .filter(country_count_in_season__gt(1))
    //         .prefetch(Herd::countries())
    //         .fetch_all(ctx)
    //         .await?;
    todo!("wire M2M traversal with season filter")
}
