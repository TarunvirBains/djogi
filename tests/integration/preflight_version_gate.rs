//! Integration tests for the PostgreSQL version preflight gate.
//!
//! These tests exercise the live `check_postgres_version` path against
//! the local development database. The dev environment runs PG 18+,
//! so the "passing" path is verified against a real server.
//!
//! The "failing" path (PG < 18) is covered by unit tests in
//! `djogi/src/pg/preflight.rs` that test the comparison logic in
//! isolation — we do not maintain a PG 17 instance for CI.

/// The live database passes the version preflight.
///
/// This test verifies the full round-trip: pool construction, `SHOW
/// server_version_num` query, integer parse, version comparison. If
/// the dev database is PG 18+, the test passes. If it is below 18,
/// the test fails with `UnsupportedPostgresVersion` — which is the
/// correct behavior and signals a dev-environment misconfiguration.
#[tokio::test]
async fn live_database_passes_version_preflight() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://djogi:djogi@localhost:5432/djogi_test".to_string());
    let pool = djogi::pg::pool::DjogiPool::connect(&url)
        .await
        .expect("pool construction should succeed against dev database");

    let report = djogi::pg::preflight::check_postgres_version(&pool)
        .await
        .expect("dev database should be PG 18+");

    assert!(
        report.major >= 18,
        "dev database major version should be >= 18, got {}",
        report.major,
    );
    assert_eq!(
        report.major,
        report.server_version_num / 10000,
        "major should match server_version_num decomposition"
    );
    assert_eq!(
        report.minor,
        (report.server_version_num % 10000) / 100,
        "minor should match server_version_num decomposition"
    );
}

/// `PreflightReport` fields are internally consistent.
#[tokio::test]
async fn preflight_report_fields_are_consistent() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://djogi:djogi@localhost:5432/djogi_test".to_string());
    let pool = djogi::pg::pool::DjogiPool::connect(&url)
        .await
        .expect("pool construction should succeed");

    let report = djogi::pg::preflight::check_postgres_version(&pool)
        .await
        .expect("preflight should pass");

    // Verify round-trip: major * 10000 + minor * 100 + patch =
    // server_version_num (within the patch range).
    let reconstructed = report.major * 10000 + report.minor * 100;
    assert!(
        report.server_version_num >= reconstructed
            && report.server_version_num < reconstructed + 100,
        "version_num {} should reconstruct to major={} minor={} \
         (expected range [{}, {})), got reconstructed={}",
        report.server_version_num,
        report.major,
        report.minor,
        reconstructed,
        reconstructed + 100,
        reconstructed,
    );
}
