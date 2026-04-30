//! Markdown documentation generation — Phase 7 v3 §8 / T8.
//!
//! `djogi docs` renders a per-model reference page from the global
//! [`crate::descriptor::ModelDescriptor`] inventory. Each model's
//! generated page lists table identity, the field set with SQL types
//! and nullability, declared indexes, foreign-key edges, and any
//! per-field rationale.
//!
//! # Output layout
//!
//! ```text
//! <output_root>/
//! ├── README.md                 — index page + per-app summaries
//! └── <app>/                    — one directory per registered app
//!     └── <ModelName>.md        — one file per model
//! ```
//!
//! The synthetic global bucket (model registered with no `app = X`)
//! lives under `<output_root>/_global_/<ModelName>.md`; the directory
//! token mirrors [`crate::migrate::target::GLOBAL_BUCKET_DIRNAME`] for
//! filesystem-tooling consistency.
//!
//! # Determinism
//!
//! Two invocations against the same descriptor inventory MUST produce
//! byte-identical output. The renderer therefore:
//!
//! - Iterates the descriptor inventory through a sorted `BTreeMap`
//!   keyed by `(app_directory, type_name)`. The natural
//!   `inventory::iter` order is registration order, which is
//!   inherently non-deterministic across compilation runs — sorting
//!   reproducibly resolves that.
//! - Sorts per-model field lists in their declaration order
//!   (preserved by the macro so user-source ordering survives) but
//!   leaves index lists in declaration order too.
//! - Embeds no timestamps, no version strings (other than the
//!   per-page heading content sourced from descriptors), and no
//!   environment-derived data. The output is purely a function of
//!   the descriptor input.
//!
//! # No regex
//!
//! Identifier comparison and filename derivation use byte-level
//! checks. The accepted filename grammar mirrors the Postgres
//! identifier rules used elsewhere in the migrate substrate
//! (`u8::is_ascii_alphanumeric` / underscore).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::descriptor::{
    FieldDescriptor, FieldSqlType, IndexKind, IndexSpec, IndexTarget, ModelDescriptor, PkType,
};

use super::target::GLOBAL_BUCKET_DIRNAME;

// ── Public types ──────────────────────────────────────────────────────────

/// Errors surfaced by [`generate_docs`].
#[derive(Debug)]
pub enum DocsError {
    /// I/O failure while creating an output directory or writing a
    /// per-model markdown file.
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for DocsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocsError::Io { path, source } => {
                write!(f, "docs I/O at {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for DocsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DocsError::Io { source, .. } => Some(source),
        }
    }
}

/// Successful-generation report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsReport {
    /// Total number of model pages written.
    pub models_rendered: usize,
    /// Output root directory the report wrote into.
    pub output_root: PathBuf,
    /// Per-model files written, in apply order. Useful for tooling
    /// that follows up with a diff or a publish step.
    pub written_files: Vec<PathBuf>,
}

// ── Public entry point ────────────────────────────────────────────────────

/// Render the descriptor inventory to per-model markdown pages under
/// `output_root`.
///
/// **Inventory source.** Walks
/// [`inventory::iter::<ModelDescriptor>`](::inventory::iter)
/// directly. Adopters that want to render a fixture set instead of
/// the global inventory should call [`render_inventory`] with their
/// own slice.
pub fn generate_docs(output_root: &Path) -> Result<DocsReport, DocsError> {
    // Snapshot the inventory into an owned slice so the two callers
    // (production + tests) share a single render path.
    let descriptors: Vec<&'static ModelDescriptor> =
        ::inventory::iter::<ModelDescriptor>.into_iter().collect();
    render_inventory(&descriptors, output_root)
}

