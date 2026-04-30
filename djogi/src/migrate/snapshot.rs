//! Snapshot file I/O — load and save [`AppliedSchema`] as JSON on
//! disk. The loader is the single gatekeeper for `format_version`
//! compatibility; the saver writes pretty-printed, deterministic
//! output with a trailing newline.
//!
//! # File shape
//!
//! Pretty-printed JSON, 2-space indentation, alphabetical keys
//! within every object (deterministic via `BTreeMap` + alphabetical
//! struct field declarations — see [`crate::migrate::schema`] module
//! docs). Files end with a single trailing `\n`. The first
//! `format_version` field in the JSON is checked on load.
//!
//! # `format_version` policy
//!
//! See [`crate::migrate::schema::SNAPSHOT_FORMAT_VERSION`]. Every
//! snapshot struct carries `#[serde(deny_unknown_fields)]`, so any
//! shape change (additive, subtractive, or rename) requires a
//! `format_version` bump. There is no silently-additive path.
//!
//! [`parse_snapshot_bytes`] inspects `format_version` **before**
//! attempting full deserialization. A version mismatch surfaces as
//! [`SnapshotError::UnsupportedFormatVersion`] with an actionable
//! upgrade message; this prevents the user from seeing a generic
//! `unknown field` parse error when the real issue is a Djogi
//! version skew.
//!
//! # Robustness
//!
//! - The loader strips a UTF-8 BOM if the file starts with one.
//!   Some editors silently insert a BOM into JSON files; we tolerate
//!   that on read but never emit one ourselves.
//! - The loader version-checks before structural deserialize so a
//!   newer snapshot rejected by an older Djogi names the version
//!   mismatch first, rather than failing on a `deny_unknown_fields`
//!   trip on whatever new field landed in `"2"`.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::schema::{AppliedSchema, SNAPSHOT_FORMAT_VERSION};

/// Errors surfaced by snapshot load / save. Distinct from
/// [`crate::error::DjogiError`] because snapshot I/O happens both
/// inside the runtime (T4 runner) and at build time (`build.rs`,
/// T6) — `build.rs` cannot depend on the full `DjogiError` machinery.
#[derive(Debug)]
pub enum SnapshotError {
    /// File-system I/O error — read, write, create-dir-all.
    Io {
        /// Path the operation was attempted against, when known.
        path: Option<std::path::PathBuf>,
        source: io::Error,
    },
    /// JSON parse or serialize error — the file exists but does not
    /// match the expected shape.
    Parse {
        path: Option<std::path::PathBuf>,
        source: serde_json::Error,
    },
    /// `format_version` mismatch. The snapshot file declares a
    /// version that this Djogi does not recognise. Includes both
    /// the found and expected versions for actionable operator
    /// messaging.
    UnsupportedFormatVersion {
        /// The version recorded in the snapshot file.
        found: String,
        /// The version this Djogi understands.
        expected: &'static str,
        /// Path the snapshot was loaded from, when known.
        path: Option<std::path::PathBuf>,
    },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io { path, source } => match path {
                Some(p) => write!(f, "snapshot I/O at {}: {source}", p.display()),
                None => write!(f, "snapshot I/O: {source}"),
            },
            SnapshotError::Parse { path, source } => match path {
                Some(p) => write!(f, "snapshot parse at {}: {source}", p.display()),
                None => write!(f, "snapshot parse: {source}"),
            },
            SnapshotError::UnsupportedFormatVersion {
                found,
                expected,
                path,
            } => match path {
                Some(p) => write!(
                    f,
                    "snapshot format version '{found}' at {} is not supported by this \
                     Djogi (expected '{expected}'); upgrade or check out a newer djogi",
                    p.display()
                ),
                None => write!(
                    f,
                    "snapshot format version '{found}' is not supported by this Djogi \
                     (expected '{expected}'); upgrade or check out a newer djogi"
                ),
            },
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnapshotError::Io { source, .. } => Some(source),
            SnapshotError::Parse { source, .. } => Some(source),
            SnapshotError::UnsupportedFormatVersion { .. } => None,
        }
    }
}

