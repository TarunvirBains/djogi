//! On-disk plan file I/O — read, write, hash, and verify the JSON
//! documents stored at `migrations/<target>/live/<plan_id>_<slug>.json`.
//! # Immutability contract
//! [`write_plan`] refuses to overwrite an existing plan file. Once a
//! plan is committed to disk it is the immutable definition of the
//! rollout; any change requires a new plan file with a fresh
//! `plan_id`. The DB row in `djogi_live_plans` records the SHA-256 of
//! the file at write time; resume / finalize call sites use
//! [`verify_checksum`] to assert the file is byte-identical before
//! advancing the runner.
//! # Checksum format
//! All checksums in this module are `V1:<sha256-hex>` per the same
//! convention used by [`crate::migrate::ledger`] for migration files.
//! The leading `V1:` is a version prefix; bumping past it would require
//! the runner to dual-verify both forms during the transition window.
//! # No-regex rule
//! Path / slug validation is byte-level only — see
//! [`crate::live_migrate::plan::PlanValidationError::SlugByte`].

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::migrate::common;
use sha2::{Digest, Sha256};

use crate::live_migrate::plan::{LivePlan, PlanValidationError};

// Re-use the same `V1:` prefix and 64-hex shape as the migration
// ledger so operator-facing tooling treats both checksums uniformly.
const CHECKSUM_PREFIX: &str = "V1:";
const SHA256_HEX_LEN: usize = 64;
const CHECKSUM_LEN: usize = CHECKSUM_PREFIX.len() + SHA256_HEX_LEN;

/// Errors surfaced by the plan-file I/O layer. `thiserror`-based so
/// the runner can wrap-and-rethrow without per-variant boilerplate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanFileError {
    /// Underlying I/O error. The runner surfaces these verbatim — most
    /// I/O failures here are environmental (permissions, disk full)
    /// rather than Djogi bugs.
    #[error("plan file I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The plan file's JSON could not be parsed. Carries the on-disk
    /// path so the operator can locate the offending file.
    #[error("plan file at {path} failed to parse as JSON: {source}")]
    JsonParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Serialising an in-memory [`LivePlan`] back to JSON failed. Rare;
    /// surfaces only when a user-supplied step parameter contains a
    /// non-serialisable value (none of the current variants can).
    #[error("plan file serialisation failed: {source}")]
    JsonSerialize {
        #[source]
        source: serde_json::Error,
    },
    /// The recomputed checksum disagrees with the ledger-stored value.
    /// The runner surfaces this verbatim with the actionable message
    /// "plan file edited after start; re-generate or abandon and
    /// retry".
    #[error(
        "plan file at {path} checksum mismatch: expected {expected}, computed {actual}; \
         plan file edited after start — re-generate or abandon and retry"
    )]
    ChecksumMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    /// The plan file does not exist at the resolved path. Distinct
    /// from [`PlanFileError::Io`] so the runner can suggest
    /// `djogi live abandon` when the operator deleted the file.
    #[error("plan file not found at {0}")]
    NotFound(PathBuf),
    /// [`write_plan`] was called against a path that already exists.
    /// The immutability contract refuses overwrite — a new plan needs
    /// a new `plan_id`.
    #[error(
        "plan file at {0} already exists; live plans are immutable. \
         Generate a new plan with a fresh plan_id"
    )]
    AlreadyExists(PathBuf),
    /// In-memory plan failed [`LivePlan::validate`] before the file
    /// was written / after it was read. Wraps the structural reason.
    #[error("plan file at {path} failed validation: {source}")]
    Validation {
        path: PathBuf,
        #[source]
        source: PlanValidationError,
    },
    /// A stored checksum string did not match the `V1:<64-hex>` shape.
    /// The runner refuses to compare a malformed string against a
    /// freshly-computed one — see [`verify_checksum`].
    #[error("malformed checksum string `{value}`: {reason}")]
    MalformedChecksum { value: String, reason: &'static str },
}

/// Resolve the on-disk path for a live plan.
/// Format: `<migrations_root>/<target_database>/live/<plan_id>_<slug>.json`.
/// Per hybrid naming. `plan_id` is rendered as a decimal i64
/// (the `Display` impl of [`crate::types::HeerId`]).
pub fn plan_path(
    migrations_root: &Path,
    target_database: &str,
    plan_id: crate::types::HeerId,
    slug: &str,
) -> PathBuf {
    let filename = format!("{}_{}.json", plan_id, slug);
    migrations_root
        .join(target_database)
        .join("live")
        .join(filename)
}

