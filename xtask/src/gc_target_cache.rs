use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

use sha2::{Digest, Sha256};

const CACHE_ROOT_ENV: &str = "DJOGI_TARGET_CACHE_ROOT";
const DEFAULT_CACHE_SUBPATH: &str = ".cache/djogi-target";

pub fn run(dry_run: bool) -> ExitCode {
    let cache_root = match resolve_cache_root() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("gc-target-cache: {error}");
            return ExitCode::FAILURE;
        }
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; cannot locate cache root".to_string())
        .unwrap_or_else(|e| panic!("{e}"));
    let cache_root =
        validate_cache_root_within_home(&home, &cache_root).unwrap_or_else(|e| panic!("{e}"));

    if !cache_root
        .canonicalize()
        .map(|path| path.starts_with(&home))
        .unwrap_or(false)
    {
        println!(
            "gc-target-cache: cache root {} does not exist; nothing to do",
            cache_root.display()
        );
        return ExitCode::SUCCESS;
    }

    let active_ids = match active_worktree_ids() {
        Ok(set) => set,
        Err(error) => {
            eprintln!("gc-target-cache: {error}");
            return ExitCode::FAILURE;
        }
    };

    let cache_ids = match read_cache_ids_from_validated_root(&cache_root) {
        Ok(set) => set,
        Err(error) => {
            eprintln!(
                "gc-target-cache: failed to read {}: {error}",
                cache_root.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let orphans: Vec<&String> = cache_ids.difference(&active_ids).collect();

    if orphans.is_empty() {
        println!(
            "gc-target-cache: {} cache entr{} match active worktrees; nothing to remove",
            cache_ids.len(),
            if cache_ids.len() == 1 { "y" } else { "ies" }
        );
        return ExitCode::SUCCESS;
    }

    let action = if dry_run { "would remove" } else { "removing" };
    for id in &orphans {
        let path = cache_root.join(id);
        println!("gc-target-cache: {action} {}", path.display());
        if dry_run {
            continue;
        }
        let vetted = path.canonicalize().unwrap_or_else(|e| panic!("{e}"));
        if !vetted.starts_with(&cache_root) {
            panic!(
                "refusing to remove cache path outside root: {}",
                vetted.display()
            );
        }
        if let Err(error) = fs::remove_dir_all(&vetted) {
            eprintln!(
                "gc-target-cache: failed to remove {}: {error}",
                vetted.display()
            );
            return ExitCode::FAILURE;
        }
    }

    if dry_run {
        println!(
            "gc-target-cache: dry-run complete; {} orphan entr{} would be removed",
            orphans.len(),
            if orphans.len() == 1 { "y" } else { "ies" }
        );
    } else {
        println!(
            "gc-target-cache: removed {} orphan entr{}",
            orphans.len(),
            if orphans.len() == 1 { "y" } else { "ies" }
        );
    }
    ExitCode::SUCCESS
}

fn resolve_cache_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; cannot locate cache root".to_string())?;
    let home = PathBuf::from(home);

    if let Ok(value) = std::env::var(CACHE_ROOT_ENV)
        && !value.is_empty()
    {
        let candidate = PathBuf::from(value);
        return validate_cache_root_within_home(&home, &candidate);
    }

    let default_root = home.join(DEFAULT_CACHE_SUBPATH);
    validate_cache_root_within_home(&home, &default_root)
}

fn validate_cache_root_within_home(home: &Path, candidate: &Path) -> Result<PathBuf, String> {
    let canonical_home = fs::canonicalize(home)
        .map_err(|error| format!("failed to canonicalize HOME {}: {error}", home.display()))?;

    // Canonicalize when possible (existing paths). For non-existing paths, validate parent.
    let canonical_candidate = match fs::canonicalize(candidate) {
        Ok(path) => path,
        Err(_) => {
            let parent = candidate.parent().ok_or_else(|| {
                format!(
                    "invalid cache root {}; no parent directory to validate",
                    candidate.display()
                )
            })?;
            let canonical_parent = fs::canonicalize(parent).map_err(|error| {
                format!(
                    "failed to canonicalize cache root parent {}: {error}",
                    parent.display()
                )
            })?;
            canonical_parent.join(
                candidate
                    .file_name()
                    .ok_or_else(|| format!("invalid cache root {}", candidate.display()))?,
            )
        }
    };

    if !canonical_candidate.starts_with(&canonical_home) {
        return Err(format!(
            "cache root {} is outside HOME {}",
            candidate.display(),
            canonical_home.display()
        ));
    }

    Ok(canonical_candidate)
}

fn active_worktree_ids() -> Result<BTreeSet<String>, String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|error| format!("failed to invoke `git worktree list`: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "`git worktree list --porcelain` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("`git worktree list` produced non-UTF-8 output: {error}"))?;
    let mut ids = BTreeSet::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("worktree ") else {
            continue;
        };
        let path = Path::new(rest.trim());
        let canonical = match fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let canonical_str = canonical.to_string_lossy();
        ids.insert(worktree_id(&canonical_str));
    }
    Ok(ids)
}

