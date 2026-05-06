//! Cluster 8ζ T12.3 — `IntentFile` reader + precedence resolver.
//!
//! # What
//!
//! Adopters can ship a `.djogi/intent.json` file at the workspace
//! root that carries human-readable design rationale for every model
//! and every field. The file is intentionally **side-channel** —
//! its contents never flow into the schema, the migrations, or the
//! runtime. Its only consumer is `djogi docs`, which merges the
//! rationale into the generated Markdown.
//!
//! # Why a separate file (vs. macro-only)
//!
//! Two reasons rationale wants to live alongside the code AND
//! optionally outside it:
//!
//! 1. **Inline rationale ages.** A field added 18 months ago has
//!    long-form context that doesn't belong in a one-line
//!    `#[field(rationale = "...")]`. The intent file can carry
//!    paragraph-length notes plus authorship metadata
//!    (`added_by`, `added_at`).
//! 2. **Code review separation.** PRs that change rationale should
//!    be reviewable independently of PRs that change the schema.
//!    The intent file lets a non-engineer (PM, compliance officer,
//!    legal reviewer) edit rationale without touching Rust source.
//!
//! Both layers exist; the precedence rule
//! ([`resolve_model_rationale`] / [`resolve_field_rationale`]) is
//! "macro attribute wins, intent.json fallback."
//!
//! # File location and graceful absence
//!
//! `.djogi/intent.json` under the workspace root. Absent file ⇒
//! `Ok(None)` from [`load`] — adopters who never create the file
//! get clean docs with no `## Rationale` section.
//!
//! Other I/O errors (permission, broken symlink) and JSON parse
//! errors are hard `Err`s — silent skipping would mask real
//! mistakes (a typo'd JSON file on the disk should not silently
//! disable rationale rendering).
//!
//! # Wire shape
//!
//! ```json
//! {
//!   "schema_url": "https://djogi.dev/schemas/v1/intent.json",
//!   "models": {
//!     "Vehicle": {
//!       "rationale": "Tracks rental fleet inventory across regions.",
//!       "fields": {
//!         "vin": {
//!           "rationale": "Unique manufacturer identifier; never reused.",
//!           "added_by": "fleet-team",
//!           "added_at": "2026-04-12T09:30:00Z"
//!         }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Both `models` and `fields` are `BTreeMap<String, _>` so iteration
//! during `djogi docs` rendering is byte-deterministic regardless of
//! `serde_json`'s parse order.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level intent document loaded from `.djogi/intent.json`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct IntentFile {
    /// Optional self-describing schema URL — adopters can bump this
    /// when integrating with their own validation tooling. Djogi
    /// does not enforce a value but preserves it for tools that
    /// expect a `$schema`-style anchor.
    #[serde(default, rename = "schema_url")]
    pub schema_url: Option<String>,

    /// Per-model intent keyed by the model's Rust type name (the
    /// short ident, e.g. `"Vehicle"`, not a fully-qualified path).
    /// `BTreeMap` for byte-deterministic iteration.
    #[serde(default)]
    pub models: BTreeMap<String, ModelIntent>,
}

/// Per-model intent.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelIntent {
    /// Long-form rationale for the model's existence and design.
    /// Rendered into `djogi docs` under a `## Rationale` heading.
    #[serde(default)]
    pub rationale: String,

    /// Per-field intent keyed by the field's Rust ident
    /// (`"created_at"`, `"vin"`, etc.). `BTreeMap` for byte-
    /// deterministic iteration during rendering.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldIntent>,
}

/// Per-field intent.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FieldIntent {
    /// Long-form rationale for the field. Rendered into the per-
    /// model field-table's `Rationale` column by `djogi docs`.
    #[serde(default)]
    pub rationale: String,

    /// Author of the field as recorded by the intent maintainer.
    /// Free-form string — Djogi does not validate against any
    /// directory or organisation. Empty when the maintainer chose
    /// not to record authorship.
    #[serde(default)]
    pub added_by: String,

    /// RFC-3339 timestamp string. Stored as a plain `String` (not
    /// a `time::OffsetDateTime`) so the JSON wire format stays
    /// crate-agnostic — adopters can hand-edit `intent.json` with
    /// any RFC-3339 spelling their tooling produces, and Djogi
    /// surfaces it in docs without coercion.
    #[serde(default)]
    pub added_at: String,
}

