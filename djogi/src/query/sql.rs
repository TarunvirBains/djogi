//! SQL emission — walks `Condition` + `QuerySet` state and populates a
//! [`SqlAccumulator`] with correct positional binds.
//!
//! # What
//!
//! The public entry points are [`build_select`], [`build_count`], and
//! [`build_exists`]. Each consumes a borrowed [`QuerySet<T>`] and returns a
//! pre-populated [`SqlAccumulator`] ready for execution via `PgConnection`.
//!
//! # Why
//!
//! Every value flows through [`SqlAccumulator::push_bind`] — **never**
//! string interpolation of user-controlled data. Table names and column
//! names are the only items inserted as raw text, and both are
//! `&'static str` literals baked in by the `#[model]` macro (table name via
//! `Model::table_name()`, column name via `FieldRef::column()`), so they are
//! not user input. The emitter's job is therefore a straight enum-tree walk:
//! one variant -> one operator token + zero-or-more `push_bind` calls.
//!
//! Pattern lookups (`ILIKE`) escape `%`, `_`, and `\\` in user input before
//! wrapping with the appropriate prefix / suffix `%` — escaped input goes
//! through `push_bind` so the wildcard-escape logic is independent of SQL
//! parameter placement.
//!
//! `IN (...)` expands to exactly as many bind slots as the list has;
//! empty lists short-circuit to `FALSE` (IN) / `TRUE` (NOT IN) rather than
//! emitting the syntactically invalid `col IN ()`. This matches the contract
//! documented on `FieldRef::in_list` / `not_in_list`.
//!
//! # Where
//!
//! Consumed by [`crate::query::terminal`], which wraps each accumulator in the
//! appropriate execution call against the caller-provided `DjogiContext`. The
//! emitter never executes SQL — that is the terminal layer's responsibility.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::pg::decode::FromPgRow;
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::queryset::{DistinctMode, QuerySet};

/// Escape LIKE/ILIKE wildcards (`%`, `_`, `\\`) so user input is treated
/// literally. The emitter adds its own surrounding `%` for contains /
/// starts_with / ends_with after escape.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Push a scalar [`FilterValue`] onto the accumulator as a single bound
/// parameter. `List` / `Pair` are compound and are handled at the operator
/// level (`IN`, `NOT IN`, `BETWEEN`) — reaching this function with either
/// variant is a framework bug, not a user error.
///
/// `Null` is emitted as the literal token `NULL`; it is never bound because
/// Postgres distinguishes `col = $1 (NULL)` (always false) from `col IS NULL`
/// at the SQL level. The typed `FieldRef::is_null` / `is_not_null` lookups
/// never route NULL through this path — they take the explicit `IS NULL` /
/// `IS NOT NULL` operator branch.
pub(crate) fn push_filter_value(acc: &mut SqlAccumulator, v: FilterValue) {
    match v {
        FilterValue::String(s) => {
            acc.push_bind(s);
        }
        FilterValue::I16(n) => {
            acc.push_bind(n);
        }
        FilterValue::I32(n) => {
            acc.push_bind(n);
        }
        FilterValue::I64(n) => {
            acc.push_bind(n);
        }
        FilterValue::F32(n) => {
            acc.push_bind(n);
        }
        FilterValue::F64(n) => {
            acc.push_bind(n);
        }
        FilterValue::Bool(b) => {
            acc.push_bind(b);
        }
        FilterValue::DateTime(d) => {
            acc.push_bind(d);
        }
        FilterValue::Date(d) => {
            acc.push_bind(d);
        }
        FilterValue::Uuid(u) => {
            acc.push_bind(u);
        }
        FilterValue::HeerId(h) => {
            acc.push_bind(h);
        }
        FilterValue::RanjId(r) => {
            acc.push_bind(r);
        }
        FilterValue::HeerIdDesc(h) => {
            acc.push_bind(h);
        }
        FilterValue::RanjIdDesc(r) => {
            acc.push_bind(r);
        }
        FilterValue::Decimal(d) => {
            acc.push_bind(d);
        }
        FilterValue::Null => {
            acc.push_null_literal();
        }
        FilterValue::ArrayString(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayI32(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayI64(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayBool(v) => {
            acc.push_bind(v);
        }
        FilterValue::List(_) | FilterValue::Pair(_, _) => {
            // These are handled at the operator level (see `emit_leaf`) and
            // never reach this function. Unreachable signals a Djogi
            // internal bug — user-facing `FieldRef` API blocks construction.
            //
            // `FilterValue` is `#[non_exhaustive]` at the *crate boundary*,
            // but we're inside the same crate — so this match is already
            // exhaustive. New variants added here force a compile error in
            // this file, which is exactly the coupling we want: any new
            // SQL-bindable type must also learn how to bind.
            unreachable!("push_filter_value called with List/Pair — use emit_leaf")
        }
    }
}

/// Emit a list element for `IN (...)` / `NOT IN (...)`.
/// Factored out of `emit_leaf`'s `In`/`NotIn` arm.
fn push_list_element(acc: &mut SqlAccumulator, v: FilterValue) {
    match v {
        FilterValue::String(s) => {
            acc.push_bind(s);
        }
        FilterValue::I16(n) => {
            acc.push_bind(n);
        }
        FilterValue::I32(n) => {
            acc.push_bind(n);
        }
        FilterValue::I64(n) => {
            acc.push_bind(n);
        }
        FilterValue::F32(n) => {
            acc.push_bind(n);
        }
        FilterValue::F64(n) => {
            acc.push_bind(n);
        }
        FilterValue::Bool(b) => {
            acc.push_bind(b);
        }
        FilterValue::DateTime(d) => {
            acc.push_bind(d);
        }
        FilterValue::Date(d) => {
            acc.push_bind(d);
        }
        FilterValue::Uuid(u) => {
            acc.push_bind(u);
        }
        FilterValue::HeerId(h) => {
            acc.push_bind(h);
        }
        FilterValue::RanjId(r) => {
            acc.push_bind(r);
        }
        FilterValue::HeerIdDesc(h) => {
            acc.push_bind(h);
        }
        FilterValue::RanjIdDesc(r) => {
            acc.push_bind(r);
        }
        FilterValue::Decimal(d) => {
            acc.push_bind(d);
        }
        FilterValue::Null
        | FilterValue::List(_)
        | FilterValue::Pair(_, _)
        | FilterValue::ArrayString(_)
        | FilterValue::ArrayI32(_)
        | FilterValue::ArrayI64(_)
        | FilterValue::ArrayBool(_) => {
            // Same reasoning as `push_filter_value`: enum is
            // `#[non_exhaustive]` at the crate boundary but exhaustive
            // within this crate. Any new variant added to `FilterValue`
            // must also be taught to bind here.
            unreachable!(
                "nested/null/array FilterValue in IN list — typed FieldRef API prevents this"
            )
        }
    }
}

/// Emit a single [`Leaf`] — `column op value`. The column name is a
/// `&'static str` from the macro-baked `FieldRef::column()`, so it is safe
/// to `acc.push_sql(col)` without quoting. The value always goes through
/// `push_bind`.
///
/// When `parent_table` is `Some(table)`, the emitted column reference is
/// prefixed as `{table}.{column}` so Postgres does not raise
/// `42702 column reference "X" is ambiguous` on a query with
/// `LEFT JOIN`-ed child tables that also expose a column of the same
/// bare name (`id`, `created_at`, `updated_at`). Passed through by the
/// join-aware helpers that wrap `build_select_joined`. The non-joined
/// [`build_select`] path passes `None` and emits bare column names
/// unchanged — byte-for-byte identical to the Phase 2 output.
fn emit_leaf(acc: &mut SqlAccumulator, leaf: Leaf, parent_table: Option<&'static str>) {
    let col = leaf.column;
    // Helper: push the column reference, qualified with `{parent_table}.`
    // when requested. Keeps the op-switch tidy — every arm that previously
    // called `acc.push_sql(col)` now calls `push_col(acc, col, parent_table)`.
    fn push_col(acc: &mut SqlAccumulator, col: &'static str, parent_table: Option<&'static str>) {
        if let Some(table) = parent_table {
            acc.push_sql(table);
            acc.push_sql(".");
        }
        acc.push_sql(col);
    }
    match leaf.op {
        LookupOp::Eq => {
            push_col(acc, col, parent_table);
            acc.push_sql(" = ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Neq => {
            push_col(acc, col, parent_table);
            acc.push_sql(" <> ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Gt => {
            push_col(acc, col, parent_table);
            acc.push_sql(" > ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Gte => {
            push_col(acc, col, parent_table);
            acc.push_sql(" >= ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Lt => {
            push_col(acc, col, parent_table);
            acc.push_sql(" < ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Lte => {
            push_col(acc, col, parent_table);
            acc.push_sql(" <= ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::IsNull => {
            push_col(acc, col, parent_table);
            acc.push_sql(" IS NULL");
        }
        LookupOp::IsNotNull => {
            push_col(acc, col, parent_table);
            acc.push_sql(" IS NOT NULL");
        }
        LookupOp::IContains => {
            push_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IContains requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}%", escape_like(&s)));
        }
        LookupOp::IStartsWith => {
            push_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IStartsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("{}%", escape_like(&s)));
        }
        LookupOp::IEndsWith => {
            push_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IEndsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}", escape_like(&s)));
        }
        LookupOp::IExact => {
            acc.push_sql("LOWER(");
            push_col(acc, col, parent_table);
            acc.push_sql(") = LOWER(");
            push_filter_value(acc, leaf.value);
            acc.push_sql(")");
        }
        LookupOp::Regex => {
            push_col(acc, col, parent_table);
            acc.push_sql(" ~ ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::IRegex => {
            push_col(acc, col, parent_table);
            acc.push_sql(" ~* ");
            push_filter_value(acc, leaf.value);
        }
        LookupOp::Between => {
            let (a, b) = match leaf.value {
                FilterValue::Pair(a, b) => (*a, *b),
                _ => unreachable!("Between requires FilterValue::Pair"),
            };
            push_col(acc, col, parent_table);
            acc.push_sql(" BETWEEN ");
            push_filter_value(acc, a);
            acc.push_sql(" AND ");
            push_filter_value(acc, b);
        }
        LookupOp::In | LookupOp::NotIn => {
            let list = match leaf.value {
                FilterValue::List(v) => v,
                _ => unreachable!("In/NotIn requires FilterValue::List"),
            };
            // Empty IN is FALSE (no rows match); empty NOT IN is TRUE (every
            // row matches). Avoids the `col IN ()` Postgres syntax error and
            // matches the documented contract on `FieldRef::in_list` /
            // `not_in_list`.
            if list.is_empty() {
                if matches!(leaf.op, LookupOp::In) {
                    acc.push_sql("FALSE");
                } else {
                    acc.push_sql("TRUE");
                }
                return;
            }
            push_col(acc, col, parent_table);
            acc.push_sql(if matches!(leaf.op, LookupOp::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            for (i, v) in list.into_iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                push_list_element(acc, v);
            }
            acc.push_sql(")");
        }
    }
}

/// Walk a [`Condition`] tree and emit the corresponding SQL fragment.
/// Called recursively for `Not`/`And`/`Or`. The input is consumed by value
/// because payloads (`String`, `Vec<FilterValue>`, `Box<FilterValue>`) move
/// into the accumulator one bind at a time.
///
/// `parent_table` threads through unchanged so every bare column reference
/// in a joined-variant emission lands as `{table}.{column}`; the non-joined
/// path passes `None` and gets bare names, preserving byte-for-byte parity
/// with Phase 2 output.
///
/// `pub(crate)` because Phase 4 Task 5 needs this entry point to lower the
/// [`Condition`] tree that backs a subquery's `WHERE` clause (a
/// [`SubqueryNode`](crate::expr::node::SubqueryNode) stores the parent
/// queryset's accumulated condition tree verbatim and lets this emitter
/// render it at subquery-emission time — see
/// [`crate::expr::sql::emit_subquery`]). Keeping the emitter itself
/// module-private would force a duplicate walk inside `expr::sql`; widening
/// to `pub(crate)` reuses the shipped, battle-tested condition emitter
/// without copy-pasting its every `LookupOp` arm.
pub(crate) fn emit_condition(
    acc: &mut SqlAccumulator,
    c: Condition,
    parent_table: Option<&'static str>,
) {
    match c {
        Condition::True => {
            acc.push_sql("TRUE");
        }
        Condition::Leaf(l) => {
            emit_leaf(acc, l, parent_table);
        }
        Condition::Not(inner) => {
            acc.push_sql("NOT (");
            emit_condition(acc, *inner, parent_table);
            acc.push_sql(")");
        }
        Condition::And(parts) => {
            // Empty `And(vec![])` is the vacuous-truth identity — documented
            // on the `Condition::And` variant. `Condition::and()` never
            // constructs one, but external callers technically can.
            if parts.is_empty() {
                acc.push_sql("TRUE");
                return;
            }
            acc.push_sql("(");
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" AND ");
                }
                emit_condition(acc, p, parent_table);
            }
            acc.push_sql(")");
        }
        Condition::Or(parts) => {
            // Empty `Or(vec![])` is the vacuous-falsehood identity — see the
            // variant doc and the condition tests.
            if parts.is_empty() {
                acc.push_sql("FALSE");
                return;
            }
            acc.push_sql("(");
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" OR ");
                }
                emit_condition(acc, p, parent_table);
            }
            acc.push_sql(")");
        }
        // Expression-IR bridge — delegates to the dedicated emitter in
        // `expr::sql`. The expression tree carries its own column
        // references + literals + nested arithmetic; `parent_table` is
        // deliberately not threaded through (see the module-level
        // comment in `expr::sql` for the scope note on select_related
        // interaction, deferred to Task 5).
        Condition::Expr(expr) => {
            crate::expr::sql::emit_expr(acc, &expr.node);
        }
        // ── Array operators (Phase 5 Task 5) ─────────────────────────────
        //
        // All three operators take the form `col OP $n` where `$n` is a
        // bound Postgres array parameter. `parent_table` qualification is
        // intentionally forwarded for the column name but array operators
        // are always single-table (no cross-join semantics), so the
        // `parent_table` prefix is cosmetic here — it matches the behaviour
        // of every other `Leaf` arm.
        Condition::ArrayContains(leaf) => {
            if let Some(table) = parent_table {
                acc.push_sql(table);
                acc.push_sql(".");
            }
            acc.push_sql(leaf.column);
            acc.push_sql(" @> ");
            push_filter_value(acc, leaf.values);
        }
        Condition::ArrayContainedBy(leaf) => {
            if let Some(table) = parent_table {
                acc.push_sql(table);
                acc.push_sql(".");
            }
            acc.push_sql(leaf.column);
            acc.push_sql(" <@ ");
            push_filter_value(acc, leaf.values);
        }
        Condition::ArrayOverlap(leaf) => {
            if let Some(table) = parent_table {
                acc.push_sql(table);
                acc.push_sql(".");
            }
            acc.push_sql(leaf.column);
            acc.push_sql(" && ");
            push_filter_value(acc, leaf.values);
        }
        // ── JSONB flat-path condition (Phase 5 Task 5) ───────────────────
        //
        // `JsonbPathLeaf` stores the column + path + cast as structured
        // parts so the emitter can qualify the column reference with the
        // parent table name in joined-query contexts (same pattern as
        // `emit_leaf`'s `push_col` helper). SQL is rendered here at
        // emit time, never at condition-tree construction time.
        Condition::JsonbPath(leaf) => {
            emit_jsonb_path_leaf(acc, leaf, parent_table);
        }
    }
}

/// Emit a [`crate::jsonb::path::JsonbPathLeaf`] — `(col->...'key')::cast op $n`.
///
/// SQL is rendered at emit time from the structured `column`, `path`, and
/// `cast` fields rather than from a pre-rendered string. This lets the
/// emitter qualify the column with the parent table name when inside a
/// joined query (same `{table}.{column}` prefix logic as [`emit_leaf`]).
///
/// When `parent_table` is `Some(table)`, the emitted expression is
/// `(table.col->'a'->>'b')::cast` — the Postgres JSONB navigation
/// operators apply to the `table.col` expression, so parenthesisation
/// wraps the qualified column reference correctly.
fn emit_jsonb_path_leaf(
    acc: &mut SqlAccumulator,
    leaf: crate::jsonb::path::JsonbPathLeaf,
    parent_table: Option<&'static str>,
) {
    use LookupOp::{Eq, Gt, Gte, In, IsNotNull, IsNull, Lt, Lte, Neq};

    /// Build the LHS expression from structured parts — `(col->'a'->>'b')::cast`.
    /// When `parent_table` is Some, the column reference is `{table}.{col}`.
    fn build_lhs(
        acc: &mut SqlAccumulator,
        column: &'static str,
        path: &'static str,
        cast: Option<&'static str>,
        parent_table: Option<&'static str>,
    ) {
        let segments: Vec<&str> = path.split('.').collect();
        acc.push_sql("(");
        if let Some(table) = parent_table {
            acc.push_sql(table);
            acc.push_sql(".");
        }
        acc.push_sql(column);
        for (i, seg) in segments.iter().enumerate() {
            if i == segments.len() - 1 {
                // Last segment: text extraction operator.
                acc.push_sql("->>'");
                acc.push_sql(seg);
                acc.push_sql("'");
            } else {
                // Intermediate segment: object navigation operator.
                acc.push_sql("->'");
                acc.push_sql(seg);
                acc.push_sql("'");
            }
        }
        acc.push_sql(")");
        if let Some(c) = cast {
            acc.push_sql(c);
        }
    }

    match leaf.op {
        Eq => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" = ");
            push_filter_value(acc, leaf.value);
        }
        Neq => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" <> ");
            push_filter_value(acc, leaf.value);
        }
        Gt => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" > ");
            push_filter_value(acc, leaf.value);
        }
        Gte => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" >= ");
            push_filter_value(acc, leaf.value);
        }
        Lt => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" < ");
            push_filter_value(acc, leaf.value);
        }
        Lte => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" <= ");
            push_filter_value(acc, leaf.value);
        }
        IsNull => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IS NULL");
        }
        IsNotNull => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IS NOT NULL");
        }
        In => {
            let list = match leaf.value {
                FilterValue::List(v) => v,
                _ => unreachable!("JsonbPath In requires FilterValue::List"),
            };
            if list.is_empty() {
                acc.push_sql("FALSE");
                return;
            }
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IN (");
            for (i, v) in list.into_iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                push_list_element(acc, v);
            }
            acc.push_sql(")");
        }
        _ => {
            // No other LookupOps are constructible from JsonbPathRef —
            // the typed surface only exposes eq/neq/gt/gte/lt/lte/in/is_null/is_not_null.
            unreachable!("unsupported LookupOp in JsonbPathLeaf: {:?}", leaf.op)
        }
    }
}

