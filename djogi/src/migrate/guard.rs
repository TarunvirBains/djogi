//! File-level workspace lock primitive (Phase 7 v3 §6 file-lock contract).
//!
//! # What it does
//!
//! Every migration-engine entry point that mutates on-disk state
//! (`compose`, `attune`, `apply`, `repair`, `baseline`) acquires a
//! single `LOCK_EX` advisory file lock on
//! `<workspace-root>/.djogi-migrations-lock`. The lock serialises
//! concurrent invocations of the migration tooling so two operators
//! running `djogi migrate` simultaneously cannot race on the same
//! `migrations/` tree or shared `target/djogi_pending/` staging area.
//! T4 owns the primitive; T5 (`repair`) and T6 (`compose` /
//! `apply` orchestration) consume it.
//!
//! # Mechanism
//!
//! Unix-only today. The implementation calls `flock(2)` directly via
//! `libc::flock` because the alternatives are heavier:
//!
//! - The full `nix` crate would pull in a large dependency just for
//!   one syscall wrapper.
//! - `fs2` and `file-lock` are abandoned or carry their own subtle
//!   bugs around timeout semantics.
//! - `flock` releases automatically on `close(fd)` and is reaped on
//!   abnormal process exit by the kernel; that gives us the
//!   "no stale lock cleanup needed" property without writing our own
//!   reaper.
//!
//! Windows support is deferred — the [`acquire`] entry point returns
//! a typed [`GuardError::WindowsUnsupported`] on non-unix targets.
//! When Windows lands it will use `LockFileEx` against the same path;
//! callers do not need to change.
//!
//! # PID file
//!
//! On a successful acquire the lock holder writes its PID (decimal
//! ASCII, terminated with `\n`) to the lock file. On a timeout, the
//! second acquirer reads the file, parses the PID, and surfaces it in
//! [`GuardError::Timeout`] so operators can identify the holder. We
//! intentionally write the PID *after* acquiring the lock and
//! *before* returning the [`WorkspaceGuard`]: the file content is
//! advisory only — `flock` itself protects mutual exclusion.
//!
//! # Bounded retry
//!
//! `flock(2)` has no native timeout, so the implementation polls
//! `flock(LOCK_EX | LOCK_NB)` every `RETRY_INTERVAL` (50 ms) up to
//! the deadline. The polling cadence is deliberately tight enough
//! that a process exiting cleanly never holds another up by more
//! than ~50 ms, but loose enough to avoid pegging a CPU core.
//!
//! # Composition with `pg_advisory_lock`
//!
//! The runner T4 acquires the file lock first, then the Postgres
//! advisory lock. The order is deterministic: every Djogi process
//! takes (file-lock, advisory-lock) in that sequence, so two
//! operators running concurrently cannot deadlock — one of them
//! waits on the file lock, the other waits on the advisory lock.
//!
//! # No regex
//!
//! The PID parser uses byte-level checks
//! (`u8::is_ascii_digit`) per the Djogi-wide no-regex policy. Plain
//! decimal ASCII, optionally followed by a trailing newline.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default acquire timeout — 30 seconds per Phase 7 v3 §6 file-lock
/// contract.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling cadence for the bounded-retry acquire loop. The kernel
/// hands the lock over with sub-millisecond latency once the holder
/// closes its file descriptor; this interval bounds the worst-case
/// wait between holder-exit and acquirer-wakeup.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// File name of the lock file inside the workspace root.
pub const LOCK_FILE_NAME: &str = ".djogi-migrations-lock";

/// Errors surfaced by [`acquire`].
#[derive(Debug)]
pub enum GuardError {
    /// `flock` could not be acquired within the timeout. `holder_pid`
    /// is the PID read from the lock file, or `None` if the file was
    /// empty / unparseable / unreadable. The runner surfaces this as
    /// the structured diagnostic `D025 lock held by another invocation`.
    Timeout {
        path: PathBuf,
        timeout: Duration,
        holder_pid: Option<i32>,
    },

    /// File-system I/O error opening, reading, or writing the lock
    /// file. Distinct from `Timeout` because the underlying cause is
    /// not contention — it might be a missing parent directory, a
    /// read-only filesystem, or a permissions problem.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// `flock(2)` returned an error other than `EWOULDBLOCK`. Wraps
    /// the underlying `errno` so operators can pinpoint the kernel
    /// failure mode (e.g. `EBADF`, `EINVAL`).
    Flock { path: PathBuf, errno: i32 },

