//! Compile-fail fixture for #89: the variadic `grouping_of(...)` form
//! is metadata-kind and must not expose `.over(...)`.
fn main() {
    let _ = djogi::grouping_of(&["region", "dept"]).over(|w| w);
}
