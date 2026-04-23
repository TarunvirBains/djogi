//! Test-harness runtime helpers for `#[djogi_test]`.
//!
//! This module provides the per-test-database lifecycle machinery used by the
//! `#[djogi_test]` proc-macro attribute:
//!
//! 1. Connect to the admin database (via `DATABASE_URL`).
//! 2. Create a fresh `djogi_test_<uuid>` database.
//! 3. Install HeeRanjID schema and seed the default node.
//! 4. Optionally provision extra Postgres extensions (e.g. `postgis`) via
//!    `CREATE EXTENSION IF NOT EXISTS` — driven by the
//!    `#[djogi_test(extensions = [...])]` attribute argument.
//! 5. Return a `DjogiContext` backed by a pool pointed at the new database.
//! 6. Drop the database via `teardown_test_db` — called explicitly by the
//!    macro-generated wrapper, both on normal return and after a caught panic.
//!
//! # Substrate
//!
//! Internals use `tokio_postgres` directly (no sqlx) and call the
//! `heeranjid::postgres_schema` helpers from heeranjid 0.2.1 for schema
//! installation and node seeding.
//!
//! # Database name format
//!
//! Each test gets `djogi_test_<uuid-simple>` — 32 hex characters, no
//! hyphens, fully lowercase. The name is always under 63 bytes (Postgres
//! identifier length limit), is safe as a double-quoted identifier, and
//! contains only ASCII alphanumeric characters plus the `djogi_test_` prefix.
//!
//! # Extension name validation
//!
//! Extension names flow from Rust source (attribute arguments) into SQL as
//! quoted identifiers. To keep that path safe even when the attribute is
//! written by hand, `setup_test_db_with_extensions` rejects any name that
//! is not a plain Postgres identifier: ASCII letter or underscore followed
//! by ASCII letters, digits, or underscores, from 1 to 63 bytes total. This
//! matches how Postgres itself tokenizes unquoted identifiers and covers
//! every real extension on PGXN and `contrib/`. Per the Djogi-wide no-regex
//! rule (`docs/spec/decisions.md`), the validator is implemented with
//! stdlib byte primitives only.
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
//! in the djogi runtime dep-graph: tokio-postgres, heeranjid, uuid). Only call
//! its functions from test code — the runtime overhead of importing this module
//! in production is negligible, but its entry points are meaningless outside
//! tests.

use crate::pg::pool::DjogiPool;
use crate::{DbError, DjogiContext, DjogiError};
use tokio_postgres::NoTls;
use uuid::Uuid;

/// Cleanup token returned by `setup_test_db`.
///
/// Carries the information needed to drop the per-test database. Passed to
/// `teardown_test_db` by the macro-generated wrapper. Not a RAII guard — the
/// cleanup is explicit and async so it runs cleanly inside the Tokio test
/// runtime without hitting the `block_on`-from-async-context constraint.
pub struct TestDbCleanup {
    /// Admin database URL, used to reconnect for `DROP DATABASE`.
    admin_url: String,
    /// The per-test database name — ASCII alphanumeric + underscore,
    /// always double-quoted in SQL.
    db_name: String,
}

/// Set up a fresh per-test database and return the cleanup token + context.
///
/// Convenience wrapper over [`setup_test_db_with_extensions`] for callers
/// that do not need any extra Postgres extensions beyond the HeeRanjID
/// schema. Equivalent to passing an empty extensions slice.
///
/// Called by macro-generated code from `#[djogi_test]`-annotated tests that
/// omit the `extensions = [...]` argument. Also available for direct use
/// from hand-written test harnesses that do not go through the attribute.
///
/// # Errors
///
/// Returns `DjogiError::Db` on all setup failures.
pub async fn setup_test_db() -> Result<(TestDbCleanup, DjogiContext), DjogiError> {
    setup_test_db_with_extensions(&[]).await
}

