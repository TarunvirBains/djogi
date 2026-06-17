use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

const EXACT_PATTERNS: &[&str] = &["tokio_postgres::", "djogi::__bypass", "::djogi::__bypass"];

const IDENT_PATTERNS: &[&str] = &[
    "__bypass",
    "__execute_for_macros",
    "__query_all_for_macros",
    "__query_one_for_macros",
    "__query_opt_for_macros",
    "raw_query",
    "raw_rows",
    "raw_fetch_one",
    "raw_scalar",
    "raw_execute",
    "raw_ddl",
    "raw_stream",
    "raw_stream_with_fetch_size",
    "raw_pool",
    "raw_conn",
    "raw_with_client",
    "batch_execute",
];

const CALL_PATTERNS: &[&str] = &["pool", "conn", "with_client"];

// Ordinary adopter-shaped test roots where raw SQL escape hatches are banned.
// Djogi-owned internal test surfaces under `tests/internal` and
// `djogi-cli/tests/internal` are covered by JUSTIFICATION comments instead.
const RAW_SQL_TEST_ROOTS: &[&str] = &[
    "tests/integration",
    "djogi/tests",
    "djogi-cli/tests/integration",
];

const ACTIVE_RUST_COMPONENTS: &[&str] = &["src", "tests", "examples", "benches"];

const WORKFLOW_ROOTS: &[&str] = &[".github/workflows"];

const WORKFLOW_QUARANTINE_PATTERNS: &[&str] = &[
    "--ignored",
    "--include-ignored",
    "run-ignored",
    "quarantine",
];

#[derive(Debug)]
struct Finding {
    path: PathBuf,
    line: usize,
    pattern: &'static str,
}

pub fn run(list_only: bool) -> ExitCode {
    let mut raw_files = Vec::new();

    for root in RAW_SQL_TEST_ROOTS.iter().map(Path::new) {
        if root.exists()
            && let Err(error) = collect_rs_files(root, &mut raw_files)
        {
            eprintln!("{}: failed to walk: {error}", display_path(root));
            return ExitCode::FAILURE;
        }
    }

    raw_files.sort();

    let mut active_rust_files = Vec::new();
    if let Err(error) =
        collect_active_rs_files(Path::new("."), Path::new("."), &mut active_rust_files)
    {
        eprintln!(".: failed to walk: {error}");
        return ExitCode::FAILURE;
    }
    active_rust_files.sort();
    active_rust_files.dedup();

    let mut workflow_files = Vec::new();
    for root in WORKFLOW_ROOTS.iter().map(Path::new) {
        if root.exists()
            && let Err(error) = collect_workflow_files(root, &mut workflow_files)
        {
            eprintln!("{}: failed to walk: {error}", display_path(root));
            return ExitCode::FAILURE;
        }
    }
    workflow_files.sort();

    let mut findings = Vec::new();
    for path in &raw_files {
        match fs::read_to_string(path) {
            Ok(source) => findings.extend(scan_source(path, &source)),
            Err(error) => {
                eprintln!("{}: failed to read: {error}", display_path(path));
                return ExitCode::FAILURE;
            }
        }
    }
    for path in &active_rust_files {
        match fs::read_to_string(path) {
            Ok(source) => findings.extend(scan_rust_no_quarantine(path, &source)),
            Err(error) => {
                eprintln!("{}: failed to read: {error}", display_path(path));
                return ExitCode::FAILURE;
            }
        }
    }
    for path in &workflow_files {
        match fs::read_to_string(path) {
            Ok(source) => findings.extend(scan_workflow_no_quarantine(path, &source)),
            Err(error) => {
                eprintln!("{}: failed to read: {error}", display_path(path));
                return ExitCode::FAILURE;
            }
        }
    }

    let scanned_files: BTreeSet<_> = raw_files
        .iter()
        .chain(active_rust_files.iter())
        .chain(workflow_files.iter())
        .cloned()
        .collect();

    if list_only {
        let paths: BTreeSet<_> = findings
            .iter()
            .map(|finding| finding.path.clone())
            .collect();
        for path in &paths {
            println!("{}", display_path(path));
        }
    } else {
        for finding in &findings {
            eprintln!(
                "{}:{}: forbidden test-surface reference `{}`",
                display_path(&finding.path),
                finding.line,
                finding.pattern,
            );
        }
        eprintln!(
            "check-test-surface: scanned {} files; {} violations",
            scanned_files.len(),
            findings.len(),
        );
    }

    if findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn collect_rs_files(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let root_canon = root
        .canonicalize()
        .map_err(|e| io::Error::new(e.kind(), format!("canonicalize {}: {e}", root.display())))?;
    let cwd = std::env::current_dir().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot determine current directory",
        )
    })?;
    if !root_canon.starts_with(&cwd) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} escapes workspace root", root.display()),
        ));
    }

    for entry in fs::read_dir(&root_canon)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            // `compile_fail/` subdirectories under a raw-SQL test root
            // hold lihaaf compile-fail fixtures that intentionally name
            // the forbidden bypass methods to prove they do not resolve
            // (e.g. `djogi/tests/compile_fail/raw_execute_without_bypass.rs`).
            // Those fixtures are gated through `cargo lihaaf` and are not
            // adopter-shaped test code — skip them here so the surface
            // check does not double-fire on the very fixtures that pin
            // the gate.
            if path.file_name().is_some_and(|name| name == "compile_fail") {
                continue;
            }
            collect_rs_files(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "rs")
        {
            files.push(path);
        }
    }

    Ok(())
}