    /// Windows is not supported in T4. When Windows lands it will
    /// use `LockFileEx`; until then this variant lets callers fail
    /// fast with an actionable message rather than a silent panic.
    #[cfg(not(unix))]
    WindowsUnsupported,
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Timeout {
                path,
                timeout,
                holder_pid,
            } => match holder_pid {
                Some(pid) => write!(
                    f,
                    "workspace migration lock at {path} held by another invocation \
                     (PID {pid}); waited {timeout:?} before giving up",
                    path = path.display(),
                ),
                None => write!(
                    f,
                    "workspace migration lock at {path} held by another invocation \
                     (PID unknown — lock file empty or unreadable); waited {timeout:?} \
                     before giving up",
                    path = path.display(),
                ),
            },
            GuardError::Io { path, source } => write!(
                f,
                "I/O error on workspace migration lock {path}: {source}",
                path = path.display(),
            ),
            GuardError::Flock { path, errno } => write!(
                f,
                "flock(2) on workspace migration lock {path} failed (errno={errno})",
                path = path.display(),
            ),
            #[cfg(not(unix))]
            GuardError::WindowsUnsupported => write!(
                f,
                "workspace migration lock: Windows support deferred; build on Linux \
                 or macOS, or run the migration tooling under WSL"
            ),
        }
    }
}

impl std::error::Error for GuardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GuardError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// RAII guard returned by [`acquire`]. The `flock` is released when
/// this value is dropped — either via the explicit `drop()` call or
/// when the holding scope exits (including the panic-unwind path).
///
/// The guard intentionally does NOT delete the lock file on drop:
/// keeping the file around keeps its inode stable across invocations,
/// which is what `flock` keys on. Deleting and recreating between
/// invocations would re-allocate the inode and could re-order kernel
/// state in surprising ways.
#[derive(Debug)]
pub struct WorkspaceGuard {
    /// Lock file descriptor. Closed (and the `flock` released) on
    /// drop. Wrapped in `Option` so the destructor can take it cleanly
    /// without reaching into raw FDs.
    file: Option<File>,
    /// Path to the lock file — kept for diagnostics.
    path: PathBuf,
}

impl WorkspaceGuard {
    /// Path to the lock file held by this guard.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        // Closing the File drops its underlying fd, and `flock` is
        // automatically released by the kernel on close. We do NOT
        // truncate the PID file here — leaving the last-holder PID in
        // the file is informational; the next acquirer overwrites it.
        // The `take()` pattern ensures we always release even if a
        // future revision adds a fallible step before `drop_file`.
        if let Some(f) = self.file.take() {
            drop(f);
        }
    }
}

/// Acquire the workspace migration lock. Bounded-retry; on timeout,
/// reads the holder PID from the lock file and surfaces it via
/// [`GuardError::Timeout`].
///
/// `path` is typically `<workspace-root>/.djogi-migrations-lock` —
/// see [`LOCK_FILE_NAME`]. Callers that want to compose their own
/// path (tests, cargo-djogi sub-tools that pin the workspace root)
/// pass it directly.
///
/// `timeout` is the bound on the polling loop; production callers
/// pass [`DEFAULT_TIMEOUT`]. Tests use a much shorter value to keep
/// the suite under a few seconds even on contended runners.
///
/// # Errors
///
/// - [`GuardError::Io`] — could not open / create the lock file.
/// - [`GuardError::Timeout`] — another invocation held the lock for
///   the full `timeout` duration.
/// - [`GuardError::Flock`] — the kernel returned a non-`EWOULDBLOCK`
///   `flock(2)` error.
/// - [`GuardError::WindowsUnsupported`] — non-unix targets (compile-
///   time gate); the symbol exists for type compatibility only.
pub fn acquire(path: &Path, timeout: Duration) -> Result<WorkspaceGuard, GuardError> {
    #[cfg(unix)]
    {
        acquire_unix(path, timeout)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, timeout);
        Err(GuardError::WindowsUnsupported)
    }
}

