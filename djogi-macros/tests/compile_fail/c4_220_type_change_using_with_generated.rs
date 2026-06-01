// Cluster 4 djogi#220 — `#[field(type_change_using = "...")]`
// paired with `#[field(generated = "...")]` is rejected.
//
// A stored generated column derives its storage type from the expression;
// an adopter-supplied USING cannot meaningfully drive the resulting type,
// and Postgres' semantics for `ALTER COLUMN ... TYPE ... USING (<expr>)`
// on a stored generated column are surprising at best. Adopters who need
// to flip a generated column's storage type hand-write the migration.

use djogi::prelude::*;

#[model(table = "items_220_gen", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item220Generated {
    pub email: String,
    #[field(generated = "LOWER(email)", type_change_using = "LOWER(email)::TEXT")]
    pub email_lower: Option<String>,
}

fn main() {}
