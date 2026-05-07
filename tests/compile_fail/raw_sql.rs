#[test]
fn raw_sql_escape_hatches_require_bypass_trait() {
    let fixture_dir = std::path::Path::new("../tests/compile_fail/raw_sql");
    let fixture_count = std::fs::read_dir(fixture_dir)
        .expect("raw SQL compile-fail fixture directory must exist")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
        .count();
    assert!(
        fixture_count > 0,
        "raw SQL compile-fail suite has no fixtures"
    );

    let t = trybuild::TestCases::new();
    t.compile_fail("../tests/compile_fail/raw_sql/*.rs");
}
