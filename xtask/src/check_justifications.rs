use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

use syn::{Attribute, Item, spanned::Spanned};

use crate::check_test_surface::{contains_identifier, strip_comments_and_literals};

const BYPASS_ATTR: &str = "deliberately_bypass_convention_with_raw_sql";
const DJOGI_ISSUE_MESSAGE: &str = "JUSTIFICATION must reference djogi's issue tracker (`djogi#<n>`), not your application's. Reaching for raw_* signals a gap in djogi's typed surface - that gap belongs to djogi to fix. File at github.com/Tarunvir/djogi/issues, then update the justification with the resulting issue number.";

#[derive(Debug, Eq, PartialEq)]
enum Justification {
    DjogiIssue(String),
    Pin,
}

struct Stats {
    decorated_items: usize,
    issue_refs: BTreeSet<String>,
}

struct Violation {
    path: PathBuf,
    line: usize,
    message: String,
}

fn parse_justification_line(line: &str, pin_allowed: bool) -> Result<Justification, String> {
    let trimmed = line.trim_start();
    let bytes = trimmed.as_bytes();
    let prefix = b"// JUSTIFICATION (";

    if !bytes.starts_with(prefix) {
        return Err(DJOGI_ISSUE_MESSAGE.to_owned());
    }

    let rest = &bytes[prefix.len()..];
    if let Some(rest) = rest.strip_prefix(b"djogi#") {
        let digit_count = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digit_count == 0 {
            return Err(DJOGI_ISSUE_MESSAGE.to_owned());
        }

        let issue = &rest[..digit_count];
        let rest = &rest[digit_count..];
        if !rest.starts_with(b"): ") || reason_is_empty(&rest[3..]) {
            return Err(DJOGI_ISSUE_MESSAGE.to_owned());
        }

        let issue = std::str::from_utf8(issue)
            .expect("issue digits are ASCII")
            .to_owned();
        return Ok(Justification::DjogiIssue(issue));
    }

    if let Some(reason) = rest.strip_prefix(b"PIN): ") {
        if pin_allowed && !reason_is_empty(reason) {
            return Ok(Justification::Pin);
        }
        return Err(DJOGI_ISSUE_MESSAGE.to_owned());
    }

    Err(DJOGI_ISSUE_MESSAGE.to_owned())
}

