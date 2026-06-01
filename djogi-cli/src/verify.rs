//! `djogi verify` — read-only HMAC cross-check of on-disk
//! `schema_snapshot.json` files against the `djogi_ddl_audit` ledger
//! living on the `crud_log_url` audit DB.
//! # What this command does
//! For every snapshot file under
//! `migrations/<target>/<app>/schema_snapshot.json`:
//! 1. Read the bytes from disk.
//! 2. Compute `sign_snapshot(bytes, &key)` where `key` comes from
//! `DJOGI_SNAPSHOT_SIGNING_KEY` (or the no-op zero key when the
//! env var is unset — same sentinel contract the runner uses, see
//! [`djogi::snapshot::sign`]).
//! 3. SELECT the latest `snapshot_signature_hex` from
//! `djogi_ddl_audit` for `(target_database, app_label)` from the
//! audit DB.
//! 4. Compare the computed hex against the stored hex. Print
//! `OK <path>` for matches and `MISMATCH <path>: expected …, got …`
//! on stderr otherwise.
//! # Read-only by contract
//! Verify never issues `INSERT`, `UPDATE`, `DELETE`, or DDL — the only
//! SQL leaving the CLI is the single `SELECT` on `djogi_ddl_audit`. If
//! the audit table does not exist the query surfaces SQLSTATE `42P01`
//! (`undefined_table`); the runner CATCHES that and treats the snapshot
//! as `Skipped` (warn on stderr, exit code unchanged) per risk
//! row 11. The verify path itself NEVER bootstraps the table — that is
//! the migration runner's job (T9.5).
//! # Audit DB URL resolution
//! The "audit DB" is the same database the runner writes to via
//! `RunnerCtx::audit_pool` (T9.4). Resolution is delegated to
//! [`djogi::migrate::resolve_audit_url`] — a shared helper used by
//! both `djogi verify` (here) and `djogi db reset` (issue
//! #118). Resolution order:
//! 1. `CRUD_LOG_URL` env var — primary explicit override for operators
//! who keep the audit DB on a separate authority.
//! 2. `DJOGI_CRUD_LOG_URL` env var — backwards-compatible spelling
//! accepted by the shared resolver.
//! 3. `[database].crud_log_url` in `Djogi.toml`.
//! 4. Splice `crud_log` (the
//! [`djogi::migrate::AUDIT_DB_DERIVED_NAME`] constant) into the
//! application URL's path component. Matches the on-disk migration
//! tree convention (`migrations/crud_log/<app>/`) the bootstrap
//! layer documents in [`djogi::migrate::target`].
//! When neither resolves to a usable URL, verify surfaces
//! [`VerifyError::Config`] and exits `1` (config / runtime error).
//! Promoting the resolver to `djogi::migrate` keeps the verify and
//! reset paths in lockstep so an operator's `Djogi.toml` cannot mean
//! one thing to verify and another to reset.
//! # Exit code semantics (matches ledger-verify)
//! - `0` — every snapshot scanned reported `Ok` or `Skipped`.
//! - `1` — at least one snapshot reported `Mismatch`, OR a runtime
//! error occurred (config load, key decode, audit pool unreachable,
//! walkdir I/O).
//! `Skipped` (audit table absent) does NOT count as a mismatch — the
//! cross-check is best-effort when the operator has not provisioned
//! the second DB. The `tracing`-style warn line on stderr makes the
//! skip visible to the operator.
//! # Determinism
//! Snapshot files are walked via [`djogi::migrate::scan_filesystem`]
//! which returns a `BTreeSet<FilesystemBucket>` — already sorted by
//! `(database, app)`. Verify converts that to a `Vec` and does NOT
//! re-shuffle, so failure messages are reproducible across machines.
//! Symlinks are not followed (the scanner uses `file_type` which
//! returns `false` for `is_dir` on symlinks).
//! # Spec / memory anchors
//! - v3 plan §452 (snapshot signing surface)
//! - v3 plan §459–460 (audit cross-check contract)
//! - v3 plan §470 (read-only verify)
//! - v3 plan §824 (graceful absence of audit table)
//! - Plan.6 (`docs/superpowers/plans/granular-phase8/cluster-8epsilon-granular.md`)

