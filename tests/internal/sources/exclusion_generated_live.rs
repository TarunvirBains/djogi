// PR 7 — Live integration tests for `EXCLUDE` constraints
// and stored-generated columns under a real Postgres 18.
//
// Three tests cover the v3 plan's PR 7 surface:
//
// 1. **`empty_table_exclusion_emits_and_enforces`** — apply the SQL
//  emitted by `lower_operation` for an `AddTable` carrying a
//  declared `EXCLUDE` constraint. Verifies the constraint
//  registers under `pg_constraint.contype = 'x'`, accepts a
//  non-overlapping row, and rejects a second row that would
//  violate the exclusion.
//
// 2. **`empty_table_stored_generated_emits_and_materialises`** —
//  apply the SQL emitted for an `AddTable` carrying a column with
//  `GENERATED ALWAYS AS (LOWER(email)) STORED`. Inserts a row with
//  a mixed-case email; reads back the generated column and asserts
//  it materialises to the lowercased value (Postgres-side
//  evaluation, not application-side).
//
// 3. **`classifier_routes_exclusion_and_generated_to_offline_only`**
//  — build a SchemaDelta that names (a) an EXCLUDE addition on an
//  existing table, (b) a stored-generated column addition on an
//  existing table, and (c) a stored-generated expression change
//  via AlterColumn::SetGenerated. Run `classify_delta` and assert
//  every verdict is `OfflineOnly`. The "live" framing here verifies
//  the differ → classifier pipeline against a fixture that mimics
//  a real populated-table schema (RowCount above the validation
//  threshold), not just isolated SchemaOperation values.
//
// All three run inside `#[djogi::djogi_test]` for per-test database
// provisioning, even where the assertion is purely classifier-side
// — keeping the file's connection lifecycle uniform with the rest
// of the live suite.

use std::collections::BTreeMap;

use djogi::live_migrate::{ClassifyContext, LoggingProfile, TargetDatabase, classify_delta};
use djogi::migrate::diff::{Classification, ColumnChange, SchemaDelta, SchemaOperation};
use djogi::migrate::projection::BucketKey;
use djogi::migrate::schema::{
  ColumnSchema, ExclusionConstraintSchema, ExclusionElementSchema, GeneratedColumnSchema,
  OnlineSafetyClassification, PkKindSchema, PrimaryKeySchema, TableSchema,
};
use djogi::migrate::sql::lower_delta;

const BOOKINGS_TABLE: &str = "pr7_bookings";
const USERS_TABLE: &str = "pr7_users";

fn lower_single_op(op: SchemaOperation) -> djogi::migrate::sql::OperationSql {
  let delta = SchemaDelta {
    bucket: BucketKey {
      database: "main".to_string(),
      app: String::new(),
    },
    operations: vec![op],
    classification: Classification::Additive,
  };
  let mut ops = lower_delta(&delta).expect("lower_delta succeeds");
  assert_eq!(ops.len(), 1, "expected exactly one OperationSql");
  ops.remove(0)
}

fn pk_id() -> PrimaryKeySchema {
  PrimaryKeySchema {
    columns: vec!["id".to_string()],
    kind: PkKindSchema::HeerId,
  }
}

fn id_column() -> ColumnSchema {
  ColumnSchema {
    check: None,
    codec: None,
    comment: None,
    default_sql: Some("generate_id()".to_string()),
    foreign_key: None,
    generated: None,
    identity: None,
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
    type_change_using: None,
  }
}

/// `bookings` table carrying a GiST `EXCLUDE` constraint that
/// prevents two rows from sharing the same `room_id` with overlapping
/// `period` ranges.
fn bookings_table_with_exclusion() -> TableSchema {
  TableSchema {
    app: None,
    columns: vec![
      id_column(),
      ColumnSchema {
        name: "room_id".to_string(),
        sql_type: "BIGINT".to_string(),
        nullable: false,
        ..id_column()
      },
      ColumnSchema {
        name: "period".to_string(),
        sql_type: "tstzrange".to_string(),
        nullable: false,
        default_sql: None,
        ..id_column()
      },
    ],
    exclusion_constraints: vec![ExclusionConstraintSchema {
      deferrable: false,
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
      // djogi#148 — the descriptor-driven composer would
      // emit `CREATE EXTENSION IF NOT EXISTS btree_gist` from this
      // slot. This test bypasses the bootstrap composer (it calls
      // `lower_delta` directly) and still installs btree_gist via
      // `raw_execute` below so the constraint can be applied; the
      // bootstrap-driven install path is exercised by the unit
      // tests in `djogi/src/migrate/bootstrap.rs`.
      extension_dependency: Some("btree_gist".to_string()),
      initially_deferred: false,
      name: "pr7_bookings_no_overlap".to_string(),
      using: "gist".to_string(),
      where_clause: None,
    }],
    fts: None,
    is_through: false,
    moved_from_app: None,
    partition: None,
    primary_key: pk_id(),
    rationale: None,
    renamed_from: None,
    rls_enabled: false,
    table: BOOKINGS_TABLE.to_string(),
    table_comment: None,
    storage_params: None,
    tablespace: None,
    tenant_key: None,
  }
}

