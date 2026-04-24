// Phase 7-Zero v3 T5 fixup — §6.2 positive case: unique + concurrently.
//
// `unique(fields = [...], concurrently = true)` escalates the kind to
// `UniqueIndex` (ALTER TABLE ADD CONSTRAINT UNIQUE has no CONCURRENTLY
// form). The macro must accept this declaration and emit the
// escalated kind; the generated index name carries the `_uidx` stem
// to match.
use djogi::prelude::*;

#[model(table = "accounts", indexes(
    unique(fields = [tenant_id, email], concurrently = true),
))]
#[derive(Debug, Clone)]
pub struct Account {
    pub tenant_id: String,
    pub email: String,
}

fn main() {}