/// Render an explicit `&[&ModelDescriptor]` slice — used by tests and
/// by future tools that want to render a curated subset.
///
/// **Determinism.** The slice is internally sorted by
/// `(app_directory, type_name)` before rendering so the output is
/// byte-stable regardless of the input ordering.
pub fn render_inventory(
    descriptors: &[&ModelDescriptor],
    output_root: &Path,
) -> Result<DocsReport, DocsError> {
    fs::create_dir_all(output_root).map_err(|e| DocsError::Io {
        path: output_root.to_path_buf(),
        source: e,
    })?;

    // Group by app directory for both the per-app sidebar and the
    // README index. A `BTreeMap` keyed by (app_dir, type_name) gives
    // us a sorted iteration order without an extra sort step.
    let mut by_app: BTreeMap<String, BTreeMap<&str, &ModelDescriptor>> = BTreeMap::new();
    for d in descriptors {
        let app_dir = app_directory(d.app);
        by_app.entry(app_dir).or_default().insert(d.type_name, *d);
    }

    let mut written: Vec<PathBuf> = Vec::with_capacity(descriptors.len());
    let mut total = 0usize;

    for (app_dir, models) in &by_app {
        let app_path = output_root.join(app_dir);
        fs::create_dir_all(&app_path).map_err(|e| DocsError::Io {
            path: app_path.clone(),
            source: e,
        })?;
        for desc in models.values() {
            let body = render_model_page(desc);
            let file_path = app_path.join(model_filename(desc.type_name));
            fs::write(&file_path, body.as_bytes()).map_err(|e| DocsError::Io {
                path: file_path.clone(),
                source: e,
            })?;
            written.push(file_path);
            total += 1;
        }
    }

    // Top-level README index — one bullet per model, grouped by app.
    let readme_body = render_readme(&by_app);
    let readme_path = output_root.join("README.md");
    fs::write(&readme_path, readme_body.as_bytes()).map_err(|e| DocsError::Io {
        path: readme_path.clone(),
        source: e,
    })?;
    written.push(readme_path);

    // Remove any `.md` files that were present in `output_root` from a
    // previous generation run but are no longer in the current inventory
    // (e.g., a model was renamed or moved to a different app). Only
    // `.md` files are touched — non-markdown artifacts in the output
    // tree are preserved. A missing-file removal failure is ignored
    // (best-effort cleanup): the primary contract is that the current
    // inventory is accurately reflected.
    let written_set: std::collections::BTreeSet<&std::path::Path> =
        written.iter().map(PathBuf::as_path).collect();
    if let Ok(walk) = fs::read_dir(output_root) {
        for entry in walk.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if !written_set.contains(path.as_path()) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    // Recurse into per-app subdirectories for stale model pages.
    for app_dir_name in by_app.keys() {
        let app_path = output_root.join(app_dir_name);
        if let Ok(walk) = fs::read_dir(&app_path) {
            for entry in walk.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                if !written_set.contains(path.as_path()) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    Ok(DocsReport {
        models_rendered: total,
        output_root: output_root.to_path_buf(),
        written_files: written,
    })
}

// ── Rendering ─────────────────────────────────────────────────────────────

/// Render a single model's reference page as Markdown.
pub fn render_model_page(desc: &ModelDescriptor) -> String {
    let mut s = String::with_capacity(2048);
    let _ = writeln!(s, "# {}\n", desc.type_name);

    // Header bullets — the at-a-glance facts.
    let _ = writeln!(s, "- **App:** {}", display_app(desc.app));
    let _ = writeln!(s, "- **Table:** `{}`", desc.table_name);
    let _ = writeln!(s, "- **PK kind:** {}", display_pk_type(&desc.pk_type));
    if desc.is_through {
        let _ = writeln!(s, "- **Through model:** yes");
    }
    if let Some(part) = &desc.partition_by {
        let _ = writeln!(s, "- **Partitioned:** {}", display_partition(part));
    }
    if let Some(tk) = desc.tenant_key {
        let _ = writeln!(s, "- **Tenant key:** `{tk}`");
    }
    if desc.has_outbox {
        let _ = writeln!(s, "- **Outbox:** enabled");
    }
    if let Some(ttl) = desc.cache_ttl {
        let _ = writeln!(s, "- **Cache TTL:** {ttl} seconds");
    }
    if let Some(idem) = desc.idempotency_key {
        let _ = writeln!(s, "- **Idempotency key:** `{idem}`");
    }
    if let Some(rfm) = desc.renamed_from {
        let _ = writeln!(s, "- **Renamed from:** `{rfm}`");
    }
    if let Some(moved) = desc.moved_from_app {
        let _ = writeln!(s, "- **Moved from app:** `{moved}`");
    }
    if let Some(rationale) = desc.rationale {
        let _ = writeln!(s, "\n> {rationale}");
    }

    // Field table.
    //
    // Codex round-1 A-3 — the T8 docs contract requires a `Default`
    // column. `FieldDescriptor` itself does not carry a default-SQL
    // string; the projection layer (`migrate::projection`) injects
    // the PK column's `generate_id()` / `generate_id_desc()` /
    // `generate_ranj_id()` / `generate_ranj_id_desc()` /
    // `<custom>` default at the snapshot boundary. The renderer
    // mirrors that policy so the generated reference page reflects
    // what the operator will actually see on the live table — for
    // every other column we render an em-dash (no descriptor-side
    // default).
    s.push_str("\n## Fields\n\n");
    s.push_str("| Name | SQL type | Nullable | Default | Notes |\n");
    s.push_str("|------|----------|----------|---------|-------|\n");
    for f in desc.fields {
        let _ = writeln!(
            s,
            "| `{name}` | `{ty}` | {nullable} | {default} | {notes} |",
            name = f.name,
            ty = f.sql_type,
            nullable = if f.nullable { "yes" } else { "no" },
            default = render_field_default(f, desc),
            notes = render_field_notes(f),
        );
    }

    // Indexes.
    if !desc.indexes.is_empty() {
        s.push_str("\n## Indexes\n\n");
        s.push_str("| Name | Kind | Method | Target | Notes |\n");
        s.push_str("|------|------|--------|--------|-------|\n");
        for idx in desc.indexes {
            let _ = writeln!(
                s,
                "| `{name}` | {kind} | {method:?} | {target} | {notes} |",
                name = idx.name,
                kind = display_index_kind(idx.kind),
                method = idx.index_type,
                target = display_index_target(&idx.target),
                notes = render_index_notes(idx),
            );
        }
    }

    // Foreign keys — a separate table extracted from the field list.
    let fk_rows: Vec<&FieldDescriptor> = desc
        .fields
        .iter()
        .filter(|f| f.relation_kind.is_some())
        .collect();
    if !fk_rows.is_empty() {
        s.push_str("\n## Relations\n\n");
        s.push_str("| Column | Kind | Target | On delete |\n");
        s.push_str("|--------|------|--------|-----------|\n");
        for f in fk_rows {
            let kind = match f.relation_kind {
                Some(crate::relation::RelationKind::ForeignKey) => "ForeignKey",
                Some(crate::relation::RelationKind::OneToOne) => "OneToOne",
                None => "—",
            };
            let target = f.target_type_name.unwrap_or("—");
            let on_delete = match f.on_delete {
                Some(crate::relation::OnDelete::Cascade) => "CASCADE",
                Some(crate::relation::OnDelete::Restrict) => "RESTRICT",
                Some(crate::relation::OnDelete::SetNull) => "SET NULL",
                Some(crate::relation::OnDelete::SetDefault) => "SET DEFAULT",
                Some(crate::relation::OnDelete::Protect) => "RESTRICT (Protect)",
                Some(crate::relation::OnDelete::DoNothing) => "NO ACTION",
                None => "RESTRICT (default)",
            };
            let _ = writeln!(
                s,
                "| `{col}` | {kind} | `{target}` | {on_delete} |",
                col = f.name,
            );
        }
    }

    s
}

/// Render the top-level README index.
fn render_readme(by_app: &BTreeMap<String, BTreeMap<&str, &ModelDescriptor>>) -> String {
    let mut s = String::new();
    s.push_str("# Djogi model reference\n\n");
    s.push_str(
        "Generated by `djogi docs` from the descriptor inventory. \
         One page per registered model, grouped by app.\n\n",
    );
    if by_app.is_empty() {
        s.push_str(
            "_No models registered. Run `cargo build` so `#[model]` macros emit \
             their descriptors, then re-run `djogi docs`._\n",
        );
        return s;
    }
    for (app_dir, models) in by_app {
        let _ = writeln!(s, "## {app_dir}\n");
        for desc in models.values() {
            let _ = writeln!(
                s,
                "- [`{name}`](./{app_dir}/{filename})",
                name = desc.type_name,
                filename = model_filename(desc.type_name),
            );
        }
        s.push('\n');
    }
    s
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Map a descriptor's `app: Option<&'static str>` to its on-disk
/// directory name. Mirrors [`crate::migrate::target::app_dirname`]
/// but operates on the `Option` form so the doc renderer doesn't have
/// to flatten before passing in.
fn app_directory(app: Option<&'static str>) -> String {
    match app {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => GLOBAL_BUCKET_DIRNAME.to_string(),
    }
}

/// Operator-facing app label — shows the synthetic global bucket as
/// `<global>` in documentation rather than the on-disk `_global_`
/// token. The directory name is stable; the human-facing label is
/// chosen for readability.
fn display_app(app: Option<&'static str>) -> &'static str {
    match app {
        Some(s) if !s.is_empty() => s,
        _ => "<global>",
    }
}

/// Render a `PkType` as a human-facing label.
fn display_pk_type(pk: &PkType) -> String {
    match pk {
        PkType::HeerId => "HeerId (ascending)".to_string(),
        PkType::RanjId => "RanjId (ascending)".to_string(),
        PkType::HeerIdDesc => "HeerId (recency-biased)".to_string(),
        PkType::RanjIdDesc => "RanjId (recency-biased)".to_string(),
        PkType::Serial => "Serial".to_string(),
        PkType::None => "None".to_string(),
        PkType::Composite(cols) => format!("Composite({})", cols.join(", ")),
        PkType::Custom(c) => format!("Custom({})", c.type_name),
    }
}

/// Format a `PartitionSpec` for the header bullet.
fn display_partition(part: &crate::descriptor::PartitionSpec) -> String {
    match part {
        crate::descriptor::PartitionSpec::Range { column } => format!("RANGE({column})"),
        crate::descriptor::PartitionSpec::Hash { column, partitions } => {
            format!("HASH({column}, {partitions} partitions)")
        }
    }
}

/// Render the `IndexKind` enum as a short label.
fn display_index_kind(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::NonUnique => "non-unique",
        IndexKind::UniqueConstraint => "unique constraint",
        IndexKind::UniqueIndex => "unique index",
    }
}

/// Render an `IndexTarget` as a comma-joined column list or the
/// literal `expression`.
fn display_index_target(target: &IndexTarget) -> String {
    match target {
        IndexTarget::Columns(cols) => {
            let names: Vec<String> = cols.iter().map(|c| format!("`{}`", c.name)).collect();
            names.join(", ")
        }
        IndexTarget::Expression(_) => "expression".to_string(),
    }
}

/// Compose the per-field "Default" cell.
///
/// The descriptor surface intentionally does NOT carry a per-column
/// default-SQL string — Phase 1 / Phase 5 chose to push the
/// PK-default expansion (`generate_id()`, `generate_id_desc()`, …)
/// down into the projection layer instead so the snapshot stays the
/// single source of truth for `default_sql`. The renderer mirrors
/// that policy here: the `id` column's default is derived from the
/// model's [`PkType`] via
/// [`super::projection::pk_default_sql`], and every other field
/// renders as an em-dash (`—`) for "no descriptor-side default".
///
/// **Known limitation (round-2 A-3):** today's `FieldDescriptor`
/// only carries the PK column's default expression (derived from
/// `parent.pk_type`). Non-PK fields with declared defaults render
/// as `—` because the descriptor doesn't carry their `default_sql`
/// — that information lives in the snapshot's `ColumnSchema` but is
/// not threaded through the descriptor inventory the macro emits.
/// TODO(post-Phase-7): extend `FieldDescriptor` with a
/// `default_sql: Option<&'static str>` field populated by the macro
/// from `#[field(default = "...")]` attributes, and update this
/// renderer's else-branch to surface it.
///
/// Codex round-1 A-3 / round-2 A-3.
fn render_field_default(f: &FieldDescriptor, parent: &ModelDescriptor) -> String {
    if f.name == "id" {
        match super::projection::pk_default_sql(&parent.pk_type) {
            Some(sql) => format!("`{sql}`"),
            None => "—".to_string(),
        }
    } else {
        // TODO(post-Phase-7): when FieldDescriptor.default_sql exists,
        // render it here. See module-level note above.
        "—".to_string()
    }
}

/// Compose the per-field "notes" cell — primary surface for the
/// flags that don't get their own column. Empty cells render as a
/// single space so the markdown table stays aligned.
fn render_field_notes(f: &FieldDescriptor) -> String {
    let mut bits: Vec<String> = Vec::new();
    if f.unique {
        bits.push("UNIQUE".to_string());
    }
    if f.indexed {
        bits.push("indexed".to_string());
    }
    if let Some(max) = f.max_length {
        bits.push(format!("max {max} bytes"));
    }
    if let Some(rfm) = f.renamed_from {
        bits.push(format!("renamed from `{rfm}`"));
    }
    if f.outbox_exclude {
        bits.push("outbox-exclude".to_string());
    }
    if let Some(seq) = f.sequence_within {
        bits.push(format!("sequence within `{seq}`"));
    }
    if let Some(rationale) = f.rationale {
        bits.push(format!("_{rationale}_"));
    }
    if bits.is_empty() {
        " ".to_string()
    } else {
        bits.join("; ")
    }
}

/// Compose the per-index "notes" cell.
fn render_index_notes(idx: &IndexSpec) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(p) = idx.predicate {
        bits.push(format!("WHERE `{p}`"));
    }
    if !idx.include.is_empty() {
        bits.push(format!("INCLUDE ({})", join_quoted(idx.include)));
    }
    if idx.nulls_not_distinct {
        bits.push("NULLS NOT DISTINCT".to_string());
    }
    if idx.requires_out_of_transaction {
        bits.push("CONCURRENTLY".to_string());
    }
    if let Some(ext) = idx.extension_dependency {
        bits.push(format!("requires `{ext}` extension"));
    }
    if bits.is_empty() {
        " ".to_string()
    } else {
        bits.join("; ")
    }
}

/// Join a slice of `&str` as backticked, comma-separated markdown.
fn join_quoted(items: &[&str]) -> String {
    let mut s = String::new();
    for (i, name) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('`');
        s.push_str(name);
        s.push('`');
    }
    s
}

/// Map a model type name to a filesystem-safe markdown filename.
///
/// Any byte that is not an ASCII letter, ASCII digit, or underscore
/// is replaced with `_`. The Rust grammar already restricts type
/// names to ASCII identifiers, so this is a belt-and-braces
/// normaliser; in practice the mapping is the identity for every
/// realistic input. Codex round-1 N-1 — phrased in plain English
/// per `docs/spec/decisions.md` (no regex notation in comments).
fn model_filename(type_name: &str) -> String {
    let mut out = String::with_capacity(type_name.len() + 3);
    for byte in type_name.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            out.push(byte as char);
        } else {
            out.push('_');
        }
    }
    out.push_str(".md");
    out
}

