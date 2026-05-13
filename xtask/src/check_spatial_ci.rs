use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
};

const CARGO_TOML: &str = "djogi/Cargo.toml";
const MANIFEST: &str = "tests/spatial-ci-tests.txt";
const WORKFLOW: &str = ".github/workflows/ci.yml";

#[derive(Debug, Default)]
struct CargoTest {
    name: Option<String>,
    path: Option<String>,
    required_spatial: bool,
    cargo_block: String,
}

pub fn run() -> ExitCode {
    match run_inner() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner() -> Result<(), String> {
    let tests = parse_cargo_tests(Path::new(CARGO_TOML))
        .map_err(|error| format!("{CARGO_TOML}: failed to read: {error}"))?;
    let manifest = parse_manifest(Path::new(MANIFEST))
        .map_err(|error| format!("{MANIFEST}: failed to read: {error}"))?;

    let by_name: BTreeMap<&str, &CargoTest> = tests
        .iter()
        .filter_map(|test| test.name.as_deref().map(|name| (name, test)))
        .collect();

    let mut failures = Vec::new();

    for name in &manifest {
        if !by_name.contains_key(name.as_str()) {
            failures.push(format!(
                "{MANIFEST}: listed test {name:?} is not a djogi Cargo test"
            ));
        }
    }

    for test in &tests {
        let Some(name) = &test.name else { continue };
        if is_spatial_ci_required(test)? && !manifest.contains(name) {
            failures.push(format!(
                "{MANIFEST}: missing spatial/all-features test {name:?}"
            ));
        }
    }

    let workflow = fs::read_to_string(WORKFLOW)
        .map_err(|error| format!("{WORKFLOW}: failed to read: {error}"))?;
    let active_workflow_lines = active_yaml_lines(&workflow);
    if !active_workflow_lines
        .iter()
        .any(|line| line == "run: cargo xtask check-spatial-ci")
    {
        failures.push(format!(
            "{WORKFLOW}: check job must run `cargo xtask check-spatial-ci`"
        ));
    }
    if active_workflow_lines
        .iter()
        .filter(|line| line.as_str() == "image: postgis/postgis:18-3.6")
        .count()
        < 2
    {
        failures.push(format!(
            "{WORKFLOW}: default and spatial DB service lanes must both use postgis/postgis:18-3.6"
        ));
    }
    if !active_workflow_lines
        .iter()
        .any(|line| line == &format!("done < {MANIFEST}"))
    {
        failures.push(format!(
            "{WORKFLOW}: spatial job must read {MANIFEST} instead of hard-coding test steps"
        ));
    }
    if !active_workflow_lines.iter().any(|line| {
        line.starts_with("cargo test -p djogi --test")
            && line.contains("\"$test_name\"")
            && line.contains("--all-features")
    }) {
        failures.push(format!(
            "{WORKFLOW}: spatial manifest loop must run `$test_name` with --all-features"
        ));
    }

    if failures.is_empty() {
        println!(
            "check-spatial-ci: {} manifest entries cover all spatial-gated Cargo tests",
            manifest.len()
        );
        Ok(())
    } else {
        for failure in failures {
            eprintln!("{failure}");
        }
        Err("check-spatial-ci: manifest/workflow contract failed".to_string())
    }
}

fn parse_cargo_tests(path: &Path) -> io::Result<Vec<CargoTest>> {
    let source = fs::read_to_string(path)?;
    let mut tests = Vec::new();
    let mut current: Option<CargoTest> = None;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == "[[test]]" {
            if let Some(mut test) = current.take() {
                finalize_cargo_test(&mut test);
                tests.push(test);
            }
            current = Some(CargoTest::default());
            continue;
        }

        let Some(test) = current.as_mut() else {
            continue;
        };
        test.cargo_block.push_str(trimmed);
        test.cargo_block.push('\n');
        if let Some(value) = string_assignment(trimmed, "name") {
            test.name = Some(value.to_string());
        } else if let Some(value) = string_assignment(trimmed, "path") {
            test.path = Some(value.to_string());
        }
    }

    if let Some(mut test) = current {
        finalize_cargo_test(&mut test);
        tests.push(test);
    }

    Ok(tests)
}

fn finalize_cargo_test(test: &mut CargoTest) {
    let compact = compact(&test.cargo_block);
    test.required_spatial =
        compact.contains("required-features") && compact.contains("\"spatial\"");
}

