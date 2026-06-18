// djogi-allow-secret-file: this scanner module exists to detect the very
// patterns named below. Every literal placeholder, dummy credential, and
// inline example in this file is an obvious fake used to exercise detection;
// no real secret string is permitted to land here. The whole-file marker
// (with required colon) prevents the scanner from self-reporting on its own
// pattern tables and test fixtures when it walks the repo.
//
// Goal — flag obvious credential-bearing text before it lands in commits or
// gets pasted into public GitHub issues. This is a *guardrail*, not a complete
// DLP product. We aim for high precision on a small, well-described pattern
// set rather than broad heuristics that would drown adopters in false positives.
//
// What is checked:
//   1. URLs of the form `scheme://user:password@host[:port][/path]`
//      where scheme is a known credential-bearing scheme (postgres, mysql,
//      mongodb, redis, http, ssh, …), and the password is not a placeholder
//      or a known intentional dummy pair (`djogi:djogi` on localhost, etc.).
//   2. Assignments of well-known secret-bearing environment variables
//      (`DATABASE_URL`, `PGPASSWORD`, `POSTGRES_PASSWORD`, …) and any
//      variable whose name ends in a credential-shaped suffix
//      (`_PASSWORD`, `_TOKEN`, `_SECRET`, `_API_KEY`, …), to a value that
//      is not a placeholder.
//   3. PEM private-key block headers (`-----BEGIN … PRIVATE KEY-----`).
//
// What is not checked:
//   - Generic high-entropy strings — too noisy without a curated allowlist
//     of "this is intentionally a base64 fixture" sites. Out of scope for
//     #193's preflight-prevention goal.
//   - Historical commits, branch contents, remote GitHub bodies — those are
//     a separate later sweep. This module only runs against working-tree
//     content, staged diffs, or stdin pasted by an adopter.
//
// Suppression — two markers, deliberately verbose so they are easy to grep
// and review. The trailing colon is required and is what distinguishes the
// line marker from the file marker (so a file marker on the previous line
// does not accidentally fire the line-marker check on the following line):
//   - `djogi-allow-secret: <reason>`         — line-scoped. Skips findings
//                                              on the same line, or on any
//                                              line whose contiguous comment
//                                              block immediately above
//                                              contains the marker (walk-back
//                                              capped at 20 lines, stopped
//                                              by any non-comment / blank
//                                              line).
//   - `djogi-allow-secret-file: <reason>`    — file-scoped. Anywhere in the
//                                              first 20 lines of a file
//                                              removes the whole file from
//                                              scanning.
// The colon and rationale are required; reviewers should reject suppressions
// without a rationale.
//
// No regex — pattern detection is byte-level (`u8::is_ascii_*`, explicit
// slice equality, sorted-const-slice `binary_search`). The framework-wide
// no-regex rule (`docs/spec/decisions.md`) applies here too.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

// ===== public surface =====

/// Where the scanner reads its input from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ScanMode {
    /// Scan every tracked file in the working tree (enumerated via
    /// `git ls-files`). Use this for a periodic sweep or in CI.
    Repo,
    /// Scan only the lines added in the current staged diff
    /// (`git diff --cached`). Intended as a pre-commit guard.
    Staged,
    /// Scan stdin and treat findings as referring to the pasted text.
    /// Use this to vet an issue body or PR description before posting.
    Stdin,
}

pub fn run(mode: ScanMode) -> ExitCode {
    let outcome = match mode {
        ScanMode::Repo => scan_repo(),
        ScanMode::Staged => scan_staged(),
        ScanMode::Stdin => scan_stdin(),
    };

    let findings = match outcome {
        Ok(findings) => findings,
        Err(error) => {
            eprintln!("check-secrets: {error}");
            return ExitCode::FAILURE;
        }
    };

    report(&findings, mode)
}

// ===== types =====

#[derive(Debug, Eq, PartialEq)]
struct Finding {
    /// `None` indicates a stdin-mode finding (no on-disk path).
    path: Option<PathBuf>,
    /// 1-indexed line number within the scanned source.
    line: usize,
    kind: SecretKind,
    /// Human-readable description. Must NEVER contain the raw secret value;
    /// always redacts to the structural pattern + minimal context.
    excerpt: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
enum SecretKind {
    /// `scheme://user:password@host[...]` where the password looks real.
    UrlWithCredentials,
    /// `KNOWN_VAR=value` where the variable name is in `NAMED_SECRET_VARS`.
    EnvAssignmentKnown(&'static str),
    /// `<word ending in suffix>=value`, e.g. `MY_SERVICE_TOKEN=...`.
    EnvAssignmentSuffix(&'static str),
    /// PEM header line introducing a private-key block.
    PrivateKeyBlock,
}

impl SecretKind {
    fn label(self) -> String {
        match self {
            SecretKind::UrlWithCredentials => "url with embedded credentials".to_owned(),
            SecretKind::EnvAssignmentKnown(name) => format!("secret env var assignment ({name})"),
            SecretKind::EnvAssignmentSuffix(suffix) => {
                format!("credential-suffix env var assignment ({suffix})")
            }
            SecretKind::PrivateKeyBlock => "private-key block header".to_owned(),
        }
    }
}

/// Classification of a `scheme://[user:pass@]host[:port]` authority. Shared
/// between the URL scanner (decides whether to emit a finding) and the
/// env-var scanner (decides whether to defer to the URL scanner when an
/// env value happens to be a credentialed URL).
#[derive(Debug, Eq, PartialEq)]
enum UrlAuthority {
    /// Scheme is not in [`CREDENTIAL_URL_SCHEMES`], or the authority has no
    /// `user:pass@host` shape. The URL scanner does not emit.
    NotCredential,
    /// Password slot is a placeholder (shell-var expansion, `<password>`,
    /// mustache, …). The URL scanner intentionally suppresses; the env
    /// scanner also defers so doc snippets like
    /// `DATABASE_URL=postgres://user:${PGPASSWORD}@host` are not noisy.
    Placeholder,
    /// `user:pass` pair is in [`KNOWN_DUMMY_USERPASS`]. Same suppression
    /// policy as [`UrlAuthority::Placeholder`].
    Dummy,
    /// Real-looking credentials; `excerpt` is the redacted text suitable
    /// for a [`Finding`].
    Real { excerpt: String },
}

// ===== constants =====

// Each table is kept sorted in ASCII order so `binary_search` is correct and
// review of additions stays trivial. Bytes / chars only — no regex.

/// URL schemes that commonly carry credentials in the `user:pass@host` form.
const CREDENTIAL_URL_SCHEMES: &[&str] = &[
    "amqp",
    "amqps",
    "ftp",
    "http",
    "https",
    "mariadb",
    "mongodb",
    "mongodb+srv",
    "mysql",
    "postgres",
    "postgresql",
    "redis",
    "rediss",
    "sftp",
    "smtp",
    "smtps",
    "ssh",
];

/// Env-var names that always carry secrets when assigned a real value.
/// Sorted so new entries are easy to slot in.
const NAMED_SECRET_VARS: &[&str] = &[
    "API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "CLIENT_SECRET",
    "CRUD_LOG_URL",
    "DATABASE_PASSWORD",
    "DATABASE_URL",
    "DB_PASSWORD",
    "EVENT_LOG_URL",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "MONGODB_URI",
    "MONGO_PASSWORD",
    "MYSQL_PASSWORD",
    "OPENAI_API_KEY",
    "PGPASSWORD",
    "POSTGRES_PASSWORD",
    "REDIS_URL",
    "SLACK_BOT_TOKEN",
    "SLACK_WEBHOOK",
    "SLACK_WEBHOOK_URL",
    "STRIPE_API_KEY",
    "STRIPE_SECRET_KEY",
    "TWILIO_API_KEY",
    "TWILIO_AUTH_TOKEN",
];

/// Suffix forms (case-sensitive, upper) that mark the assigned value as a
/// likely secret. Every suffix begins with `_` so we only match at a clear
/// word boundary within the key (no partial-token false positives).
const NAMED_SECRET_SUFFIXES: &[&str] = &[
    "_ACCESS_TOKEN",
    "_APIKEY",
    "_API_KEY",
    "_AUTH_TOKEN",
    "_BEARER_TOKEN",
    "_CLIENT_SECRET",
    "_PASSWD",
    "_PASSWORD",
    "_PRIVATE_KEY",
    "_REFRESH_TOKEN",
    "_SECRET",
    "_WEBHOOK",
    "_WEBHOOK_SECRET",
    "_TOKEN",
];

/// Substrings that, if present anywhere in the value, mark it as an obvious
/// placeholder rather than a real secret. Mix of case variants because real
/// docs use both.
const PLACEHOLDER_NEEDLES: &[&str] = &[
    "${", // shell parameter expansion: ${VAULT_PASSWORD}
    "<",  // angle-bracket placeholder: <password>
    "***",
    "CHANGEME",
    "Changeme",
    "REDACTED",
    "YOUR_",
    "changeme",
    "example.com",
    "example.org",
    "placeholder",
    "redacted",
    "supersecretpassword", // explicit "this is a doc example" marker
    "xxx",
    "your-",
    "your_",
    "{{", // mustache / jinja-style template placeholder
];

/// Known intentionally-dummy `user:pass` pairs that appear in our local dev
/// and CI fixtures (docker-compose, GHA service env, integration test docs).
/// When both halves match, the URL is allowed regardless of host.
const KNOWN_DUMMY_USERPASS: &[(&str, &str)] = &[
    ("djogi", "djogi"), // local CI + docker-compose
    ("postgres", "postgres"),
    ("user", "pass"),
    ("user", "password"),
];

/// Hosts that mark a URL as local / containerised / test by themselves.
const DUMMY_HOSTS: &[&str] = &[
    "0.0.0.0",
    "127.0.0.1",
    "::1",
    "db",
    "host.docker.internal",
    "localhost",
    "postgres",
];

/// Line-level suppression marker. Any line containing this string, or whose
/// immediately preceding line contains it, is excluded from scanning. The
/// trailing colon is part of the marker — it forces adopters to add a
/// rationale and keeps the line marker distinct from `ALLOW_FILE_MARKER`
/// (whose extra `-file` segment falls between `secret` and the colon).
const ALLOW_LINE_MARKER: &str = "djogi-allow-secret:";

/// File-level suppression marker. A line containing this string within the
/// first 20 lines of a file removes the entire file from scanning. The
/// trailing colon is part of the marker for the same reason as
/// [`ALLOW_LINE_MARKER`].
const ALLOW_FILE_MARKER: &str = "djogi-allow-secret-file:";

/// Maximum file size to scan. Larger files are skipped silently; they are
/// either binary blobs (skip is correct) or generated lookup tables where
/// secrets are unlikely to be authored by hand and noise would be high.
const SCAN_SIZE_LIMIT: u64 = 1024 * 1024;

/// PEM private-key markers. A header line trims to `-----BEGIN ... PRIVATE KEY-----`
/// where `...` is the optional algorithm label (`RSA`, `EC`, `OPENSSH`, etc.).
const PEM_HEADER_PREFIX: &str = "-----BEGIN ";
const PEM_HEADER_DASHES: &str = "-----";
const PEM_KEY_LABEL_NEEDLE: &str = "PRIVATE KEY";

// ===== top-level scan drivers =====

fn scan_repo() -> Result<Vec<Finding>, String> {
    let mut findings = Vec::new();
    let files = list_repo_files()?;
    let cwd = env::current_dir()
        .and_then(|p| p.canonicalize())
        .map_err(|error| format!("cannot resolve current directory: {error}"))?;
    for path in files {
        if should_skip_file(&path) {
            continue;
        }
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
        if !canonical.starts_with(&cwd) {
            continue;
        }
        let Some(content) = read_text_file(&canonical) else {
            continue;
        };
        scan_text(&content, Some(&path), &mut findings);
    }
    Ok(findings)
}

fn scan_staged() -> Result<Vec<Finding>, String> {
    // Strategy: parse the unified diff to discover which (path, line) pairs
    // the commit will introduce, then scan the *staged* content of each
    // affected path with the full text scanner. Filtering at the end keeps
    // file markers, line markers, and contiguous-comment-block markers
    // working exactly the same way they do in a repo sweep — adopters only
    // have to learn one suppression model.
    let mut findings = Vec::new();
    let diff = run_git(&["diff", "--cached", "--no-color", "--unified=0"])?;

    let mut by_path: BTreeMap<PathBuf, BTreeSet<usize>> = BTreeMap::new();
    for line in parse_staged_diff(&diff) {
        by_path
            .entry(line.path)
            .or_default()
            .insert(line.new_line_number);
    }

    for (path, added_lines) in by_path {
        let staged = match read_staged_file(&path) {
            Some(content) => content,
            None => continue,
        };
        let mut local = Vec::new();
        scan_text(&staged, Some(&path), &mut local);
        for finding in local {
            if added_lines.contains(&finding.line) {
                findings.push(finding);
            }
        }
    }

    Ok(findings)
}

fn read_staged_file(path: &Path) -> Option<String> {
    // Read the staged blob (index version) so line numbers match what
    // `git diff --cached` reported. A missing index entry (deletion) or any
    // binary content falls through to `None`.
    let spec = format!(":{}", path.display());
    run_git(&["show", &spec]).ok()
}

fn scan_stdin() -> Result<Vec<Finding>, String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|error| format!("failed to read stdin: {error}"))?;
    let mut findings = Vec::new();
    scan_text(&buf, None, &mut findings);
    Ok(findings)
}

// ===== reporting =====

fn report(findings: &[Finding], mode: ScanMode) -> ExitCode {
    for finding in findings {
        let location = match &finding.path {
            Some(path) => format!("{}:{}", display_path(path), finding.line),
            None => format!("<stdin>:{}", finding.line),
        };
        // Excerpt is already redacted to a structural template; we never
        // print the raw secret here.
        eprintln!(
            "{location}: {kind}: {excerpt}",
            kind = finding.kind.label(),
            excerpt = finding.excerpt,
        );
    }

    let label = match mode {
        ScanMode::Repo => "repo sweep",
        ScanMode::Staged => "staged diff",
        ScanMode::Stdin => "stdin",
    };

    if findings.is_empty() {
        eprintln!("check-secrets: {label}: no findings");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "check-secrets: {label}: {n} finding{plural}; \
             redact and remove before commit / before pasting into public text. \
             For intentional examples, add `// {marker} <reason>` on the line above.",
            n = findings.len(),
            plural = if findings.len() == 1 { "" } else { "s" },
            marker = ALLOW_LINE_MARKER,
        );
        ExitCode::FAILURE
    }
}