/// Display impl wired up so `FieldSqlType` and friends format as
/// expected. The descriptor module already provides `Display` for
/// `FieldSqlType`; we forward through `format!`.
#[allow(dead_code)]
fn display_sql_type(t: &FieldSqlType) -> String {
    format!("{t}")
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder,
        IndexSpec, IndexTarget, IndexType, ModelDescriptor, PkType, field_descriptor,
        model_descriptor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_root(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("djogi-docs-{tag}-{nanos}-{n}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn fixture_users() -> ModelDescriptor {
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                rationale: Some("primary key"),
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            },
            FieldDescriptor {
                unique: true,
                max_length: Some(254),
                ..field_descriptor("email", FieldSqlType::Text, false)
            },
        ];
        static INDEXES: &[IndexSpec] = &[IndexSpec {
            name: "users_email_uidx",
            target: IndexTarget::Columns(&[IndexColumnSpec {
                name: "email",
                opclass: None,
                order: IndexOrder::Asc,
                nulls: IndexNullsOrder::Default,
            }]),
            kind: IndexKind::UniqueIndex,
            index_type: IndexType::BTree,
            predicate: Some("deleted_at IS NULL"),
            include: &[],
            nulls_not_distinct: false,
            requires_out_of_transaction: false,
            extension_dependency: None,
        }];
        ModelDescriptor {
            tenant_key: Some("org_id"),
            rationale: Some("Application user accounts."),
            indexes: INDEXES,
            app: Some("accounts"),
            ..model_descriptor("User", "users", PkType::HeerIdDesc, FIELDS)
        }
    }

    fn fixture_global_post() -> ModelDescriptor {
        static FIELDS: &[FieldDescriptor] = &[FieldDescriptor {
            ..field_descriptor("id", FieldSqlType::BigInt, false)
        }];
        ModelDescriptor {
            // app: None → global bucket
            ..model_descriptor("Post", "posts", PkType::HeerId, FIELDS)
        }
    }

    #[test]
    fn render_model_page_includes_table_pk_and_field_table() {
        let user = fixture_users();
        let body = render_model_page(&user);
        assert!(body.starts_with("# User\n"));
        assert!(body.contains("**App:** accounts"));
        assert!(body.contains("**Table:** `users`"));
        assert!(body.contains("**PK kind:** HeerId (recency-biased)"));
        assert!(body.contains("**Tenant key:** `org_id`"));
        // Field table headers + at least one row.
        // Codex round-1 A-3: the field table now carries a `Default`
        // column.
        assert!(body.contains("| Name | SQL type | Nullable | Default | Notes |"));
        assert!(body.contains("`email`"));
        assert!(body.contains("UNIQUE"));
        assert!(body.contains("max 254 bytes"));
        // Index section.
        assert!(body.contains("## Indexes"));
        assert!(body.contains("`users_email_uidx`"));
        assert!(body.contains("WHERE `deleted_at IS NULL`"));
    }

    /// Codex round-1 A-3 — the `Default` column must surface the
    /// PK column's `generate_id_desc()` (or whichever PK kind the
    /// model declares) and an em-dash for every other field.
    #[test]
    fn render_model_page_default_column_renders_pk_default_and_em_dash() {
        let user = fixture_users(); // PkType::HeerIdDesc → generate_id_desc()
        let body = render_model_page(&user);
        // The PK row carries the matching DEFAULT expression.
        assert!(
            body.contains("`generate_id_desc()`"),
            "PK default must render as `generate_id_desc()` for HeerIdDesc; \
             body was:\n{body}",
        );
        // The non-PK row (`email`) carries an em-dash.
        let email_row = body
            .lines()
            .find(|l| l.contains("`email`"))
            .expect("email row");
        assert!(
            email_row.contains(" — "),
            "non-PK rows must render Default as em-dash; got:\n{email_row}",
        );

        // A `PkType::HeerId` model emits `generate_id()` (ascending
        // variant — projection-side parity).
        let post = fixture_global_post();
        let body = render_model_page(&post);
        assert!(
            body.contains("`generate_id()`"),
            "PK default must render as `generate_id()` for HeerId; \
             body was:\n{body}",
        );
    }

    #[test]
    fn render_inventory_writes_per_app_directories_and_readme() {
        let user = fixture_users();
        let post = fixture_global_post();
        let descriptors: Vec<&ModelDescriptor> = vec![&user, &post];
        let root = temp_root("layout");
        let report = render_inventory(&descriptors, &root).expect("render");
        assert_eq!(report.models_rendered, 2);

        // README at the root.
        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(readme.contains("# Djogi model reference"));
        assert!(readme.contains("`User`"));
        assert!(readme.contains("`Post`"));
        // The global bucket directory token (`_global_`) sorts BEFORE
        // `accounts` in ASCII byte order (`b'_'` < `b'a'`). The
        // BTreeMap iteration the renderer relies on therefore lists
        // global first, then accounts. Pinning the actual order
        // documents the determinism contract.
        let glb_idx = readme.find("## _global_").unwrap();
        let acc_idx = readme.find("## accounts").unwrap();
        assert!(
            glb_idx < acc_idx,
            "BTreeMap ASCII byte order — `_global_` sorts before `accounts`",
        );

        // Per-app dirs and per-model files.
        assert!(root.join("accounts/User.md").is_file());
        assert!(root.join("_global_/Post.md").is_file());

        // The global model lists `<global>` in its body, not the
        // on-disk directory token.
        let post_body = fs::read_to_string(root.join("_global_/Post.md")).unwrap();
        assert!(post_body.contains("**App:** <global>"));
    }

    #[test]
    fn render_inventory_is_byte_deterministic() {
        // Render twice — same inputs ⇒ identical outputs.
        let user = fixture_users();
        let post = fixture_global_post();
        let descriptors: Vec<&ModelDescriptor> = vec![&user, &post];

        let root_a = temp_root("det_a");
        let root_b = temp_root("det_b");
        render_inventory(&descriptors, &root_a).unwrap();
        render_inventory(&descriptors, &root_b).unwrap();

        let user_a = fs::read(root_a.join("accounts/User.md")).unwrap();
        let user_b = fs::read(root_b.join("accounts/User.md")).unwrap();
        assert_eq!(user_a, user_b);

        let readme_a = fs::read(root_a.join("README.md")).unwrap();
        let readme_b = fs::read(root_b.join("README.md")).unwrap();
        assert_eq!(readme_a, readme_b);
    }

    #[test]
    fn render_inventory_handles_input_order_invariance() {
        // Rendering with two different input orderings must produce
        // identical bytes — the renderer sorts internally.
        let user = fixture_users();
        let post = fixture_global_post();
        let order_a: Vec<&ModelDescriptor> = vec![&user, &post];
        let order_b: Vec<&ModelDescriptor> = vec![&post, &user];

        let root_a = temp_root("order_a");
        let root_b = temp_root("order_b");
        render_inventory(&order_a, &root_a).unwrap();
        render_inventory(&order_b, &root_b).unwrap();

        let readme_a = fs::read(root_a.join("README.md")).unwrap();
        let readme_b = fs::read(root_b.join("README.md")).unwrap();
        assert_eq!(readme_a, readme_b, "render must be input-order-invariant");
    }

    #[test]
    fn render_inventory_against_empty_input_writes_sentinel_readme() {
        let root = temp_root("empty");
        let report = render_inventory(&[], &root).expect("render");
        assert_eq!(report.models_rendered, 0);
        // README still gets written, with the sentinel message.
        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        assert!(readme.contains("No models registered"));
    }

    #[test]
    fn model_filename_replaces_unsafe_bytes() {
        assert_eq!(model_filename("User"), "User.md");
        assert_eq!(model_filename("Order_Line"), "Order_Line.md");
        assert_eq!(model_filename("Foo::Bar"), "Foo__Bar.md");
    }

    #[test]
    fn app_directory_maps_global_bucket() {
        assert_eq!(app_directory(None), GLOBAL_BUCKET_DIRNAME);
        assert_eq!(app_directory(Some("")), GLOBAL_BUCKET_DIRNAME);
        assert_eq!(app_directory(Some("billing")), "billing");
    }

    #[test]
    fn display_pk_type_formats_known_kinds() {
        assert_eq!(display_pk_type(&PkType::HeerId), "HeerId (ascending)");
        assert_eq!(
            display_pk_type(&PkType::HeerIdDesc),
            "HeerId (recency-biased)"
        );
        assert_eq!(display_pk_type(&PkType::Serial), "Serial");
        assert_eq!(
            display_pk_type(&PkType::Composite(&["a", "b"])),
            "Composite(a, b)"
        );
    }
}
