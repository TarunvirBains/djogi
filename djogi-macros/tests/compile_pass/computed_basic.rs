// Minimal compile-pass fixture for computed fields.
//
// Declares a `Vehicle` model with one `#[computed(sql = "...")]`
// field and exercises the SQL-projectable surface via
// `Vehicle::computed().total_price()`. Proves the descriptor emission
// + `{Model}Computed` ZST + accessor return type all wire together.
//
// Every lihaaf compile-fixture must
// have `fn main() {}` so the stored `.stderr` does not pick up
// E0601. Compile-pass fixtures need it for the same reason — the
// binary still has to link.
//
// also asserts that `total_price` is NOT a
// real struct field after macro expansion. Computed fields are
// virtual and must be stripped before `inject::expand` /
// `from_row::expand` see them, otherwise the projection emits a
// non-existent column in `SELECT {COLUMN_LIST}` and the migration
// differ generates `ADD COLUMN total_price` DDL. The fixture verifies
// the strip by:
//   1. Constructing `Vehicle { base_price, tax_rate, .. }` — succeeds
//      only when `total_price` is absent from the struct (the
//      `Default` impl `inject::expand` emits fills `id`, `created_at`,
//      `updated_at`).
//   2. Asserting `Vehicle::COLUMN_LIST` does not contain `total_price`.

use djogi::__private::pg::FromPgRow;
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
    // Construct without `total_price`. If the macro left the computed
    // field on the struct, this constructor would error with
    // "missing field `total_price` in initializer".
    let _v = Vehicle {
        base_price: 100.0,
        tax_rate: 0.1,
        ..Default::default()
    };

    // `COLUMN_LIST` must not include the computed field — otherwise
    // every `SELECT {COLUMN_LIST} FROM phase8_computed_basic_vehicles`
    // would request a non-existent column at runtime.
    assert!(
        !Vehicle::COLUMN_LIST.contains("total_price"),
        "computed field must not appear in COLUMN_LIST: {}",
        Vehicle::COLUMN_LIST,
    );

    // The SQL-projectable surface still works — `Vehicle::computed()`
    // returns the ZST whose accessors compose with the typed `Expr`
    // API.
    let _qs = Vehicle::objects()
        .filter_expr(|_| Vehicle::computed().total_price().gte(Expr::literal(100.0_f64)));
}
