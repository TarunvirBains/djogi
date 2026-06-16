// V3  — integration tests for the `#[model(indexes(...))]`
// grammar against live Postgres 18.
//
// # What this file proves
//
// - Every `#[model(indexes(...))]` form declared in §5 of the plan produces
//   a well-formed [`IndexSpec`] in the `ModelDescriptor`.
// - When the DDL implied by each `IndexSpec` is applied to a real Postgres
//   18 instance, the resulting `pg_index` / `pg_constraint` rows match the
//   kind, column order, predicate, include-list, and nulls-distinct flag
//   that the descriptor claims.
// - The `unique(..., concurrently = true)` escalation lands as a
//   `UniqueIndex` (no `pg_constraint` row) — proving the §6.2 contract
//   round-trips end-to-end, not just at the descriptor boundary.
// - The GiST spatial path is unchanged: a `#[model]` with a
//   `GeoPoint` field still emits one GiST `IndexSpec` on the geography
//   column, and that index round-trips as `pg_am.amname = 'gist'`.
//
// # Why there is a local DDL-rendering helper
//
// Lands the descriptor-level contract; the real DDL emitter
// lives in proper. The `render_indexes_ddl` helper below is a
// test-only translator from `IndexSpec` into `CREATE INDEX` /
// `ALTER TABLE … ADD CONSTRAINT` SQL so the pg_catalog round-trip can be
// exercised today. Keeping it local to this file (not promoted into the
// `djogi` crate) matches the plan's "test-only" boundary — will
// own a richer emitter with renames, transactional scoping, and
// statement-timeout policy; this helper only needs to cover the
// Grammar.
//
// # How each test runs
//
// 1. The model is declared with `#[model(table = "…", indexes(…))]`.
// 2. `Model::descriptor()` exposes the populated `ModelDescriptor`.
// 3. `setup_schema` creates a minimal matching table via `ctx.raw_ddl`.
// 4. `render_indexes_ddl(&desc)` produces one DDL statement per index.
// 5. Each statement is applied via `ctx.raw_ddl`.
// 6. The test queries `pg_catalog` to assert the expected row shape.
//
// The per-test database is provisioned and dropped by `#[djogi_test]`
// (see `djogi-macros::djogi_test`), so every test starts with a clean
// schema — no cross-test contamination, no ordering coupling.

// Every test model references its descriptor; the struct fields themselves
// never have values constructed. The wrapper module carries allow(dead_code).

use djogi::descriptor::{
    IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder, IndexSpec, IndexTarget, IndexType,
    ModelDescriptor,
};
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test models — one per §5 grammar form plus a handful of combinations that
// exercise the UniqueConstraint / UniqueIndex boundary.
// ---------------------------------------------------------------------------

/// Plain composite index on `(tenant_id, created_at)`.
/// §5 simple-ident form — no per-column knobs, no uniqueness.
#[model(table = "events", no_default, indexes(
    index(fields = [tenant_id, created_at_evt]),
))]
#[derive(Debug, Clone)]
pub struct Event {
    pub tenant_id: HeerId,
    pub created_at_evt: DateTime,
    pub payload: String,
}

/// Simple `unique(fields = [...])` — lowers to a `UniqueConstraint` because
/// every v3 constraint-adoptable property (predicate / include /
/// nulls_not_distinct / expression / concurrently) is absent.
#[model(table = "simple_unique", indexes(
    unique(fields = [email]),
))]
#[derive(Debug, Clone)]
pub struct SimpleUnique {
    pub email: String,
}

/// Partial unique — `where = "deleted_at IS NULL"` forces `UniqueIndex`
/// because `ADD CONSTRAINT` has no partial-predicate form.
#[model(table = "partial_unique", no_default, indexes(
    unique(fields = [email], where = "deleted_at IS NULL"),
))]
#[derive(Debug, Clone)]
pub struct PartialUnique {
    pub email: String,
    pub deleted_at: Option<DateTime>,
}

