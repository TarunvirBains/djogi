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

#[derive(Debug)]
struct Finding {
    path: PathBuf,
    line: usize,
    pattern: &'static str,
}

pub fn run(list_only: bool) -> ExitCode {
    let mut files = Vec::new();

    for root in [
        Path::new("tests/integration"),
        Path::new("djogi-cli/tests/integration"),
    ] {
        if root.exists()
            && let Err(error) = collect_rs_files(root, &mut files)
        {
            eprintln!("{}: failed to walk: {error}", display_path(root));
            return ExitCode::FAILURE;
        }
    }

    files.sort();

    let mut findings = Vec::new();
    for path in &files {
        match fs::read_to_string(path) {
            Ok(source) => findings.extend(scan_source(path, &source)),
            Err(error) => {
                eprintln!("{}: failed to read: {error}", display_path(path));
                return ExitCode::FAILURE;
            }
        }
    }

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
            files.len(),
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
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
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
    use super::{contains_identifier, scan_source, strip_comments_and_literals};
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
}