/// Set up a fresh per-test database, provision extra extensions, and return
/// the cleanup token + context.
///
/// Called by macro-generated code from `#[djogi_test(extensions = [...])]`
/// tests. Also available for direct use from hand-written test harnesses.
///
/// # Steps
///
/// 1. Read `DATABASE_URL` from the environment (same convention as sqlx::test).
/// 2. Connect to the admin database via `tokio_postgres`.
/// 3. Generate a unique database name `djogi_test_<uuid-simple>`.
/// 4. Issue `CREATE DATABASE "<name>"`.
/// 5. Connect to the new database via `tokio_postgres`.
/// 6. Install HeeRanjID schema + seed the default node via
///    `heeranjid::postgres_schema::install_schema` and `seed_default_node`.
/// 7. For each entry in `extensions`, validate the name against the
///    identifier rule (see module docs) and issue
///    `CREATE EXTENSION IF NOT EXISTS "<name>"`. `IF NOT EXISTS` means a
///    repeated name or a pre-installed extension is a no-op.
/// 8. Set `heer.node_id = '1'` at the database level so every new connection
///    inherits it without a per-connection SET call.
/// 9. Open a `DjogiPool` (deadpool-postgres) and return it as a `DjogiContext`.
///
/// # Errors
///
/// Returns `DjogiError::Db` on all setup failures. Failure modes that
/// mention the offending extension by name:
///
/// - The extension name does not match the identifier rule (handled in Rust).
/// - The Postgres server rejects `CREATE EXTENSION IF NOT EXISTS` — for
///   example, when the named extension is not installed on the server
///   (missing `.control` file). The original `tokio_postgres::Error` is
///   preserved in the `DjogiError::Db` source chain so the adopter can see
///   the full server message.
pub async fn setup_test_db_with_extensions(
    extensions: &[&str],
) -> Result<(TestDbCleanup, DjogiContext), DjogiError> {
    // Validate extension names up front so we fail before CREATE DATABASE
    // when the attribute author made a typo. This means an invalid name
    // does not leak a stray `djogi_test_<uuid>` database.
    for name in extensions {
        validate_extension_name(name)?;
    }

    // Read DATABASE_URL — same env var convention as #[sqlx::test].
    let database_url = std::env::var("DATABASE_URL").map_err(|_| {
        DjogiError::Db(DbError::other(
            "DATABASE_URL env var is not set; djogi_test requires it to connect to Postgres",
        ))
    })?;

    // Connect to the admin database to issue CREATE DATABASE.
    let (admin_client, admin_conn) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("admin connect failed: {e}"))))?;

    // Spawn the connection driver — must be running while admin_client is alive.
    tokio::spawn(async move {
        if let Err(e) = admin_conn.await {
            eprintln!("[djogi_test] admin connection error: {e}");
        }
    });

    // Generate a unique database name: djogi_test_ + 32 hex chars (UUID v4,
    // simple format, no hyphens). Always fits in 63 bytes.
    let unique_suffix = Uuid::new_v4().simple().to_string();
    let db_name = format!("djogi_test_{unique_suffix}");

    // CREATE DATABASE — double-quoted to handle any future non-alphanumeric
    // characters in the prefix (currently not possible, but defensive).
    let create_sql = format!("CREATE DATABASE \"{db_name}\"");
    admin_client
        .batch_execute(&create_sql)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("CREATE DATABASE failed: {e}"))))?;

    // Build the per-test database URL by replacing the database component.
    let test_url = replace_db_in_url(&database_url, &db_name)?;

    // Connect to the fresh database for HeeRanjID setup.
    let (test_client, test_conn) = tokio_postgres::connect(&test_url, NoTls)
        .await
        .map_err(|e| DjogiError::Db(DbError::other(format!("test DB connect failed: {e}"))))?;

    tokio::spawn(async move {
        if let Err(e) = test_conn.await {
            eprintln!("[djogi_test] test connection error: {e}");
        }
    });

    // Install HeeRanjID schema (CREATE EXTENSION + functions) via heeranjid 0.2.1.
    heeranjid::postgres_schema::install_schema(&test_client)
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "heeranjid install_schema failed: {e}"
            )))
        })?;

    // Seed the default node (node_id = 1).
    heeranjid::postgres_schema::seed_default_node(&test_client)
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "heeranjid seed_default_node failed: {e}"
            )))
        })?;

    // Auto-provision extra extensions requested via `#[djogi_test(extensions = [...])]`.
    // Each name is already validated above, so interpolating it into a
    // double-quoted identifier is safe. `IF NOT EXISTS` makes this a no-op
    // if the extension is already present (for example, if a previous step
    // pulled it in transitively).
    for name in extensions {
        let sql = format!("CREATE EXTENSION IF NOT EXISTS \"{name}\"");
        test_client.batch_execute(&sql).await.map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "failed to create Postgres extension `{name}` \
                 (is it installed on the server?): {e}"
            )))
        })?;
    }

    // Set heer.node_id at the database level so every NEW connection inherits
    // it. This must happen before we open the app pool, so that all connections
    // in the DjogiPool see node_id = 1 without needing per-connection SET calls.
    admin_client
        .batch_execute(&format!(
            "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
        ))
        .await
        .map_err(|e| {
            DjogiError::Db(DbError::other(format!(
                "ALTER DATABASE SET heer.node_id failed: {e}"
            )))
        })?;

    // The setup client is dropped here — the connection driver task will finish.
    // The DjogiPool opens fresh connections that will inherit heer.node_id.
    drop(test_client);

    // Build the DjogiPool (tokio-postgres / deadpool) for the app context.
    let app_pool = DjogiPool::connect(&test_url).await?;
    let ctx = DjogiContext::from_pool(app_pool);

    let cleanup = TestDbCleanup {
        admin_url: database_url,
        db_name,
    };

    Ok((cleanup, ctx))
}