fn read_cache_ids_from_validated_root(root: &Path) -> io::Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            ids.insert(name.to_string());
        }
    }
    Ok(ids)
}

/// Derive the per-worktree cache id from an absolute worktree path. Must match
/// the shell derivation in `.envrc.example` (12-char SHA-256 hex prefix of the
/// absolute path bytes, with no trailing newline).
fn worktree_id(absolute_path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(absolute_path.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        // 6 bytes → 12 hex chars
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{read_cache_ids_from_validated_root, worktree_id};
    #[test]
    fn worktree_id_is_stable_12_hex_chars() {
        let id = worktree_id("/home/dev/projects/djogi/.worktrees/c1");
        assert_eq!(id.len(), 12, "id length must be 12: {id}");
        assert!(
            id.bytes().all(|b| b.is_ascii_hexdigit()),
            "id must be hex: {id}"
        );
        // Stable across calls
        assert_eq!(id, worktree_id("/home/dev/projects/djogi/.worktrees/c1"));
    }

    #[test]
    fn worktree_id_distinguishes_sibling_basenames() {
        let a = worktree_id("/home/dev/repo-a/.worktrees/c1");
        let b = worktree_id("/home/dev/repo-b/.worktrees/c1");
        assert_ne!(
            a, b,
            "two different absolute paths sharing a basename must hash to different ids"
        );
    }

    #[test]
    fn worktree_id_matches_shell_derivation_for_known_input() {
        // This locks parity with `.envrc.example`'s shell derivation:
        //   printf '%s' "<path>" | sha256sum | cut -c1-12
        // Verified by running that shell pipeline against the same input on a
        // POSIX system with coreutils sha256sum. If this test fails, the Rust
        // derivation has drifted from the shell derivation and a worktree's
        // direnv-managed cache directory will not match what gc-target-cache
        // sees as the active id, leaving cache entries orphaned.
        assert_eq!(
            worktree_id("/home/dev/projects/djogi"),
            "f1befb31ba62",
            "Rust derivation must match `printf '%%s' <path> | sha256sum | cut -c1-12`"
        );
    }

    #[test]
    fn read_cache_ids_from_validated_root_collects_subdirs_only() {
        let tmp_name = format!(
            "djogi-gc-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        if tmp_name.contains('/') || tmp_name.contains('\\') || tmp_name.contains("..") {
            panic!("unsafe temp path component: {tmp_name}");
        }
        let temp_canon = std::env::temp_dir().canonicalize().unwrap();
        let tmp = super::validate_cache_root_within_home(&temp_canon, &temp_canon.join(tmp_name))
            .expect("resolve tmp");
        djogi::migrate::create_workspace_dir_all(&temp_canon, &tmp).unwrap();
        let tmp = tmp.canonicalize().unwrap();

        let abc = djogi::migrate::resolve_write_workspace_path(&tmp, "abc123def456").unwrap();
        let def = djogi::migrate::resolve_write_workspace_path(&tmp, "789abc012def").unwrap();
        djogi::migrate::create_workspace_dir_all(&tmp, &abc).unwrap();
        djogi::migrate::create_workspace_dir_all(&tmp, &def).unwrap();
        djogi::migrate::write_workspace_file(&tmp, tmp.join("not-a-dir"), b"ignored").unwrap();

        let ids = read_cache_ids_from_validated_root(&tmp).expect("read");
        assert!(ids.contains("abc123def456"));
        assert!(ids.contains("789abc012def"));
        assert!(!ids.contains("not-a-dir"));

        let _ = djogi::migrate::remove_workspace_dir_all(
            &std::env::temp_dir().canonicalize().unwrap(),
            &tmp,
        );
    }
}