/// `users` table with an `email_lower` column declared as `GENERATED
/// ALWAYS AS (LOWER(email)) STORED`.
fn users_table_with_generated_column() -> TableSchema {
  TableSchema {
    app: None,
    columns: vec![
      id_column(),
      ColumnSchema {
        name: "email".to_string(),
        sql_type: "TEXT".to_string(),
        nullable: false,
        default_sql: None,
        ..id_column()
      },
      ColumnSchema {
        name: "email_lower".to_string(),
        sql_type: "TEXT".to_string(),
        nullable: true,
        default_sql: None,
        generated: Some(GeneratedColumnSchema {
          expression: "LOWER(email)".to_string(),
          stored: true,
        }),
        ..id_column()
      },
    ],
    exclusion_constraints: Vec::new(),
    fts: None,
    is_through: false,
    moved_from_app: None,
    partition: None,
    primary_key: pk_id(),
    rationale: None,
    renamed_from: None,
    rls_enabled: false,
    table: USERS_TABLE.to_string(),
    table_comment: None,
    storage_params: None,
    tablespace: None,
    tenant_key: None,
  }
}

#[djogi::djogi_test]
async fn empty_table_exclusion_emits_and_enforces(mut ctx: DjogiContext) {
  // `btree_gist` provides operator classes that let GiST indexes
  // accept BIGINT (and other btree-only types) — required for the
  // `room_id WITH =` element in the exclusion clause. The
  // `tstzrange WITH &&` element works under stock GiST, but the
  // joint constraint needs both sides to share an operator class
  // family.
  ctx.raw_execute("CREATE EXTENSION IF NOT EXISTS btree_gist", &[])
    .await
    .expect("CREATE EXTENSION btree_gist");
  ctx.raw_execute(&format!("DROP TABLE IF EXISTS {BOOKINGS_TABLE}"), &[])
    .await
    .expect("DROP IF EXISTS bookings");

  let table = bookings_table_with_exclusion();
  let op = SchemaOperation::AddTable(table);
  let lowered = lower_single_op(op);

  ctx.raw_execute(&lowered.up, &[])
    .await
    .expect("CREATE TABLE with inline EXCLUDE applies");

  // pg_constraint.contype = 'x' means EXCLUSION.
  let constraint_count: i64 = ctx
    .raw_scalar(
      "SELECT COUNT(*)::bigint FROM pg_constraint c \
       JOIN pg_class t ON c.conrelid = t.oid \
       WHERE t.relname = $1 AND c.contype = 'x' \
        AND c.conname = $2",
      &[&BOOKINGS_TABLE, &"pr7_bookings_no_overlap"],
    )
    .await
    .expect("query pg_constraint for exclusion");
  assert_eq!(
    constraint_count, 1,
    "expected the named EXCLUDE constraint to register in pg_constraint",
  );

  // Insert a non-overlapping row — must succeed.
  ctx.raw_execute(
    &format!(
      "INSERT INTO {BOOKINGS_TABLE} (room_id, period) \
       VALUES (1, tstzrange('2026-01-01 09:00Z', '2026-01-01 10:00Z'))"
    ),
    &[],
  )
  .await
  .expect("first booking row inserts cleanly");

  // Insert an overlapping row in the same room — must fail with
  // exclusion-violation. We rely on the error surfacing via
  // raw_execute's Result; we don't pin the exact error variant
  // because the surfacing layer can vary across postgres-error
  // helpers.
  let conflicting = ctx
    .raw_execute(
      &format!(
        "INSERT INTO {BOOKINGS_TABLE} (room_id, period) \
         VALUES (1, tstzrange('2026-01-01 09:30Z', '2026-01-01 10:30Z'))"
      ),
      &[],
    )
    .await;
  assert!(
    conflicting.is_err(),
    "overlapping booking must be rejected by EXCLUDE constraint",
  );

  // Different room — must succeed (proves the exclusion is keyed
  // on (room_id, period) jointly, not just period).
  ctx.raw_execute(
    &format!(
      "INSERT INTO {BOOKINGS_TABLE} (room_id, period) \
       VALUES (2, tstzrange('2026-01-01 09:30Z', '2026-01-01 10:30Z'))"
    ),
    &[],
  )
  .await
  .expect("overlapping period in DIFFERENT room must succeed");
}

