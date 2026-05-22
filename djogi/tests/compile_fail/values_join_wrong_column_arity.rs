//! Verify that passing the wrong number of column names to `InlineValues::new`
//! does not compile.  A two-element row (`(i64, f64)`) requires a two-element
//! column-name tuple; supplying three names is a type error.
use djogi::prelude::*;

fn main() {
    // Row type is (i64, f64), which has Columns = (&'static str, &'static str).
    // Passing a three-tuple should fail to compile.
    let _: InlineValues<(i64, f64)> = InlineValues::new(
        vec![(1_i64, 0.5_f64)],
        "w",
        ("a", "b", "c"),  // ← wrong arity: three names for a two-column row
    )
    .unwrap();
}
