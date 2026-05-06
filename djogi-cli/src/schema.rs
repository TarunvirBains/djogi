//! Cluster 8ζ T12.2 — `djogi schema --format json` subcommand.
//!
//! # What
//!
//! Iterates `inventory::iter::<&'static ModelDescriptor>` and emits a
//! deterministic JSON document covering every registered model's
//! shape. Adopters and tooling consume the document for agent
//! integration (LLMs, schema browsers), CI assertions on schema
//! drift, and machine-readable handoffs to downstream codegen.
//!
//! # Why a CLI subcommand
//!
//! The per-model `{MODEL}_SCHEMA` const (T12.1) is great for a single
//! adopter-side type, but cross-model views require enumeration.
//! `inventory::iter` is the right tool for the job, but linker quirks
//! around `--gc-sections` make calling it from arbitrary contexts
//! brittle. Surfacing the JSON behind a CLI subcommand puts the
//! enumeration in a known-good binary (`djogi`) so adopters can
//! consume it via `cargo run -p djogi-cli -- schema --format json`
//! or by invoking the installed `djogi` binary.
//!
//! # JSON shape (schema_version = 1)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "models": [
//!     {
//!       "type_name": "Vehicle",
//!       "table_name": "vehicles",
//!       "app": "main",
//!       "pk_type": "HeerId",
//!       "fields": [
//!         { "name": "id", "sql_type": "BIGINT", "nullable": false, "unique": false, "indexed": false },
//!         ...
//!       ],
//!       "indexes": [...],
//!       "relations": [...]
//!     }
//!   ]
//! }
//! ```
//!
//! # Determinism
//!
//! - `models` is sorted by `(app, type_name)`, both ascending.
//! - Within each model, `fields` follows declaration order (the
//!   descriptor preserves it via Phase 1's macro emission contract).
//! - `relations` is sorted alphabetically by source-column name.
//! - `indexes` follows source declaration order.
//!
//! Two consecutive runs against the same compiled binary produce
//! byte-equal output, suitable for `diff` in CI.

use djogi::descriptor::{FieldDescriptor, ModelDescriptor, PkType};
use serde::Serialize;
use std::path::PathBuf;

/// `--format` value for `djogi schema`. v0.1.0 ships JSON only;
/// `openapi` and `markdown` are reserved for Phase 9 (per spec
/// §538) and will slot in without reshaping the existing flag.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SchemaFormat {
    Json,
}

/// Errors surfaced by [`run`].
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error("failed to write schema output to {path}: {source}")]
    WriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize schema document: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("no models registered — link the binary against a crate that uses #[derive(Model)]")]
    NoModelsRegistered,
}

