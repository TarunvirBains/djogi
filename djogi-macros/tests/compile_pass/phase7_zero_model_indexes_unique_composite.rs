// Phase 7-Zero v3 T3 — §5 positive case: composite unique constraint.
use djogi::prelude::*;

#[model(table = "orgs_externals", no_default, indexes(
    unique(fields = [org_id, external_id]),
))]
#[derive(Debug, Clone)]
pub struct OrgExternal {
    pub org_id: HeerId,
    pub external_id: String,
}

fn main() {}
