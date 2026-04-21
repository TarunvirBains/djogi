//! Test-harness runtime helpers for `#[djogi_test]`.
//!
//! This module provides the per-test-database lifecycle machinery used by the
//! `#[djogi_test]` proc-macro attribute:
//!
//! 1. Connect to the admin database (via `DATABASE_URL`).
//! 2. Create a fresh `djogi_test_<uuid>` database.
//! 3. Install HeeRanjID schema and seed the default node.
//! 4. Return a `DjogiContext` backed by a pool pointed at the new database.
//! 5. Drop the database via `teardown_test_db` — called explicitly by the
//!    macro-generated wrapper, both on normal return and after a caught panic.
//!
//! # Why sqlx here, through T9
//!
//! Internals use sqlx machinery through Phase 5-Zero T9 per the v3 plan's
//! RQ-1 resolution: swapping the test harness in lock-step with the runtime
//! substrate swap would inflate T2's surface area. T10 rewrites these
//! internals to tokio-postgres + deadpool-postgres and removes sqlx from
//! dev-dependencies entirely.
//!
//! # Database name format
//!
//! Each test gets `djogi_test_<uuid-simple>` — 32 hex characters, no
//! hyphens, fully lowercase. The name is always under 63 bytes (Postgres
//! identifier length limit), is safe as a double-quoted identifier, and
//! contains only ASCII alphanumeric characters plus the `djogi_test_` prefix.
//!
//! # Teardown approach
//!
//! The macro-generated wrapper uses `futures::FutureExt::catch_unwind` to
//! intercept panics from the test body, then calls `teardown_test_db` as an
//! ordinary `async` function before resuming the panic. This avoids the
//! "block_on called from async context" panic that a Drop impl would face
//! inside a `#[tokio::test]` harness.
//!
//! # Usage
//!
//! This module is always compiled (it depends only on crates that are already
//! in the djogi runtime dep-graph: sqlx, heeranjid-sqlx, uuid). Only call its
//! functions from test code — the runtime overhead of importing this module in
//! production is negligible, but its entry points are meaningless outside tests.

use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiContext, DjogiError};
use sqlx::{Executor, PgPool};
use uuid::Uuid;

/// Cleanup token returned by `setup_test_db`.
///
/// Carries the information needed to drop the per-test database. Passed to
/// `teardown_test_db` by the macro-generated wrapper. Not a RAII guard — the
/// cleanup is explicit and async so it runs cleanly inside the Tokio test
/// runtime without hitting the `block_on`-from-async-context constraint.
pub struct TestDbCleanup {
    /// Pool pointed at the admin database, used to issue `DROP DATABASE`.
    admin_pool: PgPool,
    /// The per-test database name — ASCII alphanumeric + underscore,
    /// always double-quoted in SQL.
    db_name: String,
    /// Per-test sqlx pool. Closed before DROP DATABASE is issued so all
    /// connections to the test database are released first.
    test_pool_sqlx: PgPool,
}