fn collect_active_rs_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let root_canon = root
        .canonicalize()
        .map_err(|e| io::Error::new(e.kind(), format!("canonicalize {}: {e}", root.display())))?;
    let dir_canon = dir
        .canonicalize()
        .map_err(|e| io::Error::new(e.kind(), format!("canonicalize {}: {e}", dir.display())))?;
    if !dir_canon.starts_with(&root_canon) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} escapes root {}", dir.display(), root.display()),
        ));
    }

    for entry in fs::read_dir(&dir_canon)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");

        if file_type.is_dir() {
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_active_rs_files(&root_canon, &path, files)?;
        } else if file_type.is_file()
            && path.extension().is_some_and(|extension| extension == "rs")
            && is_active_rust_surface(&root_canon, &path)
        {
            files.push(path);
        }
    }

    Ok(())
}

fn is_active_rust_surface(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };

    relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| ACTIVE_RUST_COMPONENTS.contains(&name))
    })
}

fn collect_workflow_files(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    let root_canon = root
        .canonicalize()
        .map_err(|e| io::Error::new(e.kind(), format!("canonicalize {}: {e}", root.display())))?;
    let cwd = std::env::current_dir().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot determine current directory",
        )
    })?;
    if !root_canon.starts_with(&cwd) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path {} escapes workspace root", root.display()),
        ));
    }

    for entry in fs::read_dir(&root_canon)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            collect_workflow_files(&path, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "yml" || extension == "yaml")
        {
            files.push(path);
        }
    }

    Ok(())
}

fn scan_source(path: &Path, source: &str) -> Vec<Finding> {
    let stripped = strip_comments_and_literals(source);
    let mut findings = Vec::new();

    for (line_index, line) in stripped.lines().enumerate() {
        let line_number = line_index + 1;

        for pattern in EXACT_PATTERNS {
            if line.contains(pattern) {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: line_number,
                    pattern,
                });
            }
        }

        for pattern in IDENT_PATTERNS {
            if contains_identifier(line, pattern, false) {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: line_number,
                    pattern,
                });
            }
        }

        for pattern in CALL_PATTERNS {
            if contains_identifier(line, pattern, true) {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: line_number,
                    pattern,
                });
            }
        }
    }

    findings
}

fn scan_rust_no_quarantine(path: &Path, source: &str) -> Vec<Finding> {
    let stripped = strip_comments_and_literals(source);
    let bytes = stripped.as_bytes();
    let mut findings = Vec::new();
    let mut offset = 0;

    while let Some(relative_index) = stripped[offset..].find('#') {
        let hash_index = offset + relative_index;
        let mut cursor = hash_index + 1;

        if bytes.get(cursor) == Some(&b'!') {
            cursor += 1;
        }

        cursor = skip_ascii_whitespace(bytes, cursor);
        if bytes.get(cursor) != Some(&b'[') {
            offset = cursor;
            continue;
        }

        let Some(attr_end) = find_attribute_end(bytes, cursor) else {
            offset = cursor + 1;
            continue;
        };

        let attr = &stripped[cursor + 1..attr_end];
        if attr_starts_with_ident(attr, "ignore")
            || (attr_starts_with_ident(attr, "cfg_attr")
                && cfg_attr_payload_contains_ident(attr, "ignore"))
        {
            findings.push(Finding {
                path: path.to_owned(),
                line: line_number_at(&stripped, hash_index),
                pattern: "#[ignore]",
            });
        }

        offset = attr_end + 1;
    }

    findings
}