// ===== file enumeration =====

fn list_repo_files() -> Result<Vec<PathBuf>, String> {
    match list_tracked_files() {
        Ok(files) => Ok(files),
        Err(error) if should_use_act_filesystem_fallback(&error) => {
            eprintln!("check-secrets: git ls-files unavailable under act; using filesystem sweep");
            list_filesystem_repo_files(Path::new("."))
        }
        Err(error) => Err(error),
    }
}

fn list_tracked_files() -> Result<Vec<PathBuf>, String> {
    let output = run_git(&["ls-files", "-z"])?;
    let mut paths = Vec::new();
    for chunk in output.split('\0') {
        if chunk.is_empty() {
            continue;
        }
        paths.push(PathBuf::from(chunk));
    }
    paths.sort();
    Ok(paths)
}

fn should_use_act_filesystem_fallback(error: &str) -> bool {
    env::var_os("ACT").is_some() && error.contains("not a git repository")
}

fn list_filesystem_repo_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve root path {}: {error}", root.display()))?;
    let mut paths = Vec::new();
    collect_filesystem_repo_files(&canonical_root, &canonical_root, &mut paths)
        .map_err(|error| format!("filesystem sweep failed: {error}"))?;
    paths.sort();
    Ok(paths)
}

fn collect_filesystem_repo_files(
    canonical_root: &Path,
    dir: &Path,
    paths: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let canonical_dir = match dir.canonicalize() {
        Ok(path) => path,
        Err(_) => return Ok(()),
    };
    if !canonical_dir.starts_with(canonical_root) {
        return Ok(());
    }

    for entry in fs::read_dir(&canonical_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Validate containment before following symlinks or processing entries.
        if !path.starts_with(canonical_root) {
            continue;
        }

        let file_type = entry.file_type()?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");

        if file_type.is_dir() {
            if matches!(name, ".git" | ".worktrees" | "node_modules" | "target") {
                continue;
            }
            collect_filesystem_repo_files(canonical_root, &path, paths)?;
        } else if file_type.is_file() {
            paths.push(
                path.strip_prefix(canonical_root)
                    .unwrap_or(&path)
                    .to_owned(),
            );
        }
    }

    Ok(())
}

fn should_skip_file(path: &Path) -> bool {
    // The scanner runs only on text files; binary files are caught by
    // the UTF-8 check in `read_text_file`. We additionally skip a small
    // set of paths that the project owns and where every match would be
    // a known intentional fixture — these are also marked with the
    // file-level allow marker, but skipping here saves the read.
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("Cargo.lock"),
    )
}

fn read_text_file(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > SCAN_SIZE_LIMIT {
        return None;
    }
    fs::read_to_string(path).ok()
}

// ===== git plumbing =====

fn run_git(args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.args(args);
    clear_invalid_git_plumbing_env(&mut command);
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("failed to invoke `git {}`: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git {} produced non-UTF-8 output: {error}", args.join(" ")))
}

const GIT_PLUMBING_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_OBJECT_DIRECTORY_RELATIVE",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

fn clear_invalid_git_plumbing_env(command: &mut Command) {
    for name in GIT_PLUMBING_ENV_VARS {
        if is_invalid_git_env_value(name) {
            command.env_remove(name);
        }
    }
}

fn is_invalid_git_env_value(name: &str) -> bool {
    match env::var_os(name) {
        Some(value) => {
            let value = value.to_string_lossy();
            let value = value.trim();
            value.is_empty() || value == "(null)"
        }
        None => false,
    }
}

// ===== staged-diff parsing =====

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct StagedLine {
    pub path: PathBuf,
    pub new_line_number: usize,
    pub line_text: String,
}