/// `NULLS NOT DISTINCT` unique — Q2 — forces `UniqueIndex` because
/// `ADD CONSTRAINT ... UNIQUE` has no nulls-distinct knob.
#[model(table = "nnd_unique", no_default, indexes(
    unique(fields = [tenant_id, slug], nulls_not_distinct = true),
))]
#[derive(Debug, Clone)]
pub struct NndUnique {
    pub tenant_id: HeerId,
    pub slug: Option<String>,
}

/// Covering index with `INCLUDE` payload columns.
#[model(table = "covering", no_default, indexes(
    index(fields = [created_at_c], include = [status, priority]),
))]
#[derive(Debug, Clone)]
pub struct Covering {
    pub created_at_c: DateTime,
    pub status: String,
    pub priority: i32,
}

/// Expression index — `lower(email)` must show up as a functional index
/// in `pg_index.indexprs` (non-null).
#[model(table = "expression", indexes(index(expr = "lower(email)"),))]
#[derive(Debug, Clone)]
pub struct Expression {
    pub email: String,
}

/// Per-column record form — one column is `DESC NULLS FIRST`, another
/// carries a custom opclass. The column-order (indkey) and descending
/// direction (indoption bit 0) must round-trip through `pg_index`.
#[model(table = "per_column", no_default, indexes(
    index(fields = [
        (col = created_at_pc, order = desc, nulls = first),
        (col = status, opclass = "text_pattern_ops"),
    ]),
))]
#[derive(Debug, Clone)]
pub struct PerColumn {
    pub created_at_pc: DateTime,
    pub status: String,
}

/// `unique(fields = [...], concurrently = true)` — §6.2 escalation.
/// Must land as `UniqueIndex` (no `pg_constraint` row) with name stem
/// `_uidx`.
#[model(table = "unique_concurrent", no_default, indexes(
    unique(fields = [tenant_id, email], concurrently = true),
))]
#[derive(Debug, Clone)]
pub struct UniqueConcurrent {
    pub tenant_id: HeerId,
    pub email: String,
}

/// `unique(fields = [...], include = [...])` — §6.4 escalation: the
/// covering payload is a unique-index-only feature that
/// `ALTER TABLE … ADD CONSTRAINT … UNIQUE` cannot express, so the
/// macro must emit `IndexKind::UniqueIndex` (not `UniqueConstraint`).
/// Regression guard for `forces_unique_index` dropping the
/// `!body.include.is_empty()` clause.
#[model(table = "unique_include", no_default, indexes(
    unique(fields = [email], include = [display_name]),
))]
#[derive(Debug, Clone)]
pub struct UniqueInclude {
    pub email: String,
    pub display_name: String,
}

/// `unique(expr = "lower(email)")` — §6.4 escalation: expression-target
/// uniqueness is also a unique-index-only feature. Regression guard for
/// `forces_unique_index` dropping the `IndexDeclTarget::Expr` clause.
#[model(
    table = "unique_expr",
    no_default,
    indexes(unique(expr = "lower(email)"),)
)]
#[derive(Debug, Clone)]
pub struct UniqueExpr {
    pub email: String,
}

/// Regression guard — GiST path. A model with a `GeoPoint` field
/// must still emit exactly one GiST `IndexSpec` on the geography column,
/// and that index must land in `pg_am.amname = 'gist'`.
#[cfg(feature = "spatial")]
#[model(table = "places", no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: djogi::GeoPoint,
}

// ---------------------------------------------------------------------------
// DDL rendering — test-only helper.
// ---------------------------------------------------------------------------

/// Render the `CREATE TABLE` statement that every live-DB test uses.
///
/// The column list mirrors the `#[model]` definition (framework columns
/// `id / created_at / updated_at` first, then user-declared columns in
/// declaration order). Column types are derived from `FieldSqlType` via
/// its `Display` impl, which matches what the migration emitter will
/// eventually produce — so the DDL here is representative, not ad hoc.
fn render_create_table(desc: &ModelDescriptor) -> String {
    let mut cols: Vec<String> = Vec::with_capacity(desc.fields.len());
    for f in desc.fields {
        // Framework columns keep their DEFAULT; everything else is NOT NULL
        // unless the field was declared `Option<T>`.
        let not_null = if f.nullable { "" } else { " NOT NULL" };
        let default = match f.name {
            "id" => " PRIMARY KEY DEFAULT generate_id()",
            "created_at" | "updated_at" => " DEFAULT now()",
            _ => "",
        };
        cols.push(format!(
            "    {} {}{}{}",
            f.name, f.sql_type, not_null, default
        ));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);",
        desc.table_name,
        cols.join(",\n")
    )
}

