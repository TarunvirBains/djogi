// Phase 8.5 Cluster 4 djogi#216 Piece A — `#[field(domain = "...",
// strict_id_check)]` is rejected.
//
// The strict-id structural CHECK applies only to HeerId / RanjId
// family columns (BIGINT / UUID with bit-layout invariants). Domain
// columns reference an adopter-managed Postgres type whose constraints
// are declared on the domain itself — the HeerRanjID structural CHECK
// would be both inapplicable (the column's storage type is the
// domain, not the HeerId / RanjId carrier) and redundant against the
// domain's own checks. The macro rejects the combination at parse time
// with a span-precise diagnostic.
//
// The field type is `HeerId` so the existing `strict_id_check on
// non-compatible-type` check does NOT fire — HeerId IS strict-id
// compatible. The path the test exercises is the new
// `domain + strict_id_check` conflict guard introduced by Piece A.

use djogi::prelude::*;

#[model(table = "tokens_216_strict", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Token216Strict {
    #[field(domain = "positive_id_range", strict_id_check)]
    pub owner_id: ::djogi::types::HeerId,
}

fn main() {}
