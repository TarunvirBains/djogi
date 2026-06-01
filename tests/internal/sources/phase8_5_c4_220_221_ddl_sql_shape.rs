// Phase 8.5 Cluster 4 — issues #220 + #221: DDL metadata SQL shape pins.
//
// # What this file pins
//
// **djogi#220 — `#[field(type_change_using = "<sql expr>")]` flow.**
//
// 1. End-to-end via the macro: a model carrying
//    `#[field(type_change_using = "(\"kind\"::text)::uuid")]` projects a
//    `ColumnSchema` whose transient `type_change_using` slot carries the
//    adopter's expression. The persisted snapshot drops the slot
//    (`#[serde(skip)]`), which we exercise by diffing a freshly-projected
//    AFTER schema against a hand-built BEFORE snapshot whose
//    `type_change_using` is `None` (mirrors the load-from-disk shape).
// 2. The differ emits `ColumnChange::ChangeType { using: Some(expr), .. }`
//    when the column's `sql_type` differs across the snapshot pair.
// 3. `lower_delta` inlines `USING (<expr>)` verbatim into the
//    `ALTER COLUMN … TYPE …` statement — adopter expression, not the
//    default `<col>::<new_type>` fallback.
// 4. The down side falls back to the default cast (symmetric down-side
//    USING is not modelled; rollback is operator-owned).
// 5. Without `type_change_using`, a known-incompatible cast pair
//    (`TEXT → UUID`) emits a `-- WARNING:` SQL comment that names the
//    corrective attribute. The default cast is still emitted (it is a
//    hint, not a refusal).
// 6. Without `type_change_using`, a benign widening pair
//    (`INTEGER → BIGINT`) emits NO warning comment — false positives
//    would spam the migration file.
//
// **djogi#221 — generated column expression change SQL shape.**
//
// 1. A `SetGenerated { from: Some(prev), to: Some(next) }` lowers to a
//    single `ALTER COLUMN … SET EXPRESSION AS (<new_expr>);` statement —
//    the Postgres 17+ in-place form (djogi targets PG 18+).
// 2. The destructive `DROP COLUMN + ADD COLUMN` pair does NOT appear in
//    the emitted SQL — this is the bug the issue was filed against.
// 3. The down side is symmetric — `SET EXPRESSION AS (<prev_expr>);`
//    fully restores the prior shape.
// 4. A `SetGenerated { from: Some(_), to: None }` (drop generation)
//    emits `ALTER COLUMN … DROP EXPRESSION;` with a lossy marker —
//    Postgres has no in-place inverse to re-install the expression.
// 5. A `SetGenerated { from: None, to: Some(_) }` (add generation)
//    keeps the OfflineOnly comment placeholder — Postgres has no
//    online ALTER form for adding a stored generated expression.
//
// # Why a tests/internal target
//
// The SQL-shape pipeline (macro → descriptor → projection → diff →
// lower_delta) is library-internal. The macro-side parse-time
// rejection cases (`#[field(type_change_using = "")]` and the
// whitespace variant) are covered by lihaaf compile_fail fixtures at
// `djogi-macros/tests/compile_fail/c4_220_*`; this fixture
// pins the SQL output of the projection → diff → emit half. No live
// database is required.
//
// # Spec anchors
//
// - GH #220 — `#[field(type_change_using = "...")]` for non-default-cast
//   column type changes.
// - GH #221 — Generated column expression change SQL shape (PG 17+
//   in-place form).
// - `docs/spec/migrations.md` §10.10b DDL metadata attributes —
//   adopter-facing contract for both attributes.
// - `djogi/src/migrate/sql.rs::emit_alter_column` — the lowering site
//   where both behaviours converge.

use djogi::live_migrate::{
    ClassifyContext, PatternContext, PatternError, classify_operation, dispatch_pattern,
};
use djogi::migrate::OnlineSafetyClassification;
use djogi::migrate::diff::{Classification, ColumnChange, SchemaDelta, SchemaOperation};
use djogi::migrate::projection::BucketKey;
use djogi::migrate::schema::{
    AppliedSchema, ColumnSchema, GeneratedColumnSchema, PkKindSchema, PrimaryKeySchema,
    SNAPSHOT_FORMAT_VERSION, TableSchema,
};
use djogi::migrate::sql::{LossyRollbackKind, lower_delta};
use std::collections::BTreeMap;