/// Render the DDL implied by each `IndexSpec` on a model, scoped to
/// `table`. Returns one statement per index, in declaration order.
///
/// The mapping is:
///
/// - `IndexKind::NonUnique` → `CREATE INDEX [CONCURRENTLY] name ON table
///   USING <method>(cols)` with optional `INCLUDE` / `WHERE` clauses.
/// - `IndexKind::UniqueIndex` → `CREATE UNIQUE INDEX [CONCURRENTLY] name
///   ON table USING <method>(cols) [INCLUDE ...] [NULLS NOT DISTINCT]
///   [WHERE ...]`.
/// - `IndexKind::UniqueConstraint` → `ALTER TABLE table ADD CONSTRAINT
///   name UNIQUE (cols) [INCLUDE ...]`. Note: the constraint form has
///   **no** `CONCURRENTLY`, **no** predicate, and **no** `NULLS NOT
///   DISTINCT` clause — that's exactly why §6.2 / §6.4 escalate those
///   shapes to `UniqueIndex` before they reach this helper.
fn render_indexes_on_table(table: &str, specs: &[IndexSpec]) -> Vec<String> {
    specs.iter().map(|s| render_spec_on(table, s)).collect()
}

fn render_spec_on(table: &str, spec: &IndexSpec) -> String {
    let method = match spec.index_type {
        IndexType::BTree => "btree",
        IndexType::Gist => "gist",
        IndexType::Gin => "gin",
        IndexType::Hash => "hash",
        IndexType::Spgist => "spgist",
        IndexType::Brin => "brin",
    };
    let concurrently = if spec.requires_out_of_transaction {
        " CONCURRENTLY"
    } else {
        ""
    };
    let target_sql = match spec.target {
        IndexTarget::Columns(cols) => cols
            .iter()
            .map(render_column)
            .collect::<Vec<_>>()
            .join(", "),
        IndexTarget::Expression(expr) => format!("({expr})"),
    };
    let include_clause = if spec.include.is_empty() {
        String::new()
    } else {
        format!(" INCLUDE ({})", spec.include.join(", "))
    };
    let nnd_clause = if spec.nulls_not_distinct {
        " NULLS NOT DISTINCT"
    } else {
        ""
    };
    let where_clause = match spec.predicate {
        Some(p) => format!(" WHERE {p}"),
        None => String::new(),
    };

    match spec.kind {
        IndexKind::UniqueConstraint => {
            // UniqueConstraint path: ALTER TABLE … ADD CONSTRAINT … UNIQUE (cols)
            // [INCLUDE ...]. No CONCURRENTLY, no predicate, no NND — the
            // lowerer is responsible for escalating those shapes to
            // UniqueIndex before they arrive here; this helper enforces the
            // contract by panicking if any of those fields is set.
            assert!(
                spec.predicate.is_none(),
                "UniqueConstraint cannot carry a predicate (spec.name = {:?})",
                spec.name
            );
            assert!(
                !spec.nulls_not_distinct,
                "UniqueConstraint cannot carry NULLS NOT DISTINCT"
            );
            assert!(
                !spec.requires_out_of_transaction,
                "UniqueConstraint cannot be concurrent"
            );
            format!(
                "ALTER TABLE {table} ADD CONSTRAINT {name} UNIQUE ({target_sql}){include_clause};",
                name = spec.name,
            )
        }
        IndexKind::UniqueIndex | IndexKind::NonUnique => {
            let create_kw = if matches!(spec.kind, IndexKind::UniqueIndex) {
                "CREATE UNIQUE INDEX"
            } else {
                "CREATE INDEX"
            };
            format!(
                "{create_kw}{concurrently} {name} ON {table} USING {method} ({target_sql})\
                 {include_clause}{nnd_clause}{where_clause};",
                name = spec.name,
            )
        }
    }
}

