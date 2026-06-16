// Issue #462 — cross-model set-op SQL-shape tests.
// Mirrors `set_op_sql_shape.rs`; exercises `union_as` / `union_all_as` /
// `intersect_as` / `except_as` SQL emission via the typed surface.

#[allow(unused_imports)]
use djogi::prelude::*;