/// Serialise a [`LivePlan`] to disk and return the path it was
/// written to.
/// Performs [`LivePlan::validate`] first; refuses to overwrite an
/// existing file (immutability contract). Creates parent directories
/// as needed (`migrations/<target>/live/`).
pub fn write_plan(migrations_root: &Path, plan: &LivePlan) -> Result<PathBuf, PlanFileError> {
    let migrations_root =
        common::canonicalize_base(migrations_root).map_err(|source| PlanFileError::Io {
            path: migrations_root.to_path_buf(),
            source,
        })?;
    let path = plan_path(
        &migrations_root,
        &plan.header.target_database,
        plan.header.plan_id,
        &plan.header.slug,
    );
    let path = common::resolve_within_base(
        &migrations_root,
        &path,
        common::CandidateResolutionMode::MayCreate,
    )
    .map_err(|source| PlanFileError::Io {
        path: path.clone(),
        source,
    })?;
    let _parent =
        common::create_workspace_parent_dirs(&migrations_root, &path).map_err(|source| {
            PlanFileError::Io {
                path: path.parent().unwrap_or(&path).to_path_buf(),
                source,
            }
        })?;
    plan.validate()
        .map_err(|source| PlanFileError::Validation {
            path: path.clone(),
            source,
        })?;
    let bytes = serde_json::to_vec_pretty(plan)
        .map_err(|source| PlanFileError::JsonSerialize { source })?;
    // Use create_new(true) for the immutability check — atomic at the
    // filesystem layer, no TOCTOU race against a concurrent writer.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|source| match source.kind() {
            std::io::ErrorKind::AlreadyExists => PlanFileError::AlreadyExists(path.clone()),
            _ => PlanFileError::Io {
                path: path.clone(),
                source,
            },
        })?;
    file.write_all(&bytes).map_err(|source| PlanFileError::Io {
        path: path.clone(),
        source,
    })?;
    file.sync_all().map_err(|source| PlanFileError::Io {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Read and parse a plan file from disk. Validates the resulting
/// [`LivePlan`] before returning so callers always receive a
/// structurally-sound plan or an actionable error.
pub fn read_plan(migrations_root: &Path, plan_file_path: &Path) -> Result<LivePlan, PlanFileError> {
    let migrations_root =
        common::canonicalize_base(migrations_root).map_err(|source| PlanFileError::Io {
            path: migrations_root.to_path_buf(),
            source,
        })?;
    let plan_file_path = common::resolve_within_base(
        &migrations_root,
        plan_file_path,
        common::CandidateResolutionMode::Existing,
    )
    .map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => PlanFileError::NotFound(plan_file_path.to_path_buf()),
        _ => PlanFileError::Io {
            path: plan_file_path.to_path_buf(),
            source,
        },
    })?;
    let bytes = read_file_bytes(&plan_file_path)?;
    let plan: LivePlan =
        serde_json::from_slice(&bytes).map_err(|source| PlanFileError::JsonParse {
            path: plan_file_path.to_path_buf(),
            source,
        })?;
    plan.validate()
        .map_err(|source| PlanFileError::Validation {
            path: plan_file_path.to_path_buf(),
            source,
        })?;
    Ok(plan)
}

/// Compute the `V1:<sha256-hex>` checksum of the on-disk plan file's
/// raw bytes. Hashes the file as written rather than re-serialising
/// the in-memory plan so byte-for-byte changes (whitespace, key
/// ordering) are caught by [`verify_checksum`].
pub fn compute_checksum(
    migrations_root: &Path,
    plan_file_path: &Path,
) -> Result<String, PlanFileError> {
    let migrations_root =
        common::canonicalize_base(migrations_root).map_err(|source| PlanFileError::Io {
            path: migrations_root.to_path_buf(),
            source,
        })?;
    let plan_file_path = common::resolve_within_base(
        &migrations_root,
        plan_file_path,
        common::CandidateResolutionMode::Existing,
    )
    .map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => PlanFileError::NotFound(plan_file_path.to_path_buf()),
        _ => PlanFileError::Io {
            path: plan_file_path.to_path_buf(),
            source,
        },
    })?;
    let bytes = read_file_bytes(&plan_file_path)?;
    Ok(format_checksum(&bytes))
}