pub(crate) fn parse_staged_diff(diff: &str) -> Vec<StagedLine> {
    // We walk a unified diff produced with `--unified=0` — every change is
    // expressed as a `+` (add) or `-` (delete) line under a hunk header
    // `@@ -a,b +c,d @@`. We track the new-file line counter through `+` and
    // context lines (context lines do not exist with `-U0`, but the counter
    // logic stays correct either way).
    let mut out = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut new_line = 0usize;

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // `+++ b/path` — record the post-image path. `git diff` adds a
            // `b/` prefix on every entry except `/dev/null`, which marks a
            // pure deletion (no `+` lines follow, so we don't need to scan).
            let rest = rest.trim_start_matches("b/");
            if rest == "/dev/null" {
                current_path = None;
            } else {
                current_path = Some(PathBuf::from(rest));
            }
            continue;
        }
        if line.starts_with("--- ") {
            // `--- a/path` — ignored; we only care about the post-image side.
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@") {
            // Hunk header: `@@ -a[,b] +c[,d] @@ <optional section heading>`.
            new_line = parse_new_line_start(rest).unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            // `+++ ` is handled above; any other `+` is an added line.
            if let Some(path) = current_path.as_ref() {
                out.push(StagedLine {
                    path: path.clone(),
                    new_line_number: new_line,
                    line_text: rest.to_owned(),
                });
            }
            new_line += 1;
        } else if line.starts_with(' ') {
            // Context line — advance the new-file counter, don't emit.
            new_line += 1;
        }
        // `-` lines and any other prefix do not advance the new-file counter.
    }

    out
}

fn parse_new_line_start(hunk_tail: &str) -> Option<usize> {
    // `hunk_tail` is everything after the opening `@@`. We look for the next
    // `+<digits>` token and parse it. The diff format guarantees `+` for the
    // post-image hunk header, so a plain byte search is sufficient.
    let bytes = hunk_tail.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'+' {
            let start = cursor + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                return hunk_tail[start..end].parse::<usize>().ok();
            }
        }
        cursor += 1;
    }
    None
}

// ===== text-level scanning =====

fn scan_text(content: &str, path: Option<&Path>, findings: &mut Vec<Finding>) {
    // File-level allow marker — only the first 20 lines are inspected so that
    // a bottom-of-file comment cannot accidentally suppress a leaked secret
    // higher up.
    if content
        .lines()
        .take(20)
        .any(|line| line.contains(ALLOW_FILE_MARKER))
    {
        return;
    }

    let lines: Vec<&str> = content.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.contains(ALLOW_LINE_MARKER) || preceded_by_allow_marker(&lines, index) {
            continue;
        }
        scan_one_line(line, index + 1, path, findings);
    }
}

fn scan_one_line(line: &str, line_number: usize, path: Option<&Path>, findings: &mut Vec<Finding>) {
    scan_url_with_credentials(line, line_number, path, findings);
    scan_env_assignment(line, line_number, path, findings);
    scan_pem_private_key_header(line, line_number, path, findings);
}

/// Returns true if the contiguous comment block immediately above `index`
/// contains [`ALLOW_LINE_MARKER`]. "Comment line" is recognised across the
/// major repo file types (`//`, `#`, `;`, `<!--`); a non-comment or blank
/// line terminates the walk-back. The walk-back is capped at 20 lines to
/// keep accidental suppression of unrelated comment paragraphs bounded.
fn preceded_by_allow_marker(lines: &[&str], index: usize) -> bool {
    let mut cursor = index;
    let mut steps = 0;
    while cursor > 0 && steps < 20 {
        cursor -= 1;
        steps += 1;
        let trimmed = lines[cursor].trim_start();
        if !is_comment_line(trimmed) {
            return false;
        }
        if trimmed.contains(ALLOW_LINE_MARKER) {
            return true;
        }
    }
    false
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
        || trimmed.starts_with(';')
        || trimmed.starts_with("<!--")
}

// ----- pattern 1: url with credentials -----

fn scan_url_with_credentials(
    line: &str,
    line_number: usize,
    path: Option<&Path>,
    findings: &mut Vec<Finding>,
) {
    let bytes = line.as_bytes();
    let mut cursor = 0;
    while cursor + 3 <= bytes.len() {
        let Some(relative) = find_subslice(&bytes[cursor..], b"://") else {
            return;
        };
        let scheme_end = cursor + relative;
        let auth_start = scheme_end + 3;

        // Walk backwards while bytes are scheme-eligible. The scheme is a
        // standalone token, so any non-scheme byte to its left terminates it.
        let mut scheme_start = scheme_end;
        while scheme_start > 0 && is_scheme_byte(bytes[scheme_start - 1]) {
            scheme_start -= 1;
        }

        if scheme_start == scheme_end {
            cursor = auth_start;
            continue;
        }

        let scheme = &line[scheme_start..scheme_end];

        // Walk forwards to the end of the authority section. Authority
        // terminators per RFC 3986: `/`, `?`, `#`, whitespace, and the
        // surrounding-text terminators we treat as boundaries (quotes,
        // commas, parens, brackets, etc.).
        let mut auth_end = auth_start;
        while auth_end < bytes.len() && is_authority_byte(bytes[auth_end]) {
            auth_end += 1;
        }
        let authority = &line[auth_start..auth_end];

        if let UrlAuthority::Real { excerpt } = classify_authority(scheme, authority) {
            findings.push(Finding {
                path: path.map(|p| p.to_owned()),
                line: line_number,
                kind: SecretKind::UrlWithCredentials,
                excerpt,
            });
        }

        cursor = auth_end.max(auth_start + 1);
    }
}

fn classify_authority(scheme: &str, authority: &str) -> UrlAuthority {
    if !is_credential_scheme(scheme) {
        return UrlAuthority::NotCredential;
    }
    // Split userinfo from host on the LAST '@' rather than the first.
    // Real-world userinfo sometimes carries an unencoded '@' in the
    // password slot (RFC 3986 requires percent-encoding but browsers and
    // many shell tools accept the literal). Splitting on the first '@'
    // would leak the password tail into `host_and_port`, where the
    // redacted excerpt would then echo it back; splitting on the last '@'
    // keeps any extras on the password side, where they are redacted
    // along with the rest of the password.
    let Some(at_index) = authority.rfind('@') else {
        return UrlAuthority::NotCredential;
    };
    let userinfo = &authority[..at_index];
    let host_and_port = &authority[at_index + 1..];

    let Some(colon_index) = userinfo.find(':') else {
        return UrlAuthority::NotCredential;
    };
    let user = &userinfo[..colon_index];
    let password = &userinfo[colon_index + 1..];
    let host = strip_port(host_and_port);

    if password.is_empty() || is_placeholder_value(password) {
        return UrlAuthority::Placeholder;
    }
    if is_known_dummy_userpass(user, password) && is_local_dummy_host(host) {
        return UrlAuthority::Dummy;
    }

    UrlAuthority::Real {
        excerpt: redact_url(scheme, host),
    }
}

fn is_local_dummy_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let host_lower = host.to_ascii_lowercase();
    DUMMY_HOSTS
        .binary_search(&host)
        .or_else(|_| DUMMY_HOSTS.binary_search(&host_lower.as_str()))
        .is_ok()
}

fn is_credential_scheme(scheme: &str) -> bool {
    // Schemes are case-insensitive per RFC 3986; we compare lowercase.
    if !scheme
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
    {
        return false;
    }
    let lower: String = scheme
        .bytes()
        .map(|b| b.to_ascii_lowercase() as char)
        .collect();
    CREDENTIAL_URL_SCHEMES
        .binary_search(&lower.as_str())
        .is_ok()
}

fn is_scheme_byte(byte: u8) -> bool {
    // Per RFC 3986: ALPHA / DIGIT / `+` / `-` / `.`. The first byte must be
    // alphabetic, but we only need a permissive walk-back here because the
    // scheme-validity check is done separately.
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.'
}

fn is_authority_byte(byte: u8) -> bool {
    // Anything except path/query/fragment/whitespace/quote/bracket boundary
    // characters. We are deliberately generous so that URL embedded in shell
    // expressions, YAML strings, etc. all parse the way an adopter intends.
    !matches!(
        byte,
        b'/' | b'?'
            | b'#'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b'`'
            | b'<'
            | b'>'
            | b'('
            | b')'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b','
            | b';'
    )
}

fn strip_port(host_and_port: &str) -> &str {
    if let Some(colon_index) = host_and_port.find(':') {
        &host_and_port[..colon_index]
    } else {
        host_and_port
    }
}

fn redact_url(scheme: &str, host: &str) -> String {
    // Excerpts never echo the user, password, or password-tail. Modern
    // basic-auth token forms place the secret in the user half rather
    // than the password half (GitHub fine-grained tokens use
    // `<token>:x-oauth-basic@…`, npm registry uses `<token>:_authToken=…`),
    // so a template that only redacted the password slot would leak the
    // token through the user slot. Both halves of the userinfo are
    // redacted to a single `<REDACTED>` token.
    //
    // The host is echoed back only when it parses as a clean hostname or
    // IPv4 address. Any byte outside `[A-Za-z0-9.-]` falls back to
    // `<host>`, so a malformed authority (extra '@', escape oddities,
    // partial percent-encoding) cannot smuggle credential bytes into the
    // host display field.
    let safe_host = if is_safe_host_for_display(host) {
        host
    } else {
        "<host>"
    };
    format!("{scheme}://<REDACTED>@{safe_host}")
}