fn scan_workflow_no_quarantine(path: &Path, source: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    for (line_index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }

        let active = strip_yaml_inline_comment(line);
        for pattern in WORKFLOW_QUARANTINE_PATTERNS {
            if workflow_line_has_pattern(active, pattern) {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: line_index + 1,
                    pattern,
                });
            }
        }
    }

    findings
}

fn strip_yaml_inline_comment(line: &str) -> &str {
    line.find(" #").map_or(line, |position| &line[..position])
}

fn workflow_line_has_pattern(line: &str, pattern: &str) -> bool {
    if pattern == "quarantine" {
        return contains_identifier(&line.to_ascii_lowercase(), pattern, false);
    }

    line.contains(pattern)
}

pub(crate) fn contains_identifier(line: &str, ident: &str, require_call: bool) -> bool {
    let mut offset = 0;

    while let Some(relative_index) = line[offset..].find(ident) {
        let index = offset + relative_index;
        let before = index.checked_sub(1).and_then(|i| line.as_bytes().get(i));
        let after_index = index + ident.len();
        let after = line.as_bytes().get(after_index);

        if !before.is_some_and(|byte| is_ident_byte(*byte))
            && !after.is_some_and(|byte| is_ident_byte(*byte))
            && (!require_call || is_followed_by_call_paren(&line[after_index..]))
        {
            return true;
        }

        offset = after_index;
    }

    false
}

fn find_attribute_end(bytes: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(bytes.get(start), Some(&b'['));

    let mut depth = 0usize;
    let mut cursor = start;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }

    None
}

fn attr_starts_with_ident(attr: &str, ident: &str) -> bool {
    let bytes = attr.as_bytes();
    let start = skip_ascii_whitespace(bytes, 0);

    starts_with(bytes, start, ident.as_bytes())
        && !bytes
            .get(start + ident.len())
            .is_some_and(|byte| is_ident_byte(*byte))
}

fn cfg_attr_payload_contains_ident(attr: &str, ident: &str) -> bool {
    let Some(open_paren) = attr.find('(') else {
        return false;
    };
    let mut depth = 0usize;

    for (relative_index, byte) in attr.as_bytes()[open_paren + 1..].iter().enumerate() {
        match *byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                let payload_start = open_paren + 1 + relative_index + 1;
                return contains_identifier(&attr[payload_start..], ident, false);
            }
            _ => {}
        }
    }

    false
}

