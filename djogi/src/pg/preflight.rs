//! PostgreSQL support-boundary preflight checks.
//!
//! # What
//!
//! Before any database-touching workflow (migration apply, db reset,
//! db seed, status query, live migration, verify), Djogi verifies that
//! the connected PostgreSQL server meets the framework's support
//! boundary. Today that boundary is **PostgreSQL 18+** — see
//! [`docs/spec/decisions.md`] for the design rationale.
//!
//! # Why a separate module
//!
//! The version check is a pool-level concern, not a migration-specific
//! concern. Any code that checks out a connection from `DjogiPool`
//! benefits from knowing the server meets the support floor. Placing
//! the check beside the pool and connection modules keeps it
//! discoverable and reusable without coupling it to the migration
//! engine.
//!
//! # Extension point
//!
//! [`PreflightReport`] carries the parsed version number. Future
//! preflight checks (required extensions, capability probes) can extend
//! this struct without changing the call sites — callers that only care
//! about the version gate ignore the extra fields.

use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiError};

/// Minimum PostgreSQL major version Djogi supports.
///
/// This is a framework-level constant, not an operator-tunable knob.
/// Djogi targets PostgreSQL 18+ exclusively; earlier versions are
/// untested and unsupported.
pub const MINIMUM_PG_MAJOR: u32 = 18;

/// Result of a successful preflight check.
///
/// Carries parsed version fields so callers can log or branch on the
/// exact server version without re-querying. Future preflight checks
/// (required extensions, advisory-lock capability, HeeRanjID function
/// presence) can add fields here without changing existing call sites.
#[derive(Debug, Clone, Copy)]
pub struct PreflightReport {
    /// Raw `server_version_num` value from `SHOW server_version_num`.
    /// Format: `XXYYZZ` where `XX` = major, `YY` = minor, `ZZ` = patch.
    pub server_version_num: u32,
    /// Parsed major version (e.g. 18).
    pub major: u32,
    /// Parsed minor version (e.g. 0).
    pub minor: u32,
}

/// Parse `server_version_num` into `(major, minor)`.
///
/// The format is `XXYYZZ`: `major = num / 10000`, `minor = (num %
/// 10000) / 100`. For PG 10+, minor releases increment the `YY`
/// digits (e.g. PG 18.3 = `180300`, PG 17.4 = `170400`). The `ZZ`
/// digits are zero for release builds. This is the same decomposition
/// `pg_dump`, `psql`, and every Postgres client library uses.
fn parse_version_num(num: u32) -> (u32, u32) {
    (num / 10000, (num % 10000) / 100)
}

/// Evaluate a parsed version number against the minimum requirement.
///
/// Returns a [`PreflightReport`] when the version meets the floor, or
/// [`DjogiError::UnsupportedPostgresVersion`] when it does not. This
/// is the testable core of [`check_postgres_version`] — separated so
/// unit tests can exercise the full parse + compare + error-construction
/// path without a live database connection.
///
/// # Hardening note
///
/// Added by Finding 5 resolution: the original plan tested only the
/// comparison logic in isolation. This function lets tests verify the
/// full error shape (correct `detected_major`, `detected_minor`,
/// `minimum_major` fields and `Display` output) without mocking a
/// Postgres connection.
fn evaluate_version(version_num: u32) -> Result<PreflightReport, DjogiError> {
    let (major, minor) = parse_version_num(version_num);

    if major < MINIMUM_PG_MAJOR {
        return Err(DjogiError::unsupported_postgres_version(
            major,
            minor,
            MINIMUM_PG_MAJOR,
        ));
    }

    Ok(PreflightReport {
        server_version_num: version_num,
        major,
        minor,
    })
}

/// Check that the connected PostgreSQL server meets Djogi's minimum
/// version requirement.
///
/// Checks out one connection from `pool`, queries
/// `SHOW server_version_num`, and compares the major version against
/// [`MINIMUM_PG_MAJOR`]. Returns a [`PreflightReport`] on success or
/// [`DjogiError::UnsupportedPostgresVersion`] when the server is below
/// the floor.
///
/// # When to call
///
/// Call once per top-level CLI command, immediately after pool
/// construction and before the first SQL statement that could produce a
/// confusing secondary failure. Do NOT call inside library functions
/// that may be invoked multiple times per operation (e.g. inside
/// `apply_plan` per migration) — the CLI entry point is the
/// enforcement boundary.
///
/// # Errors
///
/// - [`DjogiError::UnsupportedPostgresVersion`] — server major version
///   is below [`MINIMUM_PG_MAJOR`].
/// - [`DjogiError::Db`] — connection checkout or `SHOW` query failed
///   (network, auth, etc.).
pub async fn check_postgres_version(pool: &DjogiPool) -> Result<PreflightReport, DjogiError> {
    let version_num = query_server_version_num(pool).await?;
    evaluate_version(version_num)
}