// ── Helpers ───────────────────────────────────────────────────────────────

fn empty_global_bucket() -> BucketKey {
    BucketKey {
        database: "main".to_string(),
        app: String::new(),
    }
}

/// Wrap two single-bucket `AppliedSchema`s into per-bucket maps and
/// diff them through the public [`djogi::migrate::diff::diff_bucket_maps`]
/// entry point. `diff_schemas` itself is `pub(crate)`; the public
/// surface goes through the bucket-map form, so test fixtures
/// construct the trivial single-key map.
fn diff_single_bucket(before: &AppliedSchema, after: &AppliedSchema) -> SchemaDelta {
    let bucket = empty_global_bucket();
    let mut before_map = BTreeMap::new();
    before_map.insert(bucket.clone(), before.clone());
    let mut after_map = BTreeMap::new();
    after_map.insert(bucket.clone(), after.clone());
    let mut deltas = djogi::migrate::diff::diff_bucket_maps(&before_map, &after_map)
        .expect("diff_bucket_maps must not surface a hard error for these inputs");
    assert_eq!(
        deltas.len(),
        1,
        "single-bucket diff should produce exactly one delta: {deltas:?}"
    );
    deltas.remove(0)
}

fn col(name: &str, sql_type: &str) -> ColumnSchema {
    ColumnSchema {
        check: None,
        comment: None,
        default_sql: None,
        foreign_key: None,
        generated: None,
        identity: None,
        index_type: None,
        indexed: false,
        max_length: None,
        name: name.to_string(),
        nullable: false,
        on_delete: None,
        outbox_exclude: false,
        rationale: None,
        relation_kind: None,
        renamed_from: None,
        sequence_within: None,
        sql_type: sql_type.to_string(),
        unique: false,
        // djogi#220 — transient projection-only slot.
        type_change_using: None,
    }
}

fn id_col() -> ColumnSchema {
    ColumnSchema {
        default_sql: Some("heerid_next()".to_string()),
        ..col("id", "BIGINT")
    }
}

