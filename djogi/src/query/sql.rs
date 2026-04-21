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
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::order::{Direction, NullsOrder};
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
        FilterValue::Decimal(d) => {
            acc.push_bind(d);
        }
        FilterValue::Null => {
            acc.push_null_literal();
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
        FilterValue::Decimal(d) => {
            acc.push_bind(d);
        }
        FilterValue::Null | FilterValue::List(_) | FilterValue::Pair(_, _) => {
            // Same reasoning as `push_filter_value`: enum is
            // `#[non_exhaustive]` at the crate boundary but exhaustive
            // within this crate. Any new variant added to `FilterValue`
            // must also be taught to bind here.
            unreachable!("nested/null FilterValue in IN list — typed FieldRef API prevents this")
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
    }
}

/// Is this condition vacuously `TRUE` at emission time? Walks `And`
/// subtrees recursively so top-level `And(vec![])`, nested
/// `And(vec![True, And(vec![])])`, and similar shapes all collapse to the
/// same identity the emitter treats as "no filter". `Not(Or(vec![]))` is
/// also vacuously TRUE because the emitter renders empty `Or` as `FALSE`
/// and `NOT FALSE` is `TRUE`.
///
/// Used by [`push_where`] to skip the `WHERE` clause entirely rather than
/// emitting `WHERE TRUE`. Keeps logs readable and avoids any chance of an
/// optimizer surprise on trivially-true predicates.
fn is_vacuously_true(c: &Condition) -> bool {
    match c {
        Condition::True => true,
        Condition::And(xs) => xs.iter().all(is_vacuously_true),
        Condition::Not(inner) => matches!(inner.as_ref(), Condition::Or(xs) if xs.is_empty()),
        _ => false,
    }
}

