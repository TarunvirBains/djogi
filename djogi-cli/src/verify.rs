//! `djogi verify` — read-only HMAC cross-check of on-disk
//! `schema_snapshot.json` files against the `djogi_ddl_audit` ledger
//! living on the `crud_log_url` audit DB.
//!
//! # What this command does
//!
//! For every snapshot file under
//! `migrations/<target>/<app>/schema_snapshot.json`:
//!
//! 1. Read the bytes from disk.
//! 2. Compute `sign_snapshot(bytes, &key)` where `key` comes from
//!    `DJOGI_SNAPSHOT_SIGNING_KEY` (or the no-op zero key when the
//!    env var is unset — same sentinel contract the runner uses, see
//!    [`djogi::snapshot::sign`]).
//! 3. SELECT the latest `snapshot_signature_hex` from
//!    `djogi_ddl_audit` for `(target_database, app_label)` from the
//!    audit DB.
//! 4. Compare the computed hex against the stored hex. Print
//!    `OK <path>` for matches and `MISMATCH <path>: expected …, got …`
//!    on stderr otherwise.
//!
//! # Read-only by contract (v3 §470)
//!
//! Verify never issues `INSERT`, `UPDATE`, `DELETE`, or DDL — the only
//! SQL leaving the CLI is the single `SELECT` on `djogi_ddl_audit`. If
//! the audit table does not exist the query surfaces SQLSTATE `42P01`
//! (`undefined_table`); the runner CATCHES that and treats the snapshot
//! as `Skipped` (warn on stderr, exit code unchanged) per v3 §824 risk
//! row 11. The verify path itself NEVER bootstraps the table — that is
//! the migration runner's job (T9.5).
//!
//! # Audit DB URL resolution
//!
//! The "audit DB" is the same database the runner writes to via
//! `RunnerCtx::audit_pool` (T9.4). Its URL is resolved in priority
//! order:
//!
//! 1. `DJOGI_CRUD_LOG_URL` env var — explicit override for operators
//!    who keep the audit DB on a separate authority.
//! 2. `derive_per_database_url(&config.database.url, "crud_log")` —
//!    splice `crud_log` into the application URL's path component.
//!    Matches the on-disk migration tree convention
//!    (`migrations/crud_log/<app>/`) the bootstrap layer documents
//!    in [`djogi::migrate::target`].
//!
//! When neither resolves to a usable URL, verify surfaces
//! [`VerifyError::Config`] and exits `1` (config / runtime error).
//!
//! # Exit code semantics (matches Phase 7 ledger-verify)
//!
//! - `0` — every snapshot scanned reported `Ok` or `Skipped`.
//! - `1` — at least one snapshot reported `Mismatch`, OR a runtime
//!   error occurred (config load, key decode, audit pool unreachable,
//!   walkdir I/O).
//!
//! `Skipped` (audit table absent) does NOT count as a mismatch — the
//! cross-check is best-effort when the operator has not provisioned
//! the second DB. The `tracing`-style warn line on stderr makes the
//! skip visible to the operator.
//!
//! # Determinism
//!
//! Snapshot files are walked via [`djogi::migrate::scan_filesystem`]
//! which returns a `BTreeSet<FilesystemBucket>` — already sorted by
//! `(database, app)`. Verify converts that to a `Vec` and does NOT
//! re-shuffle, so failure messages are reproducible across machines.
//! Symlinks are not followed (the scanner uses `file_type()` which
//! returns `false` for `is_dir()` on symlinks).
//!
//! # Spec / memory anchors
//!
//! - v3 plan §452 (snapshot signing surface)
//! - v3 plan §459–460 (audit cross-check contract)
//! - v3 plan §470 (read-only verify)
//! - v3 plan §824 (graceful absence of audit table)
//! - Plan §T9.6 (`docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`)

use std::path::PathBuf;
use std::process::ExitCode;

use djogi::config::DjogiConfig;
use djogi::migrate::{
    FilesystemBucket, SNAPSHOT_FILENAME, app_dirname, derive_per_database_url, migrations_root,
    scan_filesystem,
};
use djogi::pg::pool::DjogiPool;
use djogi::snapshot::sign::{SnapshotKeyError, load_signing_key_from_env, sign_snapshot};

