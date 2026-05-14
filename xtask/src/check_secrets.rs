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
    fs, io,
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
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
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
    "_TOKEN",
    "_WEBHOOK_SECRET",
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
    let files = list_tracked_files()?;
    for path in files {
        if should_skip_file(&path) {
            continue;
        }
        let Some(content) = read_text_file(&path) else {
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
    let output = Command::new("git")
        .args(args)
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

        if let Some(finding) = classify_authority(scheme, authority) {
            findings.push(Finding {
                path: path.map(|p| p.to_owned()),
                line: line_number,
                kind: SecretKind::UrlWithCredentials,
                excerpt: finding,
            });
        }

        cursor = auth_end.max(auth_start + 1);
    }
}

fn classify_authority(scheme: &str, authority: &str) -> Option<String> {
    if !is_credential_scheme(scheme) {
        return None;
    }
    let at_index = authority.find('@')?;
    let userinfo = &authority[..at_index];
    let host_and_port = &authority[at_index + 1..];

    let colon_index = userinfo.find(':')?;
    let user = &userinfo[..colon_index];
    let password = &userinfo[colon_index + 1..];

    if password.is_empty() || is_placeholder_value(password) {
        return None;
    }
    if is_known_dummy_userpass(user, password) {
        return None;
    }

    // Dummy hosts ONLY excuse the very weak local dev creds — they should not
    // make a real password on localhost silent, because copying a real
    // production password into a localhost test fixture still leaks it on the
    // first paste into a public issue.
    let host = strip_port(host_and_port);
    if is_dummy_host(host) && is_known_dummy_userpass(user, password) {
        return None;
    }

    Some(redact_url(scheme, user, host))
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

fn redact_url(scheme: &str, user: &str, host: &str) -> String {
    // Excerpts are deliberately templated — we never echo the password back.
    let safe_user = if user.is_empty() { "<user>" } else { user };
    let safe_host = if host.is_empty() { "<host>" } else { host };
    format!("{scheme}://{safe_user}:<REDACTED>@{safe_host}")
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

    // Strip optional `export ` / `set ` prefixes so shell `export FOO=bar`
    // is parsed the same as `FOO=bar`.
    let active = trimmed
        .strip_prefix("export ")
        .or_else(|| trimmed.strip_prefix("set "))
        .unwrap_or(trimmed);

    let Some((key, value_raw)) = split_assignment(active) else {
        return;
    };
    let key = key.trim_end();
    if !is_envvar_like_key(key) {
        return;
    }

    let value = strip_value_wrappers(value_raw.trim());
    if value.is_empty() || is_placeholder_value(value) {
        return;
    }

    if let Some(known) = match_named_secret_var(key) {
        // For `*_URL` style vars we want at least one finding when the value
        // contains a credentialed URL, but the URL scanner has already done
        // that. Avoid double-reporting by suppressing the env-assignment
        // finding when the value itself parses as a credential URL.
        if key.ends_with("_URL") && value_contains_credential_url(value) {
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
        // Same precaution against double-reporting URLs.
        if key.ends_with("_URL") && value_contains_credential_url(value) {
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

fn value_contains_credential_url(value: &str) -> bool {
    let bytes = value.as_bytes();
    find_subslice(bytes, b"://").is_some() && bytes.contains(&b'@')
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

fn is_dummy_host(host: &str) -> bool {
    DUMMY_HOSTS.binary_search(&host).is_ok()
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
    use std::path::Path;

    fn scan(line: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        scan_text(line, Some(Path::new("example.rs")), &mut findings);
        findings
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
}