/// Set up a fresh per-test database and return the cleanup token + context.
///
/// Called by macro-generated code from `#[djogi_test]`-annotated tests.
/// Do not call directly from production code.
///
/// # Steps
///
/// 1. Read `DATABASE_URL` from the environment (same convention as sqlx::test).
/// 2. Connect to the admin database via `DATABASE_URL`.
/// 3. Generate a unique database name `djogi_test_<uuid-simple>`.
/// 4. Issue `CREATE DATABASE "<name>"`.
/// 5. Connect to the new database.
/// 6. Install HeeRanjID schema + seed the default node.
/// 7. Return `(TestDbCleanup, DjogiContext)`.
///
/// # Errors
///
/// Returns `DjogiError::Db` on framework-generated setup errors and maps the
/// temporary sqlx-based test-harness failures into message-only `DbError`s.
pub async fn setup_test_db() -> Result<(TestDbCleanup, DjogiContext), DjogiError> {
    // Read DATABASE_URL — same env var convention as #[sqlx::test].
    let database_url = std::env::var("DATABASE_URL").map_err(|_| {
        DjogiError::Db(DbError::other(
            "DATABASE_URL env var is not set; djogi_test requires it to connect to Postgres",
        ))
    })?;

    // Connect to the admin database to issue CREATE DATABASE.
    let admin_pool = PgPool::connect(&database_url)
        .await
        .map_err(sqlx_test_harness_error)?;

    // Generate a unique database name: djogi_test_ + 32 hex chars (UUID v4,
    // simple format, no hyphens). Always fits in 63 bytes.
    let unique_suffix = Uuid::new_v4().simple().to_string();
    let db_name = format!("djogi_test_{unique_suffix}");

    // CREATE DATABASE — double-quoted to handle any future non-alphanumeric
    // characters in the prefix (currently not possible, but defensive).
    let create_sql = format!("CREATE DATABASE \"{db_name}\"");
    admin_pool
        .execute(create_sql.as_str())
        .await
        .map_err(sqlx_test_harness_error)?;

    // Build the per-test database URL by replacing the database component.
    let test_url = replace_db_in_url(&database_url, &db_name)?;

    // Connect to the fresh database via sqlx (for heeranjid_sqlx setup).
    let test_pool_sqlx = PgPool::connect(&test_url)
        .await
        .map_err(sqlx_test_harness_error)?;

    // Install HeeRanjID schema (CREATE EXTENSION + functions) and seed node 1.
    heeranjid_sqlx::install_schema(&test_pool_sqlx)
        .await
        .map_err(sqlx_test_harness_error)?;
    heeranjid_sqlx::seed_default_node(&test_pool_sqlx)
        .await
        .map_err(sqlx_test_harness_error)?;

    // Set heer.node_id at the database level so every NEW connection inherits
    // it. This must happen before we close the setup pool and open the app
    // pool, so that all connections in the app pool see node_id = 1 without
    // needing per-connection SET calls.
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
    ))
    .execute(&test_pool_sqlx)
    .await
    .map_err(sqlx_test_harness_error)?;

    // Close the sqlx setup pool so that the DjogiPool starts fresh — all
    // new connections will inherit heer.node_id from the ALTER DATABASE above.
    test_pool_sqlx.close().await;

    // Build the DjogiPool (tokio-postgres / deadpool) for the app context.
    let app_pool = DjogiPool::connect(&test_url).await?;
    let ctx = DjogiContext::from_pool(app_pool);

    // Reconnect sqlx pool for the cleanup token (teardown needs it to DROP DATABASE).
    let cleanup_pool_sqlx = PgPool::connect(&test_url)
        .await
        .map_err(sqlx_test_harness_error)?;

    let cleanup = TestDbCleanup {
        admin_pool,
        db_name,
        test_pool_sqlx: cleanup_pool_sqlx,
    };

    Ok((cleanup, ctx))
}

/// Drop the per-test database created by `setup_test_db`.
///
/// Called by macro-generated code after the test body returns — whether
/// normally or via a caught panic.
pub async fn teardown_test_db(cleanup: TestDbCleanup) {
    let TestDbCleanup {
        admin_pool,
        db_name,
        test_pool_sqlx,
    } = cleanup;

    // Close the per-test pool first. This releases all connections to the test
    // database so DROP DATABASE can succeed.
    test_pool_sqlx.close().await;

    let sql = format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)");
    if let Err(e) = admin_pool.execute(sql.as_str()).await {
        eprintln!("[djogi_test] WARNING: failed to drop test database \"{db_name}\": {e}");
    }
}

/// Replace the database name component in a Postgres connection URL.
///
/// Accepts URLs in the form `postgres://user:pass@host:port/dbname` or
/// `postgresql://user:pass@host:port/dbname`. The function finds the last
/// `/` in the URL (which precedes the database name) and replaces everything
/// after it with `new_db`.
///
/// # Errors
///
/// Returns an error if the URL does not contain a `/` after the scheme.
fn replace_db_in_url(url: &str, new_db: &str) -> Result<String, DjogiError> {
    let last_slash = url.rfind('/').ok_or_else(|| {
        DjogiError::Db(DbError::other(
            "DATABASE_URL does not contain a database name component (no '/' found)",
        ))
    })?;
    let base = &url[..=last_slash]; // includes the trailing slash
    Ok(format!("{base}{new_db}"))
}

fn sqlx_test_harness_error(error: impl std::fmt::Display) -> DjogiError {
    DjogiError::Db(DbError::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::replace_db_in_url;

    #[test]
    fn replace_db_preserves_host_and_port() {
        let url = "postgres://user:pass@localhost:5432/old_db";
        let result = replace_db_in_url(url, "new_db").unwrap();
        assert_eq!(result, "postgres://user:pass@localhost:5432/new_db");
    }

    #[test]
    fn replace_db_no_port() {
        let url = "postgres://user:pass@localhost/old_db";
        let result = replace_db_in_url(url, "new_db").unwrap();
        assert_eq!(result, "postgres://user:pass@localhost/new_db");
    }

    #[test]
    fn replace_db_postgresql_scheme() {
        let url = "postgresql://localhost/old_db";
        let result = replace_db_in_url(url, "fresh_db").unwrap();
        assert_eq!(result, "postgresql://localhost/fresh_db");
    }
}