use std::path::PathBuf;
use std::process::ExitCode;

use djogi::__bypass::RawAccessExt as _;
use djogi::config::DjogiConfig;
use djogi::migrate::{
    FilesystemBucket, SNAPSHOT_FILENAME, app_dirname, migrations_root, resolve_audit_url,
    scan_filesystem, signature_to_hex,
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
    /// A snapshot path resolved to a symlink rather than a regular
    /// file. Verify refuses to follow it — a malicious or accidental
    /// symlink could escape the workspace and cause `djogi verify` to
    /// hash an attacker-controlled file (e.g. `/etc/passwd`) before
    /// reporting a confusing MISMATCH against the audit ledger. The
    /// scanner already skips symlinked directories via
    /// `entry.file_type.is_dir` returning `false` for symlinks; this
    /// variant closes the file-side gap on the same defense.
    /// Residual TOCTOU window: between this `symlink_metadata` check
    /// and the subsequent `std::fs::read`, an attacker with write
    /// access to the migrations tree could swap the regular file for a
    /// symlink. Closing that window properly requires `openat`
    /// semantics (re-checking the metadata via the open file handle's
    /// fd); for v0.1.0 the symlink-reject is the main exploit vector
    /// and the residual window is documented here. may revisit.
    SymlinkSnapshot {
        /// The snapshot path that resolved to a symlink.
        path: PathBuf,
    },
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
            VerifyError::SymlinkSnapshot { path } => write!(
                f,
                "snapshot path is a symlink; refusing to follow to prevent path-traversal escapes: {} \
                 (replace the symlink with the real `schema_snapshot.json` file or remove the \
                 offending entry from the migrations tree)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VerifyError::Io { source, .. } => Some(source),
            VerifyError::KeyDecode(err) => Some(err),
            VerifyError::AuditPoolUnreachable { .. }
            | VerifyError::Config(_)
            | VerifyError::SymlinkSnapshot { .. } => None,
        }
    }
}

