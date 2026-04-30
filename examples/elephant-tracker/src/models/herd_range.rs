//! HerdRange — the explicit through model for the Herd ↔ Country M2M.
//!
//! Demonstrates:
//! - Explicit-through M2M with a payload (`season`). Djogi refuses to
//!   guess the through model; this file is the through model.
//! - Composite uniqueness via `unique_together` so a herd-country-season
//!   triple is at most one row.
//!
//! Why `season` is a payload: African elephants genuinely cross borders
//! seasonally — the same Amboseli herd might be in Kenya in dry season
//! and Tanzania in wet season. A payload-free join row would lose that.

use djogi::prelude::*;
use crate::models::{Herd, Country};

#[djogi::model(
    table = "herd_ranges",
    unique_together = [("herd_id", "country_id", "season")]
)]
#[derive(Debug, Clone)]
pub struct HerdRange {
    pub herd: ForeignKey<Herd>,

    pub country: ForeignKey<Country>,

    /// `dry` or `wet` — short controlled vocabulary. Real apps would
    /// use a `DjogiEnum` here; the example keeps it as `TEXT` to avoid
    /// pulling another feature into a model that's already busy.
    #[field(max_length = 8)]
    pub season: String,
}