fn render_column(c: &IndexColumnSpec) -> String {
    let mut parts = vec![c.name.to_string()];
    if let Some(op) = c.opclass {
        parts.push(op.to_string());
    }
    match c.order {
        IndexOrder::Asc => {}
        IndexOrder::Desc => parts.push("DESC".into()),
    }
    match c.nulls {
        IndexNullsOrder::Default => {}
        IndexNullsOrder::First => parts.push("NULLS FIRST".into()),
        IndexNullsOrder::Last => parts.push("NULLS LAST".into()),
    }
    parts.join(" ")
}

/// Provision the table and apply every declared index for `M`.
///
/// Returns the list of DDL statements that were executed so tests can
/// re-assert expected strings (useful for CONCURRENTLY / name-stem
/// checks that don't require a catalog probe).
async fn setup_schema_for<M: Model>(ctx: &mut djogi::DjogiContext) -> Vec<String> {
    let desc = M::descriptor();
    let create = render_create_table(desc);
    ctx.raw_ddl(&create).await.expect("CREATE TABLE");

    let ddl = render_indexes_on_table(desc.table_name, desc.indexes);
    // CREATE INDEX CONCURRENTLY cannot run inside an explicit transaction.
    // `raw_ddl` (via batch_execute) uses the simple-query protocol; a
    // pool-backed DjogiContext — which is what `#[djogi_test]` provides —
    // does not wrap individual statements in a transaction, so this is
    // safe. If a future harness change wraps tests in an implicit txn, the
    // concurrent cases will need `ctx.raw_ddl_outside_transaction` or
    // similar; flagged here to keep the coupling visible.
    for stmt in &ddl {
        ctx.raw_ddl(stmt)
            .await
            .unwrap_or_else(|e| panic!("apply index DDL failed: {stmt}\nerror: {e:?}"));
    }
    ddl
}

// ---------------------------------------------------------------------------
// pg_catalog probes — helpers for the live-DB assertions.
// ---------------------------------------------------------------------------

/// Read the pg_index row for an index by name. Returns
/// `(is_unique, is_primary, nulls_not_distinct, has_predicate, has_expr,
/// amname, indnatts, indnkeyatts)`.
#[allow(clippy::type_complexity)]
async fn read_pg_index(
    ctx: &mut djogi::DjogiContext,
    index_name: &str,
) -> (bool, bool, bool, bool, bool, String, i16, i16) {
    let rows = ctx
        .raw_rows(
            "SELECT \
                 i.indisunique, \
                 i.indisprimary, \
                 i.indnullsnotdistinct, \
                 (i.indpred IS NOT NULL)   AS has_pred, \
                 (i.indexprs IS NOT NULL)  AS has_expr, \
                 am.amname, \
                 i.indnatts, \
                 i.indnkeyatts \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_am    am ON am.oid = c.relam \
             WHERE c.relname = $1",
            &[&index_name],
        )
        .await
        .unwrap_or_else(|e| panic!("pg_index probe for {index_name} failed: {e:?}"));
    let row = rows.into_iter().next().expect("single pg_index row");
    (
        row.try_get::<_, bool>("indisunique").unwrap(),
        row.try_get::<_, bool>("indisprimary").unwrap(),
        row.try_get::<_, bool>("indnullsnotdistinct").unwrap(),
        row.try_get::<_, bool>("has_pred").unwrap(),
        row.try_get::<_, bool>("has_expr").unwrap(),
        row.try_get::<_, String>("amname").unwrap(),
        row.try_get::<_, i16>("indnatts").unwrap(),
        row.try_get::<_, i16>("indnkeyatts").unwrap(),
    )
}

