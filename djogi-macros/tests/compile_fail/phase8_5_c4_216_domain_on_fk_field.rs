// Phase 8.5 Cluster 4 djogi#216 Piece A — `#[field(domain = "...")]` on
// a `ForeignKey<T>` field is rejected.
//
// The column's SQL type for a relation field is determined by the
// target model's PK type — the adopter cannot override it with a
// domain reference. If the target's PK needs to flow through a domain,
// the adopter declares the domain on the target model's PK column
// instead; the FK column inherits whatever shape the target uses.

use djogi::prelude::*;

#[model(table = "owners_216_fk", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Owner216Fk {
    pub name: String,
}

#[model(table = "vehicles_216_fk", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Vehicle216Fk {
    #[field(domain = "positive_amount")]
    pub owner: ForeignKey<Owner216Fk>,
}

fn main() {}