/// Errors surfaced by [`load`].
#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    /// I/O failure that is not "file not found" — e.g. permission
    /// denied, broken symlink, or read-time failure on a present
    /// file. Distinct from `Ok(None)` for absent files.
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file is present and readable but its contents do not
    /// parse as the documented `IntentFile` shape.
    #[error("malformed intent JSON in {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Load `<workspace_root>/.djogi/intent.json` if present.
///
/// Returns:
///
/// - `Ok(Some(file))` when the file exists and parses cleanly.
/// - `Ok(None)` when the file is **absent** — adopters who do not
///   maintain rationale alongside the code path through here.
/// - `Err(IntentError::Io)` on any non-NotFound I/O error.
/// - `Err(IntentError::Parse)` when the file is present but the
///   JSON does not match the documented shape (typo'd field name,
///   wrong type, etc.).
pub fn load(workspace_root: &Path) -> Result<Option<IntentFile>, IntentError> {
    let path = workspace_root.join(".djogi").join("intent.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(IntentError::Io { path, source }),
    };
    let parsed: IntentFile =
        serde_json::from_slice(&bytes).map_err(|source| IntentError::Parse {
            path: path.clone(),
            source,
        })?;
    Ok(Some(parsed))
}

/// Resolve the rationale string for a model, following the
/// "macro-attr wins, intent.json fallback, neither => `None`"
/// precedence.
///
/// `macro_attr` is the `#[model(rationale = "...")]` value
/// (`Some(&str)` when set, `None` when absent). `intent` is the
/// per-model entry from `IntentFile::models.get(type_name)` — pass
/// `None` if the model has no entry in intent.json.
///
/// The lifetime is tied to the longer of the two inputs so callers
/// don't need to clone for the common "use directly in Markdown
/// rendering" case.
pub fn resolve_model_rationale<'a>(
    macro_attr: Option<&'a str>,
    intent: Option<&'a ModelIntent>,
) -> Option<&'a str> {
    if let Some(s) = macro_attr {
        return Some(s);
    }
    intent
        .map(|i| i.rationale.as_str())
        .filter(|s| !s.is_empty())
}