/// Save a snapshot to disk, creating parent directories as needed.
///
/// Output is pretty-printed JSON with a single trailing newline.
/// Determinism is the caller's responsibility — see
/// [`crate::migrate::schema`] module docs for the rules
/// (BTreeMap keys, sorted Vec fields, alphabetical struct field
/// declarations).
pub fn save_snapshot(snapshot: &AppliedSchema, path: &Path) -> Result<(), SnapshotError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| SnapshotError::Io {
            path: Some(parent.to_path_buf()),
            source: e,
        })?;
    }

    let mut bytes = serde_json::to_vec_pretty(snapshot).map_err(|e| SnapshotError::Parse {
        path: Some(path.to_path_buf()),
        source: e,
    })?;
    // serde_json's pretty writer does not emit a trailing newline.
    // Adding one keeps file diffs sane and matches POSIX text-file
    // convention.
    bytes.push(b'\n');

    let mut f = fs::File::create(path).map_err(|e| SnapshotError::Io {
        path: Some(path.to_path_buf()),
        source: e,
    })?;
    f.write_all(&bytes).map_err(|e| SnapshotError::Io {
        path: Some(path.to_path_buf()),
        source: e,
    })?;
    Ok(())
}

/// Serialize a snapshot to bytes — same shape as
/// [`save_snapshot`] but returns the buffer instead of writing to
/// disk. Used by tests and by `compose` when staging a pending
/// snapshot at `target/djogi_pending/<target>/<app>.json`.
pub fn serialize_snapshot(snapshot: &AppliedSchema) -> Result<Vec<u8>, SnapshotError> {
    let mut bytes = serde_json::to_vec_pretty(snapshot).map_err(|e| SnapshotError::Parse {
        path: None,
        source: e,
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Load a snapshot from disk. Performs the `format_version` check
/// after parsing — see module docs.
pub fn load_snapshot(path: &Path) -> Result<AppliedSchema, SnapshotError> {
    let bytes = fs::read(path).map_err(|e| SnapshotError::Io {
        path: Some(path.to_path_buf()),
        source: e,
    })?;
    parse_snapshot_bytes(&bytes, Some(path.to_path_buf()))
}

/// Parse a snapshot from a raw byte slice. The `path` is purely for
/// error reporting; pass `None` when parsing in-memory data (tests,
/// build-script usage) and the path-aware error variants degrade to
/// path-less messages.
///
/// Strips a leading UTF-8 BOM if present. **Inspects
/// `format_version` before full structural deserialization** so a
/// newer snapshot read by an older Djogi surfaces
/// [`SnapshotError::UnsupportedFormatVersion`] (actionable upgrade
/// message) rather than a generic `deny_unknown_fields` parse error
/// from a field that didn't exist when the older Djogi was built.
pub fn parse_snapshot_bytes(
    bytes: &[u8],
    path: Option<std::path::PathBuf>,
) -> Result<AppliedSchema, SnapshotError> {
    let bytes = strip_utf8_bom(bytes);

    // Stage 1 — peek at `format_version` only. Use a permissive
    // `serde_json::Value` so the peek succeeds even when the rest of
    // the document carries fields the strict struct deserializer
    // doesn't recognise (the case that matters: an older Djogi
    // reading a future version with new fields). If the document
    // isn't even a JSON object, fall through to the structural
    // parse so its error message is more specific.
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(serde_json::Value::String(found)) = map.get("format_version")
        && found != SNAPSHOT_FORMAT_VERSION
    {
        return Err(SnapshotError::UnsupportedFormatVersion {
            found: found.clone(),
            expected: SNAPSHOT_FORMAT_VERSION,
            path,
        });
    }

    // Stage 2 — full strict deserialize. Reaching here means
    // `format_version` is either absent (will fail strict deserialize
    // because the field is required) or matches the expected
    // version.
    let snapshot: AppliedSchema =
        serde_json::from_slice(bytes).map_err(|e| SnapshotError::Parse {
            path: path.clone(),
            source: e,
        })?;

    // Defense-in-depth: re-check after the strict deserialize. The
    // peek catches the common case (newer-version snapshot); this
    // covers a malformed file that somehow passes both layers.
    if snapshot.format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedFormatVersion {
            found: snapshot.format_version,
            expected: SNAPSHOT_FORMAT_VERSION,
            path,
        });
    }

    Ok(snapshot)
}

/// Strip a UTF-8 BOM (`EF BB BF`) if the slice starts with one.
/// Editors and some toolchains insert a BOM into JSON files; tolerate
/// it on read but never emit it ourselves.
fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        &bytes[3..]
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::super::schema::*;
    use super::*;
    use std::collections::BTreeMap;

    fn empty_snapshot() -> AppliedSchema {
        AppliedSchema {
            djogi_version: "0.1.0".to_string(),
            enums: BTreeMap::new(),
            generated_at: "2026-04-25T00:00:00Z".to_string(),
            indexes: Vec::new(),
            format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
            models: BTreeMap::new(),
            registered_apps: vec!["".to_string()],
        }
    }

    #[test]
    fn round_trip_empty_snapshot() {
        let snap = empty_snapshot();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let parsed = parse_snapshot_bytes(&bytes, None).expect("parse");
        assert_eq!(snap, parsed);
    }

    #[test]
    fn output_ends_with_newline() {
        let snap = empty_snapshot();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        assert_eq!(bytes.last(), Some(&b'\n'));
    }

    #[test]
    fn output_is_pretty_printed_two_space_indent() {
        let snap = empty_snapshot();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let text = std::str::from_utf8(&bytes).expect("utf-8");
        // First indented line should start with exactly two spaces.
        let mut saw_indented_line = false;
        for line in text.lines() {
            if line.starts_with("  ") && !line.starts_with("   ") {
                saw_indented_line = true;
                break;
            }
        }
        assert!(
            saw_indented_line,
            "expected at least one 2-space-indented line"
        );
    }

    #[test]
    fn output_keys_alphabetical_within_top_level() {
        let snap = empty_snapshot();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let text = std::str::from_utf8(&bytes).expect("utf-8");
        let key_order: Vec<&str> = text
            .lines()
            .filter_map(|l| {
                let l = l.trim_start();
                if let Some(rest) = l.strip_prefix('"') {
                    let end = rest.find('"')?;
                    let key = &rest[..end];
                    let after = &rest[end + 1..];
                    if after.starts_with(": ") {
                        Some(key)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        let mut sorted = key_order.clone();
        sorted.sort();
        assert_eq!(key_order, sorted, "top-level keys must be alphabetical");
    }

    #[test]
    fn unknown_format_version_rejected() {
        let mut snap = empty_snapshot();
        snap.format_version = "99".to_string();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        match parse_snapshot_bytes(&bytes, None) {
            Err(SnapshotError::UnsupportedFormatVersion {
                found, expected, ..
            }) => {
                assert_eq!(found, "99");
                assert_eq!(expected, "1");
            }
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    #[test]
    fn future_format_version_error_message_is_actionable() {
        let mut snap = empty_snapshot();
        snap.format_version = "2".to_string();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let err = parse_snapshot_bytes(&bytes, None).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("'2'"), "must name found version: {msg}");
        assert!(msg.contains("'1'"), "must name expected version: {msg}");
        assert!(
            msg.contains("upgrade") || msg.contains("checkout") || msg.contains("check out"),
            "must suggest upgrade path: {msg}"
        );
    }

    #[test]
    fn newer_snapshot_with_unknown_fields_surfaces_version_error_first() {
        // Simulates an older Djogi reading a future snapshot whose
        // `format_version = "2"` carries new fields the older Djogi
        // doesn't recognise. With the format_version peek, we get
        // UnsupportedFormatVersion (actionable) instead of a generic
        // deny_unknown_fields parse error.
        let blob = r#"{
            "djogi_version": "0.2.0",
            "enums": {},
            "format_version": "2",
            "future_field_added_in_v2": "some value",
            "generated_at": "2027-01-01T00:00:00Z",
            "indexes": [],
            "models": {},
            "registered_apps": []
        }"#;
        let err = parse_snapshot_bytes(blob.as_bytes(), None).expect_err("must fail");
        match err {
            SnapshotError::UnsupportedFormatVersion {
                found, expected, ..
            } => {
                assert_eq!(found, "2");
                assert_eq!(expected, "1");
            }
            other => panic!("expected UnsupportedFormatVersion (peek path), got {other:?}"),
        }
    }

    #[test]
    fn utf8_bom_stripped_on_load() {
        let snap = empty_snapshot();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(&bytes);
        let parsed = parse_snapshot_bytes(&with_bom, None).expect("parse with BOM");
        assert_eq!(snap, parsed);
    }

    #[test]
    fn save_creates_parent_directories() {
        let tmp = std::env::temp_dir().join(format!(
            "djogi-snap-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = tmp.join("a").join("b").join("schema_snapshot.json");
        let snap = empty_snapshot();
        save_snapshot(&snap, &nested).expect("save");
        let loaded = load_snapshot(&nested).expect("load");
        assert_eq!(snap, loaded);
        // Cleanup.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn round_trip_preserves_registered_apps_order() {
        let mut snap = empty_snapshot();
        snap.registered_apps = vec!["".to_string(), "billing".to_string(), "users".to_string()];
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let parsed = parse_snapshot_bytes(&bytes, None).expect("parse");
        assert_eq!(parsed.registered_apps, snap.registered_apps);
    }

    #[test]
    fn parse_invalid_json_surfaces_parse_error() {
        let bytes = b"not json {{";
        match parse_snapshot_bytes(bytes, None) {
            Err(SnapshotError::Parse { .. }) => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    /// Recursively walk the parsed JSON and assert every object's
    /// keys are alphabetically ordered. Catches struct-field-order
    /// drift at every nesting depth — not just the top level.
    fn assert_alphabetical_keys(value: &serde_json::Value, path: &str) {
        match value {
            serde_json::Value::Object(map) => {
                let keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
                let mut sorted = keys.clone();
                sorted.sort();
                assert_eq!(
                    keys, sorted,
                    "object at {path} has non-alphabetical keys; got {keys:?}, want {sorted:?}",
                );
                for (k, v) in map {
                    let child = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    assert_alphabetical_keys(v, &child);
                }
            }
            serde_json::Value::Array(arr) => {
                for (i, v) in arr.iter().enumerate() {
                    assert_alphabetical_keys(v, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn alphabetical_keys_at_every_nesting_depth() {
        // Build a non-trivial snapshot with each nested struct
        // populated, so the alphabetic-key walk exercises real shapes.
        let mut snap = empty_snapshot();
        let columns = vec![
            ColumnSchema {
                check: None,
                default_sql: Some("generate_id_desc()".to_string()),
                foreign_key: None,
                generated: None,
                index_type: None,
                indexed: false,
                max_length: None,
                name: "id".to_string(),
                nullable: false,
                on_delete: None,
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: "BIGINT".to_string(),
                unique: false,
            },
            ColumnSchema {
                check: None,
                default_sql: None,
                foreign_key: Some(ForeignKeySchema {
                    deferrable: false,
                    initially_deferred: false,
                    on_delete: OnDeleteSchema::Restrict,
                    ref_column: "id".to_string(),
                    ref_table: "owners".to_string(),
                }),
                generated: None,
                index_type: None,
                indexed: true,
                max_length: None,
                name: "owner_id".to_string(),
                nullable: false,
                on_delete: Some(OnDeleteSchema::Restrict),
                outbox_exclude: false,
                rationale: None,
                relation_kind: Some(RelationKindSchema::ForeignKey),
                renamed_from: None,
                sequence_within: None,
                sql_type: "BIGINT".to_string(),
                unique: false,
            },
        ];
        let table = TableSchema {
            app: Some("billing".to_string()),
            columns,
            exclusion_constraints: Vec::new(),
            fts: Some(FtsSchema {
                column: "search".to_string(),
                dictionary: "english".to_string(),
                source: "title".to_string(),
            }),
            is_through: false,
            moved_from_app: None,
            partition: Some(PartitionSchema::Range {
                column: "created_at".to_string(),
            }),
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: PkKindSchema::HeerIdRecencyBiased,
            },
            rationale: Some("billing audit trail".to_string()),
            renamed_from: None,
            rls_enabled: true,
            table: "vehicles".to_string(),
            tenant_key: Some("org_id".to_string()),
        };
        snap.models.insert("vehicles".to_string(), table);

        snap.indexes.push(IndexSchema {
            extension_dependency: None,
            include: vec!["name".to_string()],
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: "vehicles_owner_id_idx".to_string(),
            nulls_not_distinct: false,
            predicate: Some("deleted_at IS NULL".to_string()),
            requires_out_of_transaction: false,
            table: "vehicles".to_string(),
            target: IndexTargetSchema::Columns(vec![IndexColumnSchema {
                name: "owner_id".to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            }]),
        });

        snap.enums.insert(
            "vehicle_status".to_string(),
            EnumSchema {
                name: "vehicle_status".to_string(),
                variants: vec!["active".to_string(), "decommissioned".to_string()],
            },
        );

        let bytes = serialize_snapshot(&snap).expect("serialize");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("re-parse to Value");
        assert_alphabetical_keys(&value, "");
    }

    // ── Phase 7.5 PR 7: EXCLUSION + stored-generated round-trip ──────

    /// Build a minimal `TableSchema` carrying an `EXCLUDE` constraint
    /// plus a stored-generated column, so the round-trip exercises
    /// both new fields together.
    fn snap_with_pr7_fields() -> AppliedSchema {
        let mut snap = empty_snapshot();
        let columns = vec![
            ColumnSchema {
                check: None,
                default_sql: Some("generate_id()".to_string()),
                foreign_key: None,
                generated: None,
                index_type: None,
                indexed: false,
                max_length: None,
                name: "id".to_string(),
                nullable: false,
                on_delete: None,
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: "BIGINT".to_string(),
                unique: false,
            },
            ColumnSchema {
                check: None,
                default_sql: None,
                foreign_key: None,
                generated: Some(GeneratedColumnSchema {
                    expression: "LOWER(email)".to_string(),
                    stored: true,
                }),
                index_type: None,
                indexed: false,
                max_length: None,
                name: "email_lower".to_string(),
                nullable: true,
                on_delete: None,
                outbox_exclude: false,
                rationale: None,
                relation_kind: None,
                renamed_from: None,
                sequence_within: None,
                sql_type: "TEXT".to_string(),
                unique: false,
            },
        ];
        let table = TableSchema {
            app: None,
            columns,
            exclusion_constraints: vec![ExclusionConstraintSchema {
                deferrable: true,
                elements: vec![
                    ExclusionElementSchema {
                        expr: "room_id".to_string(),
                        with_operator: "=".to_string(),
                    },
                    ExclusionElementSchema {
                        expr: "period".to_string(),
                        with_operator: "&&".to_string(),
                    },
                ],
                initially_deferred: true,
                name: "no_overlap".to_string(),
                using: "gist".to_string(),
                where_clause: Some("status = 'confirmed'".to_string()),
            }],
            fts: None,
            is_through: false,
            moved_from_app: None,
            partition: None,
            primary_key: PrimaryKeySchema {
                columns: vec!["id".to_string()],
                kind: PkKindSchema::HeerId,
            },
            rationale: None,
            renamed_from: None,
            rls_enabled: false,
            table: "bookings".to_string(),
            tenant_key: None,
        };
        snap.models.insert("bookings".to_string(), table);
        snap
    }

    #[test]
    fn round_trip_preserves_exclusion_and_generated_fields() {
        let snap = snap_with_pr7_fields();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let parsed = parse_snapshot_bytes(&bytes, None).expect("parse");
        assert_eq!(snap, parsed, "PR 7 fields must round-trip byte-for-byte");
    }

    #[test]
    fn pr7_fields_serialize_alphabetically_at_every_depth() {
        let snap = snap_with_pr7_fields();
        let bytes = serialize_snapshot(&snap).expect("serialize");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("re-parse to Value");
        assert_alphabetical_keys(&value, "");
    }

    #[test]
    fn snapshot_predating_pr7_fields_loads_with_serde_defaults() {
        // Older snapshots written before PR 7 don't carry the new
        // `generated` / `exclusion_constraints` fields. The
        // `#[serde(default)]` markers ensure those snapshots load
        // cleanly with the new fields populated to their empty
        // defaults — `None` for ColumnSchema.generated and an empty
        // Vec for TableSchema.exclusion_constraints.
        let blob = r#"{
            "djogi_version": "0.1.0",
            "enums": {},
            "format_version": "1",
            "generated_at": "2026-04-25T00:00:00Z",
            "indexes": [],
            "models": {
                "old_table": {
                    "app": null,
                    "columns": [
                        {
                            "check": null,
                            "default_sql": null,
                            "foreign_key": null,
                            "index_type": null,
                            "indexed": false,
                            "max_length": null,
                            "name": "name",
                            "nullable": true,
                            "on_delete": null,
                            "outbox_exclude": false,
                            "rationale": null,
                            "relation_kind": null,
                            "renamed_from": null,
                            "sequence_within": null,
                            "sql_type": "TEXT",
                            "unique": false
                        }
                    ],
                    "fts": null,
                    "is_through": false,
                    "moved_from_app": null,
                    "partition": null,
                    "primary_key": { "columns": ["id"], "kind": "HeerId" },
                    "rationale": null,
                    "renamed_from": null,
                    "rls_enabled": false,
                    "table": "old_table",
                    "tenant_key": null
                }
            },
            "registered_apps": [""]
        }"#;
        let parsed = parse_snapshot_bytes(blob.as_bytes(), None).expect("parse legacy snapshot");
        let table = parsed.models.get("old_table").expect("old_table present");
        assert!(table.exclusion_constraints.is_empty());
        let column = &table.columns[0];
        assert!(column.generated.is_none());
    }
}
