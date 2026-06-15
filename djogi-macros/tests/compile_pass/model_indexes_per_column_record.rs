// §5 positive case: per-column record form
// (opclass / order / nulls set individually).
use djogi::prelude::*;

#[model(table = "audit_log", indexes(
 index(fields = [
  (col = created_at, order = desc, nulls = first),
  (col = status, opclass = "text_pattern_ops"),
 ]),
))]
#[derive(Debug, Clone)]
pub struct AuditLog {
 pub status: String,
}

fn main() {}
