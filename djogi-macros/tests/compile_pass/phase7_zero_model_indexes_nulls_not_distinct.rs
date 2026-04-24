// Phase 7-Zero v3 T3 — §5 positive case: NULLS NOT DISTINCT unique index (Q2).
use djogi::prelude::*;

#[model(table = "posts_per_tenant", no_default, indexes(
    unique(fields = [tenant_id, slug], nulls_not_distinct = true),
))]
#[derive(Debug, Clone)]
pub struct PostPerTenant {
    pub tenant_id: HeerId,
    pub slug: Option<String>,
}

fn main() {}