fn is_safe_host_for_display(host: &str) -> bool {
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

// ----- pattern 2: env-var assignment -----

fn scan_env_assignment(
    line: &str,
    line_number: usize,
    path: Option<&Path>,
    findings: &mut Vec<Finding>,
) {
    // Skip pure comment lines so doc text discussing env var names does not
    // trip the scanner. We still accept assignments that follow a key, even
    // if the file is a Markdown code fence — the line itself looks like an
    // assignment.
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with("//") || trimmed.starts_with(';') {
        return;
    }

    // Normalise leading shape so the same `KEY=value` parser handles all of:
    //   FOO=bar                              (bare)
    //   export FOO=bar                       (shell)
    //   set FOO=bar                          (cmd.exe / some POSIX shells)
    //   - FOO=bar                            (YAML / compose env list)
    //   - "FOO=bar"                          (YAML quoted env list)
    //   - export FOO=bar                     (compose env list of shell line)
    // YAML list-item entries were previously missed because the trimmed line
    // started with `- ` and the env-var-shape check rejected the dash.
    let active = if let Some(rest) = trimmed.strip_prefix("- ") {
        strip_outer_quotes(rest.trim_start())
    } else {
        trimmed
    };
    let active = active
        .strip_prefix("export ")
        .or_else(|| active.strip_prefix("set "))
        .unwrap_or(active);

    let Some((key, value_raw)) = split_assignment(active) else {
        return;
    };
    let key = normalize_key(key);
    if !is_envvar_like_key(key) {
        return;
    }

    let value = strip_value_wrappers(value_raw.trim());
    if value.is_empty() || is_placeholder_value(value) {
        return;
    }

    // Dedup against the URL scanner. If the value parses as a credentialed
    // URL whose scheme the URL scanner recognises — real, dummy, or
    // placeholder — the URL scanner has already acted (emitted, or
    // intentionally suppressed) and the env scanner defers to avoid double-
    // reporting and to preserve the existing dummy/placeholder allowlist.
    //
    // Critically, this only defers when the URL scanner ACTUALLY recognised
    // the scheme. Previously the env scanner skipped on any value
    // containing `://` and `@`, which silently dropped findings for
    // credential URLs with non-listed schemes (`tcp://`, `ldap://`, etc.).
    let defer_to_url_scanner = matches!(
        classify_value_as_credential_url(value),
        UrlAuthority::Real { .. } | UrlAuthority::Dummy | UrlAuthority::Placeholder,
    );

    if let Some(known) = match_named_secret_var(key) {
        if defer_to_url_scanner {
            return;
        }
        findings.push(Finding {
            path: path.map(|p| p.to_owned()),
            line: line_number,
            kind: SecretKind::EnvAssignmentKnown(known),
            excerpt: format!("{key}=<REDACTED {bytes} bytes>", bytes = value.len()),
        });
        return;
    }

    if let Some(suffix) = match_secret_suffix(key) {
        if defer_to_url_scanner {
            return;
        }
        findings.push(Finding {
            path: path.map(|p| p.to_owned()),
            line: line_number,
            kind: SecretKind::EnvAssignmentSuffix(suffix),
            excerpt: format!("{key}=<REDACTED {bytes} bytes>", bytes = value.len()),
        });
    }
}

fn normalize_key(key: &str) -> &str {
    // Trim, then strip a single leading and a single trailing ASCII quote
    // char independently. Handles JSON `"KEY": "v"`, half-quoted YAML
    // `"KEY=v"` (where the `=` split leaves an unbalanced quote on one
    // side), and single-quoted variants. Independent prefix/suffix strip
    // is intentional — `"KEY=value` (key got a leading quote, value got
    // none) still cleans up.
    let key = key.trim();
    let key = key
        .strip_prefix('"')
        .or_else(|| key.strip_prefix('\''))
        .unwrap_or(key);
    let key = key
        .strip_suffix('"')
        .or_else(|| key.strip_suffix('\''))
        .unwrap_or(key);
    key.trim()
}

fn strip_outer_quotes(s: &str) -> &str {
    // Strip a single matching pair of surrounding ASCII quotes. Called on
    // the inner content of a YAML list item so `- "KEY=value"` parses
    // identically to `- KEY=value`. Unlike `normalize_key`, this requires
    // BOTH endpoints to match, so a JSON-shaped mapping like
    // `"KEY":"value"` is not stripped here and instead handled by
    // `normalize_key`'s asymmetric strip on the split key.
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Parse `value` looking for the first `scheme://authority` substring and
/// classify it via [`classify_authority`]. Used by the env-var scanner to
/// decide whether to defer to the URL scanner.
fn classify_value_as_credential_url(value: &str) -> UrlAuthority {
    let bytes = value.as_bytes();
    let Some(rel) = find_subslice(bytes, b"://") else {
        return UrlAuthority::NotCredential;
    };
    let scheme_end = rel;
    let auth_start = scheme_end + 3;

    let mut scheme_start = scheme_end;
    while scheme_start > 0 && is_scheme_byte(bytes[scheme_start - 1]) {
        scheme_start -= 1;
    }
    if scheme_start == scheme_end {
        return UrlAuthority::NotCredential;
    }
    let scheme = &value[scheme_start..scheme_end];

    let mut auth_end = auth_start;
    while auth_end < bytes.len() && is_authority_byte(bytes[auth_end]) {
        auth_end += 1;
    }
    let authority = &value[auth_start..auth_end];

    classify_authority(scheme, authority)
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    // Prefer the shell-style `KEY=value` form. Fall back to YAML `KEY: value`
    // only when the line looks like a plain assignment (no `:` in the value
    // part of a URL or otherwise — we want the *first* `:` after the key,
    // and only if there is no `=` earlier on the line).
    if let Some(eq_index) = line.find('=') {
        return Some((&line[..eq_index], &line[eq_index + 1..]));
    }
    let colon_index = line.find(':')?;
    Some((&line[..colon_index], &line[colon_index + 1..]))
}

fn is_envvar_like_key(key: &str) -> bool {
    // Env-var-style keys: ASCII letters, digits, underscores; must start with
    // a letter or underscore; must contain at least one uppercase letter
    // (otherwise we are looking at a snake_case config key, not an env var).
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    let mut saw_upper = false;
    for byte in bytes {
        if byte.is_ascii_uppercase() {
            saw_upper = true;
        } else if !(byte.is_ascii_alphanumeric() || *byte == b'_') {
            return false;
        }
    }
    saw_upper
}

fn match_named_secret_var(key: &str) -> Option<&'static str> {
    NAMED_SECRET_VARS
        .binary_search(&key)
        .ok()
        .map(|idx| NAMED_SECRET_VARS[idx])
}

fn match_secret_suffix(key: &str) -> Option<&'static str> {
    for suffix in NAMED_SECRET_SUFFIXES {
        if key.len() > suffix.len() && key.ends_with(suffix) {
            return Some(*suffix);
        }
    }
    None
}

fn strip_value_wrappers(value: &str) -> &str {
    // Strip surrounding ASCII quotes if present, then trim again. Handles
    // shell `FOO="bar"`, TOML `foo = "bar"`, YAML `foo: "bar"`, etc.
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].trim();
        }
    }
    trimmed
}

// ----- pattern 3: PEM private-key header -----

fn scan_pem_private_key_header(
    line: &str,
    line_number: usize,
    path: Option<&Path>,
    findings: &mut Vec<Finding>,
) {
    let trimmed = line.trim();
    let Some(after_begin) = trimmed.strip_prefix(PEM_HEADER_PREFIX) else {
        return;
    };
    let Some(dashes_index) = after_begin.find(PEM_HEADER_DASHES) else {
        return;
    };
    let label = &after_begin[..dashes_index];
    if !label.contains(PEM_KEY_LABEL_NEEDLE) {
        return;
    }
    findings.push(Finding {
        path: path.map(|p| p.to_owned()),
        line: line_number,
        kind: SecretKind::PrivateKeyBlock,
        excerpt: format!("PEM `{label}` header — never commit private keys"),
    });
}

// ===== shared helpers =====

fn is_placeholder_value(value: &str) -> bool {
    PLACEHOLDER_NEEDLES
        .iter()
        .any(|needle| value.contains(needle))
}

fn is_known_dummy_userpass(user: &str, password: &str) -> bool {
    KNOWN_DUMMY_USERPASS
        .iter()
        .any(|(u, p)| *u == user && *p == password)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    'outer: for start in 0..=haystack.len() - needle.len() {
        for offset in 0..needle.len() {
            if haystack[start + offset] != needle[offset] {
                continue 'outer;
            }
        }
        return Some(start);
    }
    None
}

fn display_path(path: &Path) -> String {
    let current_dir = std::env::current_dir().ok();
    let display = current_dir
        .as_deref()
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path);
    display.display().to_string()
}

// Compile-time guards — keep the sorted-slice invariant honest so
// `binary_search` stays correct as the tables grow. The `_`-prefixed const
// names tell rustc these are evaluated for side effects only; the panic
// inside `assert_sorted` lights up at compile time if a table goes
// out of order. The `#[allow(dead_code)]` is needed because rustc's dead-
// code pass does not currently treat const-fn calls from a `const _: () = ...`
// item as a use of the function.
#[allow(dead_code)]
const SCHEMES_SORTED_GUARD: () = assert_sorted(CREDENTIAL_URL_SCHEMES);
#[allow(dead_code)]
const NAMED_SECRET_VARS_SORTED_GUARD: () = assert_sorted(NAMED_SECRET_VARS);
#[allow(dead_code)]
const DUMMY_HOSTS_SORTED_GUARD: () = assert_sorted(DUMMY_HOSTS);

#[allow(dead_code)]
const fn assert_sorted(slice: &[&str]) {
    let mut idx = 1;
    while idx < slice.len() {
        let previous = slice[idx - 1].as_bytes();
        let current = slice[idx].as_bytes();
        if !byte_slice_less(previous, current) {
            panic!("check_secrets constant table is not strictly ASCII-sorted");
        }
        idx += 1;
    }
}