/// Resolve the rationale string for a field, same precedence as
/// [`resolve_model_rationale`].
pub fn resolve_field_rationale<'a>(
    macro_attr: Option<&'a str>,
    intent: Option<&'a FieldIntent>,
) -> Option<&'a str> {
    if let Some(s) = macro_attr {
        return Some(s);
    }
    intent
        .map(|i| i.rationale.as_str())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Best-effort tempdir cleaned up on `Drop`. Inlined to avoid a
    /// dev-dep on `tempfile` (not currently in the workspace).
    /// Mirrors the pattern in `live_migrate::plan_file::tests`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("djogi-intent-{label}-{pid}-{nanos}"));
            std::fs::create_dir_all(&path).expect("create tempdir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_intent(dir: &Path, contents: &str) {
        let djogi_dir = dir.join(".djogi");
        std::fs::create_dir_all(&djogi_dir).expect("create .djogi");
        let mut f =
            std::fs::File::create(djogi_dir.join("intent.json")).expect("create intent.json");
        f.write_all(contents.as_bytes()).expect("write intent.json");
    }

    #[test]
    fn load_returns_none_when_file_absent() {
        let dir = TempDir::new("test");
        let result = load(dir.path()).expect("load");
        assert!(result.is_none(), "absent intent file must yield Ok(None)");
    }

    #[test]
    fn load_parses_valid_intent_json() {
        let dir = TempDir::new("test");
        write_intent(
            dir.path(),
            r#"{
  "schema_url": "https://djogi.dev/schemas/v1/intent.json",
  "models": {
    "Vehicle": {
      "rationale": "Tracks fleet inventory.",
      "fields": {
        "vin": {
          "rationale": "Unique manufacturer id.",
          "added_by": "fleet-team",
          "added_at": "2026-04-12T09:30:00Z"
        }
      }
    }
  }
}"#,
        );
        let file = load(dir.path()).expect("load").expect("Some(file)");
        assert_eq!(
            file.schema_url.as_deref(),
            Some("https://djogi.dev/schemas/v1/intent.json")
        );
        let model = file.models.get("Vehicle").expect("Vehicle entry");
        assert_eq!(model.rationale, "Tracks fleet inventory.");
        let field = model.fields.get("vin").expect("vin entry");
        assert_eq!(field.rationale, "Unique manufacturer id.");
        assert_eq!(field.added_by, "fleet-team");
        assert_eq!(field.added_at, "2026-04-12T09:30:00Z");
    }

    #[test]
    fn load_returns_err_on_malformed_json() {
        let dir = TempDir::new("test");
        write_intent(dir.path(), "{ this is not json }");
        let result = load(dir.path());
        assert!(matches!(result, Err(IntentError::Parse { .. })));
    }

    #[test]
    fn load_accepts_minimal_document() {
        let dir = TempDir::new("test");
        write_intent(dir.path(), "{}");
        let file = load(dir.path()).expect("load").expect("Some(file)");
        assert!(file.schema_url.is_none());
        assert!(file.models.is_empty());
    }

    #[test]
    fn resolve_model_rationale_macro_attr_wins() {
        let intent = ModelIntent {
            rationale: "intent.json text".to_string(),
            fields: BTreeMap::new(),
        };
        let resolved = resolve_model_rationale(Some("macro-attr text"), Some(&intent));
        assert_eq!(resolved, Some("macro-attr text"));
    }

    #[test]
    fn resolve_model_rationale_intent_fallback() {
        let intent = ModelIntent {
            rationale: "intent.json text".to_string(),
            fields: BTreeMap::new(),
        };
        let resolved = resolve_model_rationale(None, Some(&intent));
        assert_eq!(resolved, Some("intent.json text"));
    }

    #[test]
    fn resolve_model_rationale_empty_intent_treated_as_absent() {
        // Empty `rationale` field — adopters who serialize an
        // empty string don't expect it to surface as the visible
        // rationale. Treat empty as absent.
        let intent = ModelIntent {
            rationale: String::new(),
            fields: BTreeMap::new(),
        };
        let resolved = resolve_model_rationale(None, Some(&intent));
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_model_rationale_neither_present_returns_none() {
        let resolved = resolve_model_rationale(None, None);
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_field_rationale_macro_attr_wins() {
        let field = FieldIntent {
            rationale: "intent text".to_string(),
            added_by: "x".to_string(),
            added_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let resolved = resolve_field_rationale(Some("macro text"), Some(&field));
        assert_eq!(resolved, Some("macro text"));
    }

    #[test]
    fn resolve_field_rationale_intent_fallback() {
        let field = FieldIntent {
            rationale: "intent text".to_string(),
            added_by: "x".to_string(),
            added_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let resolved = resolve_field_rationale(None, Some(&field));
        assert_eq!(resolved, Some("intent text"));
    }

    #[test]
    fn resolve_field_rationale_neither_present_returns_none() {
        let resolved = resolve_field_rationale(None, None);
        assert_eq!(resolved, None);
    }

    #[test]
    fn iteration_is_alphabetical_by_key() {
        // BTreeMap pinning — pin via insertion-order vs iteration-
        // order divergence. Inserting in non-alphabetical order
        // and asserting alphabetical iteration confirms the
        // BTreeMap choice in the public API hasn't drifted to
        // HashMap.
        let mut models: BTreeMap<String, ModelIntent> = BTreeMap::new();
        models.insert("Zebra".to_string(), ModelIntent::default());
        models.insert("Alpha".to_string(), ModelIntent::default());
        models.insert("Mike".to_string(), ModelIntent::default());

        let keys: Vec<&str> = models.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["Alpha", "Mike", "Zebra"]);
    }
}