/// `djogi verify` entry point — consumed by `main.rs::TopCommand::Verify`.
/// `workspace`: optional workspace-root override. Defaults to
/// `std::env::current_dir`.
/// Returns:
/// - `ExitCode::SUCCESS` when every entry is `Ok` or `Skipped`.
/// - `ExitCode::from(1)` when at least one entry is `Mismatch` OR a
/// runtime error stops the verification before completion.
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

    // Step 4 — resolve the audit DB URL via the shared
    // `djogi::migrate::resolve_audit_url` helper. Env var wins;
    // otherwise derive `crud_log` from `database.url`. The resolver
    // enforces the "audit DB must be a separate database" invariant
    // a derived URL identical to `database.url` is rejected so a
    // misconfigured app pointing at `…/crud_log` cannot silently audit
    // itself. Errors are mapped onto [`VerifyError::Config`] so the
    // verify CLI's exit-code matrix stays uniform (any config-side
    // failure → exit 1).
    let audit_url = resolve_audit_url(&config).map_err(|e| VerifyError::Config(e.to_string()))?;

    // Step 5 — connect to the audit DB once. Re-use one pool for every
    // snapshot's per-bucket query.
    let pool = match DjogiPool::connect(&audit_url).await {
        Ok(p) => (audit_url.clone(), p),
        Err(e) => {
            return Err(VerifyError::AuditPoolUnreachable {
                url: audit_url,
                message: e.to_string(),
            });
        }
    };

    // Version preflight on the audit DB cluster.
    if let Err(e) = djogi::pg::preflight::check_postgres_version(&pool.1).await {
        return Err(VerifyError::Config(format!("support boundary: {e}")));
    }

    // Step 6 — verify each snapshot. Track only whether any bucket
    // mismatched; the per-bucket diagnostics print to stderr/stdout
    // inline (deterministic order is `buckets` iteration order, set
    // by `discover_filesystem_buckets`).
    let mut any_mismatch = false;
    let (audit_url_for_log, audit_pool) = pool;
    let mut audit_ctx = djogi::context::DjogiContext::from_pool(audit_pool);

    for bucket in &buckets {
        let snapshot = workspace
            .join("migrations")
            .join(&bucket.database)
            .join(app_dirname(&bucket.app))
            .join(SNAPSHOT_FILENAME);
        let bytes = match read_snapshot_bytes(&snapshot)? {
            Some(b) => b,
            None => continue,
        };
        let computed = sign_snapshot(&bytes, &key);
        let computed_hex = signature_to_hex(&computed);

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
                // 42P01 — graceful skip per .
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
                continue;
            }
            Err(FetchAuditError::Other(message)) => {
                return Err(VerifyError::AuditPoolUnreachable {
                    url: audit_url_for_log.clone(),
                    message,
                });
            }
        };

        match stored {
            Some(stored_hex) if eq_ignore_ascii_case_hex(&stored_hex, &computed_hex) => {
                println!("OK {}", snapshot.display());
            }
            Some(stored_hex) => {
                eprintln!(
                    "MISMATCH {}: expected {stored_hex}, got {computed_hex}",
                    snapshot.display()
                );
                any_mismatch = true;
            }
            None => {
                // Audit table exists but no row for this bucket — skip,
                // mirroring the table-absent case. The operator either
                // has not yet applied any migrations for this bucket
                // (audit row is the post-apply artefact) or the audit
                // DB was provisioned after the last apply.
                eprintln!(
                    "warn: no djogi_ddl_audit row for {}/{} — skipping",
                    bucket.database,
                    if bucket.app.is_empty() {
                        "_global_"
                    } else {
                        &bucket.app
                    }
                );
            }
        }
    }

    // Step 7 — exit code: any Mismatch → 1; otherwise 0.
    Ok(if any_mismatch {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

// Audit DB URL resolution moved to `djogi::migrate::resolve_audit_url`
// (#118) so the verify CLI and `db reset` share one
// resolver. See `djogi/src/migrate/audit.rs` for the implementation
// and the unit test suite covering env-var priority, derive fallback,
// and the self-audit guard.

/// Read a snapshot file's bytes, refusing to follow symlinks.
/// DIRECTORIES via `entry.file_type?.is_dir`, but the verify path's
/// per-bucket file lookup previously used `path.is_file` (which
/// follows symlinks) followed by `std::fs::read`. A symlinked
/// `schema_snapshot.json` pointing at `/etc/passwd` (or any
/// attacker-controlled file) would have its bytes hashed and
/// cross-checked against the audit ledger, leaking content of the
/// target file via the MISMATCH diagnostic OR — when the attacker can
/// also write the audit row — producing a successful match against
/// attacker-controlled bytes.
/// This helper:
/// - Returns `Ok(None)` when the path does not exist (typical of a
/// freshly composed migrations directory before the first apply).
/// - Returns `Err(VerifyError::SymlinkSnapshot)` when the path is a
/// symlink — refusing to follow.
/// - Returns `Ok(None)` when the path is some other non-regular file
/// (named pipe, device file). The migrations tree is supposed to
/// contain regular files; non-file entries are silently skipped.
/// - Returns `Ok(Some(bytes))` for regular files.
/// **Residual TOCTOU.** Between `symlink_metadata` and `std::fs::read`
/// an attacker with write access to the migrations tree could swap the
/// regular file for a symlink. Closing that window properly requires
/// `openat`-style fd re-checking (open the file, then `metadata`
/// the open handle); for v0.1.0 the symlink-reject is the main exploit
/// vector and the residual window is documented on
/// [`VerifyError::SymlinkSnapshot`]. may revisit.
fn read_snapshot_bytes(snapshot: &std::path::Path) -> Result<Option<Vec<u8>>, VerifyError> {
    let meta = match std::fs::symlink_metadata(snapshot) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(VerifyError::Io {
                path: snapshot.to_path_buf(),
                source: e,
            });
        }
    };
    if meta.file_type().is_symlink() {
        return Err(VerifyError::SymlinkSnapshot {
            path: snapshot.to_path_buf(),
        });
    }
    if !meta.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(snapshot).map_err(|e| VerifyError::Io {
        path: snapshot.to_path_buf(),
        source: e,
    })?;
    Ok(Some(bytes))
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

/// Query the latest non-NULL `snapshot_signature_hex` for
/// `(target_database, app_label)`. Returns `Ok(None)` when no row
/// matches but the table exists, `Err(TableAbsent)` on SQLSTATE
/// `42P01`, and `Err(Other)` on any other failure.
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
    // ORDER BY id DESC LIMIT 1 picks the most recent signed row; `id`
    // is BIGSERIAL so DESC ordering matches the wall-clock ordering of
    // `applied_at` for any single-writer audit DB (which is the only
    // shape the runner produces). Rows with NULL signatures are reset
    // replay rows (`snapshot: None`) and must not mask the last signed
    // apply row, otherwise `db reset` could turn a real tamper mismatch
    // into a skip. may add a tiebreak on `applied_at` when the
    // audit DB sees concurrent writers.
    let sql = "SELECT snapshot_signature_hex FROM djogi_ddl_audit \
               WHERE target_database = $1 AND app_label = $2 \
                 AND snapshot_signature_hex IS NOT NULL \
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
            // `42P01` = `undefined_table`. Per we treat this
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
    //! The integration tests' assertions match the plan.6 brief:
    //! - `verify_clean_returns_zero` — fixture workspace with
    //! matching snapshot + audit row → exit 0, `OK <path>` on
    //! stdout.
    //! - `verify_mismatch_returns_one` — snapshot bytes tampered
    //! after audit row was written → exit 1, `MISMATCH …` line on
    //! stderr.
    //! - `verify_skips_when_audit_table_absent` — audit DB has no
    //! `djogi_ddl_audit` table → exit 0, `warn: djogi_ddl_audit
    //! absent …` line on stderr.
    //! - `verify_no_op_key_passes_zero_signature` — env var unset,
    //! audit row carries 64 zero hex characters → exit 0.

    use super::*;
    // Imported in the test module only — `AuditUrlError` is referenced
    // by the `audit_url_self_audit_maps_to_verify_config_with_actionable_message`
    // test below to construct a sample resolver-side error, but the
    // verify run path delegates resolution to djogi and does not need
    // the type at module scope.
    use djogi::migrate::AuditUrlError;

    #[test]
    fn eq_ignore_ascii_case_hex_uppercase_lowercase() {
        // Uppercase from the runner, lowercase from a stale audit row
        // verify must treat them as equal.
        assert!(eq_ignore_ascii_case_hex("DEADBEEF", "deadbeef",));
        assert!(eq_ignore_ascii_case_hex(&"0".repeat(64), &"0".repeat(64),));
        assert!(!eq_ignore_ascii_case_hex("DEADBEEF", "DEADBEEE",));
        // Length mismatch is never equal.
        assert!(!eq_ignore_ascii_case_hex("DEAD", "DEADBEEF"));
    }

    #[test]
    fn audit_url_self_audit_maps_to_verify_config_with_actionable_message() {
        // #118 — verify.rs now delegates to
        // `djogi::migrate::resolve_audit_url`. The resolver's typed
        // `AuditUrlError::SelfAudit` is mapped to `VerifyError::Config`
        // via `map_err(|e| VerifyError::Config(e.to_string))`. We
        // assert the mapping preserves the operator-actionable
        // substrings the original tests pinned, so a future refactor
        // that drops `to_string` (or wraps the error differently)
        // trips the assertion before reaching production.
        // Resolver-internal coverage (env-var priority, empty-env
        // fallback, unresolvable path, self-audit refusal) lives in
        // `djogi::migrate::audit::tests::resolve_audit_url_*` so the
        // shared helper has one source of truth for its semantics.
        let mapped = VerifyError::Config(
            AuditUrlError::SelfAudit {
                application_url: "postgres://localhost/crud_log".to_string(),
            }
            .to_string(),
        );
        let display = format!("{mapped}");
        assert!(
            display.contains("audit URL derivation produced the same URL"),
            "mapped Display must surface the resolver's actionable language; got: {display}",
        );
        assert!(
            display.contains("postgres://localhost/crud_log"),
            "mapped Display must echo the offending URL; got: {display}",
        );
        assert!(
            display.contains("CRUD_LOG_URL"),
            "mapped Display must point at the env-var override; got: {display}",
        );
    }

    /// reject a symlinked snapshot file rather than reading through to
    /// the target. Without this guard, `std::fs::read` would happily
    /// hash an attacker-controlled file (e.g. `/etc/passwd`),
    /// leaking content via the MISMATCH diagnostic OR — when the
    /// attacker can also write the audit row — producing a successful
    /// match against attacker-controlled bytes.
    /// We test `read_snapshot_bytes` directly rather than driving
    /// `run(...)` end-to-end so the test does not depend on a live
    /// Postgres for the audit pool. The full end-to-end coverage lives
    /// in T9.7's `phase8_djogi_verify_cli` integration suite.
    #[cfg(unix)]
    #[test]
    fn verify_rejects_symlink_snapshot() {
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!("djogi-cli-verify-symlink-{nanos}-{n}"));
        fs::create_dir_all(&workspace).unwrap();

        // Create a target file OUTSIDE the workspace — the canonical
        // attack shape is a symlink pointing at /etc/passwd, but a
        // plain file under temp_dir exercises the same codepath
        // without depending on a system file the test runner may not
        // be permitted to read.
        let outside_target =
            std::env::temp_dir().join(format!("djogi-cli-verify-outside-{nanos}-{n}.txt"));
        fs::write(&outside_target, b"attacker-controlled bytes").unwrap();

        // Lay down `migrations/main/_global_/schema_snapshot.json`
        // as a SYMLINK to the outside file.
        let app_dir = workspace.join("migrations/main/_global_");
        fs::create_dir_all(&app_dir).unwrap();
        let snapshot_link = app_dir.join("schema_snapshot.json");
        symlink(&outside_target, &snapshot_link).unwrap();

        let result = read_snapshot_bytes(&snapshot_link);

        // Cleanup before assertion so a panic doesn't leak temp files.
        let _ = fs::remove_file(&outside_target);
        let _ = fs::remove_dir_all(&workspace);

        match result {
            Err(VerifyError::SymlinkSnapshot { path }) => {
                // Path in the error must be the in-workspace symlink,
                // NOT the resolved target — operators diagnose by the
                // path they put in the migrations tree.
                assert!(
                    path.ends_with("schema_snapshot.json"),
                    "SymlinkSnapshot path must point at the in-workspace symlink, got: {}",
                    path.display()
                );
                // Display must be operator-actionable.
                let display = format!("{}", VerifyError::SymlinkSnapshot { path });
                assert!(
                    display.contains("snapshot path is a symlink"),
                    "Display must be operator-actionable, got: {display}"
                );
                assert!(
                    display.contains("refusing to follow"),
                    "Display must explain the refusal, got: {display}"
                );
            }
            other => panic!(
                "expected VerifyError::SymlinkSnapshot rejecting symlinked snapshot, got: {other:?}"
            ),
        }
    }

    /// Companion test — `read_snapshot_bytes` must return `Ok(None)`
    /// for a path that does not exist (no snapshot composed yet) so
    /// the verify loop's `continue` branch is preserved.
    #[test]
    fn read_snapshot_bytes_returns_none_for_missing_file() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing =
            std::env::temp_dir().join(format!("djogi-cli-verify-missing-{nanos}-{n}.json"));
        // Sanity — path must not exist.
        assert!(!missing.exists());
        let result = read_snapshot_bytes(&missing);
        assert!(
            matches!(result, Ok(None)),
            "missing file must return Ok(None), got: {result:?}"
        );
    }

    /// Companion test — `read_snapshot_bytes` must return the bytes
    /// verbatim for a regular file. Pins the happy path so a future
    /// refactor cannot regress it.
    #[test]
    fn read_snapshot_bytes_returns_bytes_for_regular_file() {
        use std::fs;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("djogi-cli-verify-regular-{nanos}-{n}.json"));
        fs::write(&path, b"{\"x\":1}").unwrap();
        let result = read_snapshot_bytes(&path);
        let _ = fs::remove_file(&path);
        match result {
            Ok(Some(bytes)) => assert_eq!(bytes, b"{\"x\":1}"),
            other => panic!("expected Ok(Some(bytes)), got: {other:?}"),
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
