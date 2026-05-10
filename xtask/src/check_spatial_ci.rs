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
    if !workflow.contains(MANIFEST) {
        failures.push(format!(
            "{WORKFLOW}: spatial job must read {MANIFEST} instead of hard-coding test steps"
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
            if let Some(test) = current.take() {
                tests.push(test);
            }
            current = Some(CargoTest::default());
            continue;
        }

        let Some(test) = current.as_mut() else {
            continue;
        };
        if let Some(value) = string_assignment(trimmed, "name") {
            test.name = Some(value.to_string());
        } else if let Some(value) = string_assignment(trimmed, "path") {
            test.path = Some(value.to_string());
        } else if trimmed.starts_with("required-features") && trimmed.contains("\"spatial\"") {
            test.required_spatial = true;
        }
    }

    if let Some(test) = current {
        tests.push(test);
    }

    Ok(tests)
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

    let Some(path) = &test.path else {
        return Ok(false);
    };
    let cargo_dir = Path::new(CARGO_TOML)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let path = cargo_dir.join(path);
    let path = path.as_path();
    let source = read_test_source_with_includes(path)?;
    Ok(source.contains("feature = \"spatial\"") || source.contains("extensions = [\"postgis\"]"))
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
    use super::{CargoTest, string_assignment};

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
        };
        assert_eq!(test.name.as_deref(), Some("phase6_spatial"));
        assert!(test.required_spatial);
    }
}