/// Return the ordered column names the index points at (via `indkey`).
/// Expression-slot entries (indkey = 0) are rendered as the literal
/// `"(expr)"` so callers can distinguish them from real attributes.
async fn read_index_columns(ctx: &mut djogi::DjogiContext, index_name: &str) -> Vec<String> {
    let rows = ctx
        .raw_rows(
            "SELECT \
                 (ord - 1)::int AS pos, \
                 CASE WHEN i.indkey[ord - 1] = 0 THEN '(expr)' \
                      ELSE a.attname END AS col \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             CROSS JOIN LATERAL generate_series(1, i.indnatts) AS ord \
             LEFT JOIN pg_attribute a \
                    ON a.attrelid = i.indrelid \
                   AND a.attnum   = i.indkey[ord - 1] \
             WHERE c.relname = $1 \
             ORDER BY ord",
            &[&index_name],
        )
        .await
        .unwrap_or_else(|e| panic!("indkey probe for {index_name} failed: {e:?}"));
    rows.into_iter()
        .map(|r| r.try_get::<_, String>("col").unwrap())
        .collect()
}

/// Return `true` when a `pg_constraint` row with `contype = 'u'` exists
/// under the given name. UniqueConstraint indexes create one; UniqueIndex
/// indexes do not.
async fn has_unique_constraint(ctx: &mut djogi::DjogiContext, name: &str) -> bool {
    let rows = ctx
        .raw_rows(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1 AND contype = 'u') AS e",
            &[&name],
        )
        .await
        .expect("pg_constraint probe");
    rows.into_iter()
        .next()
        .expect("single row")
        .try_get::<_, bool>("e")
        .unwrap()
}

// ---------------------------------------------------------------------------
// Descriptor-only tests — no database needed.
// ---------------------------------------------------------------------------

// Each descriptor form gets its own `#[test]` so a regression localises
// to a single grammar form rather than short-circuiting the whole batch.
// See §5 of the plan for the grammar; the kind / name-stem / target
// discriminators are what the migration differ consumes.

#[test]
fn descriptor_composite_index() {
    let e = Event::descriptor();
    assert_eq!(e.indexes.len(), 1);
    assert!(matches!(e.indexes[0].kind, IndexKind::NonUnique));
    assert!(matches!(e.indexes[0].index_type, IndexType::BTree));
    assert!(e.indexes[0].name.ends_with("_idx"));
    let cols = match e.indexes[0].target {
        IndexTarget::Columns(c) => c,
        _ => panic!("expected Columns target"),
    };
    assert_eq!(
        cols.iter().map(|c| c.name).collect::<Vec<_>>(),
        vec!["tenant_id", "created_at_evt"]
    );
}

#[test]
fn descriptor_simple_unique_lowers_to_constraint() {
    let s = SimpleUnique::descriptor();
    assert_eq!(s.indexes.len(), 1);
    assert!(matches!(s.indexes[0].kind, IndexKind::UniqueConstraint));
    assert!(s.indexes[0].name.ends_with("_key"));
}

#[test]
fn descriptor_partial_unique_escalates_to_index() {
    let p = PartialUnique::descriptor();
    assert!(matches!(p.indexes[0].kind, IndexKind::UniqueIndex));
    assert!(p.indexes[0].name.ends_with("_uidx"));
    assert_eq!(p.indexes[0].predicate, Some("deleted_at IS NULL"));
}

#[test]
fn descriptor_nnd_unique_escalates_to_index() {
    let n = NndUnique::descriptor();
    assert!(matches!(n.indexes[0].kind, IndexKind::UniqueIndex));
    assert!(n.indexes[0].nulls_not_distinct);
    assert!(n.indexes[0].name.ends_with("_uidx"));
}

#[test]
fn descriptor_covering_index_carries_include_columns() {
    let c = Covering::descriptor();
    assert!(matches!(c.indexes[0].kind, IndexKind::NonUnique));
    assert_eq!(c.indexes[0].include, &["status", "priority"][..]);
}

#[test]
fn descriptor_expression_index_uses_expression_target() {
    let x = Expression::descriptor();
    assert!(matches!(x.indexes[0].kind, IndexKind::NonUnique));
    assert!(matches!(x.indexes[0].target, IndexTarget::Expression(_)));
    assert!(x.indexes[0].name.ends_with("_expr_idx"));
}