/// Wire-level query: check out one connection, run
/// `SHOW server_version_num`, parse the text result to `u32`.
///
/// Separated from [`evaluate_version`] so the comparison and
/// error-construction logic can be unit-tested without a live database.
#[allow(clippy::disallowed_methods)]
async fn query_server_version_num(pool: &DjogiPool) -> Result<u32, DjogiError> {
    pool.with_client(|client| {
        Box::pin(async move {
            let row = client
                .query_one("SHOW server_version_num", &[])
                .await
                .map_err(|e| DjogiError::Db(DbError::from(e)))?;
            let version_str: &str = row.get(0);
            let num: u32 = version_str.parse().map_err(|_| {
                DjogiError::Db(DbError::other(format!(
                    "preflight: server_version_num returned \
                     non-integer value: {version_str:?}"
                )))
            })?;
            Ok(num)
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // parse_version_num — arithmetic correctness
    // ---------------------------------------------------------------
    // All values use real PostgreSQL XXYY00 encoding:
    //   XX = major (2 digits), YY = minor (2 digits), ZZ = 00 (release)
    // Examples from `pg_config --version` / `SHOW server_version_num`:
    //   PG 18.0 → 180000, PG 18.3 → 180300, PG 17.4 → 170400

    #[test]
    fn parse_version_num_pg18_0() {
        let (major, minor) = parse_version_num(180000);
        assert_eq!(major, 18);
        assert_eq!(minor, 0);
    }

    #[test]
    fn parse_version_num_pg18_3() {
        // PG 18.3 = 180300 (XXYY00: XX=18, YY=03, CC=00)
        let (major, minor) = parse_version_num(180300);
        assert_eq!(major, 18);
        assert_eq!(minor, 3);
    }

    #[test]
    fn parse_version_num_pg17_4() {
        let (major, minor) = parse_version_num(170400);
        assert_eq!(major, 17);
        assert_eq!(minor, 4);
    }

    #[test]
    fn parse_version_num_pg16_1() {
        let (major, minor) = parse_version_num(160100);
        assert_eq!(major, 16);
        assert_eq!(minor, 1);
    }

    #[test]
    fn parse_version_num_pg16_0() {
        let (major, minor) = parse_version_num(160000);
        assert_eq!(major, 16);
        assert_eq!(minor, 0);
    }

    #[test]
    fn parse_version_num_pg15_8() {
        let (major, minor) = parse_version_num(150800);
        assert_eq!(major, 15);
        assert_eq!(minor, 8);
    }

    #[test]
    fn parse_version_num_pg19_1() {
        let (major, minor) = parse_version_num(190100);
        assert_eq!(major, 19);
        assert_eq!(minor, 1);
    }

    // ---------------------------------------------------------------
    // evaluate_version — full parse + compare + error-construction
    // (Finding 5 resolution: end-to-end error-path tests)
    // ---------------------------------------------------------------

    #[test]
    fn evaluate_version_below_minimum_returns_unsupported_error() {
        // Real XXYY00 encodings for sub-18 versions.
        for &(num, expected_major, expected_minor) in &[
            (170400u32, 17, 4),
            (160100, 16, 1),
            (150800, 15, 8),
            (140000, 14, 0),
            (100000, 10, 0),
        ] {
            let err =
                evaluate_version(num).expect_err(&format!("version_num {num} should be rejected"));
            let msg = err.to_string();
            match err {
                DjogiError::UnsupportedPostgresVersion {
                    detected_major,
                    detected_minor,
                    minimum_major,
                    ..
                } => {
                    assert_eq!(detected_major, expected_major, "detected_major for {num}");
                    assert_eq!(detected_minor, expected_minor, "detected_minor for {num}");
                    assert_eq!(minimum_major, MINIMUM_PG_MAJOR, "minimum_major for {num}");
                    assert!(
                        msg.contains(&expected_major.to_string()),
                        "display must name detected major for {num}, got: {msg}"
                    );
                    assert!(
                        msg.contains(&MINIMUM_PG_MAJOR.to_string()),
                        "display must name minimum for {num}, got: {msg}"
                    );
                    assert!(
                        msg.contains("Upgrade"),
                        "display must suggest upgrade for {num}, got: {msg}"
                    );
                }
                other => panic!("expected UnsupportedPostgresVersion for {num}, got: {other:?}"),
            }
        }
    }

    #[test]
    fn evaluate_version_at_minimum_returns_report() {
        let report = evaluate_version(180000).expect("PG 18.0 should be accepted");
        assert_eq!(report.major, 18);
        assert_eq!(report.minor, 0);
        assert_eq!(report.server_version_num, 180000);
    }

    #[test]
    fn evaluate_version_above_minimum_returns_report() {
        for &num in &[180300u32, 190000, 190100, 200000] {
            let report = evaluate_version(num)
                .unwrap_or_else(|_| panic!("version_num {num} should be accepted"));
            assert!(
                report.major >= MINIMUM_PG_MAJOR,
                "major for {num} should be >= {MINIMUM_PG_MAJOR}"
            );
            assert_eq!(
                report.server_version_num, num,
                "server_version_num should round-trip for {num}"
            );
        }
    }

    // ---------------------------------------------------------------
    // Threshold comparison (simpler assertions, kept for coverage)
    // ---------------------------------------------------------------

    #[test]
    fn version_below_minimum_is_rejected() {
        for &num in &[170400u32, 160100, 150800, 140000, 100000] {
            let (major, _minor) = parse_version_num(num);
            assert!(
                major < MINIMUM_PG_MAJOR,
                "version_num {num} (major {major}) should be below minimum {MINIMUM_PG_MAJOR}"
            );
        }
    }

    #[test]
    fn version_at_minimum_is_accepted() {
        let (major, _) = parse_version_num(180000);
        assert!(major >= MINIMUM_PG_MAJOR, "PG 18.0 should be accepted");
    }

    #[test]
    fn version_above_minimum_is_accepted() {
        for &num in &[180300u32, 190000, 190100, 200000] {
            let (major, _) = parse_version_num(num);
            assert!(
                major >= MINIMUM_PG_MAJOR,
                "version_num {num} (major {major}) should be accepted"
            );
        }
    }
}
