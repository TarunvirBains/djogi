//! `djogi schema --format json` — emit a deterministic JSON document
//! covering every registered model's shape.
//! Adopters and tooling consume the document for agent integration
//! (LLMs, schema browsers), CI assertions on schema drift, and
//! machine-readable handoffs to downstream codegen.
//! # JSON shape (schema_version = 1)
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
//!       "relations": [...]
//!     }
//!   ]
//! }
//! ```
//! # Determinism
//! - `models` is sorted by `(app, type_name)`, both ascending.
//! - Within each model, `fields` follows declaration order.
//! - `relations` is sorted alphabetically by source-column name.
//!   Two consecutive runs against the same compiled binary produce
//!   byte-equal output, suitable for `diff` in CI.

use djogi::descriptor::{FieldDescriptor, ModelDescriptor, PkType};
use djogi::relation::OnDelete;
use serde::Serialize;
use std::path::PathBuf;

/// `--format` value for `djogi schema`. v0.1.0 ships JSON only;
/// `openapi` and `markdown` slots are reserved for a future phase
/// and will land without reshaping the existing flag.
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
/// `schema_version: 1` lets adopters match on the version when
/// parsing future evolution. Major bumps are coordinated breaks;
/// minor additive fields land without touching the version.
#[derive(Debug, Serialize)]
struct SchemaDocument {
    schema_version: u32,
    models: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
struct ModelEntry {
    type_name: String,
    table_name: String,
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
/// Writes to `output` if `Some`, otherwise to stdout. Returns
/// [`SchemaError::NoModelsRegistered`] if the inventory is empty
/// almost always operator error (the binary was linked without a
/// crate that uses `#[derive(Model)]`).
pub fn run(
    format: SchemaFormat,
    models: &[&'static ModelDescriptor],
    output: Option<PathBuf>,
) -> Result<(), SchemaError> {
    let document = collect_document(models);
    if document.models.is_empty() {
        return Err(SchemaError::NoModelsRegistered);
    }
    let mut bytes = match format {
        SchemaFormat::Json => serde_json::to_vec_pretty(&document)?,
    };
    bytes.push(b'\n');

    match output {
        Some(path) => {
            std::fs::write(&path, &bytes).map_err(|source| SchemaError::WriteFailed {
                path: path.clone(),
                source,
            })?;
        }
        None => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle
                .write_all(&bytes)
                .map_err(|source| SchemaError::WriteFailed {
                    path: PathBuf::from("<stdout>"),
                    source,
                })?;
        }
    }
    Ok(())
}

fn collect_document(models: &[&'static ModelDescriptor]) -> SchemaDocument {
    let mut models: Vec<ModelEntry> = models.iter().map(|m| project_model(m)).collect();
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
            .map(|od| od.as_sql().to_string())
            .unwrap_or_else(|| OnDelete::default().as_sql().to_string()),
        nullable: f.nullable,
    })
}

/// Stable per-variant label for `PkType`. Avoids `Debug` formatting so
/// `Composite([...])` and `Custom(CustomPrimaryKeyKind { ... })` don't
/// leak Rust-internal shapes to JSON consumers.
fn pk_type_label(pk: PkType) -> String {
    match pk {
        PkType::HeerId => "HeerId".to_string(),
        PkType::RanjId => "RanjId".to_string(),
        PkType::HeerIdDesc => "HeerIdDesc".to_string(),
        PkType::RanjIdDesc => "RanjIdDesc".to_string(),
        PkType::Serial => "Serial".to_string(),
        PkType::None => "None".to_string(),
        PkType::Composite(cols) => format!("Composite({})", cols.join(", ")),
        PkType::Custom(c) => format!("Custom({})", c.type_name),
        // PkType is #[non_exhaustive]; future variants surface their
        // Debug name so adopters see something more useful than a
        // generic sentinel.
        other => format!("{other:?}"),
    }
}

fn relation_kind_label(kind: djogi::relation::RelationKind) -> &'static str {
    match kind {
        djogi::relation::RelationKind::ForeignKey => "ForeignKey",
        djogi::relation::RelationKind::OneToOne => "OneToOne",
        // RelationKind is #[non_exhaustive]; future variants
        // will surface as "Unknown" until added here.
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_document_serialises_known_shape() {
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
        let doc = SchemaDocument {
            schema_version: 1,
            models: vec![],
        };
        let json = serde_json::to_string(&doc).expect("serialize");
        assert_eq!(json, r#"{"schema_version":1,"models":[]}"#);
    }

    #[test]
    fn omitted_fields_skip_when_none() {
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

    #[test]
    fn pk_type_label_renders_machine_friendly_strings() {
        use djogi::descriptor::CustomPrimaryKeyKind;
        assert_eq!(pk_type_label(PkType::HeerId), "HeerId");
        assert_eq!(pk_type_label(PkType::RanjId), "RanjId");
        assert_eq!(pk_type_label(PkType::HeerIdDesc), "HeerIdDesc");
        assert_eq!(pk_type_label(PkType::RanjIdDesc), "RanjIdDesc");
        assert_eq!(pk_type_label(PkType::Serial), "Serial");
        assert_eq!(pk_type_label(PkType::None), "None");
        assert_eq!(
            pk_type_label(PkType::Composite(&["a", "b"])),
            "Composite(a, b)"
        );
        assert_eq!(
            pk_type_label(PkType::Custom(CustomPrimaryKeyKind {
                type_name: "crate::ids::UserId",
                sql_type: "UUID",
                default_sql: "gen_random_uuid()",
            })),
            "Custom(crate::ids::UserId)"
        );
    }

    #[test]
    fn on_delete_set_null_renders_with_space() {
        // Regression: format!("{:?}", OnDelete::SetNull).to_uppercase
        // would have emitted "SETNULL". Routing through OnDelete::as_sql
        // surfaces the proper DDL spelling.
        assert_eq!(OnDelete::SetNull.as_sql(), "SET NULL");
        assert_eq!(OnDelete::SetDefault.as_sql(), "SET DEFAULT");
        assert_eq!(OnDelete::DoNothing.as_sql(), "NO ACTION");
    }
}