#[test]
fn descriptor_per_column_record_carries_order_and_opclass() {
    let pc = PerColumn::descriptor();
    let cols = match pc.indexes[0].target {
        IndexTarget::Columns(cs) => cs,
        _ => panic!("expected Columns target"),
    };
    assert_eq!(cols[0].name, "created_at_pc");
    assert!(matches!(cols[0].order, IndexOrder::Desc));
    assert!(matches!(cols[0].nulls, IndexNullsOrder::First));
    assert_eq!(cols[1].name, "status");
    assert_eq!(cols[1].opclass, Some("text_pattern_ops"));
}

/// Plan §6.2 — `unique(..., concurrently = true)` escalates kind to
/// `UniqueIndex` because `ALTER TABLE ADD CONSTRAINT UNIQUE` has no
/// `CONCURRENTLY` form. This is the contract the whole file is
/// anchored on.
#[test]
fn descriptor_unique_concurrent_escalates_to_unique_index() {
    let uc = UniqueConcurrent::descriptor();
    assert!(matches!(uc.indexes[0].kind, IndexKind::UniqueIndex));
    assert!(uc.indexes[0].requires_out_of_transaction);
    assert!(uc.indexes[0].name.ends_with("_uidx"));
}

/// Plan §6.4 item 4 — `unique(..., include = [...])` escalates because
/// `ADD CONSTRAINT` cannot express the covering payload.
#[test]
fn descriptor_unique_with_include_escalates_to_unique_index() {
    let ui = UniqueInclude::descriptor();
    assert!(matches!(ui.indexes[0].kind, IndexKind::UniqueIndex));
    assert_eq!(ui.indexes[0].include, &["display_name"][..]);
    assert!(ui.indexes[0].name.ends_with("_uidx"));
}

/// Plan §6.4 item 3 — `unique(expr = "...")` escalates because
/// `ADD CONSTRAINT` cannot express an expression target.
#[test]
fn descriptor_unique_expression_escalates_to_unique_index() {
    let ux = UniqueExpr::descriptor();
    assert!(matches!(ux.indexes[0].kind, IndexKind::UniqueIndex));
    assert!(matches!(ux.indexes[0].target, IndexTarget::Expression(_)));
    assert!(ux.indexes[0].name.ends_with("_expr_uidx"));
}

/// Regression guard — a `#[model]` with a `GeoPoint` field still emits
/// exactly one GiST `IndexSpec` on the geography column ().
/// This repeats the spatial.rs assertion here so a refactor that breaks the GiST auto-emission fails this file too.
#[cfg(feature = "spatial")]
#[test]
fn geopoint_still_emits_one_gist_index() {
    let desc = Place::descriptor();
    let gist: Vec<_> = desc
        .indexes
        .iter()
        .filter(|i| matches!(i.index_type, IndexType::Gist))
        .collect();
    assert_eq!(gist.len(), 1, "expected one GiST index on Place");
    assert_eq!(gist[0].extension_dependency, Some("postgis"));
    assert!(gist[0].requires_out_of_transaction);
}

