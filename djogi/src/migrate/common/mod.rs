use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// How to resolve a candidate path against an existing base.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CandidateResolutionMode {
    /// Candidate must exist on-disk.
    Existing,
    /// Candidate may be missing; if so, resolve via the nearest existing
    /// parent directory.
    MayCreate,
}

/// Canonicalize a workspace-like base path before joining relative
/// candidates.
pub(crate) fn canonicalize_base(base: &Path) -> io::Result<PathBuf> {
    base.canonicalize()
}

/// Canonicalize a path, falling back to the canonicalized parent plus the
/// final path component when the path itself does not exist.
///
/// This is the same pattern we use for lock/lockfile-like targets and for
/// path candidates that are expected to be created as part of the operation.
pub(crate) fn canonicalize_with_parent_fallback(path: &Path) -> io::Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }

    let mut path = path.to_path_buf();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        let parent = match path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("path {} has no parent directory", path.display()),
                ));
            }
        };
        let component = path.file_name().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidInput,
                format!("path {} has no final path component", path.display()),
            )
        })?;
        suffix.push(component.to_owned());

        match parent.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for part in suffix.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(err) => {
                // Continue walking up until we hit an existing ancestor.
                if parent == path {
                    return Err(io::Error::new(
                        err.kind(),
                        format!(
                            "failed to resolve any existing ancestor for path {}: {err}",
                            path.display()
                        ),
                    ));
                }
                path = parent;
            }
        }
    }
}

/// Resolve `candidate` against `base` and assert it stays within `base`
/// after resolution.
pub(crate) fn resolve_within_base(
    base: &Path,
    candidate: &Path,
    mode: CandidateResolutionMode,
) -> io::Result<PathBuf> {
    let candidate = match mode {
        CandidateResolutionMode::Existing => candidate.canonicalize()?,
        CandidateResolutionMode::MayCreate => canonicalize_with_parent_fallback(candidate)?,
    };
    if !candidate.starts_with(base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "candidate path {} is outside allowed base {}",
                candidate.display(),
                base.display()
            ),
        ));
    }
    Ok(candidate)
}

/// Resolve a workspace-relative or workspace-absolute path against a
/// canonicalized base and verify containment according to `mode`.
pub(crate) fn resolve_workspace_path_with_mode(
    workspace_root: &Path,
    candidate: &Path,
    mode: CandidateResolutionMode,
) -> io::Result<PathBuf> {
    let workspace_root = canonicalize_base(workspace_root)?;
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    };
    resolve_within_base(&workspace_root, &candidate, mode)
}

/// Resolve a workspace path for reads.
pub(crate) fn resolve_read_workspace_path<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    resolve_workspace_path_with_mode(
        workspace_root,
        candidate.as_ref(),
        CandidateResolutionMode::Existing,
    )
}

/// Resolve a workspace path for writes.
pub(crate) fn resolve_write_workspace_path<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    resolve_workspace_path_with_mode(
        workspace_root,
        candidate.as_ref(),
        CandidateResolutionMode::MayCreate,
    )
}

/// Resolve and validate a write path's parent directory.
#[expect(
    dead_code,
    reason = "reserved typed helper for future workspace-parent call sites"
)]
pub(crate) fn resolve_parent_workspace_path<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    let workspace_root = canonicalize_base(workspace_root)?;
    let parent = candidate
        .as_ref()
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        workspace_root.join(parent)
    };
    resolve_within_base(&workspace_root, &parent, CandidateResolutionMode::Existing)
}

/// Backward-compatible wrapper used by existing call-sites:
/// resolves missing candidates as well as possible.
pub(crate) fn ensure_within_base(base: &Path, candidate: &Path) -> io::Result<PathBuf> {
    resolve_within_base(base, candidate, CandidateResolutionMode::MayCreate)
}

/// Resolve an absolute-like `candidate` path under the canonicalized
/// workspace root.
#[expect(
    dead_code,
    reason = "reserved typed helper for future existing-path call sites"
)]
pub(crate) fn resolve_existing_workspace_path<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    resolve_workspace_path_with_mode(
        workspace_root,
        candidate.as_ref(),
        CandidateResolutionMode::Existing,
    )
}

/// Resolve a relative `candidate` path under the canonicalized
/// workspace root, allowing non-existent tail elements.
pub(crate) fn resolve_maybe_missing_workspace_path<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    resolve_workspace_path_with_mode(
        workspace_root,
        candidate.as_ref(),
        CandidateResolutionMode::MayCreate,
    )
}

/// Resolve and read bytes from a file rooted at `workspace_root` (existing path only).
///
/// This helper is intentionally narrow: it performs path resolution and
/// containment before the read to avoid duplicating the same guard logic at
/// each call site.
pub(crate) fn read_workspace_file<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<Vec<u8>> {
    let candidate = resolve_read_workspace_path(workspace_root, candidate)?;
    std::fs::read(candidate)
}

/// Resolve and read UTF-8 text from a file rooted at `workspace_root` (existing path only).
#[expect(
    dead_code,
    reason = "reserved typed helper for future string-read call sites"
)]
pub(crate) fn read_workspace_file_to_string<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<String> {
    let bytes = read_workspace_file(workspace_root, candidate)?;
    String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Resolve and write bytes to a file rooted at `workspace_root`, creating missing
/// parents if needed.
#[expect(
    dead_code,
    reason = "reserved typed helper for future write call sites"
)]
pub(crate) fn write_workspace_file<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
    bytes: &[u8],
) -> io::Result<PathBuf> {
    let candidate = resolve_write_workspace_path(workspace_root, candidate)?;
    if let Some(parent) = candidate.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&candidate, bytes)?;
    Ok(candidate)
}

/// Resolve a candidate path and create its parent directories under
/// a validated workspace root.
pub(crate) fn create_workspace_parent_dirs<P: AsRef<Path>>(
    workspace_root: &Path,
    candidate: P,
) -> io::Result<PathBuf> {
    let candidate = resolve_workspace_path_with_mode(
        workspace_root,
        candidate.as_ref(),
        CandidateResolutionMode::MayCreate,
    )?;
    let parent = candidate
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    Ok(parent.to_path_buf())
}