#[cfg(unix)]
fn acquire_unix(path: &Path, timeout: Duration) -> Result<WorkspaceGuard, GuardError> {
    use std::os::unix::io::AsRawFd;

    // Ensure the parent directory exists. Treat
    // a missing parent as an I/O error rather than silently creating a
    // workspace root — the caller (T6 `compose` / runner) chose the
    // workspace; we do not invent file layout.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(GuardError::Io {
            path: parent.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "workspace migration lock parent directory does not exist",
            ),
        });
    }

    // Open / create the lock file. We do NOT truncate — the PID
    // contents from the previous holder are overwritten only after we
    // successfully acquire the lock, so a concurrent reader during
    // the transition window sees the previous PID rather than an
    // empty file.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| GuardError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;

    let fd = file.as_raw_fd();
    let deadline = Instant::now() + timeout;

    loop {
        // SAFETY: `fd` is a valid file descriptor obtained from a
        // `File` we own; `flock` accepts any open fd and the LOCK_EX
        // | LOCK_NB constants are defined for every libc target we
        // build on.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            // Acquired. Write our PID to the file. We seek to start +
            // truncate inside the helper so a smaller PID does not
            // leave stale trailing bytes from a longer prior PID.
            let mut owned = file;
            write_pid(&mut owned, path)?;
            return Ok(WorkspaceGuard {
                file: Some(owned),
                path: path.to_path_buf(),
            });
        }
        // SAFETY: `__errno_location` returns a thread-local pointer
        // that is valid for the lifetime of the calling thread. We
        // do not retain the pointer across calls.
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EWOULDBLOCK && errno != libc::EAGAIN {
            return Err(GuardError::Flock {
                path: path.to_path_buf(),
                errno,
            });
        }
        if Instant::now() >= deadline {
            // Read the holder PID for diagnostics. Failures here are
            // non-fatal — surface `holder_pid: None` and let the
            // operator look at the file directly if curious.
            let holder_pid = read_pid(path).ok();
            return Err(GuardError::Timeout {
                path: path.to_path_buf(),
                timeout,
                holder_pid,
            });
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Write the current process PID to the lock file. Truncates first
/// so a smaller decimal does not leave a tail from a previous holder.
#[cfg(unix)]
fn write_pid(file: &mut File, path: &Path) -> Result<(), GuardError> {
    let pid: i32 = std::process::id() as i32;
    file.set_len(0).map_err(|e| GuardError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|e| GuardError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let line = format!("{pid}\n");
    file.write_all(line.as_bytes())
        .map_err(|e| GuardError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    file.flush().map_err(|e| GuardError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(())
}

/// Read and parse the holder PID from the lock file. Decimal ASCII,
/// optionally followed by `\n`. No regex — byte-level
/// `is_ascii_digit` walk per the Djogi-wide policy.
fn read_pid(path: &Path) -> Result<i32, std::io::Error> {
    let mut f = File::open(path)?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    let trimmed = buf.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lock file is empty",
        ));
    }
    // Accept an optional leading minus for symmetry with `i32`,
    // although in practice PIDs are non-negative on every supported
    // OS. Reject anything else.
    let bytes = trimmed.as_bytes();
    let (sign_offset, _negative) = if bytes[0] == b'-' {
        (1, true)
    } else {
        (0, false)
    };
    if sign_offset == bytes.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lock file PID is just a sign",
        ));
    }
    for &b in &bytes[sign_offset..] {
        if !b.is_ascii_digit() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "lock file PID is not a decimal integer",
            ));
        }
    }
    trimmed
        .parse::<i32>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn temp_lock_path() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("djogi-guard-test-{stamp}.lock"))
    }

    #[test]
    fn acquire_writes_pid_to_lock_file() {
        let path = temp_lock_path();
        let guard = acquire(&path, Duration::from_secs(1)).expect("acquire");
        let contents = std::fs::read_to_string(&path).expect("read");
        let trimmed = contents.trim();
        let pid: i32 = trimmed.parse().expect("parse pid");
        assert_eq!(pid, std::process::id() as i32);
        drop(guard);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn second_acquirer_times_out_with_holder_pid() {
        // First acquirer holds the lock until the test signals it can
        // release. Second acquirer attempts with a short timeout and
        // must time out, surfacing the first acquirer's PID.
        let path = temp_lock_path();
        let release = Arc::new(AtomicBool::new(false));
        let release_clone = release.clone();
        let path_clone = path.clone();
        let holder_thread = std::thread::spawn(move || {
            let _g = acquire(&path_clone, Duration::from_secs(2)).expect("first acquire");
            // Hold until released.
            while !release_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        // Give the holder a moment to acquire and write its PID.
        std::thread::sleep(Duration::from_millis(100));
        // Second acquire — short timeout (200 ms) so the test stays
        // fast. Must time out and surface the holder's PID (which is
        // the same PID as us, since both threads live in this
        // process; the test asserts the wiring, not cross-process
        // behaviour).
        let err =
            acquire(&path, Duration::from_millis(200)).expect_err("second acquire must time out");
        match err {
            GuardError::Timeout { holder_pid, .. } => {
                assert_eq!(holder_pid, Some(std::process::id() as i32));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        // Release the holder and join.
        release.store(true, Ordering::Release);
        holder_thread.join().expect("holder thread join");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn release_on_drop_lets_next_acquirer_succeed() {
        let path = temp_lock_path();
        {
            let _g = acquire(&path, Duration::from_secs(1)).expect("first acquire");
        } // drop releases the lock
        let g = acquire(&path, Duration::from_millis(200)).expect("second acquire");
        drop(g);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_accepts_trailing_newline() {
        let path = temp_lock_path();
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(read_pid(&path).unwrap(), 12345);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_accepts_no_trailing_newline() {
        let path = temp_lock_path();
        std::fs::write(&path, "12345").unwrap();
        assert_eq!(read_pid(&path).unwrap(), 12345);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_rejects_non_digit() {
        let path = temp_lock_path();
        std::fs::write(&path, "12a45").unwrap();
        let err = read_pid(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_rejects_empty() {
        let path = temp_lock_path();
        std::fs::write(&path, "").unwrap();
        let err = read_pid(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&path);
    }
}