#[djogi::djogi_test]
async fn empty_table_stored_generated_emits_and_materialises(mut ctx: DjogiContext) {
  ctx.raw_execute(&format!("DROP TABLE IF EXISTS {USERS_TABLE}"), &[])
    .await
    .expect("DROP IF EXISTS users");

  let table = users_table_with_generated_column();
  let op = SchemaOperation::AddTable(table);
  let lowered = lower_single_op(op);

  ctx.raw_execute(&lowered.up, &[])
    .await
    .expect("CREATE TABLE with inline GENERATED applies");

  // Verify the generated column registers as STORED in pg_attribute.
  // attgenerated = 's' means STORED; 'v' would be VIRTUAL (Pg19+).
  let generation_kind: String = ctx
    .raw_scalar(
      "SELECT a.attgenerated::text FROM pg_attribute a \
       JOIN pg_class t ON a.attrelid = t.oid \
       WHERE t.relname = $1 AND a.attname = 'email_lower'",
      &[&USERS_TABLE],
    )
    .await
    .expect("query pg_attribute for generated kind");
  assert_eq!(
    generation_kind, "s",
    "email_lower must register as STORED (attgenerated = 's')",
  );

  // INSERT with a mixed-case email — Postgres evaluates the
  // generation expression server-side, so email_lower is populated
  // without the application writing it.
  ctx.raw_execute(
    &format!("INSERT INTO {USERS_TABLE} (email) VALUES ('Mixed@Example.COM')"),
    &[],
  )
  .await
  .expect("INSERT with email only — generated column should auto-populate");

  let lowered_value: String = ctx
    .raw_scalar(
      &format!("SELECT email_lower FROM {USERS_TABLE} ORDER BY id ASC LIMIT 1"),
      &[],
    )
    .await
    .expect("read back email_lower");
  assert_eq!(
    lowered_value, "mixed@example.com",
    "Postgres must materialise email_lower from LOWER(email)",
  );

  // Attempting to write to the generated column directly must fail —
  // confirms the GENERATED ALWAYS clause is enforced by Postgres.
  let direct_write = ctx
    .raw_execute(
      &format!(
        "INSERT INTO {USERS_TABLE} (email, email_lower) \
         VALUES ('test@example.com', 'CUSTOM_VALUE')"
      ),
      &[],
    )
    .await;
  assert!(
    direct_write.is_err(),
    "direct write to GENERATED ALWAYS column must be rejected",
  );
}

#[djogi::djogi_test]
async fn classifier_routes_exclusion_and_generated_to_offline_only(_ctx: DjogiContext) {
  // Build a populated-table classify context — `estimated_rows`
  // above the validation threshold so the classifier sees this as a
  // real production-scale table, not the empty-table fast-path.
  let inbound: BTreeMap<String, u32> = BTreeMap::new();
  let overrides = BTreeMap::new();
  let ctx_classify = ClassifyContext {
    estimated_rows: Some(1_000_000),
    validation_threshold_rows: 100_000,
    multi_fk_threshold: 4,
    logging_profile: LoggingProfile::Balanced,
    target_database: TargetDatabase::Application,
    inbound_fk_counts: &inbound,
    default_volatility_overrides: &overrides,
  };

  let exclusion = ExclusionConstraintSchema {
    deferrable: false,
    elements: vec![ExclusionElementSchema {
      expr: "room_id".to_string(),
      with_operator: "=".to_string(),
    }],
    // Classification test — checks that AddExclusionConstraint
    // routes to OfflineOnly. The extension-dependency slot is
    // orthogonal here; leave None so the test surface stays narrow.
    extension_dependency: None,
    initially_deferred: false,
    name: "no_overlap".to_string(),
    using: "gist".to_string(),
    where_clause: None,
  };

  let generated_column = ColumnSchema {
    name: "email_lower".to_string(),
    sql_type: "TEXT".to_string(),
    nullable: true,
    generated: Some(GeneratedColumnSchema {
      expression: "LOWER(email)".to_string(),
      stored: true,
    }),
    ..id_column()
  };

  let ops = vec![
    SchemaOperation::AddExclusionConstraint {
      table: BOOKINGS_TABLE.to_string(),
      exclusion: exclusion.clone(),
    },
    SchemaOperation::AddColumn {
      table: USERS_TABLE.to_string(),
      column: generated_column.clone(),
    },
    SchemaOperation::AlterColumn {
      table: USERS_TABLE.to_string(),
      column: "email_lower".to_string(),
      change: ColumnChange::SetGenerated {
        from: None,
        to: Some(GeneratedColumnSchema {
          expression: "LOWER(TRIM(email))".to_string(),
          stored: true,
        }),
      },
    },
  ];

  let verdicts = classify_delta(&ops, &ctx_classify);
  assert_eq!(verdicts.len(), 3, "all three ops must classify");
  for (op, verdict) in &verdicts {
    assert_eq!(
      *verdict,
      OnlineSafetyClassification::OfflineOnly,
      "{op:?} must classify as OfflineOnly on a populated table",
    );
  }
}