fn string_assignment<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start();
    let value = rest.strip_prefix('=')?.trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn parse_manifest(path: &Path) -> io::Result<BTreeSet<String>> {
    let source = fs::read_to_string(path)?;
    let mut entries = BTreeSet::new();
    for line in source.lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        entries.insert(entry.to_string());
    }
    Ok(entries)
}

fn is_spatial_ci_required(test: &CargoTest) -> Result<bool, String> {
    if test.required_spatial {
        return Ok(true);
    }

    let Some(name) = &test.name else {
        return Ok(false);
    };
    let cargo_dir = Path::new(CARGO_TOML)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let path = match &test.path {
        Some(path) => cargo_dir.join(path),
        None => cargo_dir.join("tests").join(format!("{name}.rs")),
    };
    let path = path.as_path();
    if !path.exists() {
        return Ok(false);
    }
    let source = read_test_source_with_includes(path)?;
    Ok(source_requires_spatial_ci(&source))
}

fn source_requires_spatial_ci(source: &str) -> bool {
    let compact = compact(source);
    compact.contains("feature=\"spatial\"")
        || (compact.contains("extensions=[") && compact.contains("\"postgis\""))
}

fn compact(source: &str) -> String {
    source.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn active_yaml_lines(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let active = line
                .split_once('#')
                .map_or(line, |(active, _)| active)
                .trim();
            (!active.is_empty()).then(|| active.to_string())
        })
        .collect()
}

fn read_test_source_with_includes(path: &Path) -> Result<String, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("{}: failed to read: {error}", path.display()))?;
    let mut combined = source.clone();

    for include in include_paths(path, &source) {
        let included = fs::read_to_string(&include)
            .map_err(|error| format!("{}: failed to read include: {error}", include.display()))?;
        combined.push('\n');
        combined.push_str(&included);
    }

    Ok(combined)
}

fn include_paths(path: &Path, source: &str) -> Vec<PathBuf> {
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut includes = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix("include!(") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        includes.push(base.join(&rest[..end]));
    }
    includes
}

#[cfg(test)]
mod tests {
    use super::{
        CargoTest, active_yaml_lines, finalize_cargo_test, source_requires_spatial_ci,
        string_assignment,
    };

    #[test]
    fn string_assignment_reads_quoted_values() {
        assert_eq!(
            string_assignment("name = \"phase6_spatial\"", "name"),
            Some("phase6_spatial")
        );
        assert_eq!(
            string_assignment("path = \"../tests/integration/phase6_spatial.rs\"", "path"),
            Some("../tests/integration/phase6_spatial.rs")
        );
    }

    #[test]
    fn required_spatial_flag_is_plain_data() {
        let test = CargoTest {
            name: Some("phase6_spatial".to_string()),
            path: Some("../tests/integration/phase6_spatial.rs".to_string()),
            required_spatial: true,
            cargo_block: String::new(),
        };
        assert_eq!(test.name.as_deref(), Some("phase6_spatial"));
        assert!(test.required_spatial);
    }

    #[test]
    fn cargo_block_detects_multiline_required_spatial_feature() {
        let mut test = CargoTest {
            name: Some("phase6_spatial".to_string()),
            path: None,
            required_spatial: false,
            cargo_block: "required-features = [\n    \"spatial\",\n]\n".to_string(),
        };
        finalize_cargo_test(&mut test);
        assert!(test.required_spatial);
    }

    #[test]
    fn source_detector_tolerates_whitespace_in_postgis_extensions() {
        assert!(source_requires_spatial_ci(
            "#[djogi::djogi_test(extensions = [ \"postgis\" ])]"
        ));
        assert!(source_requires_spatial_ci(
            "#[cfg( feature = \"spatial\" )]\nfn live_spatial() {}"
        ));
    }

    #[test]
    fn active_yaml_lines_drop_comments_before_workflow_checks() {
        let lines = active_yaml_lines(
            r#"
            # run: cargo xtask check-spatial-ci
            run: cargo xtask check-spatial-ci
            echo "::group::cargo test -p djogi --test ${test_name} --all-features"
            "#,
        );
        assert_eq!(lines[0], "run: cargo xtask check-spatial-ci");
        assert_eq!(lines.len(), 2);
    }
}