// ---------------------------------------------------------------------------
// Live-PG tests — each spins up a per-test database, applies the table +
// indexes, and asserts pg_catalog matches the descriptor claim.
// ---------------------------------------------------------------------------

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn composite_index_preserves_column_order_in_pg_index(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<Event>(&mut ctx).await;
    let desc = Event::descriptor();
    let name = desc.indexes[0].name;

    let (is_unique, is_primary, nnd, has_pred, has_expr, am, natts, nkey) =
        read_pg_index(&mut ctx, name).await;
    assert!(!is_unique);
    assert!(!is_primary);
    assert!(!nnd);
    assert!(!has_pred);
    assert!(!has_expr);
    assert_eq!(am, "btree");
    assert_eq!(natts, 2);
    assert_eq!(nkey, 2);

    let cols = read_index_columns(&mut ctx, name).await;
    assert_eq!(
        cols,
        vec!["tenant_id".to_string(), "created_at_evt".to_string()]
    );
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn simple_unique_lands_as_pg_constraint(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<SimpleUnique>(&mut ctx).await;
    let desc = SimpleUnique::descriptor();
    let name = desc.indexes[0].name;

    // UniqueConstraint lands as both a pg_index row AND a pg_constraint row.
    let (is_unique, _, _, _, _, am, _, _) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique, "UniqueConstraint must set pg_index.indisunique");
    assert_eq!(am, "btree");
    assert!(
        has_unique_constraint(&mut ctx, name).await,
        "UniqueConstraint must create a pg_constraint row with contype='u'"
    );
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn partial_unique_is_index_not_constraint(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<PartialUnique>(&mut ctx).await;
    let desc = PartialUnique::descriptor();
    let name = desc.indexes[0].name;

    let (is_unique, _, _, has_pred, _, _, _, _) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique);
    assert!(has_pred, "partial unique must populate pg_index.indpred");
    assert!(
        !has_unique_constraint(&mut ctx, name).await,
        "UniqueIndex must NOT create a pg_constraint row"
    );
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn nulls_not_distinct_round_trips_through_pg_index(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<NndUnique>(&mut ctx).await;
    let desc = NndUnique::descriptor();
    let name = desc.indexes[0].name;

    let (is_unique, _, nnd, _, _, _, _, _) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique);
    assert!(nnd, "pg_index.indnullsnotdistinct must be true");
    assert!(!has_unique_constraint(&mut ctx, name).await);
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn covering_index_has_include_columns(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<Covering>(&mut ctx).await;
    let desc = Covering::descriptor();
    let name = desc.indexes[0].name;

    let (_, _, _, _, _, _, natts, nkey) = read_pg_index(&mut ctx, name).await;
    // One key column (created_at_c) + two INCLUDE columns (status, priority).
    assert_eq!(nkey, 1, "one key column");
    assert_eq!(natts, 3, "one key + two include = three total");

    // Column order round-trips — the key column first, then the INCLUDE
    // payload columns in declaration order. Assert on names so a reorder
    // bug in the emitter can't hide behind matching counts.
    let cols = read_index_columns(&mut ctx, name).await;
    assert_eq!(
        cols,
        vec![
            "created_at_c".to_string(),
            "status".to_string(),
            "priority".to_string(),
        ]
    );
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn expression_index_shows_indexprs(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<Expression>(&mut ctx).await;
    let desc = Expression::descriptor();
    let name = desc.indexes[0].name;

    let (_, _, _, _, has_expr, _, natts, _) = read_pg_index(&mut ctx, name).await;
    assert!(has_expr, "expression index must populate pg_index.indexprs");
    assert_eq!(natts, 1);

    // indkey entry for an expression is 0; our probe renders this as "(expr)".
    let cols = read_index_columns(&mut ctx, name).await;
    assert_eq!(cols, vec!["(expr)".to_string()]);
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn per_column_record_round_trips_desc_and_opclass(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<PerColumn>(&mut ctx).await;
    let desc = PerColumn::descriptor();
    let name = desc.indexes[0].name;

    // Column order preserved: created_at_pc first (DESC), status second.
    let cols = read_index_columns(&mut ctx, name).await;
    assert_eq!(
        cols,
        vec!["created_at_pc".to_string(), "status".to_string()]
    );

    // Column-0 option bit 0 == descending.
    let rows = ctx
        .raw_rows(
            "SELECT (i.indoption[0] & 1)::int AS desc_flag \
             FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = $1",
            &[&name],
        )
        .await
        .expect("indoption probe");
    let desc_flag: i32 = rows.into_iter().next().expect("single row").try_get("desc_flag").unwrap();
    assert_eq!(desc_flag, 1, "column 0 must be DESC");

    // Column 1 uses text_pattern_ops opclass.
    let rows = ctx
        .raw_rows(
            "SELECT op.opcname AS name \
             FROM pg_index i \
             JOIN pg_class c ON c.oid = i.indexrelid \
             JOIN pg_opclass op ON op.oid = i.indclass[1] \
             WHERE c.relname = $1",
            &[&name],
        )
        .await
        .expect("opclass probe");
    let opname: String = rows.into_iter().next().expect("single row").try_get("name").unwrap();
    assert_eq!(opname, "text_pattern_ops");
}

/// §6.2 round-trip — the one that this whole file is anchored on.
/// `unique(fields = [...], concurrently = true)` must land as a
/// UniqueIndex (no pg_constraint row) with a `_uidx` name stem.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn unique_concurrent_lands_as_unique_index_not_constraint(mut ctx: djogi::DjogiContext) {
    let ddl = setup_schema_for::<UniqueConcurrent>(&mut ctx).await;
    // DDL string must carry CONCURRENTLY and land on the CREATE UNIQUE
    // INDEX form, not ALTER TABLE.
    assert_eq!(ddl.len(), 1);
    assert!(
        ddl[0].contains("CREATE UNIQUE INDEX CONCURRENTLY"),
        "expected CREATE UNIQUE INDEX CONCURRENTLY, got: {}",
        ddl[0]
    );

    let desc = UniqueConcurrent::descriptor();
    let name = desc.indexes[0].name;
    assert!(
        name.ends_with("_uidx"),
        "name stem must be _uidx, got {name}"
    );

    let (is_unique, _, _, _, _, am, _, _) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique);
    assert_eq!(am, "btree");
    assert!(
        !has_unique_constraint(&mut ctx, name).await,
        "UniqueIndex (escalated from concurrent unique) must NOT create a pg_constraint row"
    );
}

/// §6.4 item 4 round-trip — `unique(fields, include = [...])` escalates
/// to UniqueIndex (no pg_constraint row) because the covering payload is
/// a unique-index-only feature.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn unique_with_include_lands_as_unique_index_not_constraint(mut ctx: djogi::DjogiContext) {
    let ddl = setup_schema_for::<UniqueInclude>(&mut ctx).await;
    assert!(
        ddl[0].contains("CREATE UNIQUE INDEX") && ddl[0].contains("INCLUDE (display_name)"),
        "expected CREATE UNIQUE INDEX with INCLUDE, got: {}",
        ddl[0]
    );

    let desc = UniqueInclude::descriptor();
    let name = desc.indexes[0].name;

    let (is_unique, _, _, _, _, _, natts, nkey) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique);
    assert_eq!(nkey, 1, "one key column (email)");
    assert_eq!(natts, 2, "one key + one INCLUDE = two total");
    assert!(
        !has_unique_constraint(&mut ctx, name).await,
        "UniqueIndex (escalated from unique+include) must NOT create a pg_constraint row"
    );
}