/// Drop the per-test database created by `setup_test_db`.
///
/// Called by macro-generated code after the test body returns — whether
/// normally or via a caught panic.
pub async fn teardown_test_db(cleanup: TestDbCleanup) {
    let TestDbCleanup { admin_url, db_name } = cleanup;

    // Reconnect to admin database to issue DROP DATABASE.
    match tokio_postgres::connect(&admin_url, NoTls).await {
        Err(e) => {
            eprintln!(
                "[djogi_test] WARNING: failed to connect to admin DB for teardown \
                 (database \"{db_name}\" may need manual cleanup): {e}"
            );
        }
        Ok((admin_client, admin_conn)) => {
            tokio::spawn(async move {
                if let Err(e) = admin_conn.await {
                    eprintln!("[djogi_test] teardown connection error: {e}");
                }
            });

            let sql = format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)");
            if let Err(e) = admin_client.batch_execute(&sql).await {
                eprintln!("[djogi_test] WARNING: failed to drop test database \"{db_name}\": {e}");
            }
        }
    }
}

/// Validate that `name` is a plain Postgres identifier safe to interpolate
/// into a `CREATE EXTENSION IF NOT EXISTS "<name>"` statement.
///
/// Rules (byte-level, no regex per the Djogi-wide no-regex policy):
///
/// - Length between 1 and 63 bytes inclusive (Postgres `NAMEDATALEN` minus
///   the trailing `NUL`).
/// - First byte is an ASCII letter (upper- or lower-case) or underscore.
/// - Every subsequent byte is an ASCII letter, digit, or underscore.
///
/// All real-world extension names on PGXN and in `contrib/` (`postgis`,
/// `pg_trgm`, `pgcrypto`, `uuid-ossp` — wait, that last one contains a
/// hyphen, but it is itself aliased to `"uuid-ossp"` double-quoted) match
/// this rule with one caveat: extensions whose names require double
/// quoting are rejected here. The caveat is documented on the
/// [`setup_test_db_with_extensions`] entry point.
fn validate_extension_name(name: &str) -> Result<(), DjogiError> {
    let bytes = name.as_bytes();

    if bytes.is_empty() {
        return Err(DjogiError::Db(DbError::other(
            "djogi_test: extension name must not be empty",
        )));
    }
    if bytes.len() > 63 {
        return Err(DjogiError::Db(DbError::other(format!(
            "djogi_test: extension name `{name}` exceeds 63-byte Postgres identifier limit",
        ))));
    }

    let first = bytes[0];
    let first_ok = first.is_ascii_alphabetic() || first == b'_';
    if !first_ok {
        return Err(DjogiError::Db(DbError::other(format!(
            "djogi_test: extension name `{name}` must start with an ASCII letter or underscore",
        ))));
    }

    for &b in &bytes[1..] {
        let ok = b.is_ascii_alphanumeric() || b == b'_';
        if !ok {
            return Err(DjogiError::Db(DbError::other(format!(
                "djogi_test: extension name `{name}` contains invalid byte `{c}` \
                 (only ASCII letters, digits, and underscores are allowed)",
                c = b as char,
            ))));
        }
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{replace_db_in_url, validate_extension_name};

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

    #[test]
    fn validate_extension_name_accepts_real_extensions() {
        validate_extension_name("postgis").unwrap();
        validate_extension_name("pg_trgm").unwrap();
        validate_extension_name("pgcrypto").unwrap();
        validate_extension_name("_leading_underscore").unwrap();
        validate_extension_name("WithMixedCase").unwrap();
        validate_extension_name("ext1").unwrap();
    }

    #[test]
    fn validate_extension_name_rejects_empty() {
        let err = validate_extension_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_extension_name_rejects_too_long() {
        let name = "a".repeat(64);
        let err = validate_extension_name(&name).unwrap_err();
        assert!(err.to_string().contains("exceeds 63-byte"), "{err}");
    }

    #[test]
    fn validate_extension_name_rejects_leading_digit() {
        let err = validate_extension_name("1ext").unwrap_err();
        assert!(
            err.to_string().contains("must start with"),
            "error message should mention leading character rule: {err}",
        );
    }

    #[test]
    fn validate_extension_name_rejects_injection_attempts() {
        // These would be catastrophic if interpolated into SQL unvalidated.
        // Each must fail validation before any DB work happens.
        for candidate in [
            "postgis; DROP DATABASE postgres",
            "a\"b",
            "a b",
            "a-b",
            "a.b",
            "a/b",
            "\"postgis\"",
        ] {
            let err = validate_extension_name(candidate).unwrap_err();
            assert!(
                err.to_string().contains("invalid byte")
                    || err.to_string().contains("must start with"),
                "expected validation failure for `{candidate}`, got: {err}",
            );
        }
    }
}