/// Emit the `WHERE ...` clause for a QuerySet, if any. Any top-level
/// condition that collapses to vacuous TRUE (see [`is_vacuously_true`]) is
/// omitted entirely rather than emitted as `WHERE TRUE` — same semantics,
/// cleaner logs, and avoids touching the planner with a trivially-true
/// predicate.
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
    if !is_vacuously_true(&qs.condition) {
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
            // Qualify the column under the parent table when
            // select_related is active — same rationale as `push_col`
            // inside `emit_leaf`.
            if let Some(table) = parent_table {
                acc.push_sql(table);
                acc.push_sql(".");
            }
            acc.push_sql(o.column);
            match o.direction {
                Direction::Asc => {
                    acc.push_sql(" ASC");
                }
                Direction::Desc => {
                    acc.push_sql(" DESC");
                }
            }
            match o.nulls {
                NullsOrder::First => {
                    acc.push_sql(" NULLS FIRST");
                }
                NullsOrder::Last => {
                    acc.push_sql(" NULLS LAST");
                }
                NullsOrder::Default => {}
            }
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

/// Build `SELECT [DISTINCT [ON (...)]] * FROM <table> [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
///
/// The queryset is borrowed, not consumed — terminal methods (`fetch_all`,
/// `fetch_one`, `first`) may need to mutate the queryset (e.g. `fetch_one`
/// overrides the user-set `limit` to 2 so it can distinguish single-row
/// success from multiple-row failure) before or after calling this builder.
pub(crate) fn build_select<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
    let mut acc = SqlAccumulator::new("");
    match &qs.distinct {
        DistinctMode::None => {
            acc.push_sql("SELECT * FROM ");
        }
        DistinctMode::Plain => {
            acc.push_sql("SELECT DISTINCT * FROM ");
        }
        DistinctMode::On(cols) => {
            acc.push_sql("SELECT DISTINCT ON (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                acc.push_sql(c);
            }
            acc.push_sql(") * FROM ");
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

/// Emit `(AGG(..))::CAST` for the scalar-aggregate path — wraps the
/// bare aggregate in parens so the narrowing `::CAST` applies to the
/// whole function call, including any `FILTER (WHERE ...)` tail.
///
/// `cast_to` is pulled from the [`crate::expr::node::ExprNode::Aggregate`]
/// payload; `None` skips the cast entirely (used for `COUNT` /
/// `MIN` / `MAX` where the Postgres return type already decodes into
/// `Out` directly).
pub(crate) fn emit_aggregate_with_cast(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
) {
    if let crate::expr::node::ExprNode::Aggregate { cast_to, .. } = agg
        && cast_to.is_some()
    {
        acc.push_sql("(");
        crate::expr::sql::emit_expr(acc, agg);
        acc.push_sql(")::");
        // Safe because we matched `cast_to.is_some()` above; the
        // pattern binding borrows immutably so we re-extract here.
        if let crate::expr::node::ExprNode::Aggregate {
            cast_to: Some(ty), ..
        } = agg
        {
            acc.push_sql(ty);
        }
    } else {
        crate::expr::sql::emit_expr(acc, agg);
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
    let cast = match agg {
        crate::expr::node::ExprNode::Aggregate { cast_to, .. } => *cast_to,
        _ => None,
    };
    if cast.is_some() {
        acc.push_sql("(");
    }
    crate::expr::sql::emit_expr(acc, agg);
    acc.push_sql(" OVER ()");
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
    T: Model,
    F: FnOnce(&mut SqlAccumulator),
{
    let mut acc = SqlAccumulator::new("");
    // `SELECT t.*` prefix — the `t` alias is what the FROM clause
    // below names the table as. Using `t.*` instead of enumerating
    // every model column keeps the emitter simple; the columns land
    // in the row in schema order and the decode path picks them up by index.
    acc.push_sql("SELECT t.*");
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
            // Append user ordering after the required prefix. Direction /
            // nulls qualifiers only apply to user-supplied columns; the
            // prepended distinct columns use Postgres default ordering
            // (ASC NULLS LAST for most types), which is fine because the
            // outer COUNT does not care about row order.
            for o in qs.ordering.iter() {
                acc.push_sql(", ");
                acc.push_sql(o.column);
                match o.direction {
                    Direction::Asc => {
                        acc.push_sql(" ASC");
                    }
                    Direction::Desc => {
                        acc.push_sql(" DESC");
                    }
                }
                match o.nulls {
                    NullsOrder::First => {
                        acc.push_sql(" NULLS FIRST");
                    }
                    NullsOrder::Last => {
                        acc.push_sql(" NULLS LAST");
                    }
                    NullsOrder::Default => {}
                }
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

    // `SqlAccumulator::sql()` exposes the emitted SQL text — that is what we
    // assert on. Bind values don't appear in `.sql()`, they are tracked
    // separately and substituted as `$1`, `$2`, …; counting placeholders is
    // the unit-test-level proxy for "the right number of binds were made".

    #[test]
    fn select_no_filter_omits_where() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT * FROM fakes");
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
        assert!(acc.sql().contains("SELECT DISTINCT * FROM fakes"));
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
        let qs: QuerySet<Fake> = QuerySet::new().order_by(|_| crate::query::order::OrderExpr {
            column: "title",
            direction: Direction::Asc,
            nulls: NullsOrder::Last,
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
            sql.contains("SELECT DISTINCT ON (title, view_count) * FROM fakes"),
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
        let mut qs: QuerySet<Fake> = QuerySet::new().order_by(|_| crate::query::order::OrderExpr {
            column: "view_count",
            direction: Direction::Desc,
            nulls: NullsOrder::Last,
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
        assert_eq!(sql, "SELECT * FROM fakes");
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
        assert_eq!(sql, "SELECT * FROM fakes");
    }

    #[test]
    fn where_skipped_on_not_empty_or() {
        // `Not(Or(vec![]))` emits as `NOT FALSE` → `TRUE`, which is
        // vacuously true. Handled by the same skip path.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = Condition::Not(Box::new(Condition::Or(Vec::new())));
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT * FROM fakes");
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

    use crate::descriptor::{FieldDescriptor, FieldSqlType, PkType};
    use crate::relation::select_related::ErasedSelectRelated;

    // Minimal static descriptor for a joined child. Column layout is a
    // stand-in for `Owner` in the integration suite — `id` is the
    // framework column that triggers the ambiguity bug when both sides
    // of the join contribute it bare.
    static OWNERS_JOIN_DESC: ModelDescriptor = ModelDescriptor {
        type_name: "Owner",
        table_name: "owners_p3",
        pk_type: PkType::HeerId,
        fields: &[FieldDescriptor {
            name: "id",
            sql_type: FieldSqlType::BigInt,
            nullable: false,
            unique: true,
            indexed: true,
            max_length: None,
            renamed_from: None,
            rationale: None,
            outbox_exclude: false,
            sequence_within: None,
            index_type: None,
            relation_kind: None,
            on_delete: None,
            target_type_name: None,
            projection_map: &[],
        }],
        partition_by: None,
        has_outbox: false,
        idempotency_key: None,
        tenant_key: None,
        cache_ttl: None,
        rationale: None,
        indexes: &[],
        is_through: false,
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
        let mut qs: QuerySet<Fake> = QuerySet::new().order_by(|_| crate::query::order::OrderExpr {
            column: "created_at",
            direction: Direction::Asc,
            nulls: NullsOrder::Default,
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
}