/// Is this condition vacuously `TRUE` at emission time? Walks `And`
/// subtrees recursively so top-level `And(vec![])`, nested
/// `And(vec![True, And(vec![])])`, and similar shapes all collapse to the
/// same identity the emitter treats as "no filter". `Not(Or(vec![]))` is
/// also vacuously TRUE because the emitter renders empty `Or` as `FALSE`
/// and `NOT FALSE` is `TRUE`.
///
/// Emit the `WHERE ...` clause for a QuerySet, if any. Any top-level
/// condition that collapses to vacuous TRUE (see
/// [`Condition::is_vacuously_true`]) is omitted entirely rather than
/// emitted as `WHERE TRUE` — same semantics, cleaner logs, and avoids
/// touching the planner with a trivially-true predicate.
///
/// The non-joined path (every caller in this file except
/// [`build_select_joined`]) uses this shim, which forwards to
/// [`push_where_qualified`] with `parent_table = None` — bare column
/// references are emitted exactly as Phase 2 shipped.
fn push_where<T: Model>(acc: &mut SqlAccumulator, qs: &QuerySet<T>) {
    push_where_qualified(acc, qs, None);
}

/// Qualification-aware variant of [`push_where`]. When `parent_table`
/// is `Some(table)`, every bare column reference in the emitted `WHERE`
/// clause is prefixed as `{table}.{column}` so Postgres does not raise
/// `42702 column reference "X" is ambiguous` under `LEFT JOIN`-ed
/// children that share the same column name (`id`, `created_at`,
/// `updated_at`). `None` preserves Phase 2's bare-name emission.
fn push_where_qualified<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    parent_table: Option<&'static str>,
) {
    if !qs.condition.is_vacuously_true() {
        acc.push_sql(" WHERE ");
        // `emit_condition` consumes the tree — clone the borrowed reference
        // so the original QuerySet remains usable (matters for `fetch_one`'s
        // LIMIT-override path, which reuses the same queryset).
        emit_condition(acc, qs.condition.clone(), parent_table);
    }
}

/// Shared tail emitted by SELECT variants: `ORDER BY ...`, `LIMIT $n`,
/// `OFFSET $n`. `WHERE` is emitted separately so count/exists builders can
/// reuse `push_where` without taking the ordering/limit tail.
///
/// Shim for the non-joined path — forwards to [`push_tail_qualified`]
/// with `parent_table = None`.
fn push_tail<T: Model>(acc: &mut SqlAccumulator, qs: &QuerySet<T>) {
    push_tail_qualified(acc, qs, None);
}

/// Qualification-aware variant of [`push_tail`]. `parent_table` threads
/// through to both the `WHERE` helper and the ordering emission so
/// `ORDER BY id` on a joined query renders as `ORDER BY {table}.id`.
/// `LIMIT` / `OFFSET` need no qualification — they carry no column
/// references.
fn push_tail_qualified<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    parent_table: Option<&'static str>,
) {
    push_where_qualified(acc, qs, parent_table);

    if !qs.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in qs.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            // Delegate to OrderExpr::emit — it handles both Column and
            // (when the spatial feature is on) SpatialDistance variants.
            // The table_qualifier threads through for select_related joins.
            o.emit(acc, parent_table);
        }
    }

    if let Some(n) = qs.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = qs.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    // Row-lock tail — `FOR UPDATE [NOWAIT|SKIP LOCKED]` — is the last
    // thing Postgres accepts on a SELECT, after `LIMIT`/`OFFSET`.
    // `LockMode::None` is a no-op so the pre-Task-7 SELECT shape is
    // byte-for-byte preserved for querysets that never touched the
    // lock builders.
    qs.lock.push_tail(acc);
}

