//! HerdRange — the explicit through model for the `Herd ↔ Country` M2M.
//!
//! ## What this demonstrates
//!
//! - Explicit-through M2M with a payload (`season`). Djogi refuses to
//! guess the through model; this file is that through model.
//! - Composite uniqueness via `#[model(indexes(unique(fields = [...])))]`
//! so a `(herd, country, season)` triple is at most one row.
//! - `no_default` because `ForeignKey<T>` deliberately has no `Default`
//! implementation — there is no meaningful "empty" foreign key.
//!
//! Why `season` is a payload: African elephants genuinely cross borders
//! seasonally — the same Amboseli herd might be in Kenya in dry season
//! and Tanzania in wet season. A payload-free join row would lose that.

use crate::models::{Country, Herd};
use djogi::prelude::*;

#[model(
 table = "herd_ranges",
 pk = HeerId,
 no_default,
 indexes(unique(fields = [herd_id, country_id, season])),
)]
#[derive(Debug, Clone)]
pub struct HerdRange {
    pub herd_id: ForeignKey<Herd>,

    pub country_id: ForeignKey<Country>,

    /// `dry` or `wet` — short controlled vocabulary. Real apps would
    /// use a `DjogiEnum` here; the example keeps it as `TEXT` so the
    /// model demonstrates one feature at a time.
    #[field(max_length = 8)]
    pub season: String,
}