pub fn run() -> ExitCode {
    let mut files = Vec::new();

    for root in [Path::new("tests"), Path::new("djogi-cli/tests")] {
        if root.exists()
            && let Err(error) = collect_rs_files(root, &mut files)
        {
            eprintln!("{}: failed to walk: {error}", display_path(root));
            return ExitCode::FAILURE;
        }
    }

    files.sort();

    let mut stats = Stats {
        decorated_items: 0,
        issue_refs: BTreeSet::new(),
    };
    let mut violations = Vec::new();

    for path in &files {
        scan_file(path, &mut stats, &mut violations);
    }

    for violation in &violations {
        eprintln!(
            "{}:{}: {}",
            display_path(&violation.path),
            violation.line,
            violation.message,
        );
    }

    eprintln!(
        "check-justifications: {} decorated items; {} distinct djogi issue refs",
        stats.decorated_items,
        stats.issue_refs.len(),
    );

    if !stats.issue_refs.is_empty() {
        eprintln!(
            "check-justifications: djogi issue refs: {}",
            stats
                .issue_refs
                .iter()
                .map(|issue| format!("djogi#{issue}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    if violations.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn reason_is_empty(reason: &[u8]) -> bool {
    reason.iter().all(|byte| byte.is_ascii_whitespace())
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

fn scan_file(path: &Path, stats: &mut Stats, violations: &mut Vec<Violation>) {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            violations.push(Violation {
                path: path.to_owned(),
                line: 1,
                message: format!("failed to read file: {error}"),
            });
            return;
        }
    };

    reject_manual_bypass_refs(path, &source, violations);

    let syntax = match syn::parse_file(&source) {
        Ok(syntax) => syntax,
        Err(error) => {
            violations.push(Violation {
                path: path.to_owned(),
                line: error.span().start().line,
                message: format!("failed to parse Rust source: {error}"),
            });
            return;
        }
    };

    let lines: Vec<_> = source.lines().collect();
    scan_items(path, &lines, &syntax.items, stats, violations);
}

fn reject_manual_bypass_refs(path: &Path, source: &str, violations: &mut Vec<Violation>) {
    let stripped = strip_comments_and_literals(source);

    for (line_index, line) in stripped.lines().enumerate() {
        if contains_identifier(line, "__bypass", false) {
            violations.push(Violation {
                path: path.to_owned(),
                line: line_index + 1,
                message: "direct source reference to `djogi::__bypass` is forbidden under tests/"
                    .to_owned(),
            });
        }
    }
}

fn scan_items(
    path: &Path,
    lines: &[&str],
    items: &[Item],
    stats: &mut Stats,
    violations: &mut Vec<Violation>,
) {
    for item in items {
        let attrs = item_attrs(item);
        let has_bypass = attrs.iter().any(is_bypass_attr);
        let has_cfg_attr_bypass = attrs.iter().any(is_cfg_attr_bypass);

        if has_cfg_attr_bypass {
            violations.push(Violation {
        path: path.to_owned(),
        line: first_attr_line(attrs),
        message: format!(
          "`cfg_attr` cannot carry `{BYPASS_ATTR}` in tests; use a concrete outer attribute",
        ),
      });
        }

        if has_bypass {
            match item {
                Item::Fn(item_fn) => validate_decorated_item(
                    path,
                    lines,
                    &item_fn.attrs,
                    item_fn.sig.fn_token.span.start().line,
                    stats,
                    violations,
                ),
                Item::Impl(item_impl) => validate_decorated_item(
                    path,
                    lines,
                    &item_impl.attrs,
                    item_impl.impl_token.span.start().line,
                    stats,
                    violations,
                ),
                Item::Mod(item_mod) => {
                    if item_mod.content.is_some() {
                        validate_decorated_item(
                            path,
                            lines,
                            &item_mod.attrs,
                            item_mod.mod_token.span.start().line,
                            stats,
                            violations,
                        );
                    } else {
                        violations.push(Violation {
                            path: path.to_owned(),
                            line: item_mod.mod_token.span.start().line,
                            message: format!(
                                "`{BYPASS_ATTR}` cannot decorate file-loaded `mod {};` items",
                                item_mod.ident,
                            ),
                        });
                    }
                }
                _ => violations.push(Violation {
                    path: path.to_owned(),
                    line: item.span().start().line,
                    message: format!(
                        "`{BYPASS_ATTR}` only supports fn, impl, and inline mod items; found {}",
                        item_kind(item),
                    ),
                }),
            }
        }

        if let Item::Mod(item_mod) = item
            && let Some((_brace, nested_items)) = &item_mod.content
        {
            scan_items(path, lines, nested_items, stats, violations);
        }
    }
}

fn validate_decorated_item(
    path: &Path,
    lines: &[&str],
    attrs: &[Attribute],
    header_line: usize,
    stats: &mut Stats,
    violations: &mut Vec<Violation>,
) {
    stats.decorated_items += 1;

    match attached_justification(path, lines, attrs, header_line) {
        Ok(Justification::DjogiIssue(issue)) => {
            stats.issue_refs.insert(issue);
        }
        Ok(Justification::Pin) => {}
        Err(message) => violations.push(Violation {
            path: path.to_owned(),
            line: first_attr_line(attrs).max(1),
            message,
        }),
    }
}

fn attached_justification(
    path: &Path,
    lines: &[&str],
    attrs: &[Attribute],
    header_line: usize,
) -> Result<Justification, String> {
    let attr_ranges = attr_line_ranges(attrs);
    let first_attr_line = attr_ranges
        .iter()
        .map(|(start, _end)| *start)
        .min()
        .unwrap_or(header_line);
    let mut scan_start = first_attr_line;

    while scan_start > 1 {
        let previous = lines
            .get(scan_start - 2)
            .map(|line| line.trim_start())
            .unwrap_or_default();
        if previous.starts_with("//") {
            scan_start -= 1;
        } else {
            break;
        }
    }

    let pin_allowed = is_under_tests_pin(path);
    let mut found = None;

    for line_number in scan_start..header_line {
        let line = lines.get(line_number - 1).copied().unwrap_or_default();
        let trimmed = line.trim_start();

        if is_attr_line(line_number, &attr_ranges) {
            continue;
        }

        if trimmed.starts_with("// JUSTIFICATION") {
            let justification = parse_justification_line(line, pin_allowed)?;
            validate_continuation_lines(lines, line_number + 1, header_line, &attr_ranges)?;
            found = Some(justification);
            break;
        }

        if trimmed.is_empty() {
            return Err(format!(
                "`JUSTIFICATION` for `{BYPASS_ATTR}` must be directly attached to the decorated item; found a blank line in the attribute stack",
            ));
        }

        if !trimmed.starts_with("//") {
            return Err(format!(
                "`JUSTIFICATION` for `{BYPASS_ATTR}` must be attached to the item attribute stack, not separated by code",
            ));
        }
    }

    found.ok_or_else(|| {
    format!(
      "missing attached `// JUSTIFICATION (djogi#<digits>): <reason>` or `// JUSTIFICATION (PIN): <reason>` for `{BYPASS_ATTR}`",
    )
  })
}

fn validate_continuation_lines(
    lines: &[&str],
    start_line: usize,
    header_line: usize,
    attr_ranges: &[(usize, usize)],
) -> Result<(), String> {
    for line_number in start_line..header_line {
        if is_attr_line(line_number, attr_ranges) {
            break;
        }

        let trimmed = lines
            .get(line_number - 1)
            .map(|line| line.trim_start())
            .unwrap_or_default();

        if !trimmed.starts_with("//") {
            break;
        }

        let continuation = trimmed.strip_prefix("//").unwrap_or_default();
        if continuation.trim().is_empty() {
            return Err(
                "JUSTIFICATION continuation comments must contain non-empty text".to_owned(),
            );
        }
    }

    Ok(())
}

fn attr_line_ranges(attrs: &[Attribute]) -> Vec<(usize, usize)> {
    attrs
        .iter()
        .map(|attr| {
            let span = attr.span();
            (span.start().line, span.end().line.max(span.start().line))
        })
        .collect()
}

fn is_attr_line(line: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|(start, end)| (*start..=*end).contains(&line))
}

fn is_bypass_attr(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == BYPASS_ATTR)
}

fn is_cfg_attr_bypass(attr: &Attribute) -> bool {
    if !attr.path().is_ident("cfg_attr") {
        return false;
    }

    match &attr.meta {
        syn::Meta::List(list) => list.tokens.to_string().contains(BYPASS_ATTR),
        _ => false,
    }
}

fn first_attr_line(attrs: &[Attribute]) -> usize {
    attrs
        .iter()
        .map(|attr| attr.span().start().line)
        .min()
        .unwrap_or(1)
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn item_kind(item: &Item) -> &'static str {
    match item {
        Item::Const(_) => "const",
        Item::Enum(_) => "enum",
        Item::ExternCrate(_) => "extern crate",
        Item::Fn(_) => "fn",
        Item::ForeignMod(_) => "extern block",
        Item::Impl(_) => "impl",
        Item::Macro(_) => "macro",
        Item::Mod(_) => "mod",
        Item::Static(_) => "static",
        Item::Struct(_) => "struct",
        Item::Trait(_) => "trait",
        Item::TraitAlias(_) => "trait alias",
        Item::Type(_) => "type",
        Item::Union(_) => "union",
        Item::Use(_) => "use",
        Item::Verbatim(_) => "verbatim item",
        _ => "unknown item",
    }
}

fn is_under_tests_pin(path: &Path) -> bool {
    let mut saw_tests = false;

    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if saw_tests {
            return text == "pin";
        }
        saw_tests = text == "tests";
    }

    false
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
    use super::{Justification, attached_justification, parse_justification_line};
    use std::path::Path;

    #[test]
    fn parses_djogi_issue_justification() {
        assert_eq!(
            parse_justification_line(
                "  // JUSTIFICATION (djogi#234): citext needs LOWER()",
                false,
            ),
            Ok(Justification::DjogiIssue("234".to_owned())),
        );
    }

    #[test]
    fn rejects_bare_issue_reference() {
        assert!(parse_justification_line("// JUSTIFICATION (#234): nope", false).is_err());
    }

    #[test]
    fn pin_justification_is_pin_directory_only() {
        assert_eq!(
            parse_justification_line("// JUSTIFICATION (PIN): exercises raw_execute", true),
            Ok(Justification::Pin),
        );
        assert!(
            parse_justification_line("// JUSTIFICATION (PIN): exercises raw_execute", false)
                .is_err()
        );
    }

    #[test]
    fn rejects_empty_reason() {
        assert!(parse_justification_line("// JUSTIFICATION (djogi#1):  ", false).is_err());
    }

    #[test]
    fn finds_justification_attached_between_attribute_and_fn() {
        let source = r#"
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): citext needs case-insensitive equality.
// QuerySet cannot express LOWER(col) yet.
async fn my_test() {}
"#;
        let syntax = syn::parse_file(source).unwrap();
        let syn::Item::Fn(item_fn) = &syntax.items[0] else {
            panic!("expected fn");
        };
        let lines: Vec<_> = source.lines().collect();

        assert_eq!(
            attached_justification(
                Path::new("tests/integration/example.rs"),
                &lines,
                &item_fn.attrs,
                item_fn.sig.fn_token.span.start().line,
            ),
            Ok(Justification::DjogiIssue("234".to_owned())),
        );
    }

    #[test]
    fn rejects_blank_line_between_attribute_and_justification() {
        let source = r#"
#[djogi::deliberately_bypass_convention_with_raw_sql]

// JUSTIFICATION (djogi#234): too far away.
async fn my_test() {}
"#;
        let syntax = syn::parse_file(source).unwrap();
        let syn::Item::Fn(item_fn) = &syntax.items[0] else {
            panic!("expected fn");
        };
        let lines: Vec<_> = source.lines().collect();

        assert!(
            attached_justification(
                Path::new("tests/integration/example.rs"),
                &lines,
                &item_fn.attrs,
                item_fn.sig.fn_token.span.start().line,
            )
            .is_err(),
        );
    }

    #[test]
    fn reject_manual_bypass_refs_catches_spaced_paths() {
        let source = "use djogi :: __bypass :: RawAccessExt;\n";
        let mut violations = Vec::new();

        super::reject_manual_bypass_refs(
            Path::new("tests/integration/example.rs"),
            source,
            &mut violations,
        );

        assert_eq!(violations.len(), 1);
    }
}