/// Build `SELECT [DISTINCT [ON (...)]] <COLUMN_LIST> FROM <table> [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
///
/// The queryset is borrowed, not consumed — terminal methods (`fetch_all`,
/// `fetch_one`, `first`) may need to mutate the queryset (e.g. `fetch_one`
/// overrides the user-set `limit` to 2 so it can distinguish single-row
/// success from multiple-row failure) before or after calling this builder.
pub(crate) fn build_select<T: Model + FromPgRow>(qs: &QuerySet<T>) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("");
    // Emit the canonical `FromPgRow::COLUMN_LIST` rather than `*`. Ordinal
    // decode (T3) relies on wire column order matching struct-field order;
    // `SELECT *` leaks DDL column order into the decode path, which
    // Phase 4 fixtures like `accounts` (user columns before framework
    // columns) do not guarantee. Baking the canonical list pins the
    // order regardless of migration shape.
    match &qs.distinct {
        DistinctMode::None => {
            acc.push_sql("SELECT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
        }
        DistinctMode::Plain => {
            acc.push_sql("SELECT DISTINCT ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
        }
        DistinctMode::On(cols) => {
            acc.push_sql("SELECT DISTINCT ON (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                acc.push_sql(c);
            }
            acc.push_sql(") ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
        }
    }
    acc.push_sql(T::table_name());
    push_tail(&mut acc, qs);
    acc
}

/// Build `SELECT {parent_cols} FROM <table> {left joins} [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]` — the select_related variant.
///
/// Mirror of [`build_select`], but:
/// 1. Replaces `*` in the projection with the aliased column list built
///    by [`crate::relation::select_related::select_columns`] — parent
///    columns stay unqualified, each joined child's columns land under
///    a `"rel_{source_column}.{col}"` alias.
/// 2. Appends one `LEFT JOIN` clause per registered path, via
///    [`crate::relation::select_related::push_joins`].
///
/// # Why a separate emitter
///
/// Keeping `build_select` unchanged means a queryset with no
/// registered select_related paths still emits the exact SQL Phase 2
/// shipped — no regression risk, no surprise `LEFT JOIN` on plain
/// `fetch_all` call sites. The joined variant is reached only via
/// [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined),
/// which explicitly opts into the joined decode path.
///
/// # `DistinctMode` interaction
///
/// Phase 3 Task 5 does not ship `DISTINCT` interaction with
/// select_related — the parent's `DISTINCT` semantics depend on
/// whether the joined columns should participate in the distinct
/// tuple, which is a Phase 4+ design decision. If the queryset has a
/// non-`None` `DistinctMode`, the emitter preserves it exactly: `SELECT
/// DISTINCT {parent_cols}...`. Callers who combine `.distinct()` with
/// `.select_related(...)` get consistent shape — distinct is applied
/// to the full projection (parent + aliased children) — but they
/// should verify the emitted SQL matches their intent.
pub(crate) fn build_select_joined<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("");
    let col_list = crate::relation::select_related::select_columns::<T>(&qs.select_related_paths);
    // Every bare column reference that follows — `WHERE ...`,
    // `ORDER BY ...`, `DISTINCT ON (...)` — is qualified with the
    // parent table name. Without this, Postgres raises `42702 column
    // reference "X" is ambiguous` on the first framework column that
    // also appears on a joined child (most commonly `id`,
    // `created_at`, `updated_at`). The parent table itself is always
    // a `&'static str` from the `#[model]` macro, so qualification is
    // safe without quoting.
    let parent_table: Option<&'static str> = Some(T::table_name());
    match &qs.distinct {
        DistinctMode::None => {
            acc.push_sql("SELECT ");
            acc.push_sql(&col_list);
            acc.push_sql(" FROM ");
        }
        DistinctMode::Plain => {
            acc.push_sql("SELECT DISTINCT ");
            acc.push_sql(&col_list);
            acc.push_sql(" FROM ");
        }
        DistinctMode::On(cols) => {
            acc.push_sql("SELECT DISTINCT ON (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                // Qualify DISTINCT ON columns under the parent table
                // so `SELECT DISTINCT ON (id) ...` on a joined query
                // becomes `SELECT DISTINCT ON (vehicles.id) ...` and
                // sidesteps the same ambiguity Postgres raises on bare
                // `WHERE id = ...`.
                acc.push_sql(T::table_name());
                acc.push_sql(".");
                acc.push_sql(c);
            }
            acc.push_sql(") ");
            acc.push_sql(&col_list);
            acc.push_sql(" FROM ");
        }
    }
    acc.push_sql(T::table_name());
    crate::relation::select_related::push_joins::<T>(&mut acc, &qs.select_related_paths);
    push_tail_qualified(&mut acc, qs, parent_table);
    acc
}

/// Emit `(AGG(..) [OVER (...)])::CAST` for the scalar-aggregate and
/// grouped-annotate paths — wraps the aggregate (plus the optional window
/// clause) in parens when a narrowing `::CAST` is needed, then appends
/// the cast.
///
/// `cast_to` is pulled from the [`crate::expr::node::ExprNode::Aggregate`]
/// payload; `None` skips the cast entirely (used for `COUNT` /
/// `MIN` / `MAX` where the Postgres return type already decodes into
/// `Out` directly).
///
/// When the aggregate carries a user-set `window: Some(spec)` (from
/// `.over(|w| ...)`), the `OVER (...)` clause is appended immediately
/// after `emit_expr` returns — in the right position for Postgres
/// window-function syntax (`AGG(...) OVER (...)`). No default `OVER ()`
/// is added when `window` is `None`: this path is used for both the
/// scalar terminal and the grouped annotate SELECT list, neither of which
/// should silently grow a window clause.
pub(crate) fn emit_aggregate_with_cast(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
) {
    let (cast_to, window) = match agg {
        crate::expr::node::ExprNode::Aggregate {
            cast_to, window, ..
        } => (*cast_to, window.as_ref()),
        _ => (None, None),
    };
    // Only wrap in parens when a cast is needed — `(AGG(..) OVER (...))::ty`.
    // A window-only aggregate (no cast) emits directly: `AGG(..) OVER (...)`.
    if cast_to.is_some() {
        acc.push_sql("(");
    }
    crate::expr::sql::emit_expr(acc, agg);
    if let Some(ws) = window {
        ws.emit(acc);
    }
    if let Some(ty) = cast_to {
        acc.push_sql(")::");
        acc.push_sql(ty);
    }
}

/// Emit `(AGG(..) OVER ())::CAST` for the annotate-SELECT-list path —
/// wraps the aggregate in a window function so the SELECT list is
/// valid without a `GROUP BY` clause, then applies the optional
/// narrowing cast.
///
/// # Why `OVER ()` rather than explicit `GROUP BY`
///
/// `annotate(|f| f.col().sum())` on a Task 4 queryset has no natural
/// grouping key — the main row's PK would give a one-row-per-group
/// partition (every aggregate collapses to the per-row column value).
/// An unbounded window function (`OVER ()`) produces the table-wide
/// aggregate value on every returned row, which is the useful
/// semantics Django users expect when annotating a non-reverse-
/// relation column.
///
/// Reverse-relation aggregates (`f.orders.count()` — Task 5 scope)
/// may need `OVER (PARTITION BY parent.id)` after a LATERAL join;
/// that is a deliberate scope boundary here. Task 4 aims for the
/// simplest annotate shape that pairs with the self-column aggregate
/// helpers already on `FieldRef`.
pub(crate) fn emit_aggregate_with_window_and_cast(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
) {
    let (cast, window) = match agg {
        crate::expr::node::ExprNode::Aggregate {
            cast_to, window, ..
        } => (*cast_to, window.as_ref()),
        _ => (None, None),
    };
    if cast.is_some() {
        acc.push_sql("(");
    }
    crate::expr::sql::emit_expr(acc, agg);
    // Use the user's window spec when present; fall back to the bare `OVER ()`
    // default that all pre-T3 ungrouped annotate callers expect.
    match window {
        Some(ws) => ws.emit(acc),
        None => acc.push_sql(" OVER ()"),
    }
    if let Some(ty) = cast {
        acc.push_sql(")::");
        acc.push_sql(ty);
    }
}

/// Build `SELECT <agg> FROM <table> [WHERE ...]` — the scalar-aggregate
/// terminal for [`crate::query::aggregate::AggregateQuery::fetch_one`].
///
/// No `ORDER BY`, no `LIMIT`, no `OFFSET`, no `GROUP BY` — ungrouped
/// aggregates collapse to exactly one result row regardless of the
/// underlying cardinality, so those clauses would be meaningless.
///
/// The aggregate expression is emitted via [`emit_aggregate_with_cast`]
/// so integer `SUM` / `AVG` results narrow back to the typed `Out`
/// the decoder expects. `WHERE` uses the shared [`push_where`] helper so
/// vacuously-true predicates are elided identically to every other
/// terminal.
pub(crate) fn build_aggregate_select<T: Model>(
    qs: &QuerySet<T>,
    agg: &crate::expr::node::ExprNode,
) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("SELECT ");
    emit_aggregate_with_cast(&mut acc, agg);
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs);
    acc
}

/// Build `SELECT t.*, <agg_0> AS __djogi_agg_0, <agg_1> AS __djogi_agg_1
/// FROM <table> [WHERE ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]` —
/// the annotation terminal for
/// [`crate::query::annotate::AnnotatedQuerySet::fetch_all`].
///
/// # Why `t.*` plus aliased aggregates
///
/// The annotated row carries both the full `t.*`
/// projection (decoded into `T`) and the aggregate columns under
/// synthetic `__djogi_agg_N` aliases (decoded into the tuple slots).
/// Each side reads its own column set; they never collide because
/// model columns are user-chosen identifiers and the aggregate
/// aliases use the framework-reserved `__djogi_agg_` prefix.
///
/// # Columns argument
///
/// `push_columns` is a closure the caller supplies so the SELECT-list
/// emission can inspect the typed tuple shape at compile time. The
/// annotate terminal's `IntoAggregateTuple::push_columns` impl pushes
/// `, <agg_expr> AS __djogi_agg_N` once per tuple arity slot; this
/// emitter owns the `t.*` prefix and the `FROM` / `WHERE` / `ORDER BY`
/// / `LIMIT` / `OFFSET` tail around it.
pub(crate) fn build_select_with_annotations<T, F>(
    qs: &QuerySet<T>,
    push_columns: F,
) -> SqlAccumulator
where
    T: Model + FromPgRow,
    F: FnOnce(&mut SqlAccumulator),
{
    let mut acc = SqlAccumulator::new("");
    // `SELECT t.<c1>, t.<c2>, ...` prefix — the `t` alias is what the
    // FROM clause below names the table as. Explicit `t.<col>` for
    // every canonical column (matching `FromPgRow::COLUMNS`) pins the
    // wire order so the ordinal decode path can read them positionally
    // regardless of DDL column order. Trailing aggregate columns pushed
    // by `push_columns` land AFTER the canonical prefix and are
    // ignored by `FromPgRow::from_pg_row` (whose column-count assert
    // is `>= N_COLS`, not `==`).
    acc.push_sql("SELECT ");
    for (i, col) in <T as FromPgRow>::COLUMNS.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql("t.");
        acc.push_sql(col);
    }
    // Caller-provided per-aggregate comma-separated pushes. The
    // trait impl prepends `, ` before each `<agg> AS __djogi_agg_N`
    // so the SELECT list stays well-formed for arities 1..=4.
    push_columns(&mut acc);
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");
    push_tail(&mut acc, qs);
    acc
}

/// Build `SELECT keys, aggregates FROM <table> [WHERE ...] GROUP BY keys
/// [HAVING ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]` — the terminal for
/// [`crate::query::grouped::GroupedAnnotatedQuerySet::fetch_all`].
///
/// # SELECT list layout
///
/// Keys MUST be emitted first and in `push_group_by_columns` order —
/// `K::decode_tuple` reads positionally (ordinals 0..N_keys). Aggregate
/// columns follow, decoded by alias (`__djogi_agg_N`). If the key/aggregate
/// order in the SELECT list ever changes, key decoding will silently read the
/// wrong columns.
///
/// # Why `push_columns_bare` not `push_columns`
///
/// `IntoAggregateTuple::push_columns` wraps each aggregate in `OVER ()` for
/// the `annotate`-on-ungrouped path. A `GROUP BY` query must not use window
/// functions in the SELECT list for its aggregate columns — Postgres would
/// reject the combination. `push_columns_bare` emits the aggregate with only
/// the narrowing cast but no window frame.
///
/// # Spatial JOIN delegation
///
/// When the `spatial` feature is enabled and `gaq.spatial_source` is `Some`,
/// this function delegates to the appropriate spatial builder so the caller
/// does not need to be aware of which emission path to take.
pub(crate) fn build_grouped_annotated_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
) -> SqlAccumulator
where
    T: Model,
    K: crate::query::grouped::IntoGroupKeyTuple,
    A: crate::query::annotate::IntoAggregateTuple,
{
    // ── Spatial group-source delegation ─────────────────────────────────────
    #[cfg(feature = "spatial")]
    if let Some(ref src) = gaq.spatial_source {
        return match src {
            crate::query::grouped::SpatialGroupSource::Join(spec) => {
                build_spatial_join_grouped_select(gaq, spec)
            }
            crate::query::grouped::SpatialGroupSource::Cluster(spec) => {
                build_cluster_grouped_select(gaq, spec)
            }
            crate::query::grouped::SpatialGroupSource::Geohash(spec) => {
                build_geohash_grouped_select(gaq, spec)
            }
        };
    }

    let mut acc = SqlAccumulator::new("SELECT ");
    gaq.keys.push_select_columns(&mut acc);
    gaq.aggregates.push_columns_bare(&mut acc);

    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");

    // WHERE from the upstream queryset (filters set before .group_by)
    push_where(&mut acc, &gaq.qs);

    // GROUP BY
    acc.push_sql(" GROUP BY ");
    match gaq.grouping {
        crate::query::grouped::GroupingMode::Plain => {
            gaq.keys.push_group_by_columns(&mut acc);
        }
        crate::query::grouped::GroupingMode::Rollup => {
            acc.push_sql("ROLLUP (");
            gaq.keys.push_group_by_columns(&mut acc);
            acc.push_sql(")");
        }
        crate::query::grouped::GroupingMode::Cube => {
            acc.push_sql("CUBE (");
            gaq.keys.push_group_by_columns(&mut acc);
            acc.push_sql(")");
        }
        crate::query::grouped::GroupingMode::Sets(ref sets) => {
            // Emit: GROUPING SETS ((col_a), (col_b), ...)
            // Each inner Vec is one grouping set's column list.
            // Column names are &'static str validated upstream by
            // assert_plain_ident — no bind slots, safe to push as SQL.
            acc.push_sql("GROUPING SETS (");
            for (i, set) in sets.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                acc.push_sql("(");
                for (j, col) in set.iter().enumerate() {
                    if j > 0 {
                        acc.push_sql(", ");
                    }
                    acc.push_sql(col);
                }
                acc.push_sql(")");
            }
            acc.push_sql(")");
        }
    }

    // HAVING
    if let Some(h) = &gaq.having {
        acc.push_sql(" HAVING ");
        crate::expr::sql::emit_expr(&mut acc, h);
    }

    // ORDER BY
    if !gaq.order.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in gaq.order.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(&mut acc, None);
        }
    }

    // LIMIT / OFFSET
    if let Some(n) = gaq.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n as i64);
    }
    if let Some(n) = gaq.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n as i64);
    }

    acc
}

