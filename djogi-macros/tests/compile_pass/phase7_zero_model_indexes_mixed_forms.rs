// Phase 7-Zero v3 T3 — §5 positive case: mixed simple-ident + record form.
use djogi::prelude::*;

#[model(table = "tenant_events", no_default, indexes(
    index(fields = [tenant_id, (col = created_at, order = desc)]),
))]
#[derive(Debug, Clone)]
pub struct TenantEvent {
    pub tenant_id: HeerId,
}

fn main() {}