/// Top-level JSON document emitted by `djogi schema`.
///
/// Carries `schema_version: 1` so adopters can match on this
/// constant when parsing future evolution. Bumping the major version
/// (e.g. `2`) is a coordinated break; minor additive fields land
/// without touching the version.
#[derive(Debug, Serialize)]
struct SchemaDocument {
    schema_version: u32,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
struct ModelEntry {
    type_name: String,
    table_name: String,
    /// Descriptor's `app` field — `None` becomes a JSON `null`. CLI
    /// adopters that filter by app can match `null` for the
    /// synthetic global bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    app: Option<String>,
    pk_type: String,
    has_outbox: bool,
    is_through: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
    fields: Vec<FieldEntry>,
    relations: Vec<RelationEntry>,
}

#[derive(Debug, Serialize)]
struct FieldEntry {
    name: String,
    sql_type: String,
    nullable: bool,
    unique: bool,
    indexed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
}

#[derive(Debug, Serialize)]
struct RelationEntry {
    column: String,
    target: String,
    kind: &'static str,
    on_delete: String,
    nullable: bool,
}

/// Run `djogi schema` against the registered descriptor inventory.
///
/// Writes to `output` if `Some`; otherwise to stdout. Returns
/// [`SchemaError::NoModelsRegistered`] if the inventory is empty —
/// this almost always means the CLI binary was linked without a
/// crate that uses `#[derive(Model)]`, which is operator error
/// rather than a runtime bug.
pub fn run(format: SchemaFormat, output: Option<PathBuf>) -> Result<(), SchemaError> {
    let document = collect_document();
    if document.models.is_empty() {
        return Err(SchemaError::NoModelsRegistered);
    }
    let bytes = match format {
        SchemaFormat::Json => serde_json::to_vec_pretty(&document)?,
    };

    match output {
        Some(path) => {
            // Trailing newline is conventional for Unix files; build
            // the buffer once and write atomically rather than
            // truncating-then-rewriting.
            let mut payload = bytes.clone();
            payload.push(b'\n');
            std::fs::write(&path, &payload).map_err(|source| SchemaError::WriteFailed {
                path: path.clone(),
                source,
            })?;
        }
        None => {
            // stdout: write the bytes followed by a trailing newline
            // so terminal-paste users don't get a `%` prompt-eater.
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle
                .write_all(&bytes)
                .map_err(|source| SchemaError::WriteFailed {
                    path: PathBuf::from("<stdout>"),
                    source,
                })?;
            handle
                .write_all(b"\n")
                .map_err(|source| SchemaError::WriteFailed {
                    path: PathBuf::from("<stdout>"),
                    source,
                })?;
        }
    }
    Ok(())
}

/// Walk the descriptor inventory and project each entry into a
/// JSON-serialisable [`ModelEntry`]. Sort by `(app, type_name)` so
/// the output is deterministic regardless of macro registration
/// order.
fn collect_document() -> SchemaDocument {
    let mut models: Vec<ModelEntry> = inventory::iter::<ModelDescriptor>
        .into_iter()
        .map(project_model)
        .collect();
    models.sort_by(|a, b| {
        let app_cmp = a.app.cmp(&b.app);
        if app_cmp == std::cmp::Ordering::Equal {
            a.type_name.cmp(&b.type_name)
        } else {
            app_cmp
        }
    });
    SchemaDocument {
        schema_version: 1,
        models,
    }
}

fn project_model(desc: &ModelDescriptor) -> ModelEntry {
    let fields: Vec<FieldEntry> = desc.fields.iter().map(project_field).collect();

    let mut relations: Vec<RelationEntry> =
        desc.fields.iter().filter_map(project_relation).collect();
    relations.sort_by(|a, b| a.column.cmp(&b.column));

    ModelEntry {
        type_name: desc.type_name.to_string(),
        table_name: desc.table_name.to_string(),
        app: desc.app.map(|s| s.to_string()),
        pk_type: pk_type_label(desc.pk_type),
        has_outbox: desc.has_outbox,
        is_through: desc.is_through,
        rationale: desc.rationale.map(|s| s.to_string()),
        fields,
        relations,
    }
}

fn project_field(f: &FieldDescriptor) -> FieldEntry {
    FieldEntry {
        name: f.name.to_string(),
        sql_type: f.sql_type.to_string(),
        nullable: f.nullable,
        unique: f.unique,
        indexed: f.indexed,
        rationale: f.rationale.map(|s| s.to_string()),
    }
}

fn project_relation(f: &FieldDescriptor) -> Option<RelationEntry> {
    let kind = f.relation_kind?;
    let target = f.target_type_name?.to_string();
    Some(RelationEntry {
        column: f.name.to_string(),
        target,
        kind: relation_kind_label(kind),
        on_delete: f
            .on_delete
            .map(|od| format!("{od:?}").to_uppercase())
            .unwrap_or_else(|| "RESTRICT".to_string()),
        nullable: f.nullable,
    })
}

fn pk_type_label(pk: PkType) -> String {
    // Use `Debug` to render the variant name. Stable across
    // additions because every variant maps to a distinct ident;
    // adopters reading "HeerId" / "RanjId" / "Serial" / "None" /
    // "Custom(...)" cover the v0.1.0 surface.
    format!("{pk:?}")
}

fn relation_kind_label(kind: djogi::relation::RelationKind) -> &'static str {
    // RelationKind is `#[non_exhaustive]`. Cover the v0.1.0 known
    // variants explicitly and route any future addition through a
    // descriptive sentinel string so `djogi schema` keeps emitting
    // useful output instead of failing to compile when a new
    // variant lands.
    match kind {
        djogi::relation::RelationKind::ForeignKey => "ForeignKey",
        djogi::relation::RelationKind::OneToOne => "OneToOne",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_document_serialises_known_shape() {
        // Build a SchemaDocument by hand (no inventory) and check the
        // top-level field names and ordering. This pins the JSON wire
        // shape without depending on any specific model being linked.
        let doc = SchemaDocument {
            schema_version: 1,
            models: vec![ModelEntry {
                type_name: "Vehicle".to_string(),
                table_name: "vehicles".to_string(),
                app: Some("main".to_string()),
                pk_type: "HeerId".to_string(),
                has_outbox: false,
                is_through: false,
                rationale: None,
                fields: vec![FieldEntry {
                    name: "id".to_string(),
                    sql_type: "BIGINT".to_string(),
                    nullable: false,
                    unique: false,
                    indexed: false,
                    rationale: None,
                }],
                relations: vec![],
            }],
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        assert!(json.starts_with(r#"{"schema_version":1,"models":["#));
        assert!(json.contains(r#""type_name":"Vehicle""#));
        assert!(json.contains(r#""table_name":"vehicles""#));
        assert!(json.contains(r#""pk_type":"HeerId""#));
        assert!(json.contains(r#""sql_type":"BIGINT""#));
    }

    #[test]
    fn empty_inventory_yields_no_models() {
        // Synthesise an empty SchemaDocument and verify the
        // `models` slot is an empty array (not omitted, not null).
        let doc = SchemaDocument {
            schema_version: 1,
            models: vec![],
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        assert_eq!(json, r#"{"schema_version":1,"models":[]}"#);
    }

    #[test]
    fn omitted_fields_skip_when_none() {
        // `app` is `skip_serializing_if = "Option::is_none"`, so a
        // model in the synthetic global bucket emits no `"app":` key.
        let doc = SchemaDocument {
            schema_version: 1,
            models: vec![ModelEntry {
                type_name: "Bare".to_string(),
                table_name: "bares".to_string(),
                app: None,
                pk_type: "HeerId".to_string(),
                has_outbox: false,
                is_through: false,
                rationale: None,
                fields: vec![],
                relations: vec![],
            }],
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        assert!(
            !json.contains(r#""app""#),
            "app:None must be omitted: {json}"
        );
        assert!(
            !json.contains(r#""rationale""#),
            "rationale:None must be omitted: {json}"
        );
    }
}
