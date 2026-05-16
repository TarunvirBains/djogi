// Phase 8.5 Cluster 2 djogi#105 — `#[field(check = "   ")]` is rejected.
//
// Whitespace-only literals trip the same non-empty guard as the empty
// case. The check is `expr.trim().is_empty()`, so any combination of
// spaces / tabs / newlines reaches the same diagnostic. Surfacing this
// at macro-parse time avoids the broken `CHECK (   )` DDL that would
// otherwise fail only at migration apply.

use djogi::prelude::*;

#[model(table = "animals_105_ws", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Animal105Whitespace {
    #[field(check = "   ")]
    pub weight_kg: f64,
}

fn main() {}