fn line_number_at(source: &str, index: usize) -> usize {
    source[..index]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn is_followed_by_call_paren(rest: &str) -> bool {
    rest.bytes()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'(')
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

pub(crate) fn strip_comments_and_literals(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if starts_with(bytes, index, b"//") {
            index = mask_line_comment(bytes, index, &mut output);
        } else if starts_with(bytes, index, b"/*") {
            index = mask_block_comment(bytes, index, &mut output);
        } else if let Some(raw_start) = raw_string_start(bytes, index) {
            index = mask_raw_string(bytes, index, raw_start.hashes, &mut output);
        } else if starts_with(bytes, index, b"b\"") || starts_with(bytes, index, b"c\"") {
            index = mask_quoted(bytes, index, index + 1, b'"', &mut output);
        } else if bytes[index] == b'"' {
            index = mask_quoted(bytes, index, index, b'"', &mut output);
        } else if starts_with(bytes, index, b"b'") {
            index = mask_char_or_keep(bytes, index, index + 1, &mut output);
        } else if bytes[index] == b'\'' {
            index = mask_char_or_keep(bytes, index, index, &mut output);
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(output).expect("masking preserves UTF-8")
}

#[derive(Clone, Copy)]
struct RawStringStart {
    hashes: usize,
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<RawStringStart> {
    let raw_index = if bytes.get(index) == Some(&b'r') {
        index
    } else if matches!(bytes.get(index), Some(b'b' | b'c')) && bytes.get(index + 1) == Some(&b'r') {
        index + 1
    } else {
        return None;
    };

    let mut cursor = raw_index + 1;
    let mut hashes = 0;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }

    (bytes.get(cursor) == Some(&b'"')).then_some(RawStringStart { hashes })
}

fn mask_raw_string(bytes: &[u8], start: usize, hashes: usize, output: &mut Vec<u8>) -> usize {
    let raw_index = if bytes[start] == b'r' {
        start
    } else {
        start + 1
    };
    let content_start = raw_index + 1 + hashes + 1;
    let mut cursor = content_start;

    while cursor < bytes.len() {
        if bytes[cursor] == b'"' && raw_hashes_match(bytes, cursor + 1, hashes) {
            return mask_range(bytes, start, cursor + 1 + hashes, output);
        }
        cursor += 1;
    }

    mask_range(bytes, start, bytes.len(), output)
}

fn raw_hashes_match(bytes: &[u8], start: usize, hashes: usize) -> bool {
    (0..hashes).all(|offset| bytes.get(start + offset) == Some(&b'#'))
}

fn mask_line_comment(bytes: &[u8], start: usize, output: &mut Vec<u8>) -> usize {
    let mut cursor = start;
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        output.push(b' ');
        cursor += 1;
    }
    cursor
}

fn mask_block_comment(bytes: &[u8], start: usize, output: &mut Vec<u8>) -> usize {
    let mut cursor = start;
    let mut depth = 0usize;

    while cursor < bytes.len() {
        if starts_with(bytes, cursor, b"/*") {
            depth += 1;
            output.extend_from_slice(b"  ");
            cursor += 2;
        } else if starts_with(bytes, cursor, b"*/") {
            depth = depth.saturating_sub(1);
            output.extend_from_slice(b"  ");
            cursor += 2;
            if depth == 0 {
                break;
            }
        } else {
            mask_byte(bytes[cursor], output);
            cursor += 1;
        }
    }

    cursor
}

fn mask_quoted(
    bytes: &[u8],
    start: usize,
    quote_index: usize,
    quote: u8,
    output: &mut Vec<u8>,
) -> usize {
    let mut cursor = quote_index + 1;
    let mut escaped = false;

    while cursor < bytes.len() {
        if !escaped && bytes[cursor] == quote {
            return mask_range(bytes, start, cursor + 1, output);
        }

        escaped = !escaped && bytes[cursor] == b'\\';
        if bytes[cursor] != b'\\' {
            escaped = false;
        }
        cursor += 1;
    }

    mask_range(bytes, start, bytes.len(), output)
}

fn mask_char_or_keep(
    bytes: &[u8],
    start: usize,
    quote_index: usize,
    output: &mut Vec<u8>,
) -> usize {
    let mut cursor = quote_index + 1;
    let mut escaped = false;

    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        if !escaped && bytes[cursor] == b'\'' {
            return mask_range(bytes, start, cursor + 1, output);
        }

        escaped = !escaped && bytes[cursor] == b'\\';
        if bytes[cursor] != b'\\' {
            escaped = false;
        }
        cursor += 1;
    }

    output.push(bytes[start]);
    start + 1
}

fn mask_range(bytes: &[u8], start: usize, end: usize, output: &mut Vec<u8>) -> usize {
    for byte in &bytes[start..end] {
        mask_byte(*byte, output);
    }
    end
}

fn mask_byte(byte: u8, output: &mut Vec<u8>) {
    if byte == b'\n' {
        output.push(b'\n');
    } else {
        output.push(b' ');
    }
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes.get(index..index + needle.len()) == Some(needle)
}

