//! Compile-fail fixture for #89: the variadic `grouping_of(...)` form
//! is metadata-kind and must not expose `.filter(...)`.
fn main() {
    let _ = djogi::grouping_of(&["region", "dept"]).filter(djogi::expr::Expr::literal(true));
}