#[allow(dead_code)]
const fn byte_slice_less(left: &[u8], right: &[u8]) -> bool {
    let mut idx = 0;
    while idx < left.len() && idx < right.len() {
        if left[idx] != right[idx] {
            return left[idx] < right[idx];
        }
        idx += 1;
    }
    left.len() < right.len()
}

// ===== tests =====

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, ffi::OsString, path::Path};

    fn scan(line: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        scan_text(line, Some(Path::new("example.rs")), &mut findings);
        findings
    }

    struct ProcessEnvRestore {
        backups: Vec<(&'static str, Option<OsString>)>,
    }

    impl ProcessEnvRestore {
        fn new(names: &[&'static str]) -> Self {
            let backups = names
                .iter()
                .map(|name| (*name, env::var_os(name)))
                .collect();
            Self { backups }
        }
    }

    impl Drop for ProcessEnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.backups.iter().rev() {
                unsafe {
                    match value {
                        Some(value) => env::set_var(name, value),
                        None => env::remove_var(name),
                    }
                }
            }
        }
    }

    #[test]
    fn clear_invalid_git_plumbing_env_values_removes_from_command() {
        let _env_restore = ProcessEnvRestore::new(&[
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ]);
        unsafe {
            env::set_var("GIT_DIR", "(null)");
            env::set_var("GIT_WORK_TREE", "/tmp/check-secrets-work-tree");
            env::set_var("GIT_INDEX_FILE", "(null)");
            env::set_var("GIT_COMMON_DIR", "");
        }

        let mut command = Command::new("git");
        clear_invalid_git_plumbing_env(&mut command);

        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(name, value)| {
                let value = value.map(|value| value.to_string_lossy().into_owned());
                (name.to_string_lossy().into_owned(), value)
            })
            .collect();
        let lookup = |name: &str| {
            envs.iter()
                .find(|(env_name, _)| env_name == name)
                .map(|(_, value)| value.as_deref())
        };
        assert!(
            lookup("GIT_DIR") == Some(None),
            "invalid GIT_DIR env should be removed",
        );
        assert!(
            lookup("GIT_INDEX_FILE") == Some(None),
            "invalid GIT_INDEX_FILE env should be removed",
        );
        assert!(
            lookup("GIT_COMMON_DIR") == Some(None),
            "invalid GIT_COMMON_DIR env should be removed",
        );
        // Valid env values are inherited from the process; `clear_invalid_git_plumbing_env`
        // intentionally does nothing for them, so they do NOT appear as explicit
        // modifications on the `Command` (i.e. `get_envs()` will not return an entry
        // for them at all). The correct assertion is that the valid value was NOT
        // explicitly removed (which would appear as `Some(None)` in `get_envs()`).
        assert!(
            !matches!(lookup("GIT_WORK_TREE"), Some(None)),
            "valid GIT_WORK_TREE should not be explicitly removed from the command env",
        );
    }

    #[test]
    fn act_filesystem_fallback_requires_act_and_not_git_repo_error() {
        let _env_restore = ProcessEnvRestore::new(&["ACT"]);
        let error =
            "git ls-files -z failed (exit status: 128): fatal: not a git repository: (null)";

        unsafe {
            env::remove_var("ACT");
        }
        assert!(!should_use_act_filesystem_fallback(error));

        unsafe {
            env::set_var("ACT", "true");
        }
        assert!(should_use_act_filesystem_fallback(error));
        assert!(!should_use_act_filesystem_fallback(
            "git ls-files -z failed: permission denied",
        ));
    }

    #[test]
    fn filesystem_repo_sweep_skips_build_and_git_state_dirs_but_keeps_dotgithub() {
        let temp_canon = env::temp_dir().canonicalize().unwrap();
        let root_name = format!("djogi-check-secrets-fs-{}", std::process::id());
        let root = djogi::migrate::resolve_write_workspace_path(&temp_canon, &root_name)
            .expect("resolve temp root");
        let _ = djogi::migrate::remove_workspace_dir_all(&temp_canon, &root);
        djogi::migrate::create_workspace_dir_all(&temp_canon, &root).unwrap();
        let root = root.canonicalize().unwrap();

        let github_workflows =
            djogi::migrate::resolve_write_workspace_path(&root, ".github/workflows")
                .expect("resolve .github/workflows");
        djogi::migrate::create_workspace_dir_all(&root, &github_workflows).unwrap();
        let src_dir =
            djogi::migrate::resolve_write_workspace_path(&root, "src").expect("resolve src");
        djogi::migrate::create_workspace_dir_all(&root, &src_dir).unwrap();
        let target_debug = djogi::migrate::resolve_write_workspace_path(&root, "target/debug")
            .expect("resolve target/debug");
        djogi::migrate::create_workspace_dir_all(&root, &target_debug).unwrap();
        let git_objects = djogi::migrate::resolve_write_workspace_path(&root, ".git/objects")
            .expect("resolve .git/objects");
        djogi::migrate::create_workspace_dir_all(&root, &git_objects).unwrap();
        let worktrees_issue =
            djogi::migrate::resolve_write_workspace_path(&root, ".worktrees/issue")
                .expect("resolve .worktrees/issue");
        djogi::migrate::create_workspace_dir_all(&root, &worktrees_issue).unwrap();

        djogi::migrate::write_workspace_file(&root, github_workflows.join("ci.yml"), b"name: CI")
            .unwrap();
        djogi::migrate::write_workspace_file(&root, src_dir.join("lib.rs"), b"pub fn ok() {}")
            .unwrap();
        djogi::migrate::write_workspace_file(
            &root,
            target_debug.join("build.log"),
            b"DATABASE_URL=postgres://real:secret@db/app",
        )
        .unwrap();
        djogi::migrate::write_workspace_file(&root, root.join(".git/config"), b"ignored").unwrap();
        djogi::migrate::write_workspace_file(&root, worktrees_issue.join("file.rs"), b"ignored")
            .unwrap();

        let files = list_filesystem_repo_files(&root).unwrap();
        let as_strings: BTreeSet<_> = files
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(as_strings.contains(".github/workflows/ci.yml"));
        assert!(as_strings.contains("src/lib.rs"));
        assert!(!as_strings.contains("target/debug/build.log"));
        assert!(!as_strings.contains(".git/config"));
        assert!(!as_strings.contains(".worktrees/issue/file.rs"));

        let _ = djogi::migrate::remove_workspace_dir_all(&temp_canon, &root);
    }

    // ---- URL detection ----

    #[test]
    fn flags_postgres_url_with_credentials() {
        let findings = scan("DATABASE_URL=postgres://alice:hunter2@db.prod.example/myapp");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::UrlWithCredentials)),
            "expected URL finding, got {findings:#?}",
        );
        // The reported excerpt must NEVER contain the raw password.
        for f in &findings {
            assert!(!f.excerpt.contains("hunter2"));
        }
    }

    #[test]
    fn allows_known_local_dummy_userpass() {
        let findings = scan("DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test");
        // Both the URL scanner and the env-var scanner could fire here, but
        // djogi:djogi is an intentional local-CI fixture and the env-var
        // path defers to the URL scanner for `_URL` keys.
        assert!(
            findings.is_empty(),
            "expected no findings for known dummy local creds, got {findings:#?}",
        );
    }

    #[test]
    fn denies_known_dummy_userpass_on_service_host() {
        let findings = scan("postgres://postgres:postgres@service.company.internal/db");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::UrlWithCredentials)),
            "expected remote dummy pair URL to be flagged, got {findings:#?}",
        );
    }

    #[test]
    fn denies_known_dummy_userpass_on_https_service_host() {
        let findings = scan("https://user:password@api.company.internal/v1");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::UrlWithCredentials)),
            "expected remote auth-bearing URL to be flagged, got {findings:#?}",
        );
    }

    #[test]
    fn allows_placeholder_password_in_url() {
        let findings = scan("postgres://user:<password>@host/db");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn allows_shell_var_password_in_url() {
        let findings = scan(r#"postgres://djogi:${PGPASSWORD}@prod.example.com/myapp"#);
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn flags_https_basic_auth_url() {
        let findings = scan("https://alice:f0o-b4r@api.example.org/v1");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::UrlWithCredentials)),
            "got {findings:#?}",
        );
    }

    #[test]
    fn ignores_url_without_credentials() {
        let findings = scan("see https://docs.example.com/intro for more");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    // ---- env-var assignment detection ----

    #[test]
    fn flags_pgpassword_assignment() {
        let findings = scan("PGPASSWORD=my-real-password");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("PGPASSWORD"))),
            "got {findings:#?}",
        );
        for f in &findings {
            assert!(!f.excerpt.contains("my-real-password"));
        }
    }

    #[test]
    fn flags_postgres_password_yaml_form() {
        let findings = scan("  POSTGRES_PASSWORD: somethingreal");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("POSTGRES_PASSWORD"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn allows_postgres_password_dummy_djogi() {
        // The fixture-CI value is intentionally weak; the file marker on
        // docker-compose.yml / ci.yml should suppress it but the value
        // itself is also a known dummy (djogi:djogi). Here we test that
        // the literal value "djogi" alone is treated as a placeholder-ish
        // dummy via the known-dummy-userpass table only when paired —
        // standalone, "djogi" is still a value that could be a real word,
        // so we DO flag it. Adopters override via a marker comment.
        // This test pins the current contract: bare `POSTGRES_PASSWORD: djogi`
        // is a finding unless suppressed. The repo's own copy is suppressed
        // via the file marker.
        let findings = scan("POSTGRES_PASSWORD: djogi");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("POSTGRES_PASSWORD"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn flags_arbitrary_secret_suffix_assignment() {
        let findings = scan("MY_SERVICE_TOKEN=ghp_abcdef1234567890");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentSuffix("_TOKEN"))),
            "got {findings:#?}",
        );
        for f in &findings {
            assert!(!f.excerpt.contains("ghp_abcdef1234567890"));
        }
    }

    #[test]
    fn flags_webhook_suffix_assignment() {
        let findings = scan("PAYMENT_WEBHOOK=whsec_0123456789abcdef");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentSuffix("_WEBHOOK"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn flags_api_key_assignment() {
        let findings = scan("API_KEY=abc123def456abcdef");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("API_KEY"))),
            "got {findings:#?}",
        );
        for f in &findings {
            assert!(!f.excerpt.contains("abc123def456abcdef"));
        }
    }

    #[test]
    fn flags_client_secret_assignment() {
        let findings = scan("CLIENT_SECRET=super-secret-value");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("CLIENT_SECRET"))),
            "got {findings:#?}",
        );
        for f in &findings {
            assert!(!f.excerpt.contains("super-secret-value"));
        }
    }

    #[test]
    fn ignores_comment_line_naming_env_var() {
        let findings = scan("# To run locally, set DATABASE_URL=postgres://...");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn ignores_config_key_pointing_to_env_var_name() {
        // The value is the *name* of an env var, not the secret itself.
        let findings = scan(r#"csrf_secret_env = "DJOGI_ADMIN_CSRF_SECRET""#);
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn ignores_placeholder_value_in_env_assignment() {
        let findings = scan("API_KEY=<your-api-key>");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    // ---- PEM detection ----

    #[test]
    fn flags_pem_private_key_header() {
        for header in [
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----",
        ] {
            let findings = scan(header);
            assert!(
                findings
                    .iter()
                    .any(|f| matches!(f.kind, SecretKind::PrivateKeyBlock)),
                "expected PEM finding for `{header}`, got {findings:#?}",
            );
        }
    }

    #[test]
    fn ignores_pem_public_key_header() {
        let findings = scan("-----BEGIN PUBLIC KEY-----");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn ignores_pem_certificate_header() {
        let findings = scan("-----BEGIN CERTIFICATE-----");
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    // ---- suppression markers ----

    #[test]
    fn line_marker_on_same_line_suppresses_finding() {
        let line = "PGPASSWORD=foobar  # djogi-allow-secret: doctest fixture";
        assert!(scan(line).is_empty());
    }

    #[test]
    fn line_marker_on_previous_line_suppresses_finding() {
        let mut findings = Vec::new();
        scan_text(
            "// djogi-allow-secret: anti-pattern example\n\
             postgres://alice:realpassword@prod.example.com/myapp\n",
            Some(Path::new("docs/security.md")),
            &mut findings,
        );
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn line_marker_walks_back_through_contiguous_comment_block() {
        // The marker may live N lines up if every intervening line is itself
        // a comment. This matches how multi-line rationales actually get
        // written in YAML / shell / Rust review comments.
        let mut findings = Vec::new();
        scan_text(
            "          POSTGRES_USER: djogi\n\
             # djogi-allow-secret: GHA service-container fixture; this Postgres\n\
             # binds only to the runner-local network, never to the public\n\
             # internet, and is destroyed at job end.\n\
             POSTGRES_PASSWORD: djogi\n",
            Some(Path::new(".github/workflows/ci.yml")),
            &mut findings,
        );
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn comment_block_without_marker_does_not_suppress() {
        // A comment block above the secret that does NOT contain the marker
        // must not silently suppress the finding.
        let mut findings = Vec::new();
        scan_text(
            "# This is unrelated documentation.\n\
             # It explains the surrounding YAML.\n\
             POSTGRES_PASSWORD: realvalue\n",
            Some(Path::new(".github/workflows/ci.yml")),
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("POSTGRES_PASSWORD"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn line_marker_walk_back_terminates_at_non_comment_line() {
        // A blank or code line between marker and secret terminates the
        // walk-back; the marker no longer applies. This keeps the
        // suppression mechanism tightly scoped.
        let mut findings = Vec::new();
        scan_text(
            "# djogi-allow-secret: stale marker that no longer attaches\n\
             \n\
             POSTGRES_PASSWORD: realvalue\n",
            Some(Path::new(".github/workflows/ci.yml")),
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("POSTGRES_PASSWORD"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn marker_without_colon_does_not_suppress() {
        // The trailing colon is mandatory; "djogi-allow-secret" prose without
        // the colon should NOT silently disable scanning.
        let line = "discussion of djogi-allow-secret feature here";
        let mut findings = Vec::new();
        scan_text(
            &format!("{line}\nPGPASSWORD=realvalue\n"),
            Some(Path::new("docs/security.md")),
            &mut findings,
        );
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("PGPASSWORD"))),
            "got {findings:#?}",
        );
    }

    #[test]
    fn file_marker_suppresses_whole_file() {
        let mut findings = Vec::new();
        scan_text(
            "// djogi-allow-secret-file: scanner self-fixture\n\
             // line 2\n\
             // line 3\n\
             PGPASSWORD=this-would-otherwise-fire\n",
            Some(Path::new("xtask/src/check_secrets.rs")),
            &mut findings,
        );
        assert!(findings.is_empty(), "got {findings:#?}");
    }

    #[test]
    fn file_marker_does_not_accidentally_act_as_line_marker() {
        // The file marker is a strict superset of the line marker's prefix.
        // The trailing colon discipline keeps them distinct so a file marker
        // on the line above a secret does NOT silently suppress that secret
        // when the file marker itself falls outside the 20-line window.
        let prefix: String = (0..21).map(|_| "// padding\n").collect();
        let source = format!(
            "{prefix}\
             // djogi-allow-secret-file: too late, falls past line 20\n\
             PGPASSWORD=somethingreal\n",
        );
        let mut findings = Vec::new();
        scan_text(&source, Some(Path::new("test.rs")), &mut findings);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("PGPASSWORD"))),
            "got {findings:#?}",
        );
    }

    // ---- staged-diff parsing ----

    #[test]
    fn parse_staged_diff_tracks_new_line_numbers() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -10,0 +11,2 @@ fn main() {
+    let db = \"postgres://alice:hunter2@host/db\";
+    let token = \"abc123\";
@@ -42,0 +50,1 @@ fn other() {
+    let key = \"xyz\";
";
        let lines = parse_staged_diff(diff);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].new_line_number, 11);
        assert_eq!(lines[0].path, PathBuf::from("src/lib.rs"));
        assert!(lines[0].line_text.contains("postgres://"));
        assert_eq!(lines[1].new_line_number, 12);
        assert_eq!(lines[2].new_line_number, 50);
    }

    #[test]
    fn parse_staged_diff_skips_deleted_files() {
        let diff = "\
diff --git a/old.rs b/old.rs
deleted file mode 100644
--- a/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-something
-something else
-third line
";
        let lines = parse_staged_diff(diff);
        assert!(lines.is_empty(), "got {lines:?}");
    }

    // ---- key-shape filtering ----

    #[test]
    fn snake_case_keys_are_not_env_vars() {
        assert!(!is_envvar_like_key("csrf_secret_env"));
        assert!(!is_envvar_like_key("local_password"));
        assert!(!is_envvar_like_key("token"));
    }

    #[test]
    fn upper_keys_are_env_vars() {
        assert!(is_envvar_like_key("PGPASSWORD"));
        assert!(is_envvar_like_key("MY_SERVICE_TOKEN"));
        assert!(is_envvar_like_key("API_KEY"));
        assert!(is_envvar_like_key("AWS_SECRET_ACCESS_KEY"));
    }

    // ---- placeholder detection ----

    #[test]
    fn placeholders_match_common_forms() {
        for value in [
            "<password>",
            "${VAULT_PASSWORD}",
            "{{password}}",
            "***",
            "REDACTED",
            "your_password_here",
            "changeme",
            "xxx",
            "placeholder",
        ] {
            assert!(
                is_placeholder_value(value),
                "expected `{value}` to be detected as a placeholder",
            );
        }
    }

    #[test]
    fn placeholders_do_not_match_real_looking_values() {
        for value in ["hunter2", "f0o-b4r-baz", "ghp_1234567890", "abc123def456"] {
            assert!(
                !is_placeholder_value(value),
                "expected `{value}` to NOT be a placeholder",
            );
        }
    }

    // ---- scheme classification ----

    #[test]
    fn credential_schemes_are_case_insensitive() {
        assert!(is_credential_scheme("postgres"));
        assert!(is_credential_scheme("POSTGRES"));
        assert!(is_credential_scheme("Postgres"));
        assert!(!is_credential_scheme("file"));
        assert!(!is_credential_scheme(""));
    }

    // ---- non-greedy URL scan ----

    #[test]
    fn urls_inside_strings_with_trailing_chars_still_detected() {
        let line = r#"  url: "postgres://alice:secretpw@host:5432/db" # production"#;
        let findings = scan(line);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::UrlWithCredentials)),
            "got {findings:#?}",
        );
        for f in &findings {
            assert!(!f.excerpt.contains("secretpw"));
        }
    }

    // ---- BLOCK-regression tests (do not delete) ----
    //
    // These cover the three blocker classes flagged in the GPT-5.5 xhigh
    // review of #193: unsafe URL redaction, missed YAML list-item env
    // entries, and over-broad URL-shaped suppression of env findings.

    /// BLOCK 1 — token-in-user-position must not leak through the redacted
    /// excerpt. Modern HTTP basic-auth uses the user slot for the secret
    /// (`<token>:x-oauth-basic@host`), so a "redact password only"
    /// template would leak the token entirely.
    #[test]
    fn redacts_url_does_not_leak_token_in_user_position() {
        let findings =
            scan("https://gh_realtokenabcdef0123456789:x-oauth-basic@api.github.com/user");
        let url_findings: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, SecretKind::UrlWithCredentials))
            .collect();
        assert!(
            !url_findings.is_empty(),
            "expected a url finding, got {findings:#?}",
        );
        for f in &url_findings {
            assert!(
                !f.excerpt.contains("gh_realtokenabcdef0123456789"),
                "user/token slot leaked into excerpt: {}",
                f.excerpt,
            );
            assert!(
                !f.excerpt.contains("x-oauth-basic"),
                "password slot leaked into excerpt: {}",
                f.excerpt,
            );
        }
    }

    /// BLOCK 1 — a password containing an unencoded `@` (real-world DSN
    /// shape `postgres://u:pa@ss@host/db`) previously split userinfo on
    /// the FIRST `@`, which left the password tail in `host_and_port` and
    /// echoed it back through the excerpt.
    #[test]
    fn redacts_url_does_not_leak_password_with_embedded_at() {
        let findings = scan("postgres://u:pa@ss@host.example.com/db");
        let url_findings: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, SecretKind::UrlWithCredentials))
            .collect();
        assert!(
            !url_findings.is_empty(),
            "expected a url finding, got {findings:#?}",
        );
        for f in &url_findings {
            assert!(
                !f.excerpt.contains("pa@ss"),
                "full password leaked into excerpt: {}",
                f.excerpt,
            );
            // The password tail `ss` must not appear in the excerpt either,
            // since splitting on the first '@' would have left it as the
            // displayed host prefix.
            assert!(
                !f.excerpt.contains("ss@"),
                "password tail leaked into host slot: {}",
                f.excerpt,
            );
        }
    }

    /// BLOCK 1 — a malformed authority must not smuggle credential bytes
    /// into the host display slot. Anything outside `[A-Za-z0-9.-]` in the
    /// host position falls back to `<host>`.
    #[test]
    fn redacts_url_falls_back_to_placeholder_host_on_unusual_bytes() {
        // Construct a credential URL where the host portion contains an
        // unusual byte. `=` is allowed inside the authority walk but is
        // outside the safe-host charset, so the excerpt should redact the
        // host.
        let findings = scan("postgres://u:realpw@weird=host/db");
        let url_findings: Vec<_> = findings
            .iter()
            .filter(|f| matches!(f.kind, SecretKind::UrlWithCredentials))
            .collect();
        assert!(!url_findings.is_empty(), "got {findings:#?}");
        for f in &url_findings {
            assert!(
                f.excerpt.contains("<host>"),
                "expected `<host>` placeholder, got: {}",
                f.excerpt,
            );
        }
    }

    /// BLOCK 2 — YAML / docker-compose list-item form
    /// `  - POSTGRES_PASSWORD=value` was previously missed because the
    /// trimmed line started with `- ` and the env-var-shape check
    /// rejected the dash. The list-item prefix must be stripped before
    /// validating the key.
    #[test]
    fn flags_env_assignment_in_yaml_list_item() {
        let findings = scan("      - POSTGRES_PASSWORD=actualleakedvalue");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("POSTGRES_PASSWORD"))),
            "expected POSTGRES_PASSWORD finding, got {findings:#?}",
        );
        for f in &findings {
            assert!(
                !f.excerpt.contains("actualleakedvalue"),
                "raw value leaked: {}",
                f.excerpt,
            );
        }
    }

    /// BLOCK 2 — quoted YAML list-item form
    /// `- "POSTGRES_PASSWORD=value"`: the outer quotes wrap the whole
    /// assignment, so the inner content must be unquoted before split.
    #[test]
    fn flags_env_assignment_in_quoted_yaml_list_item() {
        let findings = scan("  - \"PGPASSWORD=actualleakedvalue\"");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("PGPASSWORD"))),
            "expected PGPASSWORD finding, got {findings:#?}",
        );
    }

    /// BLOCK 2 — `- export FOO=bar` (a docker-compose env list entry
    /// invoking a shell `export`) must also parse cleanly.
    #[test]
    fn flags_env_assignment_in_yaml_list_with_export() {
        let findings = scan("  - export DATABASE_PASSWORD=actualleakedvalue");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("DATABASE_PASSWORD"))),
            "got {findings:#?}",
        );
    }

    /// BLOCK 3 — a credential-bearing URL with an unlisted scheme must
    /// still trigger the env-var finding. Previously the env scanner
    /// suppressed whenever the value merely contained `://` and `@`,
    /// which silently dropped secrets when the URL scanner had not
    /// recognised the scheme. Test hosts deliberately avoid the
    /// `example.com` / `example.org` placeholder needles so the
    /// whole-value placeholder gate does not pre-empt the env scanner.
    #[test]
    fn flags_url_env_var_with_non_credential_scheme() {
        let findings = scan("DATABASE_URL=tcp://user:realleakedpw@dbhost.acme.internal/db");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown("DATABASE_URL"))),
            "expected DATABASE_URL env finding (scheme `tcp` is not recognised \
             by the url scanner, so the env scanner must report), got {findings:#?}",
        );
    }

    /// BLOCK 3 — suffix-form env vars get the same dedup-vs-suppression
    /// guarantee. A secret-suffix value containing an unlisted-scheme URL
    /// must still fire.
    #[test]
    fn flags_suffix_env_var_with_non_credential_scheme_url() {
        let findings = scan("MY_SERVICE_TOKEN=ldap://user:realleakedpw@ldap.acme.internal/dc=foo");
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.kind, SecretKind::EnvAssignmentSuffix("_TOKEN"))),
            "got {findings:#?}",
        );
    }

    /// BLOCK 3 dedup — when the URL scanner DOES emit a finding for the
    /// same line, the env scanner suppresses to avoid double-reporting.
    /// This pins the dedup contract.
    #[test]
    fn defers_env_finding_when_url_scanner_emits() {
        let findings = scan("DATABASE_URL=postgres://alice:realleakedpw@db.prod.example/myapp");
        let url_count = findings
            .iter()
            .filter(|f| matches!(f.kind, SecretKind::UrlWithCredentials))
            .count();
        let env_count = findings
            .iter()
            .filter(|f| matches!(f.kind, SecretKind::EnvAssignmentKnown(_)))
            .count();
        assert_eq!(
            (url_count, env_count),
            (1, 0),
            "expected exactly one URL finding and zero env findings (dedup), \
             got {findings:#?}",
        );
    }

    // ---- sorted-table invariant ----

    #[test]
    fn constant_tables_stay_sorted() {
        // assert_sorted runs at const time but we re-check at runtime in case
        // someone bypasses the const eval (e.g. via a clippy autofix).
        let mut tables: Vec<&[&str]> = vec![CREDENTIAL_URL_SCHEMES, NAMED_SECRET_VARS, DUMMY_HOSTS];
        for table in tables.drain(..) {
            for window in table.windows(2) {
                assert!(window[0] < window[1], "table not sorted: {window:?}");
            }
        }
    }

    // ===== workflow file structural assertions =====
    //
    // These tests pin shape-level invariants of the `public-text-secrets.yml`
    // GitHub Actions workflow that operationalises this scanner. They are
    // NOT a substitute for the workflow's own security review — they protect
    // against accidental regressions during later edits (e.g. someone
    // re-adding a `pull_request` trigger, dropping `persist-credentials:
    // false`, or bolting cargo back onto the comment job).
    //
    // We test the file as text rather than parsing the YAML because (a)
    // xtask has no YAML dependency and (b) the assertions we care about are
    // literal lexical shape (specific keys present / absent at specific
    // scopes), not semantic structure that needs a parser.
    mod workflow_structure {
        use std::path::{Path, PathBuf};

        /// Path to the workflow under test. Resolved from the xtask crate
        /// manifest dir; `..` yields the workspace root, then the workflow
        /// lives at the canonical GitHub Actions location.
        fn workflow_path() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("xtask crate has a workspace-root parent")
                .join(".github/workflows/public-text-secrets.yml")
        }

        fn workflow_yaml() -> String {
            std::fs::read_to_string(workflow_path()).unwrap_or_else(|err| {
                panic!("failed to read {}: {err}", workflow_path().display(),);
            })
        }

        /// Return the slice of `yaml` that constitutes the body of the
        /// top-level job named `name`. A job header is the literal
        /// `  <name>:` line at 2-space indent. The body continues until
        /// the next 2-space-indented sibling key (next job) or EOF.
        ///
        /// Panics if no header is found — that signals a workflow rename
        /// or restructure and the test should fail loudly.
        fn job_body<'a>(yaml: &'a str, name: &str) -> &'a str {
            let header = format!("  {name}:");

            // Locate the header line by byte offset.
            let mut offset = 0usize;
            let mut header_end: Option<usize> = None;
            for line in yaml.split_inclusive('\n') {
                let line_no_trailing_nl = line.strip_suffix('\n').unwrap_or(line);
                if line_no_trailing_nl == header {
                    header_end = Some(offset + line.len());
                    break;
                }
                offset += line.len();
            }
            let body_start = header_end
                .unwrap_or_else(|| panic!("missing job block `{name}:` in workflow YAML"));

            // Walk subsequent lines, looking for the next 2-space-indented
            // sibling key. Such lines start with exactly 2 spaces followed
            // by a non-space, non-`-` character (job names begin with a
            // letter). Comment-only lines and lines at deeper indent stay
            // in the body.
            let mut cursor = body_start;
            let mut body_end = yaml.len();
            for line in yaml[body_start..].split_inclusive('\n') {
                let trimmed_end = line.strip_suffix('\n').unwrap_or(line);
                let bytes = trimmed_end.as_bytes();
                if bytes.len() >= 3
                    && bytes[0] == b' '
                    && bytes[1] == b' '
                    && bytes[2] != b' '
                    && bytes[2] != b'#'
                    && bytes[2] != b'-'
                {
                    body_end = cursor;
                    break;
                }
                cursor += line.len();
            }
            &yaml[body_start..body_end]
        }

        /// Split a job body into per-step slices. A step starts with a
        /// 6-space-indented `- ` (the YAML sequence dash for items under
        /// `    steps:`). Each returned slice begins at that step's dash
        /// line and ends at the next step's dash line (or job body end).
        fn steps_of(body: &str) -> Vec<&str> {
            const STEP_PREFIX: &str = "      - ";
            let mut steps = Vec::new();
            let mut current_start: Option<usize> = None;
            let mut offset = 0usize;
            for line in body.split_inclusive('\n') {
                if line.starts_with(STEP_PREFIX) {
                    if let Some(start) = current_start {
                        steps.push(&body[start..offset]);
                    }
                    current_start = Some(offset);
                }
                offset += line.len();
            }
            if let Some(start) = current_start {
                steps.push(&body[start..]);
            }
            steps
        }

        // ---- top-level shape ----

        /// The PR trigger must be `pull_request_target` (base-branch
        /// context), never `pull_request` (PR head context). Distinguish
        /// from the `pull_request_review*` triggers by requiring the
        /// exact 2-space-indented key match.
        #[test]
        fn workflow_uses_pull_request_target_not_pull_request() {
            let yaml = workflow_yaml();
            assert!(
                yaml.contains("pull_request_target:"),
                "workflow must use the `pull_request_target` trigger",
            );
            for line in yaml.lines() {
                assert_ne!(
                    line.trim_end(),
                    "  pull_request:",
                    "untrusted `pull_request` trigger is forbidden; use `pull_request_target`",
                );
            }
        }

        /// The workflow-level `permissions:` block must default to
        /// `contents: read` only. Anything wider (`issues: write`,
        /// `pull-requests: write`, etc.) at the workflow level would
        /// silently grant those scopes to the cargo / Rust scan job too.
        #[test]
        fn workflow_top_level_permissions_are_contents_read_only() {
            let yaml = workflow_yaml();
            let needle = "\npermissions:\n  contents: read\n";
            assert!(
                yaml.contains(needle),
                "top-level permissions block must be exactly `contents: read`; \
                 found workflow without it",
            );
            // The block ends at the next top-level key (no leading space).
            // Verify no write scope is granted at the workflow level by
            // scanning the line directly after the `permissions:` header.
            let start = yaml
                .find("\npermissions:\n")
                .expect("permissions: block present")
                + "\npermissions:\n".len();
            let rest = &yaml[start..];
            let block_end = rest
                .find("\n\n")
                .or_else(|| rest.find("\njobs:"))
                .unwrap_or(rest.len());
            let block = &rest[..block_end];
            for line in block.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                assert!(
                    !trimmed.contains(": write"),
                    "top-level permissions block must not grant any write \
                     scope; offending line: `{line}`",
                );
            }
        }

        // ---- scan job: no write scopes, no token, trusted refs only ----

        #[test]
        fn scan_job_has_no_write_permissions() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "scan");
            for line in body.lines() {
                // Comments may use phrasing like "write scopes" without a
                // colon; the literal `: write` token is the YAML scope key.
                assert!(
                    !line.contains(": write"),
                    "scan job must not grant any `: write` scope; offending line: `{line}`",
                );
            }
        }

        #[test]
        fn scan_job_binds_no_github_token() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "scan");
            // Comments can mention `GITHUB_TOKEN` (and do, by design); the
            // YAML env binding is `GITHUB_TOKEN: ${{ ... }}` at the start
            // of a non-comment line. Walk every non-comment line and
            // assert no `GITHUB_TOKEN:` env key appears.
            for line in body.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                assert!(
                    !trimmed.starts_with("GITHUB_TOKEN:"),
                    "scan job must not bind GITHUB_TOKEN to any step or job env; \
                     offending line: `{line}`",
                );
            }
        }

        #[test]
        fn all_scan_checkouts_set_persist_credentials_false() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "scan");
            let mut checkout_steps = 0usize;
            for step in steps_of(body) {
                if step.contains("uses: actions/checkout@") {
                    checkout_steps += 1;
                    assert!(
                        step.contains("persist-credentials: false"),
                        "every scan-job checkout step must set \
                         `persist-credentials: false`; offending step:\n{step}",
                    );
                }
            }
            assert!(
                checkout_steps == 1,
                "expected exactly the trusted djogi checkout in \
                 the scan job, found {checkout_steps}",
            );
        }

        #[test]
        fn scanner_checkout_pins_trusted_default_branch_ref() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "scan");
            // The scanner checkout is the only checkout in the scan job.
            let djogi_checkout = steps_of(body)
                .into_iter()
                .find(|step| step.contains("uses: actions/checkout@"))
                .expect("scan job must contain a djogi-source checkout step");
            assert!(
                djogi_checkout.contains("ref: ${{ github.event.repository.default_branch }}"),
                "scanner checkout must pin an explicit trusted `ref:` (the \
                 repository's default branch) on every trigger; offending \
                 step:\n{djogi_checkout}",
            );
        }

        // ---- comment job: no checkout, no cargo, minimal write scope ----

        #[test]
        fn comment_job_has_no_checkout_step() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "comment_and_fail");
            assert!(
                !body.contains("uses: actions/checkout@"),
                "comment_and_fail job must NOT run actions/checkout (no \
                 source code on disk, no path for cargo to walk)",
            );
        }

        #[test]
        fn comment_job_invokes_no_cargo() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "comment_and_fail");
            for line in body.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                assert!(
                    !trimmed.contains("cargo "),
                    "comment_and_fail job must NOT invoke cargo; offending line: `{line}`",
                );
            }
            assert!(
                !body.contains("rust-toolchain"),
                "comment_and_fail job must NOT install a Rust toolchain",
            );
        }

        #[test]
        fn comment_job_permissions_are_narrow_writes_only() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "comment_and_fail");
            // Confirm the permissions block exists and only contains the
            // expected scopes: actions: read, issues: write, pull-requests:
            // write. No `contents: write`, `packages: write`, etc.
            assert!(
                body.contains("permissions:"),
                "comment_and_fail job must declare an explicit `permissions:` block",
            );
            assert!(
                body.contains("issues: write"),
                "comment_and_fail job must grant `issues: write` for comments",
            );
            assert!(
                body.contains("pull-requests: write"),
                "comment_and_fail job must grant `pull-requests: write` for PR comments",
            );
            assert!(
                body.contains("actions: read"),
                "comment_and_fail job must grant `actions: read` to download the report artifact",
            );
            for forbidden in ["contents: write", "packages: write", "deployments: write"] {
                assert!(
                    !body.contains(forbidden),
                    "comment_and_fail job must not grant `{forbidden}`",
                );
            }
        }

        #[test]
        fn comment_job_hard_fails_on_findings() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "comment_and_fail");
            // Job-level `if:` gates on scan-job has_findings == 'true', so
            // any step in this job runs only when findings exist. The
            // hard-fail step must exit 1 unconditionally and run even if
            // the comment-posting step crashed.
            let hard_fail_step = steps_of(body)
                .into_iter()
                .find(|step| step.contains("name: Hard-fail on findings"))
                .expect("comment_and_fail job must contain a `Hard-fail on findings` step");
            assert!(
                hard_fail_step.contains("if: always()"),
                "hard-fail step must run with `if: always()` so post-step \
                 failures do not mask the red workflow status:\n{hard_fail_step}",
            );
            assert!(
                hard_fail_step.contains("exit 1"),
                "hard-fail step must `exit 1`:\n{hard_fail_step}",
            );
        }

        // ---- artifact handoff: scan uploads, comment downloads ----

        #[test]
        fn scan_job_uploads_redacted_report_artifact() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "scan");
            let upload_step = steps_of(body)
                .into_iter()
                .find(|step| step.contains("uses: actions/upload-artifact@"))
                .expect("scan job must upload the redacted scanner report");
            assert!(
                upload_step.contains("name: check-secrets-report"),
                "upload artifact must be named `check-secrets-report`:\n{upload_step}",
            );
            // Only uploads when findings exist — `if: steps.scan.outputs.has_findings == 'true'`
            assert!(
                upload_step.contains("has_findings == 'true'"),
                "upload artifact step must gate on has_findings == 'true':\n{upload_step}",
            );
        }

        #[test]
        fn comment_job_downloads_redacted_report_artifact() {
            let yaml = workflow_yaml();
            let body = job_body(&yaml, "comment_and_fail");
            let download_step = steps_of(body)
                .into_iter()
                .find(|step| step.contains("uses: actions/download-artifact@"))
                .expect("comment_and_fail job must download the redacted scanner report");
            assert!(
                download_step.contains("name: check-secrets-report"),
                "download artifact name must match the upload name:\n{download_step}",
            );
        }
    }
}