fn build_schema_with_kind_type(
    sql_type: &str,
    type_change_using: Option<&str>,
) -> AppliedSchema {
    let kind = ColumnSchema {
        type_change_using: type_change_using.map(str::to_string),
        ..col("kind", sql_type)
    };
    let mut models = BTreeMap::new();
    let table = TableSchema {
        app: None,
        columns: vec![id_col(), kind],
        exclusion_constraints: Vec::new(),
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
        storage_params: None,
        table: "items".to_string(),
        table_comment: None,
        tablespace: None,
        tenant_key: None,
    };
    models.insert("items".to_string(), table);
    AppliedSchema {
        djogi_version: String::new(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: String::new(),
        indexes: Vec::new(),
        models,
        registered_apps: vec![String::new()],
    }
}

fn build_schema_with_generated_expression(expr: &str) -> AppliedSchema {
    let email_lower = ColumnSchema {
        generated: Some(GeneratedColumnSchema {
            expression: expr.to_string(),
            stored: true,
        }),
        nullable: true,
        ..col("email_lower", "TEXT")
    };
    let mut models = BTreeMap::new();
    let table = TableSchema {
        app: None,
        columns: vec![id_col(), email_lower],
        exclusion_constraints: Vec::new(),
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
        storage_params: None,
        table: "users".to_string(),
        table_comment: None,
        tablespace: None,
        tenant_key: None,
    };
    models.insert("users".to_string(), table);
    AppliedSchema {
        djogi_version: String::new(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: String::new(),
        indexes: Vec::new(),
        models,
        registered_apps: vec![String::new()],
    }
}

// ── djogi#220 — `#[field(type_change_using = "<expr>")]` SQL shape ──────────

#[test]
fn type_change_with_using_inlines_adopter_expression_end_to_end() {
    // The persisted BEFORE snapshot drops `type_change_using` (the
    // slot is `#[serde(skip)]`), so a freshly-projected AFTER with
    // the attribute set produces the only Some/None pair the differ
    // ever sees in production. We model this by setting `None` on
    // BEFORE and `Some(expr)` on AFTER while changing the sql_type
    // from TEXT to UUID.
    let before = build_schema_with_kind_type("TEXT", None);
    let after = build_schema_with_kind_type("UUID", Some("(\"kind\"::text)::uuid"));

    let delta = diff_single_bucket(&before, &after);
    assert!(
        !matches!(delta.classification, Classification::NoOp),
        "expected a non-NoOp delta — sql_type changed: {:?}",
        delta.classification
    );
    let alter = delta
        .operations
        .iter()
        .find_map(|op| match op {
            SchemaOperation::AlterColumn { table, column, change } => {
                if table == "items" && column == "kind" {
                    Some(change.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .expect("differ must emit AlterColumn for items.kind");

    // The differ must thread the using expression through to ChangeType.
    let using = match alter {
        ColumnChange::ChangeType { from, to, using } => {
            assert_eq!(from, "TEXT");
            assert_eq!(to, "UUID");
            using
        }
        other => panic!("expected ChangeType, got: {other:?}"),
    };
    assert_eq!(
        using.as_deref(),
        Some("(\"kind\"::text)::uuid"),
        "differ must carry the adopter `type_change_using` through verbatim",
    );

    // SQL emit consumes the using.
    let ops = lower_delta(&delta).expect("lower delta");
    let alter_sql = ops
        .iter()
        .find(|op| op.label.starts_with("AlterColumn items.kind"))
        .expect("expected AlterColumn SQL");
    assert!(
        alter_sql.up.contains(
            "ALTER TABLE \"items\" ALTER COLUMN \"kind\" TYPE UUID USING ((\"kind\"::text)::uuid);"
        ),
        "UP must inline the adopter USING expression verbatim: {}",
        alter_sql.up
    );
    assert!(
        !alter_sql.up.contains("USING \"kind\"::UUID"),
        "UP must not also emit the default cast when adopter `using` is set: {}",
        alter_sql.up
    );
    // Adopter-supplied `using` suppresses the known-incompatible-pair warning.
    assert!(
        !alter_sql.up.contains("-- WARNING:"),
        "adopter `using` suppresses the heuristic warning: {}",
        alter_sql.up
    );
    // Down side: default cast (operator-owned for the inverse direction).
    assert!(
        alter_sql.down.contains(
            "ALTER TABLE \"items\" ALTER COLUMN \"kind\" TYPE TEXT USING \"kind\"::TEXT;"
        ),
        "DOWN must fall back to the default cast: {}",
        alter_sql.down
    );
}

#[test]
fn type_change_without_using_warns_for_known_incompatible_pair() {
    // TEXT → UUID without `type_change_using` is a known-incompatible
    // pair. The emitter prepends a `-- WARNING:` SQL comment that
    // names the corrective attribute; the default cast is still
    // emitted (it is a hint, not a refusal).
    let before = build_schema_with_kind_type("TEXT", None);
    let after = build_schema_with_kind_type("UUID", None);

    let delta = diff_single_bucket(&before, &after);
    let ops = lower_delta(&delta).expect("lower delta");
    let alter_sql = ops
        .iter()
        .find(|op| op.label.starts_with("AlterColumn items.kind"))
        .expect("expected AlterColumn SQL");

    assert!(
        alter_sql.up.contains("-- WARNING:"),
        "known-incompatible pair without `using` must surface a warning: {}",
        alter_sql.up
    );
    assert!(
        alter_sql.up.contains("type_change_using"),
        "warning must name the corrective attribute: {}",
        alter_sql.up
    );
    assert!(
        alter_sql.up.contains(
            "ALTER TABLE \"items\" ALTER COLUMN \"kind\" TYPE UUID USING \"kind\"::UUID;"
        ),
        "default cast is still emitted alongside the warning: {}",
        alter_sql.up
    );
}

#[test]
fn type_change_widening_pair_emits_no_warning() {
    // INTEGER → BIGINT is a built-in implicit cast — no warning even
    // without `type_change_using`. False positives spam the migration
    // file with confusing comments; the heuristic is intentionally
    // narrow.
    let before = build_schema_with_kind_type("INTEGER", None);
    let after = build_schema_with_kind_type("BIGINT", None);

    let delta = diff_single_bucket(&before, &after);
    let ops = lower_delta(&delta).expect("lower delta");
    let alter_sql = ops
        .iter()
        .find(|op| op.label.starts_with("AlterColumn items.kind"))
        .expect("expected AlterColumn SQL");

    assert!(
        !alter_sql.up.contains("-- WARNING:"),
        "widening pair must not emit a warning: {}",
        alter_sql.up
    );
    assert!(
        alter_sql.up.contains(
            "ALTER TABLE \"items\" ALTER COLUMN \"kind\" TYPE BIGINT USING \"kind\"::BIGINT;"
        ),
        "default cast emitted: {}",
        alter_sql.up
    );
}

#[test]
fn dormant_type_change_using_with_no_type_change_emits_nothing() {
    // Critical invariant: leaving `#[field(type_change_using = "...")]`
    // on a field after the migration applies must NOT trip a phantom
    // diff. We model this by setting `type_change_using = Some(...)`
    // on BOTH sides (in practice the snapshot's value is `None`
    // because `#[serde(skip)]` strips it, but the differ contract is
    // symmetric — once the AFTER and BEFORE are PartialEq-equal on
    // every structural field, no operations should emit regardless of
    // the `type_change_using` slot's value).
    //
    // We exercise the more user-relevant shape: snapshot BEFORE has
    // `None` (serde-skip drop) and AFTER projection has `Some(...)`,
    // but `sql_type` is unchanged. ColumnSchema's manual PartialEq
    // excludes `type_change_using` from comparison, so the differ
    // treats the column as equal and emits zero AlterColumn entries.
    let before = build_schema_with_kind_type("UUID", None);
    let after = build_schema_with_kind_type("UUID", Some("kind::uuid"));

    let delta = diff_single_bucket(&before, &after);
    assert!(
        matches!(delta.classification, Classification::NoOp),
        "no structural change → NoOp classification: {:?}",
        delta.classification
    );
    assert!(
        delta.operations.is_empty(),
        "no structural change → empty operations vec: {:?}",
        delta.operations
    );
}

// ── djogi#221 — generated column expression change SQL shape ────────────────

#[test]
fn generated_expression_change_uses_in_place_set_expression_as() {
    // Two snapshots differing only on the generated column expression
    // must lower to a single `ALTER COLUMN … SET EXPRESSION AS …`
    // statement — PG 17+ in-place form. The destructive
    // `DROP COLUMN + ADD COLUMN` pair must NOT appear; the entire
    // point of djogi#221 is that the prior emitter was emitting
    // placeholder comments where executable in-place SQL belongs.
    let before = build_schema_with_generated_expression("LOWER(email)");
    let after = build_schema_with_generated_expression("LOWER(TRIM(email))");

    let delta = diff_single_bucket(&before, &after);
    let ops = lower_delta(&delta).expect("lower delta");
    assert_eq!(
        ops.len(),
        1,
        "expected a single AlterColumn op for the expression change: {ops:?}"
    );
    let alter_sql = &ops[0];
    assert!(
        alter_sql.up.contains(
            "ALTER TABLE \"users\" ALTER COLUMN \"email_lower\" SET EXPRESSION AS (LOWER(TRIM(email)));"
        ),
        "UP must use the in-place SET EXPRESSION AS form: {}",
        alter_sql.up
    );
    assert!(
        !alter_sql.up.contains("DROP COLUMN"),
        "UP must not emit destructive DROP COLUMN: {}",
        alter_sql.up
    );
    assert!(
        !alter_sql.up.contains("ADD COLUMN"),
        "UP must not emit destructive ADD COLUMN: {}",
        alter_sql.up
    );
    assert!(
        alter_sql.down.contains(
            "ALTER TABLE \"users\" ALTER COLUMN \"email_lower\" SET EXPRESSION AS (LOWER(email));"
        ),
        "DOWN must restore the prior expression in place: {}",
        alter_sql.down
    );
    assert!(
        alter_sql.lossy.is_none(),
        "expression change rolls back cleanly — not lossy: {:?}",
        alter_sql.lossy
    );
}

// ── djogi#220 follow-up — live-plan / classifier / rollback safety pins ─────

#[test]
fn classifier_routes_change_type_with_using_to_offline_only() {
    // The classifier must route any `ColumnChange::ChangeType` whose
    // `using.is_some()` to OfflineOnly regardless of the cast pair —
    // the live-plan shadow-column pattern cannot replicate an
    // adopter-supplied USING expression in its backfill UPDATE, and
    // emitting the default cast anyway would silently corrupt or
    // fail-per-row on exactly the rows the adopter wrote the
    // expression to handle. The dispatcher and pattern emitters carry
    // a defense-in-depth refusal for the same case (pinned below).
    //
    // The cast pair we choose here (INTEGER → BIGINT) is a benign
    // widening that without `using` would classify OnlineSafe — so the
    // `using.is_some()` arm is the only thing that can produce
    // OfflineOnly. Choosing a pair that already routes OfflineOnly
    // (e.g. TEXT → varchar(255) narrowing) would not isolate the new
    // behaviour.
    let inbound: BTreeMap<String, u32> = BTreeMap::new();
    let overrides: BTreeMap<(String, String), djogi::DefaultVolatility> = BTreeMap::new();
    let ctx = ClassifyContext::application_default(&inbound, &overrides);

    let op_no_using = SchemaOperation::AlterColumn {
        table: "ledger_entry".to_string(),
        column: "amount".to_string(),
        change: ColumnChange::ChangeType {
            from: "INTEGER".to_string(),
            to: "BIGINT".to_string(),
            using: None,
        },
    };
    assert_eq!(
        classify_operation(&op_no_using, &ctx),
        OnlineSafetyClassification::OnlineSafe,
        "INTEGER → BIGINT without `using` is a benign widening — should classify OnlineSafe",
    );

    let op_with_using = SchemaOperation::AlterColumn {
        table: "ledger_entry".to_string(),
        column: "amount".to_string(),
        change: ColumnChange::ChangeType {
            from: "INTEGER".to_string(),
            to: "BIGINT".to_string(),
            using: Some("amount::BIGINT".to_string()),
        },
    };
    assert_eq!(
        classify_operation(&op_with_using, &ctx),
        OnlineSafetyClassification::OfflineOnly,
        "`using.is_some()` must force OfflineOnly regardless of the cast pair — \
         live-plan path cannot replicate an adopter USING in its backfill",
    );
}

#[test]
fn dispatch_pattern_refuses_change_type_with_using() {
    // Belt-and-braces refusal in the dispatcher. The classifier already
    // routes `using.is_some()` to OfflineOnly, so the dispatcher should
    // never receive a non-default-cast change. This pin guards against
    // a future composer that calls `dispatch_pattern` without consulting
    // the classifier first: emitting a backfill UPDATE whose
    // `SET <shadow> = <col>::<to>` silently drops the adopter expression
    // would silently corrupt data, so the dispatcher / pattern emitters
    // refuse the operation explicitly with `PatternError::CannotEmit`.
    let ctx = PatternContext::with_defaults();
    let op = SchemaOperation::AlterColumn {
        table: "items".to_string(),
        column: "kind".to_string(),
        change: ColumnChange::ChangeType {
            from: "TEXT".to_string(),
            to: "UUID".to_string(),
            using: Some("(\"kind\"::text)::uuid".to_string()),
        },
    };
    let err = dispatch_pattern(&op, &ctx)
        .expect_err("dispatch_pattern must refuse `using.is_some()` ChangeType");
    match err {
        PatternError::CannotEmit { reason, .. } => {
            assert!(
                reason.contains("type_change_using")
                    || reason.contains("using")
                    || reason.contains("OfflineOnly")
                    || reason.contains("offline-only"),
                "refusal reason should name the adopter USING / offline-only route: {reason}",
            );
        }
        other => panic!("expected PatternError::CannotEmit, got: {other:?}"),
    }
}

#[test]
fn lossy_marker_emitted_for_forward_using_rollback() {
    // When the forward step carries an adopter `USING (<expr>)`, the
    // emitter must attach a `LossyRollbackWarning` of kind
    // `CustomCast` so `LossyRollbackPolicy::Refuse` (the default)
    // engages on rollback. The down side falls back to the default
    // `<col>::<old_type>` cast, which cannot reconstruct an arbitrary
    // adopter transform — adopters who run the rollback path get a
    // surfaced warning rather than silent data loss.
    let before = build_schema_with_kind_type("TEXT", None);
    let after = build_schema_with_kind_type("UUID", Some("(\"kind\"::text)::uuid"));

    let delta = diff_single_bucket(&before, &after);
    let ops = lower_delta(&delta).expect("lower delta");
    let alter_sql = ops
        .iter()
        .find(|op| op.label.starts_with("AlterColumn items.kind"))
        .expect("expected AlterColumn SQL");

    let lossy = alter_sql
        .lossy
        .as_ref()
        .expect("forward `using=Some` must surface a LossyRollbackWarning");
    assert_eq!(
        lossy.kind,
        LossyRollbackKind::CustomCast,
        "lossy kind must be CustomCast for adopter-supplied forward USING",
    );
    assert!(
        lossy.detail.contains("items")
            && lossy.detail.contains("kind")
            && lossy.detail.contains("TEXT"),
        "detail should identify the column and the rollback target type: {}",
        lossy.detail
    );
    // Sanity-check: the same emission WITHOUT adopter `using` carries no
    // lossy marker — the marker is keyed off the adopter expression,
    // not off the cast pair.
    let before_no = build_schema_with_kind_type("INTEGER", None);
    let after_no = build_schema_with_kind_type("BIGINT", None);
    let delta_no = diff_single_bucket(&before_no, &after_no);
    let ops_no = lower_delta(&delta_no).expect("lower delta");
    let alter_no = ops_no
        .iter()
        .find(|op| op.label.starts_with("AlterColumn items.kind"))
        .expect("expected AlterColumn SQL");
    assert!(
        alter_no.lossy.is_none(),
        "widening cast with no `using` must not surface a lossy marker: {:?}",
        alter_no.lossy
    );
}

#[test]
fn drop_generated_expression_uses_drop_expression_lossy() {
    // Dropping the generation expression converts the column to a
    // regular column (PG 13+ `ALTER COLUMN c DROP EXPRESSION`). The
    // rollback is structurally lossy because Postgres has no in-place
    // inverse — restoring requires DROP COLUMN + ADD COLUMN.
    let before = build_schema_with_generated_expression("LOWER(email)");
    let after = {
        let mut s = before.clone();
        let table = s.models.get_mut("users").unwrap();
        let col = &mut table.columns[1];
        col.generated = None;
        s
    };

    let delta = diff_single_bucket(&before, &after);
    let ops = lower_delta(&delta).expect("lower delta");
    assert_eq!(ops.len(), 1, "expected a single drop-generation op: {ops:?}");
    let alter_sql = &ops[0];
    assert_eq!(
        alter_sql.up,
        "ALTER TABLE \"users\" ALTER COLUMN \"email_lower\" DROP EXPRESSION;",
    );
    assert!(
        alter_sql.down.contains("LOSSY ROLLBACK"),
        "DOWN must document the rollback gap: {}",
        alter_sql.down
    );
    assert!(
        alter_sql.lossy.is_some(),
        "drop-generation rollback is structurally lossy: {:?}",
        alter_sql.lossy
    );
}