/// Errors surfaced by [`run`]. Each variant carries enough context for
/// an operator to act without grepping source — the I/O variants name
/// the path, the key-decode variant carries the underlying
/// `SnapshotKeyError`, and the audit-pool variant records the URL we
/// failed to reach.
#[derive(Debug)]
pub enum VerifyError {
    /// Filesystem error walking the workspace's `migrations/` tree or
    /// reading a snapshot file.
    Io {
        /// Path the operation was attempted against.
        path: PathBuf,
        source: std::io::Error,
    },
    /// `DJOGI_SNAPSHOT_SIGNING_KEY` was set but malformed. Surfaced
    /// rather than silently degrading to the no-op sentinel — see
    /// [`load_signing_key_from_env`] documentation.
    KeyDecode(SnapshotKeyError),
    /// Could not connect to the audit database. The URL is included
    /// for diagnostics; the underlying error is preserved as the
    /// `Display` source.
    AuditPoolUnreachable {
        /// The audit DB URL we attempted to connect to. Included so
        /// operator logs surface the resolution path (env var vs.
        /// derived from `database.url`).
        url: String,
        /// Underlying connection error message — the `DjogiError`
        /// types do not implement `Send + Sync` in every variant we
        /// might receive, so we capture the rendered string here for
        /// stable display.
        message: String,
    },
    /// Reading `Djogi.toml` (and its env overlays) failed.
    Config(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Io { path, source } => {
                write!(f, "I/O error at {}: {source}", path.display())
            }
            VerifyError::KeyDecode(err) => {
                write!(f, "DJOGI_SNAPSHOT_SIGNING_KEY: {err}")
            }
            VerifyError::AuditPoolUnreachable { url, message } => write!(
                f,
                "audit DB at `{url}` unreachable: {message} \
                 (set DJOGI_CRUD_LOG_URL or check Djogi.toml::database.url)",
            ),
            VerifyError::Config(message) => write!(f, "config load: {message}"),
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VerifyError::Io { source, .. } => Some(source),
            VerifyError::KeyDecode(err) => Some(err),
            VerifyError::AuditPoolUnreachable { .. } | VerifyError::Config(_) => None,
        }
    }
}

/// Outcome of verifying a single snapshot file.
///
/// `Skipped` is distinct from `Ok` so the caller can report to
/// operators that an audit-table-absent situation was tolerated rather
/// than silently passing the verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Computed signature matches the audit ledger's recorded hex.
    Ok,
    /// Computed signature does NOT match — drives a non-zero exit.
    Mismatch,
    /// Audit table absent (`42P01`) on the configured audit DB. Per
    /// v3 §824, this is graceful — the cross-check is skipped and the
    /// exit code is unaffected.
    Skipped,
}

/// One verified `(snapshot path, status)` pair. Returned from the pure
/// verification loop so tests can assert on the structured outcome
/// rather than parsing stdout/stderr.
///
/// `path` and `bucket` carry diagnostic context for the T9.7
/// integration suite (and any future programmatic caller) — the
/// binary's `main` only consumes `outcome` for the exit-code
/// computation. The `dead_code` allow on the struct keeps clippy
/// quiet about the un-read fields without losing them from the
/// surface.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VerifyEntry {
    /// Absolute path to the snapshot file we cross-checked.
    pub path: PathBuf,
    /// `(target_database, app_label)` extracted from the path, where
    /// `app_label` is the in-memory form (empty string for
    /// `_global_/`).
    pub bucket: FilesystemBucket,
    /// Verification outcome.
    pub outcome: VerifyOutcome,
}

