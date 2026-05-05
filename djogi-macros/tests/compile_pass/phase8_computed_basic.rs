// Phase 8β T4.6 — Minimal compile-pass fixture for computed fields.
//
// Declares a `Vehicle` model with one `#[computed(sql = "...")]`
// field and exercises the SQL-projectable surface via
// `Vehicle::computed().total_price()`. Proves the descriptor emission
// + `{Model}Computed` ZST + accessor return type all wire together.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main() {}` so the stored `.stderr` does not pick up
// E0601. Compile-pass fixtures need it for the same reason — the
// binary still has to link.

use djogi::expr::Expr;
use djogi::prelude::*;

#[model(table = "phase8_computed_basic_vehicles")]
#[derive(Debug, Clone)]
pub struct Vehicle {
    pub base_price: f64,
    pub tax_rate: f64,
    #[computed(sql = "base_price * (1.0 + tax_rate)")]
    pub total_price: f64,
}

fn main() {
    // Constructing the queryset's filter expression is the only thing
    // this fixture exercises — proving the `{Model}Computed` ZST
    // accessors compile and return the typed `Expr<f64>`. No DB I/O.
    let _qs = Vehicle::objects()
        .filter_expr(|_| Vehicle::computed().total_price().gte(Expr::literal(100.0_f64)));
}