/// Verify that the on-disk plan file's recomputed checksum matches
/// `expected`. Used by the runner on every `djogi live run` /
/// `resume` / `finalize` per §3 line 429-432 of the v3 plan.
/// `expected` must already be a well-formed `V1:<64-hex>` string; an
/// otherwise-malformed value returns [`PlanFileError::MalformedChecksum`]
/// rather than silently slipping through the byte compare.
pub fn verify_checksum(
    migrations_root: &Path,
    plan_file_path: &Path,
    expected: &str,
) -> Result<(), PlanFileError> {
    validate_checksum_shape(expected)?;
    let actual = compute_checksum(migrations_root, plan_file_path)?;
    if expected == actual {
        Ok(())
    } else {
        Err(PlanFileError::ChecksumMismatch {
            path: plan_file_path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Read `path` into a byte vector. Maps `NotFound` errors to the
/// dedicated [`PlanFileError::NotFound`] variant so callers can detect
/// a deleted plan without string-matching on `io::ErrorKind`.
fn read_file_bytes(path: &Path) -> Result<Vec<u8>, PlanFileError> {
    let mut file = std::fs::File::open(path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => PlanFileError::NotFound(path.to_path_buf()),
        _ => PlanFileError::Io {
            path: path.to_path_buf(),
            source,
        },
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| PlanFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(bytes)
}

/// Format a SHA-256 hash of the supplied byte slice as `V1:<64-hex>`.
/// Implementation parallels
/// [`crate::migrate::ledger::compute_checksum`] — same prefix, same
/// lowercase-hex encoding — so an operator can reuse the same byte
/// shape across both ledgers.
fn format_checksum(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(CHECKSUM_LEN);
    out.push_str(CHECKSUM_PREFIX);
    for b in digest {
        out.push(hex_digit(b >> 4));
        out.push(hex_digit(b & 0x0f));
    }
    debug_assert_eq!(out.len(), CHECKSUM_LEN);
    out
}

/// Map a 4-bit nibble to its lowercase hex character. Mirrors the
/// ledger's helper so any future hex-formatting change lands in both
/// places at once.
fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        // Caller passes a 4-bit nibble; unreachable at runtime.
        _ => unreachable!("hex_digit takes a 4-bit nibble"),
    }
}

/// Structurally validate a checksum string against the
/// `V1:<sha256-hex>` shape. Lower-case ASCII hex only — see
/// [`crate::migrate::ledger::validate_checksum_format`] for the
/// equivalent rule on the migration ledger side.
fn validate_checksum_shape(s: &str) -> Result<(), PlanFileError> {
    if !s.starts_with(CHECKSUM_PREFIX) {
        return Err(PlanFileError::MalformedChecksum {
            value: s.to_string(),
            reason: "checksum string must start with the `V1:` prefix",
        });
    }
    if s.len() != CHECKSUM_LEN {
        return Err(PlanFileError::MalformedChecksum {
            value: s.to_string(),
            reason: "checksum string must be V1: + 64 hex chars (67 bytes total)",
        });
    }
    let tail = &s.as_bytes()[CHECKSUM_PREFIX.len()..];
    for &byte in tail {
        let is_lower_hex = byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        if !is_lower_hex {
            return Err(PlanFileError::MalformedChecksum {
                value: s.to_string(),
                reason: "checksum hex tail must be lowercase ASCII hex (0-9, a-f)",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_migrate::plan::{
        LivePlan, PlanClassification, PlanHeader, Step, StepKind, StepParameters,
    };
    use crate::types::HeerId;

    fn sample_plan() -> LivePlan {
        LivePlan {
            header: PlanHeader {
                plan_id: HeerId::ZERO,
                slug: "demo_slug".to_string(),
                classification: PlanClassification::ExpandContract,
                originating_migration: "V20260428010203__demo".to_string(),
                target_database: "main".to_string(),
                app_label: "".to_string(),
            },
            steps: vec![Step {
                kind: StepKind::ExpandSchema,
                ordinal: 0,
                parameters: StepParameters::ExpandSchema {
                    sql_segments: vec!["ALTER TABLE foo ADD COLUMN bar INT".to_string()],
                },
            }],
        }
    }

    /// Best-effort tempdir that cleans up on Drop — avoids a per-test
    /// dependency on the `tempfile` crate (not currently in the
    /// workspace's dev-dependencies).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let temp_root = std::env::temp_dir();
            let dir_name = format!("djogi-plan-file-{label}-{pid}-{nanos}");
            let path =
                common::resolve_maybe_missing_workspace_path(&temp_root, Path::new(&dir_name))
                    .unwrap_or_else(|_| temp_root.join(&dir_name));
            let path = common::resolve_write_workspace_path(&temp_root, &path)
                .expect("resolve tempdir path");
            common::create_workspace_dir_all(&temp_root, &path).expect("create tempdir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let temp_root = std::env::temp_dir();
            let _ = common::remove_workspace_dir_all(&temp_root, &self.0);
        }
    }

    #[test]
    fn plan_path_uses_target_database_live_subdir() {
        let path = plan_path(
            Path::new("/tmp/migrations"),
            "main",
            HeerId::ZERO,
            "demo_slug",
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/migrations/main/live/0_demo_slug.json")
        );
    }

    #[test]
    fn write_plan_then_read_plan_round_trips() {
        let tmp = TempDir::new("round-trip");
        let plan = sample_plan();
        let written = write_plan(tmp.path(), &plan).expect("write plan");
        let back = read_plan(tmp.path(), &written).expect("read plan");
        assert_eq!(back, plan);
    }

    #[test]
    fn write_plan_refuses_overwrite() {
        let tmp = TempDir::new("overwrite");
        let plan = sample_plan();
        let path = write_plan(tmp.path(), &plan).expect("first write");
        let err = write_plan(tmp.path(), &plan).expect_err("second write should refuse");
        match err {
            PlanFileError::AlreadyExists(p) => assert_eq!(p, path),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn write_plan_rejects_invalid_plan() {
        let tmp = TempDir::new("invalid");
        let mut plan = sample_plan();
        plan.steps.clear();
        let err = write_plan(tmp.path(), &plan).expect_err("validate should fail");
        assert!(matches!(err, PlanFileError::Validation { .. }));
    }

    #[test]
    fn compute_checksum_is_deterministic_v1_prefixed_lowercase_hex() {
        let tmp = TempDir::new("checksum-shape");
        let plan = sample_plan();
        let path = write_plan(tmp.path(), &plan).expect("write plan");
        let cs1 = compute_checksum(tmp.path(), &path).expect("compute 1");
        let cs2 = compute_checksum(tmp.path(), &path).expect("compute 2");
        assert_eq!(cs1, cs2);
        assert!(cs1.starts_with("V1:"), "expected V1: prefix; got {cs1}");
        assert_eq!(cs1.len(), 67);
        let tail = &cs1[3..];
        assert!(
            tail.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "tail must be lowercase hex; got {cs1}",
        );
    }

    #[test]
    fn verify_checksum_accepts_unchanged_file() {
        let tmp = TempDir::new("verify-ok");
        let plan = sample_plan();
        let path = write_plan(tmp.path(), &plan).expect("write plan");
        let cs = compute_checksum(tmp.path(), &path).expect("compute");
        verify_checksum(tmp.path(), &path, &cs).expect("verify ok");
    }

    #[test]
    fn verify_checksum_rejects_edited_file() {
        let tmp = TempDir::new("verify-edited");
        let plan = sample_plan();
        let path = write_plan(tmp.path(), &plan).expect("write plan");
        let cs = compute_checksum(tmp.path(), &path).expect("compute");
        // Append a stray byte to simulate an edit.
        let tmp_canonical = common::canonicalize_base(tmp.path()).expect("canonicalize temp dir");
        let path_canonical = common::resolve_within_base(
            tmp.path(),
            &path,
            common::CandidateResolutionMode::Existing,
        )
        .expect("canonicalize plan path");
        assert!(
            path_canonical.starts_with(&tmp_canonical),
            "plan path should be within temp dir"
        );
        let mut f = OpenOptions::new()
            .append(true)
            .open(&path_canonical)
            .expect("open append");
        f.write_all(b"\n").expect("append byte");
        drop(f);
        let err = verify_checksum(tmp.path(), &path, &cs).expect_err("verify should fail");
        match err {
            PlanFileError::ChecksumMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, cs);
                assert_ne!(actual, cs);
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_checksum_returns_not_found_for_missing_file() {
        let tmp = TempDir::new("verify-missing");
        let missing = tmp.path().join("nope.json");
        // Use a structurally well-formed checksum so the failure is
        // file-presence, not malformed-input.
        let well_formed = format!("V1:{}", "0".repeat(64));
        let err =
            verify_checksum(tmp.path(), &missing, &well_formed).expect_err("missing must fail");
        assert!(matches!(err, PlanFileError::NotFound(_)));
    }

    #[test]
    fn verify_checksum_rejects_malformed_expected_string() {
        let tmp = TempDir::new("verify-malformed");
        let plan = sample_plan();
        let path = write_plan(tmp.path(), &plan).expect("write plan");
        let err = verify_checksum(tmp.path(), &path, "not-a-checksum").expect_err("malformed");
        assert!(matches!(err, PlanFileError::MalformedChecksum { .. }));
    }

    #[test]
    fn read_plan_returns_not_found_for_missing_file() {
        let tmp = TempDir::new("read-missing");
        let err = read_plan(tmp.path(), &tmp.path().join("nope.json")).expect_err("missing");
        assert!(matches!(err, PlanFileError::NotFound(_)));
    }

    #[test]
    fn read_plan_rejects_malformed_json() {
        let tmp = TempDir::new("read-malformed");
        let path = tmp.path().join("malformed.json");
        common::write_workspace_file(tmp.path(), &path, b"{ not json }").expect("write malformed");
        let err = read_plan(tmp.path(), &path).expect_err("parse should fail");
        assert!(matches!(err, PlanFileError::JsonParse { .. }));
    }
}