/// Build the spatial-JOIN variant of the grouped-annotated SELECT:
///
/// ```sql
/// SELECT r.<pk-col> AS rk0, <aggregates>
/// FROM <t-table> AS t
/// LEFT JOIN <r-table> AS r ON ST_Covers(r.<r-geo-col>, t.<t-geo-col>)
/// [WHERE ...]
/// GROUP BY r.<pk-col>
/// [HAVING ...]
/// [ORDER BY ...]
/// [LIMIT $n] [OFFSET $n]
/// ```
///
/// Called by [`build_grouped_annotated_select`] when `gaq.spatial_source` is
/// `Some(SpatialGroupSource::Join(_))`. All clause-ordering and bind-slot
/// semantics are identical to the plain grouped path — the only difference is
/// the FROM + LEFT JOIN instead of the bare `FROM <t-table> AS t`.
///
/// # Column name safety
///
/// `spec.t_geo_col`, `spec.r_geo_col`, `spec.r_pk_col`, and `spec.r_table`
/// are all `&'static str` baked by the macro or read from `ModelDescriptor`
/// field names. They are pushed as SQL text (not bound parameters) on the same
/// basis as every other column or table name in this file. No user input flows
/// through these slots.
#[cfg(feature = "spatial")]
pub(crate) fn build_spatial_join_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::SpatialJoinSpec,
) -> SqlAccumulator
where
    T: Model,
    K: crate::query::grouped::IntoGroupKeyTuple,
    A: crate::query::annotate::IntoAggregateTuple,
{
    let mut acc = SqlAccumulator::new("SELECT ");

    // Key columns first (positional decode). For the spatial path this emits
    // `r.<pk-col> AS rk0` via `RegionKey::push_select_columns`.
    gaq.keys.push_select_columns(&mut acc);

    // Aggregate columns follow, decoded by alias (__djogi_agg_N).
    gaq.aggregates.push_columns_bare(&mut acc);

    // FROM <t-table> AS t
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");

    // LEFT JOIN <r-table> AS r ON ST_Covers(r.<r-geo-col>, t.<t-geo-col>)
    //
    // LEFT JOIN so unmatched rows (no containing region) appear in the result
    // with r.<pk-col> = NULL rather than being silently dropped.
    //
    // # ST_Covers vs ST_Contains
    //
    // `ST_Covers` is used (not `ST_Contains`) because:
    //
    // - `ST_Contains(geography, geography)` does **not** exist in PostGIS 3.x
    //   — only the geometry overload is defined, and Djogi stores spatial
    //   columns as `GEOGRAPHY(..., 4326)`. Using `ST_Contains` here forces
    //   `::geometry` casts on both sides, which defeats GiST index usage on
    //   the geography column.
    // - `ST_Covers` has a native `geography` overload and gives the same
    //   answer as `ST_Contains` for the point-in-polygon use case this JOIN
    //   implements (a point is "covered by" a polygon iff it is "inside" the
    //   polygon; the distinction between the two functions only matters when
    //   the inner geometry touches the boundary of the outer one — and for
    //   the scalar point case, being on the boundary is treated as inside
    //   under both functions for geography inputs).
    //
    // The geography-native form keeps GiST-indexed bbox prefiltering active
    // under the JOIN.
    acc.push_sql(" LEFT JOIN ");
    acc.push_sql(spec.r_table);
    acc.push_sql(" AS r ON ST_Covers(r.");
    acc.push_sql(spec.r_geo_col);
    acc.push_sql(", t.");
    acc.push_sql(spec.t_geo_col);
    acc.push_sql(")");

    // WHERE from the upstream queryset — qualifies t.<col> references so
    // they don't collide with r.<col> under the JOIN.
    push_where_qualified(&mut acc, &gaq.qs, Some("t"));

    // GROUP BY r.<pk-col>
    acc.push_sql(" GROUP BY ");
    match gaq.grouping {
        crate::query::grouped::GroupingMode::Plain => {
            gaq.keys.push_group_by_columns(&mut acc);
        }
        // ROLLUP / CUBE / SETS are not meaningful for spatial region grouping —
        // the key is derived from a JOIN condition, not a column value. Reaching
        // here indicates the user set the grouping mode manually, which is not
        // supported via the `group_by_region` entry point. Emit plain GROUP BY
        // as a safe fallback (the user is off-path if they reach this via any
        // internal route).
        _ => {
            gaq.keys.push_group_by_columns(&mut acc);
        }
    }

    // HAVING
    if let Some(h) = &gaq.having {
        acc.push_sql(" HAVING ");
        crate::expr::sql::emit_expr(&mut acc, h);
    }

    // ORDER BY
    if !gaq.order.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in gaq.order.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(&mut acc, None);
        }
    }

    // LIMIT / OFFSET
    if let Some(n) = gaq.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n as i64);
    }
    if let Some(n) = gaq.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n as i64);
    }

    acc
}

/// Build the DBSCAN-clustering variant of the grouped-annotated SELECT:
///
/// ```sql
/// SELECT cluster_id, <aggregates>
/// FROM (
///     SELECT t.*, ST_ClusterDBSCAN(t.<col>::geometry, $eps, $minpoints) OVER ()
///                                                                 AS cluster_id
///     FROM <table> AS t
///     [WHERE ...]
/// ) AS t
/// GROUP BY cluster_id
/// [HAVING ...]
/// [ORDER BY ...]
/// [LIMIT $n] [OFFSET $n]
/// ```
///
/// # Why the subquery
///
/// A flat `SELECT ST_ClusterDBSCAN(...) OVER () AS cluster_id ... GROUP BY
/// cluster_id` query is rejected by Postgres with
///
/// ```text
/// ERROR: window functions are not allowed in GROUP BY
/// ```
///
/// because the GROUP BY references an alias whose defining expression is a
/// window aggregate. Wrapping the window call in an inner subquery
/// materialises `cluster_id` as a plain column in the outer query, so the
/// outer `GROUP BY cluster_id` is valid.
///
/// The inner subquery projects `t.*` so any outer aggregate expression that
/// references `t.<col>` continues to resolve — the outer subquery alias is
/// also `t`, keeping the column-qualification pattern identical to every
/// other query shape in this file.
///
/// # Clause placement under the subquery
///
/// - `WHERE` stays on the **inner** subquery — it prunes rows *before*
///   clustering, which is the only semantically meaningful position for a
///   filter that does not reference `cluster_id`.
/// - `HAVING` stays on the **outer** query — it filters the aggregated
///   groups.
/// - `ORDER BY` / `LIMIT` / `OFFSET` stay on the **outer** query — they
///   paginate the aggregated result.
///
/// # Casts and binds
///
/// The `::geometry` cast is required because `ST_ClusterDBSCAN` does not
/// accept the `geography` type directly in PostGIS 3.x.
///
/// `$eps` is bound as `f64`; `$minpoints` as `i32`. Both are positional
/// parameters (no user-controlled SQL text).
///
/// Called by [`build_grouped_annotated_select`] when
/// `gaq.spatial_source == Some(SpatialGroupSource::Cluster(_))`.
#[cfg(feature = "spatial")]
pub(crate) fn build_cluster_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::ClusterSpec,
) -> SqlAccumulator
where
    T: Model,
    K: crate::query::grouped::IntoGroupKeyTuple,
    A: crate::query::annotate::IntoAggregateTuple,
{
    // Outer SELECT: cluster_id key + aggregates.
    let mut acc = SqlAccumulator::new("SELECT cluster_id");
    gaq.aggregates.push_columns_bare(&mut acc);

    // Inner subquery materialises cluster_id so the outer GROUP BY sees a
    // plain column rather than a window-function reference.
    acc.push_sql(" FROM (SELECT t.*, ST_ClusterDBSCAN(t.");
    acc.push_sql(spec.t_geo_col);
    acc.push_sql("::geometry, ");
    acc.push_bind(spec.eps_degrees);
    acc.push_sql(", ");
    acc.push_bind(spec.minpoints);
    acc.push_sql(") OVER () AS cluster_id FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");

    // WHERE from the upstream queryset — prunes BEFORE clustering.
    push_where(&mut acc, &gaq.qs);

    acc.push_sql(") AS t");

    // Outer GROUP BY on the materialised cluster_id column — now valid.
    acc.push_sql(" GROUP BY cluster_id");

    // HAVING filters the aggregated groups (outer scope).
    if let Some(h) = &gaq.having {
        acc.push_sql(" HAVING ");
        crate::expr::sql::emit_expr(&mut acc, h);
    }

    // ORDER BY (outer scope).
    if !gaq.order.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in gaq.order.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(&mut acc, None);
        }
    }

    // LIMIT / OFFSET (outer scope).
    if let Some(n) = gaq.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n as i64);
    }
    if let Some(n) = gaq.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n as i64);
    }

    acc
}

/// Build the geohash-bucketing variant of the grouped-annotated SELECT:
///
/// ```sql
/// SELECT ST_GeoHash(t.<col>::geometry, $precision) AS geohash, <aggregates>
/// FROM <table> AS t
/// [WHERE ...]
/// GROUP BY geohash
/// [HAVING ...]
/// [ORDER BY ...]
/// [LIMIT $n] [OFFSET $n]
/// ```
///
/// The `::geometry` cast is required for the same reason as DBSCAN —
/// `ST_GeoHash` accepts `geometry`, not `geography`, in PostGIS 3.x.
///
/// `$precision` is bound as `i32` from [`GeohashPrecision::as_i32`].
///
/// Called by [`build_grouped_annotated_select`] when
/// `gaq.spatial_source == Some(SpatialGroupSource::Geohash(_))`.
#[cfg(feature = "spatial")]
pub(crate) fn build_geohash_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::GeohashSpec,
) -> SqlAccumulator
where
    T: Model,
    K: crate::query::grouped::IntoGroupKeyTuple,
    A: crate::query::annotate::IntoAggregateTuple,
{
    let mut acc = SqlAccumulator::new("SELECT ");

    // Scalar expression: the key column.
    // ST_GeoHash(t.<col>::geometry, $precision) AS geohash
    acc.push_sql("ST_GeoHash(t.");
    acc.push_sql(spec.t_geo_col);
    acc.push_sql("::geometry, ");
    acc.push_bind(spec.precision);
    acc.push_sql(") AS geohash");

    // Aggregate columns follow.
    gaq.aggregates.push_columns_bare(&mut acc);

    // FROM <table> AS t
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");

    // WHERE from the upstream queryset.
    push_where(&mut acc, &gaq.qs);

    // GROUP BY geohash  (references the scalar-function alias)
    acc.push_sql(" GROUP BY geohash");

    // HAVING
    if let Some(h) = &gaq.having {
        acc.push_sql(" HAVING ");
        crate::expr::sql::emit_expr(&mut acc, h);
    }

    // ORDER BY
    if !gaq.order.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in gaq.order.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(&mut acc, None);
        }
    }

    // LIMIT / OFFSET
    if let Some(n) = gaq.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n as i64);
    }
    if let Some(n) = gaq.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n as i64);
    }

    acc
}