/// §6.4 item 3 round-trip — `unique(expr = "...")` escalates to
/// UniqueIndex because expression-target uniqueness cannot be carried by
/// an `ADD CONSTRAINT ... UNIQUE` statement.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: pg_catalog probes require raw SQL to inspect index metadata
#[djogi::djogi_test]
async fn unique_expression_lands_as_unique_index_not_constraint(mut ctx: djogi::DjogiContext) {
    let ddl = setup_schema_for::<UniqueExpr>(&mut ctx).await;
    assert!(
        ddl[0].contains("CREATE UNIQUE INDEX"),
        "expected CREATE UNIQUE INDEX on expression, got: {}",
        ddl[0]
    );

    let desc = UniqueExpr::descriptor();
    let name = desc.indexes[0].name;

    let (is_unique, _, _, _, has_expr, _, _, _) = read_pg_index(&mut ctx, name).await;
    assert!(is_unique);
    assert!(has_expr, "expression-target index must populate indexprs");
    assert!(
        !has_unique_constraint(&mut ctx, name).await,
        "UniqueIndex (escalated from unique+expr) must NOT create a pg_constraint row"
    );
}

/// GiST regression — a `#[model]` with a `GeoPoint` field still
/// lands a GiST index round-trip against a live PostGIS instance.
#[cfg(feature = "spatial")]
#[djogi::djogi_test(extensions = ["postgis"])]
async fn geopoint_gist_index_round_trips(mut ctx: djogi::DjogiContext) {
    setup_schema_for::<Place>(&mut ctx).await;
    let desc = Place::descriptor();
    let gist = desc
        .indexes
        .iter()
        .find(|i| matches!(i.index_type, IndexType::Gist))
        .expect("Place must have a GiST index");

    let (_, _, _, _, _, am, _, _) = read_pg_index(&mut ctx, gist.name).await;
    assert_eq!(am, "gist");
}
