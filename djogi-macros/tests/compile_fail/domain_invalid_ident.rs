// djogi#216 — `#[field(domain = "123bad")]`
// is rejected.
//
// Domain names must start with an ASCII letter or underscore — the
// same byte-shape rule the column / table identifier validator
// enforces. A leading digit makes the identifier unquotable and would
// produce invalid `CREATE TABLE` / `ALTER TABLE` DDL when the migration
// composer emits the domain name in the column-type slot.
//
// The diagnostic spells out the full byte-shape rule in plain English
// per the project-wide no-regex policy in CLAUDE.md.

use djogi::prelude::*;

#[model(table = "orders_216_invalid", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Order216Invalid {
 #[field(domain = "123bad")]
 pub amount: rust_decimal::Decimal,
}

fn main() {}