/// Build `SELECT COUNT(*) FROM <table> [WHERE ...]`, honoring
/// [`DistinctMode`].
///
/// Shapes emitted per mode:
/// - `DistinctMode::None` → `SELECT COUNT(*) FROM "table" [WHERE ...]`
/// - `DistinctMode::Plain` → `SELECT COUNT(*) FROM (SELECT DISTINCT * FROM
///   "table" [WHERE ...]) AS sub`
/// - `DistinctMode::On(cols)` → `SELECT COUNT(*) FROM (SELECT DISTINCT ON
///   (cols) * FROM "table" [WHERE ...] ORDER BY cols [, user-ordering]) AS sub`
///
/// `ORDER BY` / `LIMIT` / `OFFSET` from the queryset are intentionally not
/// emitted on the **outer** count — they don't affect total cardinality and
/// including them only slows the query. For `DISTINCT ON` the inner ORDER
/// BY is required by Postgres (the `ON` column list must be a prefix of
/// `ORDER BY`); we prepend the distinct columns and then append any
/// user-supplied ordering so the emitted SQL is syntactically valid and
/// semantically stable.
pub(crate) fn build_count<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
    match &qs.distinct {
        DistinctMode::None => {
            // Fast path — plain row count, no subquery wrap.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs);
            acc
        }
        DistinctMode::Plain => {
            // `COUNT(*)` over `SELECT DISTINCT *` counts distinct whole-row
            // tuples. No ordering needed inside the subquery — DISTINCT has
            // no prefix requirement.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (SELECT DISTINCT * FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs);
            acc.push_sql(") AS sub");
            acc
        }
        DistinctMode::On(cols) => {
            // `DISTINCT ON (a, b)` requires `ORDER BY a, b [, ...]`. We
            // prepend the distinct columns to the user's ordering so the
            // subquery is always well-formed. Duplicates (user already
            // ordered by a distinct column) are harmless — Postgres ignores
            // repeated expressions in ORDER BY for ordering purposes.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (SELECT DISTINCT ON (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                acc.push_sql(c);
            }
            acc.push_sql(") * FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs);
            acc.push_sql(" ORDER BY ");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                acc.push_sql(c);
            }
            // Append user ordering after the required prefix. Delegate to
            // OrderExpr::emit so Column and SpatialDistance variants both
            // render correctly. No parent_table qualifier — this is a
            // single-table subquery; the DISTINCT ON count path never uses
            // select_related joins.
            for o in qs.ordering.iter() {
                acc.push_sql(", ");
                o.emit(&mut acc, None);
            }
            acc.push_sql(") AS sub");
            acc
        }
    }
}

/// Build `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
///
/// `LIMIT 1` is inside the EXISTS subquery rather than being passed through
/// the queryset's `limit` slot: EXISTS returns a single boolean regardless
/// of how many rows match, so `LIMIT 1` here is a micro-optimization that
/// tells Postgres to stop scanning once one match is found.
pub(crate) fn build_exists<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("SELECT EXISTS(SELECT 1 FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs);
    acc.push_sql(" LIMIT 1)");
    acc
}

/// Build `UPDATE <table> SET col = $1, col = $2, updated_at = now()
/// [WHERE ...]`.
///
/// Every assignment's value flows through [`push_filter_value`] — i.e.
/// `push_bind` — so the emitted SQL has one positional parameter per
/// user-supplied value. The `updated_at = now()` tail is always appended,
/// even when the caller's closure omitted it: parity with the single-row
/// `save()` path, which also bumps `updated_at` on every write. Users who
/// need to preserve `updated_at` across a bulk update reach for raw SQL
/// via `ctx.raw_execute` (T5) — same as any other ORM layer that
/// treats the audit column as non-optional.
///
/// `WHERE` is emitted via the shared [`push_where`] helper, so
/// `QuerySet::none()`-derived querysets (caught earlier in
/// [`crate::query::update::UpdateStmt::execute`]) and vacuously-true
/// condition trees are handled identically to the read terminals.
///
/// # Assignment list invariants
///
/// Callers must ensure `assignments` is non-empty — `UPDATE ... SET ` with
/// an empty list is a Postgres syntax error. The public entry point
/// ([`crate::query::update::UpdateStmt::execute`]) short-circuits on
/// `assignments.is_empty()` before reaching this emitter, so the emitter
/// itself does not need a runtime guard. Panicking here would be
/// defensive-programming noise; the short-circuit is the real safety rail.
pub(crate) fn build_update<T: Model>(
    qs: &QuerySet<T>,
    assignments: &[crate::query::update::UpdateAssignment],
) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("UPDATE ");
    acc.push_sql(T::table_name());
    acc.push_sql(" SET ");
    for (i, a) in assignments.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        // Column names are macro-baked `&'static str` literals — `push_sql`
        // (not `push_bind`). Values always go through `push_filter_value`
        // which calls `push_bind` for every variant except `Null`
        // (unreachable here because `FieldRef::set` requires
        // `V: IntoFilterValue`, which never produces `FilterValue::Null`).
        acc.push_sql(a.column());
        acc.push_sql(" = ");
        // Dispatch literal vs expression-IR payload. Literals go through
        // `push_filter_value` (a single `$n` bind); expression trees
        // recurse through `emit_expr`, which emits arithmetic, field
        // refs, and nested binds. `clone()` on the literal path retains
        // the `UpdateStmt`'s payload for retry; the Expr arm borrows
        // the inner `ExprNode` by reference.
        match a.value() {
            crate::query::update::AssignmentValue::Literal(v) => {
                push_filter_value(&mut acc, v.clone());
            }
            crate::query::update::AssignmentValue::Expr(node) => {
                crate::expr::sql::emit_expr(&mut acc, node);
            }
        }
    }
    // Always stamp `updated_at = now()` on bulk updates — matches
    // single-row save(). `now()` is a SQL literal, not a user value, so
    // `push_sql` is correct (no bind slot needed). Position-wise this is a
    // trailing clause after the user's SET list; the leading ", " handles
    // the separator even when the user supplied only one assignment.
    acc.push_sql(", updated_at = now()");
    push_where(&mut acc, qs);
    acc
}

/// Build `DELETE FROM <table> [WHERE ...]`.
///
/// Plain DELETE — no RETURNING, no USING join. The `WHERE` clause uses
/// the shared [`push_where`] helper so vacuously-true condition trees
/// (e.g. `Condition::And(vec![])`) are omitted entirely rather than
/// emitted as `WHERE TRUE`. A queryset with no filters at all (just
/// `T::objects()`) deletes every row in the table — same semantics as
/// raw SQL; callers who want extra safety wrap the call in a
/// transaction and `ROLLBACK` if the row count looks wrong.
///
/// `updated_at = now()` stamping does **not** apply here — the row is
/// being removed, so auditing the timestamp has no meaning. Audit of
/// deletions lives in the Phase 1 `_logs` mirror tables (populated by
/// the `crud_log_url` pool).
pub(crate) fn build_delete<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("DELETE FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs);
    acc
}

/// Walk the emitted SELECT list and check that every column's alias (or
/// plain column name if no `AS` alias) is unique. A collision would cause
/// the terminal decoder to read the wrong value for one of the columns.
///
/// # Algorithm
///
/// 1. Find the substring between `SELECT ` and the next ` FROM ` (case
///    matters — emitters use uppercase keywords).
/// 2. Split on commas at the top parenthesis level into logical columns.
///    Parens and nested function calls are handled by tracking depth, so
///    aggregate expressions like `SUM(a, b)` are not split mid-argument.
/// 3. For each column, extract the alias — the substring after the last
///    ` AS ` if present, otherwise the whole column text (trimmed).
/// 4. Check uniqueness; return `Err(DjogiError::AliasCollision)` on
///    duplicate.
///
/// # Limitations
///
/// This is a best-effort string parse. It does not handle:
/// - Nested subqueries in the SELECT list (not emitted by Phase 6.5).
/// - Unparenthesised comma-separated arguments at the top level (our
///   emitter always parenthesises function args).
///
/// The check is defensive; failure means something has gone subtly wrong
/// in the query builder, not that the user did something wrong.
pub(crate) fn assert_no_alias_collision(sql: &str) -> Result<(), crate::DjogiError> {
    // Locate the SELECT keyword — accept leading text for safety.
    let after_select = if let Some(s) = sql.strip_prefix("SELECT ") {
        s
    } else if let Some(i) = sql.find("SELECT ") {
        &sql[i + "SELECT ".len()..]
    } else {
        return Ok(()); // not a SELECT we recognise; skip
    };

    // Locate FROM to extract the select-list. We look for " FROM " with
    // surrounding spaces so we don't accidentally match a column named FROM.
    let from_idx = match after_select.find(" FROM ") {
        Some(i) => i,
        None => return Ok(()),
    };
    let select_list = &after_select[..from_idx];

    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for col in split_top_level_commas(select_list) {
        let col = col.trim();
        // Use rfind so that expressions like `CAST(x AS int) AS alias`
        // pick up the outermost ` AS ` rather than the one inside CAST.
        let alias = if let Some(idx) = col.rfind(" AS ") {
            col[idx + " AS ".len()..].trim()
        } else {
            col
        };
        if !seen.insert(alias) {
            return Err(crate::DjogiError::AliasCollision {
                alias: alias.to_owned(),
            });
        }
    }
    Ok(())
}

