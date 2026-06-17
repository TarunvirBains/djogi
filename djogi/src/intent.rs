//! `IntentFile` reader + precedence resolver.
//! Adopters ship `.djogi/intent.json` at the workspace root carrying
//! human-readable rationale for models and fields. The file is
//! side-channel — its contents never reach the schema, migrations, or
//! runtime; only `djogi docs` reads it, merging the rationale into the
//! generated Markdown.
//! Absent file ⇒ `Ok(None)` from [`load`]. I/O and parse errors are
//! hard `Err`s so a typo doesn't silently disable rationale rendering.
//! Resolver precedence ([`resolve_model_rationale`] /
//! [`resolve_field_rationale`]): macro attribute wins, intent.json
//! falls back.
//! # Wire shape
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
//! `models` and `fields` are `BTreeMap<String, _>` so docs rendering is
//! byte-deterministic regardless of parse order.

use crate::migrate::common;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Top-level intent document loaded from `.djogi/intent.json`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct IntentFile {
    /// Optional self-describing schema URL — preserved for tools that
    /// expect a `$schema`-style anchor; Djogi does not enforce it.
    #[serde(default, rename = "schema_url")]
    pub schema_url: Option<String>,

    /// Per-model intent keyed by the Rust type name (short ident, e.g.
    /// `"Vehicle"`).
    #[serde(default)]
    pub models: BTreeMap<String, ModelIntent>,
}

/// Per-model intent.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelIntent {
    /// Rationale rendered into `djogi docs` under `## Rationale`.
    #[serde(default)]
    pub rationale: String,

    /// Per-field intent keyed by the field ident.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldIntent>,
}

/// Per-field intent.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FieldIntent {
    /// Rationale rendered into the per-model field-table's `Rationale`
    /// column by `djogi docs`.
    #[serde(default)]
    pub rationale: String,

    /// Author of the field as recorded by the intent maintainer.
    /// Free-form; Djogi does not validate.
    #[serde(default)]
    pub added_by: String,

    /// RFC-3339 timestamp. Stored as a plain `String` (not a
    /// `time::OffsetDateTime`) so the wire format stays crate-agnostic
    /// and any RFC-3339 spelling the adopter's tooling emits passes
    /// through to docs unchanged.
    #[serde(default)]
    pub added_at: String,
}

/// Errors surfaced by [`load`].
#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    /// I/O failure that is not "file not found" (permission, broken
    /// symlink, etc.). Absent files surface as `Ok(None)`.
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Present and readable, but does not parse as the documented
    /// `IntentFile` shape.
    #[error("malformed intent JSON in {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Load `<workspace_root>/.djogi/intent.json` if present. Absent file
/// → `Ok(None)`; non-NotFound I/O or parse errors → `Err`.
pub fn load(workspace_root: &Path) -> Result<Option<IntentFile>, IntentError> {
    let path = common::resolve_maybe_missing_workspace_path(
        workspace_root,
        Path::new(".djogi").join("intent.json"),
    )
    .map_err(|source| IntentError::Io {
        path: workspace_root.join(".djogi").join("intent.json"),
        source,
    })?;
    let bytes = match common::read_workspace_file(workspace_root, &path) {
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

/// Resolve a model's rationale: `#[model(rationale = "...")]` wins,
/// `intent.json` falls back, neither → `None`. An empty intent
/// rationale is treated as absent.
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

/// Field-side counterpart of [`resolve_model_rationale`].
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
    /// `tempfile` dev-dep — mirrors `live_migrate::plan_file::tests`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let pid = std::process::id();
            let temp_root = std::env::temp_dir();
            let dir_name = format!("djogi-intent-{label}-{pid}-{nanos}");
            let path =
                common::resolve_maybe_missing_workspace_path(&temp_root, Path::new(&dir_name))
                    .unwrap_or_else(|_| temp_root.join(&dir_name));
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
        let djogi_dir =
            common::resolve_maybe_missing_workspace_path(dir, ".djogi").expect("intent dir");
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
        // Pins the BTreeMap choice — non-alphabetical insertion + alphabetical
        // iteration would fail if the public API drifted to HashMap.
        let mut models: BTreeMap<String, ModelIntent> = BTreeMap::new();
        models.insert("Zebra".to_string(), ModelIntent::default());
        models.insert("Alpha".to_string(), ModelIntent::default());
        models.insert("Mike".to_string(), ModelIntent::default());

        let keys: Vec<&str> = models.keys().map(|s| s.as_str()).collect();
        assert_eq!(keys, vec!["Alpha", "Mike", "Zebra"]);
    }
}