fn display_path(path: &Path) -> String {
    let current_dir = std::env::current_dir().ok();
    let display_path = current_dir
        .as_deref()
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path);

    display_path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        contains_identifier, scan_rust_no_quarantine, scan_source, scan_workflow_no_quarantine,
        strip_comments_and_literals,
    };
    use std::path::Path;

    #[test]
    fn strips_line_comments_and_string_literals_but_keeps_code() {
        let source = r#"
let x = "raw_execute should be ignored";
ctx.raw_execute("select 1", &[]).await?;
// tokio_postgres:: should be ignored
"#;

        let stripped = strip_comments_and_literals(source);

        assert!(!stripped.contains("\"raw_execute should be ignored\""));
        assert!(stripped.contains("ctx.raw_execute"));
        assert!(!stripped.contains("tokio_postgres:: should be ignored"));
    }

    #[test]
    fn strips_raw_strings_and_nested_block_comments() {
        let source = r##"
let s = r#"tokio_postgres::Client"#;
/* raw_query /* raw_rows */ raw_scalar */
ctx.conn().await?;
"##;

        let stripped = strip_comments_and_literals(source);

        assert!(!stripped.contains("tokio_postgres::Client"));
        assert!(!stripped.contains("raw_rows"));
        assert!(stripped.contains("ctx.conn()"));
    }

    #[test]
    fn scanner_reports_code_references_only() {
        let source = r#"
let text = "raw_execute";
ctx.raw_execute("select 1", &[]).await?;
// ctx.raw_rows("select 1", &[]).await?;
"#;

        let findings = scan_source(Path::new("tests/integration/example.rs"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 3);
        assert_eq!(findings[0].pattern, "raw_execute");
    }

    #[test]
    fn scanner_reports_public_macro_helpers_and_pool_trait_escapes() {
        let source = r#"
ctx.__query_one_for_macros("select 1", &[]).await?;
ctx.raw_with_client(|client| Box::pin(async move { Ok(()) })).await?;
"#;

        let findings = scan_source(Path::new("tests/integration/example.rs"), source);
        let patterns: Vec<_> = findings.iter().map(|finding| finding.pattern).collect();

        assert!(patterns.contains(&"__query_one_for_macros"));
        assert!(patterns.contains(&"raw_with_client"));
    }

    #[test]
    fn scanner_reports_spaced_bypass_path() {
        let source = r#"
use djogi :: __bypass :: RawAccessExt;
"#;

        let findings = scan_source(Path::new("tests/integration/example.rs"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].pattern, "__bypass");
    }

    #[test]
    fn call_patterns_require_call_syntax() {
        assert!(contains_identifier("ctx.pool().await?", "pool", true));
        assert!(contains_identifier("ctx.conn ().await?", "conn", true));
        assert!(!contains_identifier("let pool = 1;", "pool", true));
        assert!(!contains_identifier("let spool = 1;", "pool", true));
    }

    #[test]
    fn no_quarantine_rejects_ignore_attribute() {
        let source = r#"
#[djogi_test]
#[ ignore = "needs database" ]
async fn hidden() {}
"#;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 3);
        assert_eq!(findings[0].pattern, "#[ignore]");
    }

    #[test]
    fn no_quarantine_rejects_multiline_ignore_attribute() {
        let source = r#"
#[djogi_test]
#[
    ignore
]
async fn hidden() {}
"#;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 3);
        assert_eq!(findings[0].pattern, "#[ignore]");
    }

    #[test]
    fn no_quarantine_rejects_cfg_attr_ignore_attribute() {
        let source = r#"
#[test]
#[cfg_attr(feature = "slow-tests", ignore)]
fn hidden() {}
"#;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 3);
        assert_eq!(findings[0].pattern, "#[ignore]");
    }

    #[test]
    fn no_quarantine_allows_cfg_attr_condition_named_ignore() {
        let source = r#"
#[test]
#[cfg_attr(ignore, should_panic)]
fn panics() {}
"#;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert!(findings.is_empty());
    }

    #[test]
    fn no_quarantine_allows_should_panic_expected() {
        let source = r#"
#[test]
#[should_panic(expected = "clear message")]
fn panics() {}
"#;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert!(findings.is_empty());
    }

    #[test]
    fn no_quarantine_ignores_comments_and_literals() {
        let source = r##"
//! ```ignore
//! #[ignore]
//! ```
let text = "#[ignore]";
// #[ignore]
#[test]
fn visible() {}
"##;

        let findings = scan_rust_no_quarantine(Path::new("tests/integration/example.rs"), source);

        assert!(findings.is_empty());
    }

    #[test]
    fn workflow_scanner_rejects_ignored_test_lanes() {
        let source = r#"
name: CI
jobs:
  test:
    steps:
      - run: cargo test -- --ignored
      - run: cargo test # --include-ignored in comment
"#;

        let findings = scan_workflow_no_quarantine(Path::new(".github/workflows/ci.yml"), source);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].pattern, "--ignored");
    }

    #[test]
    fn workflow_scanner_rejects_run_ignored_and_quarantine_word() {
        let source = r#"
jobs:
  test:
    steps:
      - run: cargo xtask run-ignored
      - run: cargo test --skip-list quarantine.txt
      - run: cargo test --skip-list Quarantine.txt
"#;

        let findings = scan_workflow_no_quarantine(Path::new(".github/workflows/ci.yml"), source);
        let patterns: Vec<_> = findings.iter().map(|finding| finding.pattern).collect();

        assert!(patterns.contains(&"run-ignored"));
        assert_eq!(
            patterns
                .iter()
                .filter(|pattern| **pattern == "quarantine")
                .count(),
            2
        );
    }

    #[test]
    fn workflow_scanner_allows_quarantined_prose_and_yaml_hash_literals() {
        let source = r##"
jobs:
  test:
    steps:
      - name: Check test surface (no quarantined/ignored tests)
        run: printf '%s\n' "tag#not-a-comment"
      - run: cargo test # --include-ignored in comment
      # quarantine in pure comment
"##;

        let findings = scan_workflow_no_quarantine(Path::new(".github/workflows/ci.yml"), source);

        assert!(findings.is_empty());
    }
}