/// `djogi verify` entry point — consumed by `main.rs::TopCommand::Verify`.
///
/// `workspace`: optional workspace-root override. Defaults to
/// `std::env::current_dir()`.
///
/// Returns:
/// - `ExitCode::SUCCESS` when every entry is `Ok` or `Skipped`.
/// - `ExitCode::from(1)` when at least one entry is `Mismatch` OR a
///   runtime error stops the verification before completion.
///
/// All operator-facing diagnostics are printed to stderr — stdout is
/// reserved for the per-snapshot `OK <path>` lines so a downstream
/// `grep` / `wc -l` is ergonomic.
pub async fn run(workspace: Option<PathBuf>) -> Result<ExitCode, VerifyError> {
    // Step 1 — resolve workspace, load config.
    let workspace =
        workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let config = DjogiConfig::load_from_workspace(&workspace)
        .map_err(|e| VerifyError::Config(e.to_string()))?;

    // Step 2 — load the signing key. Unset → no-op sentinel.
    // Malformed → propagate as VerifyError::KeyDecode (do NOT silently
    // fall back; that's the regression T9.3's fix-up prevented).
    let key = match load_signing_key_from_env() {
        Ok(Some(k)) => k,
        Ok(None) => [0u8; 32],
        Err(e) => return Err(VerifyError::KeyDecode(e)),
    };

    // Step 3 — discover snapshot files. `scan_filesystem` returns a
    // BTreeSet sorted by (database, app); we materialise a Vec in the
    // same order so iteration is deterministic.
    let mut buckets: Vec<FilesystemBucket> = scan_filesystem(&workspace)
        .map_err(|e| VerifyError::Io {
            path: migrations_root(&workspace),
            source: e,
        })?
        .into_iter()
        .collect();
    // Defence-in-depth: BTreeSet IS sorted, but we re-sort explicitly
    // so the determinism contract does not depend on a future
    // implementation detail of the scanner.
    buckets.sort();

    // Step 4 — resolve the audit DB URL. Env var wins; otherwise
    // derive `crud_log` from `database.url`.
    let audit_url = resolve_audit_url(&config);

    // Step 5 — connect to the audit DB once. Re-use one pool for every
    // snapshot's per-bucket query.
    let pool = match audit_url.as_deref() {
        Some(url) => match DjogiPool::connect(url).await {
            Ok(p) => Some((url.to_string(), p)),
            Err(e) => {
                return Err(VerifyError::AuditPoolUnreachable {
                    url: url.to_string(),
                    message: e.to_string(),
                });
            }
        },
        None => {
            // No URL we can derive — surface as a config error rather
            // than silently treating every snapshot as Skipped, which
            // would erode the meaning of the cross-check.
            return Err(VerifyError::Config(
                "cannot resolve audit DB URL: set DJOGI_CRUD_LOG_URL or ensure \
                 Djogi.toml::database.url has a path component to splice"
                    .to_string(),
            ));
        }
    };

    // Step 6 — verify each snapshot. Collect entries; the print +
    // exit-code calculation happens after the loop so the output
    // ordering is deterministic.
    let mut entries: Vec<VerifyEntry> = Vec::with_capacity(buckets.len());
    let (audit_url_for_log, audit_pool) = pool.expect("pool established at step 5");
    let mut audit_ctx = djogi::context::DjogiContext::from_pool(audit_pool);

    for bucket in &buckets {
        let snapshot = workspace
            .join("migrations")
            .join(&bucket.database)
            .join(app_dirname(&bucket.app))
            .join(SNAPSHOT_FILENAME);
        if !snapshot.is_file() {
            // No snapshot for this bucket — typical of a fresh
            // `migrations/<db>/<app>/` directory before the first
            // compose. Skip without reporting; nothing to verify.
            continue;
        }

        let bytes = std::fs::read(&snapshot).map_err(|e| VerifyError::Io {
            path: snapshot.clone(),
            source: e,
        })?;
        let computed = sign_snapshot(&bytes, &key);
        let computed_hex = signature_to_hex_local(&computed);

        let stored = match fetch_audit_signature(
            &mut audit_ctx,
            &bucket.database,
            &bucket.app,
            &audit_url_for_log,
        )
        .await
        {
            Ok(opt) => opt,
            Err(FetchAuditError::TableAbsent) => {
                // 42P01 — graceful skip per v3 §824.
                eprintln!(
                    "warn: djogi_ddl_audit absent on `{audit_url_for_log}` — \
                     skipping cross-check for {}/{} (snapshot at {})",
                    bucket.database,
                    if bucket.app.is_empty() {
                        "_global_"
                    } else {
                        &bucket.app
                    },
                    snapshot.display()
                );
                entries.push(VerifyEntry {
                    path: snapshot,
                    bucket: bucket.clone(),
                    outcome: VerifyOutcome::Skipped,
                });
                continue;
            }
            Err(FetchAuditError::Other(message)) => {
                return Err(VerifyError::AuditPoolUnreachable {
                    url: audit_url_for_log.clone(),
                    message,
                });
            }
        };

        let outcome = match stored {
            Some(stored_hex) if eq_ignore_ascii_case_hex(&stored_hex, &computed_hex) => {
                VerifyOutcome::Ok
            }
            Some(stored_hex) => {
                eprintln!(
                    "MISMATCH {}: expected {stored_hex}, got {computed_hex}",
                    snapshot.display()
                );
                VerifyOutcome::Mismatch
            }
            None => {
                // Audit table exists but no row for this bucket — treat
                // as Skipped, mirroring the table-absent case. The
                // operator either has not yet applied any migrations
                // for this bucket (audit row is the post-apply
                // artefact) or the audit DB was provisioned after the
                // last apply.
                eprintln!(
                    "warn: no djogi_ddl_audit row for {}/{} — skipping",
                    bucket.database,
                    if bucket.app.is_empty() {
                        "_global_"
                    } else {
                        &bucket.app
                    }
                );
                VerifyOutcome::Skipped
            }
        };

        if matches!(outcome, VerifyOutcome::Ok) {
            println!("OK {}", snapshot.display());
        }

        entries.push(VerifyEntry {
            path: snapshot,
            bucket: bucket.clone(),
            outcome,
        });
    }

    // Step 7 — exit code: any Mismatch → 1; otherwise 0.
    let any_mismatch = entries
        .iter()
        .any(|e| matches!(e.outcome, VerifyOutcome::Mismatch));
    Ok(if any_mismatch {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Resolve the audit DB URL — env var first, then derive from
/// `database.url`. Returns `None` when neither path produces a URL.
fn resolve_audit_url(config: &DjogiConfig) -> Option<String> {
    if let Ok(url) = std::env::var("DJOGI_CRUD_LOG_URL")
        && !url.is_empty()
    {
        return Some(url);
    }
    derive_per_database_url(&config.database.url, "crud_log")
}

/// Result of trying to fetch a single audit row. `TableAbsent` is the
/// `42P01` graceful path; `Other` carries the rendered error for the
/// non-graceful path.
enum FetchAuditError {
    /// `djogi_ddl_audit` does not exist on the audit DB.
    TableAbsent,
    /// Anything else — connection drop, syntax error, etc.
    Other(String),
}

/// Query the latest `snapshot_signature_hex` for
/// `(target_database, app_label)`. Returns `Ok(None)` when no row
/// matches but the table exists, `Err(TableAbsent)` on SQLSTATE
/// `42P01`, and `Err(Other)` on any other failure.
///
/// **Read-only.** The only SQL emitted is a single `SELECT` with
/// positional binds. No `INSERT` / `UPDATE` / `DELETE` / DDL. The
/// `_audit_url` parameter is unused inside the function but kept on the
/// signature so call sites keep the URL handy for the error path
/// without re-resolving it.
async fn fetch_audit_signature(
    ctx: &mut djogi::context::DjogiContext,
    target_database: &str,
    app_label: &str,
    _audit_url: &str,
) -> Result<Option<String>, FetchAuditError> {
    // ORDER BY id DESC LIMIT 1 picks the most recent row; `id` is
    // BIGSERIAL so DESC ordering matches the wall-clock ordering of
    // `applied_at` for any single-writer audit DB (which is the only
    // shape the runner produces). Phase 11 may add a tiebreak on
    // `applied_at` when the audit DB sees concurrent writers.
    let sql = "SELECT snapshot_signature_hex FROM djogi_ddl_audit \
               WHERE target_database = $1 AND app_label = $2 \
               ORDER BY id DESC LIMIT 1";
    match ctx.raw_rows(sql, &[&target_database, &app_label]).await {
        Ok(rows) => {
            if let Some(row) = rows.first() {
                let hex: Option<String> = row.try_get(0).map_err(|e| {
                    FetchAuditError::Other(format!("decoding snapshot_signature_hex: {e}"))
                })?;
                Ok(hex)
            } else {
                Ok(None)
            }
        }
        Err(djogi::DjogiError::Db(db)) => {
            // `42P01` = `undefined_table`. Per v3 §824 we treat this
            // as a graceful skip — operators who have not provisioned
            // the audit DB yet should not see a hard verify failure.
            if let Some(code) = db.code()
                && code == &tokio_postgres::error::SqlState::UNDEFINED_TABLE
            {
                Err(FetchAuditError::TableAbsent)
            } else {
                Err(FetchAuditError::Other(db.to_string()))
            }
        }
        Err(other) => Err(FetchAuditError::Other(other.to_string())),
    }
}

/// Encode 32-byte HMAC output as 64-character UPPERCASE hex.
///
/// Matches [`djogi::migrate::audit::signature_to_hex`] byte-for-byte.
/// Re-implemented here rather than imported to keep the verify path's
/// dependency surface tight (we only need the encoder, not the rest of
/// the audit module). When the audit module's encoder is exposed as a
/// stable, generic helper a follow-up commit may collapse the two; for
/// now they are independently tested.
fn signature_to_hex_local(sig: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(64);
    for &byte in sig {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// ASCII-case-insensitive equality on hex strings. The runner emits
/// uppercase (per [`djogi::migrate::audit::signature_to_hex`]) but
/// older audit rows may be lowercase; tolerate both rather than
/// flagging a stale audit DB as a hard mismatch.
fn eq_ignore_ascii_case_hex(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

/// Read [`djogi::migrate::target::SNAPSHOT_FILENAME`] in case the
/// upstream constant value drifts. Surface as a `&'static str` so
/// callers don't pull in the path machinery.
#[cfg(test)]
const TEST_SNAPSHOT_FILENAME: &str = SNAPSHOT_FILENAME;

#[cfg(test)]
mod tests {
    //! Pure unit tests that don't touch the network. The four
    //! integration tests called out in the plan
    //! (`verify_clean_returns_zero`, `verify_mismatch_returns_one`,
    //! `verify_skips_when_audit_table_absent`,
    //! `verify_no_op_key_passes_zero_signature`) require a real
    //! audit DB; they are deferred to T9.7's
    //! `phase8_djogi_verify_cli` integration suite which spins up a
    //! per-test `crud_log_url` database via `#[djogi_test]` and
    //! invokes the compiled `djogi` binary end-to-end. That layer is
    //! the only place the full DB-touching contract can run; this
    //! unit-test surface covers the helpers.
    //!
    //! The integration tests' assertions match the plan §T9.6 brief:
    //!
    //! - `verify_clean_returns_zero` — fixture workspace with
    //!   matching snapshot + audit row → exit 0, `OK <path>` on
    //!   stdout.
    //! - `verify_mismatch_returns_one` — snapshot bytes tampered
    //!   after audit row was written → exit 1, `MISMATCH …` line on
    //!   stderr.
    //! - `verify_skips_when_audit_table_absent` — audit DB has no
    //!   `djogi_ddl_audit` table → exit 0, `warn: djogi_ddl_audit
    //!   absent …` line on stderr.
    //! - `verify_no_op_key_passes_zero_signature` — env var unset,
    //!   audit row carries 64 zero hex characters → exit 0.

    use super::*;

    #[test]
    fn signature_to_hex_matches_audit_encoder_for_zero() {
        // Cross-check against `djogi::migrate::audit::signature_to_hex`
        // for the all-zero input — the CLI's local encoder MUST
        // agree byte-for-byte with the runner's encoder, otherwise a
        // verify run would always report MISMATCH on the no-op key
        // path.
        let sig = [0u8; 32];
        assert_eq!(signature_to_hex_local(&sig), "0".repeat(64));
    }

    #[test]
    fn signature_to_hex_matches_audit_encoder_for_mixed_bytes() {
        // Same pattern as `audit::signature_to_hex_known_mixed_bytes`
        // — keeps the two encoders pinned together.
        let mut sig = [0u8; 32];
        for (i, byte) in sig.iter_mut().enumerate() {
            *byte = ((i as u32 * 17 + 3) & 0xFF) as u8;
        }
        let local = signature_to_hex_local(&sig);
        let canonical: String = sig.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(local, canonical);
    }

    #[test]
    fn eq_ignore_ascii_case_hex_uppercase_lowercase() {
        // Uppercase from the runner, lowercase from a stale audit row
        // — verify must treat them as equal.
        assert!(eq_ignore_ascii_case_hex("DEADBEEF", "deadbeef",));
        assert!(eq_ignore_ascii_case_hex(&"0".repeat(64), &"0".repeat(64),));
        assert!(!eq_ignore_ascii_case_hex("DEADBEEF", "DEADBEEE",));
        // Length mismatch is never equal.
        assert!(!eq_ignore_ascii_case_hex("DEAD", "DEADBEEF"));
    }

    #[test]
    fn resolve_audit_url_env_var_wins() {
        // SAFETY: tests run with --test-threads=1 across djogi-cli's
        // unit suite (no env-var-mutating peer at present). If a
        // peer test starts touching DJOGI_CRUD_LOG_URL, both must
        // share a Mutex — see `djogi::snapshot::sign::tests::ENV_MUTEX`
        // for the canonical pattern.
        unsafe {
            std::env::set_var("DJOGI_CRUD_LOG_URL", "postgres://override/audit");
        }
        let cfg = stub_config_with_url("postgres://localhost/main");
        let resolved = resolve_audit_url(&cfg);
        unsafe {
            std::env::remove_var("DJOGI_CRUD_LOG_URL");
        }
        assert_eq!(resolved.as_deref(), Some("postgres://override/audit"));
    }

    #[test]
    fn resolve_audit_url_falls_back_to_derived() {
        // No env var — fall back to splicing `crud_log` into the
        // application URL's path component.
        unsafe {
            std::env::remove_var("DJOGI_CRUD_LOG_URL");
        }
        let cfg = stub_config_with_url("postgres://localhost/main");
        let resolved = resolve_audit_url(&cfg);
        // `derive_per_database_url` swaps the path component; the
        // exact canonical form is owned by that helper, so we just
        // assert the path now ends in `/crud_log` and the authority
        // is preserved.
        let url = resolved.expect("derived audit URL");
        assert!(
            url.ends_with("/crud_log"),
            "expected derived URL to end in /crud_log, got `{url}`"
        );
        assert!(
            url.contains("localhost"),
            "expected derived URL to preserve authority, got `{url}`"
        );
    }

    #[test]
    fn resolve_audit_url_empty_env_var_falls_back() {
        // An explicitly empty env var should NOT silently override —
        // empty is treated as "unset" so the fallback fires. This
        // mirrors the no-op key sentinel rationale in
        // `djogi::snapshot::sign`: an empty string almost certainly
        // means "the operator forgot to fill it in", not "use empty".
        unsafe {
            std::env::set_var("DJOGI_CRUD_LOG_URL", "");
        }
        let cfg = stub_config_with_url("postgres://localhost/main");
        let resolved = resolve_audit_url(&cfg);
        unsafe {
            std::env::remove_var("DJOGI_CRUD_LOG_URL");
        }
        assert!(
            resolved
                .as_deref()
                .map(|u| u.ends_with("/crud_log"))
                .unwrap_or(false),
            "empty env var should fall back to derived; got {resolved:?}"
        );
    }

    /// Build a minimal [`DjogiConfig`] for the URL-resolver tests. We
    /// only need the `database.url` field populated; the resolver
    /// never reads any other field.
    fn stub_config_with_url(url: &str) -> DjogiConfig {
        DjogiConfig {
            database: djogi::config::DatabaseConfig {
                url: url.to_string(),
                max_connections: None,
                dev_mode: false,
            },
            server: djogi::config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
            },
            migrate: djogi::config::MigrateConfig {
                concurrent_warn_relpages: 128,
                strict_concurrent_warnings: false,
                pk_flip_long_tx_threshold_secs: 60,
                pk_flip_join_table_option: 'A',
            },
            profile: "development".to_string(),
            policy: djogi::config::PolicyConfig::default(),
        }
    }

    #[test]
    fn snapshot_filename_constant_matches_upstream() {
        // Defence-in-depth — if `djogi::migrate::SNAPSHOT_FILENAME`
        // ever drifts the verify path would silently look at the
        // wrong file. Pin the value here so a future rename trips
        // both sides.
        assert_eq!(TEST_SNAPSHOT_FILENAME, "schema_snapshot.json");
    }
}