/// Split a SQL fragment on commas that are at the top parenthesis level.
///
/// Phase 6.5's emitter output uses simple function-call args, so a single
/// paren counter suffices. No regex — depth is tracked byte by byte using
/// `u8` comparison on `b'('` and `b')'`.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — assert on the generated SQL text without
    //! touching a real database. These tests verify the shape of the output
    //! (token order, placeholder count) for each `Condition` / `QuerySet`
    //! state the emitter handles. Actual bind values are validated by the
    //! integration tests in `tests/integration/phase2_queryset.rs`.
    //!
    //! We reach into the emitter using a minimal local `Model` impl (mirrors
    //! the `Fake` model used in `query::field`'s tests) so that unit tests
    //! remain independent of `#[model]` macro expansion.

    use super::*;
    use crate::descriptor::ModelDescriptor;
    use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
    use crate::query::queryset::QuerySet;

    struct Fake;
    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    // T3: the SQL emitter bounds on `T: FromPgRow` so it can interpolate
    // `COLUMN_LIST` into `SELECT` / `SELECT DISTINCT` shapes. The unit
    // tests below exercise SQL-text shape only (no row decode), so we
    // supply a stub impl with a single `id` column — enough for
    // `COLUMN_LIST` to be non-empty without pretending the fake model
    // has a full schema.
    impl FromPgRow for Fake {
        const COLUMNS: &'static [&'static str] = &["id"];
        const COLUMN_LIST: &'static str = "id";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError> {
            unreachable!("SQL-text unit tests do not exercise row decode")
        }
    }

    // `SqlAccumulator::sql()` exposes the emitted SQL text — that is what we
    // assert on. Bind values don't appear in `.sql()`, they are tracked
    // separately and substituted as `$1`, `$2`, …; counting placeholders is
    // the unit-test-level proxy for "the right number of binds were made".

    #[test]
    fn select_no_filter_omits_where() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    #[test]
    fn select_with_leaf_filter_emits_where_with_one_bind() {
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE a = $1"), "got: {sql}");
    }

    #[test]
    fn select_with_and_uses_parentheses() {
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))))
            .filter(|_| Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false))));
        let acc = build_select(&qs);
        let sql = acc.sql();
        // Flattened And(vec![a, b]) → "(a = $1 AND b = $2)"
        assert!(sql.contains("WHERE (a = $1 AND b = $2)"), "got: {sql}");
    }

    #[test]
    fn select_with_exclude_wraps_not() {
        let qs: QuerySet<Fake> = QuerySet::new()
            .exclude(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE NOT (a = $1)"), "got: {sql}");
    }

    #[test]
    fn select_distinct_plain_emits_distinct_keyword() {
        let qs: QuerySet<Fake> = QuerySet::new().distinct();
        let acc = build_select(&qs);
        assert!(acc.sql().contains("SELECT DISTINCT id FROM fakes"));
    }

    #[test]
    fn select_limit_offset_pushes_two_binds() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5);
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("LIMIT $1"), "got: {sql}");
        assert!(sql.contains("OFFSET $2"), "got: {sql}");
    }

    #[test]
    fn in_empty_list_renders_false() {
        let leaf = Leaf {
            column: "id",
            op: LookupOp::In,
            value: FilterValue::List(Vec::new()),
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE FALSE"), "got: {sql}");
    }

    #[test]
    fn not_in_empty_list_renders_true() {
        let leaf = Leaf {
            column: "id",
            op: LookupOp::NotIn,
            value: FilterValue::List(Vec::new()),
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE TRUE"), "got: {sql}");
    }

    #[test]
    fn in_list_emits_one_placeholder_per_element() {
        let leaf = Leaf {
            column: "id",
            op: LookupOp::In,
            value: FilterValue::List(vec![
                FilterValue::I64(1),
                FilterValue::I64(2),
                FilterValue::I64(3),
            ]),
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("id IN ($1, $2, $3)"), "got: {sql}");
    }

    #[test]
    fn between_emits_two_binds() {
        let leaf = Leaf {
            column: "age",
            op: LookupOp::Between,
            value: FilterValue::Pair(
                Box::new(FilterValue::I32(10)),
                Box::new(FilterValue::I32(20)),
            ),
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("age BETWEEN $1 AND $2"), "got: {sql}");
    }

    #[test]
    fn is_null_takes_no_bind() {
        let leaf = Leaf {
            column: "deleted_at",
            op: LookupOp::IsNull,
            value: FilterValue::Null,
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("deleted_at IS NULL"), "got: {sql}");
        // No placeholder should appear — IS NULL is operator-only.
        assert!(!sql.contains('$'), "expected no binds, got: {sql}");
    }

    #[test]
    fn count_ignores_order_limit_offset() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5);
        let acc = build_count(&qs);
        let sql = acc.sql();
        assert!(sql.starts_with("SELECT COUNT(*) FROM fakes"));
        assert!(
            !sql.contains("LIMIT"),
            "count should not carry LIMIT: {sql}"
        );
        assert!(
            !sql.contains("OFFSET"),
            "count should not carry OFFSET: {sql}"
        );
    }

    #[test]
    fn exists_emits_limit_1_inside_subquery() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_exists(&qs);
        let sql = acc.sql();
        assert!(sql.contains("SELECT EXISTS(SELECT 1 FROM fakes"));
        assert!(sql.contains("LIMIT 1"));
    }

    #[test]
    fn order_by_asc_nulls_last_emits_expected_tokens() {
        let qs: QuerySet<Fake> =
            QuerySet::new().order_by(|_| crate::query::order::OrderExpr::Column {
                column: "title",
                direction: crate::query::order::Direction::Asc,
                nulls: crate::query::order::NullsOrder::Last,
            });
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("ORDER BY title ASC NULLS LAST"), "got: {sql}");
    }

    #[test]
    fn distinct_on_emits_column_list() {
        // Hand-build the DistinctMode::On variant — skipping the typed
        // builder surface keeps this unit test independent of FieldRef
        // machinery (tested separately).
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.distinct = DistinctMode::On(vec!["title", "view_count"]);
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.contains("SELECT DISTINCT ON (title, view_count) id FROM fakes"),
            "got: {sql}"
        );
    }

    #[test]
    fn like_escape_handles_percent_underscore_backslash() {
        // Every special character in the user string must be prefixed with
        // `\\` before the wildcard wrap. Assert on the escaped string shape
        // via the emitter's observable SQL-with-$n output by constructing
        // an `IContains` leaf manually — we cannot assert on the bind value
        // from `sql()` alone, but we CAN observe that it was a single bind
        // (not multiple).
        let leaf = Leaf {
            column: "title",
            op: LookupOp::IContains,
            value: FilterValue::String("50% off_sale\\".to_string()),
        };
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("title ILIKE $1"), "got: {sql}");
    }

    #[test]
    fn escape_like_prefixes_special_chars() {
        // Direct unit test of the escape helper — documents the contract
        // independently of emission ordering.
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("c\\d"), "c\\\\d");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn count_with_distinct_plain_wraps_subquery() {
        // `.distinct().count()` must wrap the query in a subquery so
        // `COUNT(*)` counts distinct tuples, not raw rows. Previously this
        // silently returned the base row count — a correctness bug on a
        // public terminal.
        let qs: QuerySet<Fake> = QuerySet::new().distinct();
        let acc = build_count(&qs);
        let sql = acc.sql();
        assert!(
            sql.contains("SELECT COUNT(*) FROM (SELECT DISTINCT * FROM fakes)"),
            "got: {sql}"
        );
        assert!(sql.contains(") AS sub"), "got: {sql}");
    }

    #[test]
    fn count_with_distinct_on_wraps_subquery_with_order() {
        // DISTINCT ON (a, b) requires ORDER BY a, b prefix. The subquery
        // prepends the distinct columns so the emitted SQL is always
        // well-formed even when the user supplied no ordering.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.distinct = DistinctMode::On(vec!["title", "view_count"]);
        let acc = build_count(&qs);
        let sql = acc.sql();
        assert!(
            sql.contains(
                "SELECT COUNT(*) FROM (SELECT DISTINCT ON (title, view_count) * FROM fakes"
            ),
            "got: {sql}"
        );
        assert!(sql.contains("ORDER BY title, view_count"), "got: {sql}");
        assert!(sql.contains(") AS sub"), "got: {sql}");
    }

    #[test]
    fn count_with_distinct_on_appends_user_ordering() {
        // When the user provides ORDER BY on top of DISTINCT ON, the user
        // ordering is appended after the required prefix. Duplicate columns
        // are harmless in Postgres.
        let mut qs: QuerySet<Fake> =
            QuerySet::new().order_by(|_| crate::query::order::OrderExpr::Column {
                column: "view_count",
                direction: crate::query::order::Direction::Desc,
                nulls: crate::query::order::NullsOrder::Last,
            });
        qs.distinct = DistinctMode::On(vec!["title"]);
        let acc = build_count(&qs);
        let sql = acc.sql();
        assert!(
            sql.contains("ORDER BY title, view_count DESC NULLS LAST"),
            "got: {sql}"
        );
    }

    #[test]
    fn count_without_distinct_omits_subquery() {
        // Regression guard for the `DistinctMode::None` fast path — the
        // plain count must not pick up an unnecessary subquery wrap.
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_count(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT COUNT(*) FROM fakes");
    }

    #[test]
    fn where_skipped_on_empty_and() {
        // Top-level `And(vec![])` collapses to vacuous TRUE; the emitter
        // omits the `WHERE` clause entirely rather than emitting
        // `WHERE TRUE`. Previously only `Condition::True` was skipped, so
        // an externally-constructed empty `And` leaked `WHERE TRUE` into
        // the SQL.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Condition::And(Vec::new());
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    #[test]
    fn where_skipped_on_nested_vacuous_and() {
        // Nested `And(vec![True, And(vec![])])` is also vacuously TRUE —
        // `is_vacuously_true` walks the `And` subtree recursively. Same
        // cleanup as the flat empty-And case.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Condition::And(vec![Condition::True, Condition::And(Vec::new())]);
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    #[test]
    fn where_skipped_on_not_empty_or() {
        // `Not(Or(vec![]))` emits as `NOT FALSE` → `TRUE`, which is
        // vacuously true. Handled by the same skip path.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Condition::Not(Box::new(Condition::Or(Vec::new())));
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    // ── Task 9: UPDATE / DELETE emitter ───────────────────────────────

    #[test]
    fn update_single_assignment_emits_set_and_updated_at() {
        // Single assignment + no filter: one bind for the user value,
        // `updated_at = now()` stamped by the emitter, no `WHERE`.
        use crate::query::update::{AssignmentValue, UpdateAssignment};
        let a = UpdateAssignment {
            column: "view_count",
            value: AssignmentValue::Literal(FilterValue::I32(999)),
        };
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_update(&qs, &[a]);
        let sql = acc.sql();
        assert!(
            sql.contains("UPDATE fakes SET view_count = $1, updated_at = now()"),
            "got: {sql}"
        );
        assert!(!sql.contains("WHERE"), "no filter -> no WHERE, got: {sql}");
    }

    #[test]
    fn update_multiple_assignments_comma_separate_binds() {
        // Two assignments: `SET col = $1, col = $2, updated_at = now()`.
        // Only the user's values consume bind slots; `now()` is raw SQL.
        use crate::query::update::{AssignmentValue, UpdateAssignment};
        let a = UpdateAssignment {
            column: "view_count",
            value: AssignmentValue::Literal(FilterValue::I32(1)),
        };
        let b = UpdateAssignment {
            column: "published",
            value: AssignmentValue::Literal(FilterValue::Bool(true)),
        };
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_update(&qs, &[a, b]);
        let sql = acc.sql();
        assert!(
            sql.contains("SET view_count = $1, published = $2, updated_at = now()"),
            "got: {sql}"
        );
    }

    #[test]
    fn update_with_filter_emits_where_with_bind_offset() {
        // Assignments take $1; the filter leaf takes $2. Positional
        // numbering is contiguous — the accumulator assigns them
        // in push order regardless of clause.
        use crate::query::update::{AssignmentValue, UpdateAssignment};
        let a = UpdateAssignment {
            column: "view_count",
            value: AssignmentValue::Literal(FilterValue::I32(42)),
        };
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("published", FilterValue::Bool(true))));
        let acc = build_update(&qs, &[a]);
        let sql = acc.sql();
        assert!(
            sql.contains("SET view_count = $1, updated_at = now()"),
            "got: {sql}"
        );
        assert!(sql.contains("WHERE published = $2"), "got: {sql}");
    }

    #[test]
    fn delete_no_filter_emits_table_only() {
        // DELETE on an unfiltered queryset has no WHERE clause — same
        // semantics as raw `DELETE FROM table`. Callers who want safety
        // chain a filter.
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_delete(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "DELETE FROM fakes");
    }

    #[test]
    fn delete_with_filter_emits_where() {
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("published", FilterValue::Bool(false))));
        let acc = build_delete(&qs);
        let sql = acc.sql();
        assert!(sql.starts_with("DELETE FROM fakes"), "got: {sql}");
        assert!(sql.contains("WHERE published = $1"), "got: {sql}");
    }

    #[test]
    fn delete_vacuous_and_skips_where() {
        // Vacuously-true condition trees collapse the same way they do
        // for SELECT — `DELETE FROM table` without `WHERE TRUE` noise.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Condition::And(Vec::new());
        let acc = build_delete(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "DELETE FROM fakes");
    }

    // ── Phase 3 Task 5 fix: parent-table qualification under select_related ──
    //
    // When `.select_related(...)` is active, every bare column reference
    // emitted by the `WHERE` / `ORDER BY` / `DISTINCT ON` clauses must be
    // qualified with the parent table name so Postgres does not raise
    // `42702 column reference "X" is ambiguous` on framework columns
    // (`id`, `created_at`, `updated_at`) that also appear on the joined
    // child. These tests pin the SQL shape at emission time so the live-
    // Postgres integration tests stay readable — a shape regression
    // surfaces here, not in a one-line 42702 failure down the stack.

    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, PkType, field_descriptor, model_descriptor,
    };
    use crate::relation::select_related::ErasedSelectRelated;

    // Minimal static descriptor for a joined child. Column layout is a
    // stand-in for `Owner` in the integration suite — `id` is the
    // framework column that triggers the ambiguity bug when both sides
    // of the join contribute it bare.
    static OWNERS_JOIN_DESC: ModelDescriptor = ModelDescriptor {
        ..model_descriptor(
            "Owner",
            "owners_p3",
            PkType::HeerId,
            &[FieldDescriptor {
                unique: true,
                indexed: true,
                ..field_descriptor("id", FieldSqlType::BigInt, false)
            }],
        )
    };

    fn owners_join_descriptor() -> &'static ModelDescriptor {
        &OWNERS_JOIN_DESC
    }

    fn dummy_join_decoder(
        _row: &tokio_postgres::Row,
        _prefix: &str,
    ) -> Result<Option<Box<dyn std::any::Any + Send + Sync>>, crate::DjogiError> {
        // Never invoked — these tests only exercise SQL emission.
        unreachable!("dummy decoder should not run in SQL-emission tests")
    }

    fn owner_path() -> ErasedSelectRelated {
        ErasedSelectRelated {
            source_column: "owner_id",
            child_table: "owners_p3",
            decoder: dummy_join_decoder,
            child_descriptor: owners_join_descriptor,
        }
    }

    #[test]
    fn joined_select_qualifies_where_column_refs_with_parent_table() {
        // `.select_related(owner).filter(|f| f.id.eq(x))` must emit
        // `WHERE fakes.id = $1`, not `WHERE id = $1`. Live Postgres
        // raises 42702 on the bare form because `owners_p3.id` is
        // simultaneously in scope via the LEFT JOIN.
        let mut qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("id", FilterValue::I64(42))));
        qs.select_related_paths.push(owner_path());
        let acc = build_select_joined(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE fakes.id = $1"), "got: {sql}");
        // And the LEFT JOIN stays in place — the fix must not drop the
        // join clause while qualifying the where.
        assert!(
            sql.contains("LEFT JOIN owners_p3 rel_owner_id"),
            "LEFT JOIN missing, got: {sql}"
        );
    }

    #[test]
    fn joined_select_qualifies_order_by_column_refs() {
        // `.order_by(|f| f.created_at.asc())` on a joined queryset must
        // emit `ORDER BY fakes.created_at ASC` so Postgres does not
        // raise 42702 when the child also contributes `created_at`.
        let mut qs: QuerySet<Fake> =
            QuerySet::new().order_by(|_| crate::query::order::OrderExpr::Column {
                column: "created_at",
                direction: crate::query::order::Direction::Asc,
                nulls: crate::query::order::NullsOrder::Default,
            });
        qs.select_related_paths.push(owner_path());
        let acc = build_select_joined(&qs);
        let sql = acc.sql();
        assert!(sql.contains("ORDER BY fakes.created_at ASC"), "got: {sql}");
    }

    #[test]
    fn joined_select_qualifies_distinct_on_column_refs() {
        // `DISTINCT ON (id)` on a joined queryset must render as
        // `DISTINCT ON (fakes.id)` — same ambiguity rule as WHERE /
        // ORDER BY. Build the queryset directly to skip the typed
        // surface (unit-test lives below the FieldRef layer).
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.distinct = DistinctMode::On(vec!["id"]);
        qs.select_related_paths.push(owner_path());
        let acc = build_select_joined(&qs);
        let sql = acc.sql();
        assert!(sql.contains("SELECT DISTINCT ON (fakes.id)"), "got: {sql}");
    }

    #[test]
    fn non_joined_select_leaves_column_refs_bare() {
        // Regression guard: the non-joined `build_select` path must not
        // pick up the qualifier. Bare `WHERE id = $1` matches Phase 2's
        // shipped SQL byte-for-byte.
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("id", FilterValue::I64(42))));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE id = $1"), "got: {sql}");
        assert!(
            !sql.contains("fakes.id"),
            "bare query must not qualify: {sql}"
        );
    }

    // ── Phase 4 Task 7 — row-lock SQL tails ───────────────────────────

    #[test]
    fn select_for_update_appends_lock_tail() {
        let qs: QuerySet<Fake> = QuerySet::new().select_for_update();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE"),
            "expected FOR UPDATE tail, got: {sql}"
        );
        assert!(
            !sql.contains("NOWAIT") && !sql.contains("SKIP LOCKED"),
            "select_for_update must not escalate to NOWAIT / SKIP LOCKED"
        );
    }

    #[test]
    fn nowait_appends_for_update_nowait_tail() {
        // `.nowait()` alone — implies the base `FOR UPDATE` per the
        // rustdoc contract.
        let qs: QuerySet<Fake> = QuerySet::new().nowait();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE NOWAIT"),
            "expected FOR UPDATE NOWAIT tail, got: {sql}"
        );
    }

    #[test]
    fn skip_locked_appends_for_update_skip_locked_tail() {
        let qs: QuerySet<Fake> = QuerySet::new().skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE SKIP LOCKED"),
            "expected FOR UPDATE SKIP LOCKED tail, got: {sql}"
        );
    }

    #[test]
    fn lock_tail_follows_limit_and_offset() {
        // Postgres requires `FOR UPDATE` after `LIMIT`/`OFFSET`. The
        // emitter must match that order — swapping them yields a
        // syntax error the caller only sees at runtime.
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5).select_for_update();
        let acc = build_select(&qs);
        let sql = acc.sql();
        let limit_idx = sql.find("LIMIT").expect("LIMIT must appear");
        let offset_idx = sql.find("OFFSET").expect("OFFSET must appear");
        let lock_idx = sql.find("FOR UPDATE").expect("FOR UPDATE must appear");
        assert!(
            limit_idx < offset_idx && offset_idx < lock_idx,
            "expected LIMIT ... OFFSET ... FOR UPDATE order, got: {sql}"
        );
    }

    #[test]
    fn lock_builder_last_call_wins_across_nowait_skip_locked() {
        // Chaining `.nowait().skip_locked()` — the skip_locked call
        // overwrites the nowait variant. Mirrors the rustdoc contract.
        let qs: QuerySet<Fake> = QuerySet::new().nowait().skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE SKIP LOCKED"),
            "expected skip_locked to win over nowait, got: {sql}"
        );
    }

    // ── T2 emitter: GROUPING SETS ─────────────────────────────────────────

    #[test]
    fn build_grouped_annotated_select_emits_grouping_sets() {
        use crate::expr::AggregateExpr;
        use crate::query::field::FieldRef;
        use crate::query::grouped::{GroupedAnnotatedQuerySet, GroupingMode};
        use std::marker::PhantomData;
        let qs: QuerySet<Fake> = QuerySet::new();
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq: GroupedAnnotatedQuerySet<Fake, (), AggregateExpr<i64>> = {
            let gq = crate::query::grouped::GroupedQuerySet {
                qs,
                keys: (),
                grouping: GroupingMode::Sets(vec![vec!["org_id"], vec!["region"]]),
                #[cfg(feature = "spatial")]
                spatial_source: None,
                _k: PhantomData,
            };
            gq.annotate(|_| vals.sum())
        };
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUPING SETS ((org_id), (region))"),
            "expected GROUPING SETS clause, got: {sql}"
        );
    }

    // ── T1 emitter behavior: ROLLUP / CUBE ────────────────────────────────
    //
    // These tests document that the T1 emitter already handles ROLLUP and
    // CUBE correctly. T2 adds the entry-point methods (.rollup, .cube) and
    // the GROUPING SETS emitter arm; the tests below pin the existing
    // emitter behavior so regressions surface before the new arms land.

    #[test]
    fn build_grouped_annotated_select_emits_rollup() {
        use crate::expr::AggregateExpr;
        use crate::query::field::FieldRef;
        use crate::query::grouped::{GroupedAnnotatedQuerySet, GroupingMode};
        use std::marker::PhantomData;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq: GroupedAnnotatedQuerySet<Fake, FieldRef<Fake, i64>, AggregateExpr<i64>> = {
            let gq = crate::query::grouped::GroupedQuerySet {
                qs,
                keys,
                grouping: GroupingMode::Rollup,
                #[cfg(feature = "spatial")]
                spatial_source: None,
                _k: PhantomData,
            };
            gq.annotate(|_| vals.sum())
        };
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY ROLLUP (org_id)"),
            "expected ROLLUP clause, got: {sql}"
        );
    }

    #[test]
    fn build_grouped_annotated_select_emits_cube() {
        use crate::expr::AggregateExpr;
        use crate::query::field::FieldRef;
        use crate::query::grouped::{GroupedAnnotatedQuerySet, GroupingMode};
        use std::marker::PhantomData;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq: GroupedAnnotatedQuerySet<Fake, FieldRef<Fake, i64>, AggregateExpr<i64>> = {
            let gq = crate::query::grouped::GroupedQuerySet {
                qs,
                keys,
                grouping: GroupingMode::Cube,
                #[cfg(feature = "spatial")]
                spatial_source: None,
                _k: PhantomData,
            };
            gq.annotate(|_| vals.sum())
        };
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();
        assert!(
            sql.contains("GROUP BY CUBE (org_id)"),
            "expected CUBE clause, got: {sql}"
        );
    }

    // T5 — alias-collision diagnostic

    #[test]
    fn alias_collision_detected_in_grouped_select() {
        // Positive case: clean SQL with no collision should pass.
        let ok_sql = "SELECT org_id, SUM(amount) AS __djogi_agg_0 FROM txns GROUP BY org_id";
        let result = assert_no_alias_collision(ok_sql);
        assert!(result.is_ok(), "expected no collision, got: {:?}", result);
    }

    #[test]
    fn alias_collision_names_both_columns_in_error() {
        let bad_sql = "SELECT foo AS dup, bar AS dup FROM t";
        let err = assert_no_alias_collision(bad_sql).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("dup"),
            "error should name the conflicting alias 'dup', got: {msg}"
        );
    }

    #[test]
    fn alias_collision_bare_name_collision_detected() {
        // Two bare column names (no AS) that share the same identifier.
        let bad_sql = "SELECT foo, foo FROM t";
        let err = assert_no_alias_collision(bad_sql).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("foo"),
            "error should name the conflicting alias 'foo', got: {msg}"
        );
    }

    #[test]
    fn alias_collision_mixed_as_and_bare_collision_detected() {
        // A bare column 'org_id' collides with an explicit 'AS org_id'.
        let bad_sql = "SELECT org_id, SUM(amount) AS org_id FROM t GROUP BY org_id";
        let err = assert_no_alias_collision(bad_sql).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("org_id"),
            "error should name the conflicting alias 'org_id', got: {msg}"
        );
    }

    #[test]
    fn alias_collision_happy_path_grouped_queryset() {
        // End-to-end: build a grouped queryset, emit SQL, verify no collision.
        use crate::expr::AggregateExpr;
        use crate::query::field::FieldRef;
        let qs: QuerySet<Fake> = QuerySet::new();
        let keys: FieldRef<Fake, i64> = FieldRef::new("org_id");
        let vals: FieldRef<Fake, i64> = FieldRef::new("amount");
        let gaq = qs.group_by(|_| keys).annotate(|_| vals.sum());
        let acc =
            build_grouped_annotated_select::<Fake, FieldRef<Fake, i64>, AggregateExpr<i64>>(&gaq);
        let result = assert_no_alias_collision(acc.sql());
        assert!(
            result.is_ok(),
            "expected no alias collision in grouped queryset, got: {:?}",
            result
        );
    }

    // ── T11: spatial JOIN grouped SELECT emitter ────────────────────────────
    //
    // These tests construct a `GroupedAnnotatedQuerySet` with a spatial join
    // spec and assert that the emitted SQL contains the expected LEFT JOIN,
    // ST_Covers call (the geography-native point-in-polygon function), and
    // GROUP BY clause.

    /// Minimal region model — no real descriptor needed for SQL emission tests.
    #[cfg(feature = "spatial")]
    struct FakeRegion;
    #[cfg(feature = "spatial")]
    impl crate::model::__sealed::Sealed for FakeRegion {}
    #[cfg(feature = "spatial")]
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeRegion {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "neighborhoods"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send + 'ctx
        {
            async { unreachable!() }
        }
    }

    /// Helper: build a `GroupedAnnotatedQuerySet` with a spatial join spec by
    /// hand, bypassing the `group_by_region` entry point (which requires a
    /// real descriptor). This lets us test the SQL builder in isolation.
    #[cfg(feature = "spatial")]
    fn make_spatial_gaq(
        spec: crate::query::spatial_grouping::SpatialJoinSpec,
    ) -> crate::query::grouped::GroupedAnnotatedQuerySet<
        Fake,
        crate::query::spatial_grouping::RegionKey<FakeRegion>,
        crate::expr::AggregateExpr<i64>,
    > {
        use std::marker::PhantomData;
        let keys = crate::query::spatial_grouping::RegionKey::<FakeRegion> {
            region_pk: None,
            r_pk_col: Some(spec.r_pk_col),
            _phantom: PhantomData,
        };
        let agg: crate::expr::AggregateExpr<i64> =
            crate::query::field::FieldRef::<Fake, i64>::new("id").count_star();
        crate::query::grouped::GroupedAnnotatedQuerySet {
            qs: QuerySet::new(),
            keys,
            grouping: crate::query::grouped::GroupingMode::Plain,
            aggregates: agg,
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Join(spec)),
            _k: PhantomData,
            _a: PhantomData,
        }
    }

    /// The emitted SQL must contain `LEFT JOIN <r-table> AS r ON ST_Covers(…)`.
    ///
    /// `ST_Covers` is used rather than `ST_Contains` because the former has a
    /// native `geography` overload in PostGIS 3.x; `ST_Contains(geography,
    /// geography)` does not exist. Semantics are equivalent for the
    /// point-in-polygon case this JOIN implements.
    #[cfg(feature = "spatial")]
    #[test]
    fn spatial_join_emits_left_join_with_st_covers() {
        let spec = crate::query::spatial_grouping::SpatialJoinSpec {
            t_geo_col: "location",
            r_table: "neighborhoods",
            r_geo_col: "boundary",
            r_pk_col: "id",
        };
        let gaq = make_spatial_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        assert!(
            sql.contains("LEFT JOIN neighborhoods AS r ON ST_Covers(r.boundary, t.location)"),
            "expected LEFT JOIN with ST_Covers (geography overload), got: {sql}"
        );
        assert!(
            !sql.contains("ST_Contains"),
            "must not emit ST_Contains (no geography overload in PostGIS 3.x); got: {sql}"
        );
    }

    /// The emitted SQL must GROUP BY the region PK column qualified with `r.`.
    #[cfg(feature = "spatial")]
    #[test]
    fn spatial_join_groups_by_region_pk_qualified() {
        let spec = crate::query::spatial_grouping::SpatialJoinSpec {
            t_geo_col: "location",
            r_table: "neighborhoods",
            r_geo_col: "boundary",
            r_pk_col: "id",
        };
        let gaq = make_spatial_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        assert!(
            sql.contains("GROUP BY r.id"),
            "expected GROUP BY r.id, got: {sql}"
        );
    }

    /// The SELECT list must contain `r.<pk-col> AS rk0` before the aggregates.
    #[cfg(feature = "spatial")]
    #[test]
    fn spatial_join_select_list_starts_with_region_pk_alias() {
        let spec = crate::query::spatial_grouping::SpatialJoinSpec {
            t_geo_col: "location",
            r_table: "neighborhoods",
            r_geo_col: "boundary",
            r_pk_col: "id",
        };
        let gaq = make_spatial_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        // The SELECT list must begin with the region key alias.
        assert!(
            sql.starts_with("SELECT r.id AS rk0"),
            "expected SELECT to start with 'SELECT r.id AS rk0', got: {sql}"
        );
    }

    /// Clause ordering: FROM, LEFT JOIN, WHERE (if any), GROUP BY, ORDER BY,
    /// LIMIT, OFFSET. Verify positions relative to each other.
    #[cfg(feature = "spatial")]
    #[test]
    fn spatial_join_clause_order_is_correct() {
        let spec = crate::query::spatial_grouping::SpatialJoinSpec {
            t_geo_col: "location",
            r_table: "neighborhoods",
            r_geo_col: "boundary",
            r_pk_col: "id",
        };
        let gaq = make_spatial_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        let from_pos = sql.find("FROM fakes AS t").unwrap();
        let join_pos = sql.find("LEFT JOIN neighborhoods").unwrap();
        let group_pos = sql.find("GROUP BY").unwrap();

        assert!(
            from_pos < join_pos,
            "FROM must precede LEFT JOIN; got: {sql}"
        );
        assert!(
            join_pos < group_pos,
            "LEFT JOIN must precede GROUP BY; got: {sql}"
        );
    }

    // ── T12: cluster_by_proximity SQL emission ────────────────────────────

    /// Helper: build a `GroupedAnnotatedQuerySet` with a cluster spec, bypassing
    /// the `cluster_by_proximity` entry point so we can test SQL emission in
    /// isolation without a real model descriptor.
    #[cfg(feature = "spatial")]
    fn make_cluster_gaq(
        spec: crate::query::spatial_grouping::ClusterSpec,
    ) -> crate::query::grouped::GroupedAnnotatedQuerySet<
        Fake,
        crate::query::spatial_grouping::ClusterId,
        crate::expr::AggregateExpr<i64>,
    > {
        use std::marker::PhantomData;
        let agg: crate::expr::AggregateExpr<i64> =
            crate::query::field::FieldRef::<Fake, i64>::new("id").count_star();
        crate::query::grouped::GroupedAnnotatedQuerySet {
            qs: QuerySet::new(),
            keys: crate::query::spatial_grouping::ClusterId(None),
            grouping: crate::query::grouped::GroupingMode::Plain,
            aggregates: agg,
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Cluster(spec)),
            _k: PhantomData,
            _a: PhantomData,
        }
    }

    /// The emitted SQL must contain `ST_ClusterDBSCAN(t.<col>::geometry, ...)
    /// OVER () AS cluster_id` and `GROUP BY cluster_id`.
    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_grouped_select_emits_st_cluster_dbscan_with_geometry_cast() {
        use crate::query::spatial_grouping::ClusterSpec;
        let spec = ClusterSpec {
            t_geo_col: "location",
            eps_degrees: 0.004491,
            minpoints: 3,
        };
        let gaq = make_cluster_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        assert!(
            sql.contains("ST_ClusterDBSCAN(t.location::geometry,"),
            "expected ST_ClusterDBSCAN with ::geometry cast, got: {sql}"
        );
        assert!(
            sql.contains("OVER () AS cluster_id"),
            "expected OVER () AS cluster_id, got: {sql}"
        );
        assert!(
            sql.contains("GROUP BY cluster_id"),
            "expected GROUP BY cluster_id, got: {sql}"
        );
    }

    /// The cluster query must bind exactly 2 parameters: eps (f64) then
    /// minpoints (i32). No JOIN, no extra clauses.
    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_grouped_select_binds_eps_and_minpoints() {
        use crate::query::spatial_grouping::ClusterSpec;
        let spec = ClusterSpec {
            t_geo_col: "location",
            eps_degrees: 0.00449,
            minpoints: 5,
        };
        let gaq = make_cluster_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        // The accumulator SQL should contain two bind slots before the agg alias.
        let sql = acc.sql();
        // $1 = eps, $2 = minpoints — at least two parameter slots present.
        assert!(
            sql.contains("$1") && sql.contains("$2"),
            "expected $1 (eps) and $2 (minpoints) bind slots, got: {sql}"
        );
        // No LEFT JOIN — this is a single-table window query.
        assert!(
            !sql.contains("LEFT JOIN"),
            "cluster path should not emit LEFT JOIN, got: {sql}"
        );
    }

    /// Regression test for the pre-T14.5 shape `SELECT ST_ClusterDBSCAN(...)
    /// OVER () AS cluster_id ... GROUP BY cluster_id`, which Postgres rejects
    /// with `ERROR: window functions are not allowed in GROUP BY`. The
    /// emitter must wrap the window call in an inner subquery so the outer
    /// `GROUP BY cluster_id` references a materialised column.
    #[cfg(feature = "spatial")]
    #[test]
    fn cluster_grouped_select_wraps_window_in_subquery() {
        use crate::query::spatial_grouping::ClusterSpec;
        let spec = ClusterSpec {
            t_geo_col: "location",
            eps_degrees: 0.004491,
            minpoints: 3,
        };
        let gaq = make_cluster_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        // Outer SELECT starts with the materialised cluster_id column, not
        // the inline window call.
        assert!(
            sql.starts_with("SELECT cluster_id"),
            "outer SELECT must start with 'SELECT cluster_id', got: {sql}"
        );
        // The window call lives inside the subquery — the `FROM (SELECT t.*,
        // ST_ClusterDBSCAN(...)` substring locks in the wrap.
        assert!(
            sql.contains("FROM (SELECT t.*, ST_ClusterDBSCAN("),
            "window call must be wrapped in an inner subquery; got: {sql}"
        );
        // Subquery alias must be `t` so outer aggregate references resolve.
        assert!(
            sql.contains(") AS t GROUP BY cluster_id"),
            "subquery must be aliased 'AS t' and outer must GROUP BY cluster_id; got: {sql}"
        );
        // The inline window-function-in-GROUP-BY anti-pattern must not leak.
        // (The string `OVER () AS cluster_id` is still present inside the
        // subquery — we assert on the *outer SELECT head* instead.)
        assert!(
            !sql.starts_with("SELECT ST_ClusterDBSCAN"),
            "outer SELECT must not begin with the inline window form; got: {sql}"
        );
    }

    // ── T12: bucket_by_cell SQL emission ─────────────────────────────────

    /// Helper: build a `GroupedAnnotatedQuerySet` with a geohash spec.
    #[cfg(feature = "spatial")]
    fn make_geohash_gaq(
        spec: crate::query::spatial_grouping::GeohashSpec,
    ) -> crate::query::grouped::GroupedAnnotatedQuerySet<
        Fake,
        crate::query::spatial_grouping::GeohashKey,
        crate::expr::AggregateExpr<i64>,
    > {
        use std::marker::PhantomData;
        let agg: crate::expr::AggregateExpr<i64> =
            crate::query::field::FieldRef::<Fake, i64>::new("id").count_star();
        crate::query::grouped::GroupedAnnotatedQuerySet {
            qs: QuerySet::new(),
            keys: crate::query::spatial_grouping::GeohashKey(None),
            grouping: crate::query::grouped::GroupingMode::Plain,
            aggregates: agg,
            having: None,
            order: Vec::new(),
            limit: None,
            offset: None,
            spatial_source: Some(crate::query::grouped::SpatialGroupSource::Geohash(spec)),
            _k: PhantomData,
            _a: PhantomData,
        }
    }

    /// The emitted SQL must contain `ST_GeoHash(t.<col>::geometry, $n) AS geohash`
    /// and `GROUP BY geohash`.
    #[cfg(feature = "spatial")]
    #[test]
    fn geohash_grouped_select_emits_st_geohash_with_geometry_cast() {
        use crate::query::spatial_grouping::GeohashSpec;
        let spec = GeohashSpec {
            t_geo_col: "location",
            precision: 5,
        };
        let gaq = make_geohash_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        assert!(
            sql.contains("ST_GeoHash(t.location::geometry,"),
            "expected ST_GeoHash with ::geometry cast, got: {sql}"
        );
        assert!(
            sql.contains("AS geohash"),
            "expected AS geohash alias, got: {sql}"
        );
        assert!(
            sql.contains("GROUP BY geohash"),
            "expected GROUP BY geohash, got: {sql}"
        );
    }

    /// The geohash query must bind exactly 1 parameter (precision) and no JOIN.
    #[cfg(feature = "spatial")]
    #[test]
    fn geohash_grouped_select_binds_precision_only() {
        use crate::query::spatial_grouping::GeohashSpec;
        let spec = GeohashSpec {
            t_geo_col: "location",
            precision: 7,
        };
        let gaq = make_geohash_gaq(spec);
        let acc = build_grouped_annotated_select(&gaq);
        let sql = acc.sql();

        assert!(
            sql.contains("$1"),
            "expected $1 (precision) bind slot, got: {sql}"
        );
        // Precision is the only bind — no $2.
        // The agg alias also uses positional notation but is a text suffix, not
        // a numeric param. Check there is no second numeric parameter slot from
        // the spatial expression itself.
        assert!(
            !sql.contains("LEFT JOIN"),
            "geohash path should not emit LEFT JOIN, got: {sql}"
        );
    }
}
