//! `herd-summaries` demo — visages + side-query trait.
//!
//! Loads `HerdSummary` projections (cheap columns only) and then
//! enriches each with a `herd_size` aggregate via the side-query trait.
//! The two-step shape is the point: visage select stays narrow, the
//! aggregate is opt-in per call.

use anyhow::Result;
use djogi::prelude::*;
use crate::visages::{HerdSummary, HerdSizeQuery};

pub async fn run(ctx: &DjogiContext) -> Result<()> {
    // Sketch — wired against real APIs once cluster PRs land.
    //
    //     let summaries = HerdSummary::objects().fetch_all(ctx).await?;
    //     for s in summaries {
    //         let size = s.herd_size(ctx).await?;
    //         println!("{:30} estimated={:>4} actual={:>4}",
    //                  s.name, s.estimated_population, size);
    //     }
    todo!("wire visage + side-query")
}
