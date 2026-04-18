//! SQL emission — walks `Condition` + `QuerySet` state and populates a
//! [`sqlx::QueryBuilder<Postgres>`] with correct positional binds.
//!
//! # What
//!
//! The public entry points are [`build_select`], [`build_count`], and
//! [`build_exists`]. Each consumes a borrowed [`QuerySet<T>`] and returns a
//! pre-populated [`sqlx::QueryBuilder<'_, sqlx::Postgres>`] ready for
//! [`build_query_as`](sqlx::QueryBuilder::build_query_as) or
//! [`build_query_scalar`](sqlx::QueryBuilder::build_query_scalar) at the
//! terminal-method call site.
//!
//! # Why
//!
//! Every value flows through [`sqlx::QueryBuilder::push_bind`] — **never**
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
//! Consumed by [`crate::query::terminal`], which wraps each builder in the
//! appropriate `fetch_all` / `fetch_one` / `fetch_optional` / `scalar` call
//! against a user-provided `sqlx::Executor`. The emitter never executes SQL
//! — that is the terminal layer's responsibility.

use crate::model::Model;
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::order::{Direction, NullsOrder};
use crate::query::queryset::{DistinctMode, QuerySet};
use sqlx::{Postgres, QueryBuilder};

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

/// Push a scalar [`FilterValue`] onto the builder as a single bound
/// parameter. `List` / `Pair` are compound and are handled at the operator
/// level (`IN`, `NOT IN`, `BETWEEN`) — reaching this function with either
/// variant is a framework bug, not a user error.
///
/// `Null` is emitted as the literal token `NULL`; it is never bound because
/// Postgres distinguishes `col = $1 (NULL)` (always false) from `col IS NULL`
/// at the SQL level. The typed `FieldRef::is_null` / `is_not_null` lookups
/// never route NULL through this path — they take the explicit `IS NULL` /
/// `IS NOT NULL` operator branch.
fn push_filter_value(qb: &mut QueryBuilder<'_, Postgres>, v: FilterValue) {
    match v {
        FilterValue::String(s) => {
            qb.push_bind(s);
        }
        FilterValue::I16(n) => {
            qb.push_bind(n);
        }
        FilterValue::I32(n) => {
            qb.push_bind(n);
        }
        FilterValue::I64(n) => {
            qb.push_bind(n);
        }
        FilterValue::F32(n) => {
            qb.push_bind(n);
        }
        FilterValue::F64(n) => {
            qb.push_bind(n);
        }
        FilterValue::Bool(b) => {
            qb.push_bind(b);
        }
        FilterValue::DateTime(d) => {
            qb.push_bind(d);
        }
        FilterValue::Date(d) => {
            qb.push_bind(d);
        }
        FilterValue::Uuid(u) => {
            qb.push_bind(u);
        }
        FilterValue::HeerId(h) => {
            qb.push_bind(h);
        }
        FilterValue::RanjId(r) => {
            qb.push_bind(r);
        }
        FilterValue::Null => {
            qb.push("NULL");
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

/// Emit a list element for `IN (...)` / `NOT IN (...)` via a
/// [`Separated`](sqlx::query_builder::Separated) writer. Factored out of
/// `emit_leaf`'s `In`/`NotIn` arm so the variant switch lives in one place;
/// `Separated` internally handles the `, ` between successive binds.
fn push_list_element(
    sep: &mut sqlx::query_builder::Separated<'_, '_, Postgres, &'static str>,
    v: FilterValue,
) {
    match v {
        FilterValue::String(s) => {
            sep.push_bind(s);
        }
        FilterValue::I16(n) => {
            sep.push_bind(n);
        }
        FilterValue::I32(n) => {
            sep.push_bind(n);
        }
        FilterValue::I64(n) => {
            sep.push_bind(n);
        }
        FilterValue::F32(n) => {
            sep.push_bind(n);
        }
        FilterValue::F64(n) => {
            sep.push_bind(n);
        }
        FilterValue::Bool(b) => {
            sep.push_bind(b);
        }
        FilterValue::DateTime(d) => {
            sep.push_bind(d);
        }
        FilterValue::Date(d) => {
            sep.push_bind(d);
        }
        FilterValue::Uuid(u) => {
            sep.push_bind(u);
        }
        FilterValue::HeerId(h) => {
            sep.push_bind(h);
        }
        FilterValue::RanjId(r) => {
            sep.push_bind(r);
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
/// to `qb.push(col)` without quoting. The value always goes through
/// `push_bind`.
fn emit_leaf(qb: &mut QueryBuilder<'_, Postgres>, leaf: Leaf) {
    let col = leaf.column;
    match leaf.op {
        LookupOp::Eq => {
            qb.push(col);
            qb.push(" = ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Neq => {
            qb.push(col);
            qb.push(" <> ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Gt => {
            qb.push(col);
            qb.push(" > ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Gte => {
            qb.push(col);
            qb.push(" >= ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Lt => {
            qb.push(col);
            qb.push(" < ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Lte => {
            qb.push(col);
            qb.push(" <= ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::IsNull => {
            qb.push(col);
            qb.push(" IS NULL");
        }
        LookupOp::IsNotNull => {
            qb.push(col);
            qb.push(" IS NOT NULL");
        }
        LookupOp::IContains => {
            qb.push(col);
            qb.push(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IContains requires FilterValue::String"),
            };
            qb.push_bind(format!("%{}%", escape_like(&s)));
        }
        LookupOp::IStartsWith => {
            qb.push(col);
            qb.push(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IStartsWith requires FilterValue::String"),
            };
            qb.push_bind(format!("{}%", escape_like(&s)));
        }
        LookupOp::IEndsWith => {
            qb.push(col);
            qb.push(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IEndsWith requires FilterValue::String"),
            };
            qb.push_bind(format!("%{}", escape_like(&s)));
        }
        LookupOp::IExact => {
            qb.push("LOWER(");
            qb.push(col);
            qb.push(") = LOWER(");
            push_filter_value(qb, leaf.value);
            qb.push(")");
        }
        LookupOp::Regex => {
            qb.push(col);
            qb.push(" ~ ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::IRegex => {
            qb.push(col);
            qb.push(" ~* ");
            push_filter_value(qb, leaf.value);
        }
        LookupOp::Between => {
            let (a, b) = match leaf.value {
                FilterValue::Pair(a, b) => (*a, *b),
                _ => unreachable!("Between requires FilterValue::Pair"),
            };
            qb.push(col);
            qb.push(" BETWEEN ");
            push_filter_value(qb, a);
            qb.push(" AND ");
            push_filter_value(qb, b);
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
                    qb.push("FALSE");
                } else {
                    qb.push("TRUE");
                }
                return;
            }
            qb.push(col);
            qb.push(if matches!(leaf.op, LookupOp::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            // `separated(", ")` handles the inter-element comma; each element
            // is still a separate bound parameter (`$n`).
            let mut sep = qb.separated(", ");
            for v in list {
                push_list_element(&mut sep, v);
            }
            qb.push(")");
        }
    }
}

/// Walk a [`Condition`] tree and emit the corresponding SQL fragment.
/// Called recursively for `Not`/`And`/`Or`. The input is consumed by value
/// because payloads (`String`, `Vec<FilterValue>`, `Box<FilterValue>`) move
/// into the builder one bind at a time.
fn emit_condition(qb: &mut QueryBuilder<'_, Postgres>, c: Condition) {
    match c {
        Condition::True => {
            qb.push("TRUE");
        }
        Condition::Leaf(l) => {
            emit_leaf(qb, l);
        }
        Condition::Not(inner) => {
            qb.push("NOT (");
            emit_condition(qb, *inner);
            qb.push(")");
        }
        Condition::And(parts) => {
            // Empty `And(vec![])` is the vacuous-truth identity — documented
            // on the `Condition::And` variant. `Condition::and()` never
            // constructs one, but external callers technically can.
            if parts.is_empty() {
                qb.push("TRUE");
                return;
            }
            qb.push("(");
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    qb.push(" AND ");
                }
                emit_condition(qb, p);
            }
            qb.push(")");
        }
        Condition::Or(parts) => {
            // Empty `Or(vec![])` is the vacuous-falsehood identity — see the
            // variant doc and the condition tests.
            if parts.is_empty() {
                qb.push("FALSE");
                return;
            }
            qb.push("(");
            for (i, p) in parts.into_iter().enumerate() {
                if i > 0 {
                    qb.push(" OR ");
                }
                emit_condition(qb, p);
            }
            qb.push(")");
        }
    }
}

/// Emit the `WHERE ...` clause for a QuerySet, if any. `Condition::True`
/// means "no WHERE" and is omitted entirely (rather than `WHERE TRUE`,
/// which is equivalent but visually noisy in logs).
fn push_where<T: Model>(qb: &mut QueryBuilder<'_, Postgres>, qs: &QuerySet<T>) {
    if !matches!(qs.condition, Condition::True) {
        qb.push(" WHERE ");
        // `emit_condition` consumes the tree — clone the borrowed reference
        // so the original QuerySet remains usable (matters for `fetch_one`'s
        // LIMIT-override path, which reuses the same queryset).
        emit_condition(qb, qs.condition.clone());
    }
}

/// Shared tail emitted by SELECT variants: `ORDER BY ...`, `LIMIT $n`,
/// `OFFSET $n`. `WHERE` is emitted separately so count/exists builders can
/// reuse `push_where` without taking the ordering/limit tail.
fn push_tail<T: Model>(qb: &mut QueryBuilder<'_, Postgres>, qs: &QuerySet<T>) {
    push_where(qb, qs);

    if !qs.ordering.is_empty() {
        qb.push(" ORDER BY ");
        for (i, o) in qs.ordering.iter().enumerate() {
            if i > 0 {
                qb.push(", ");
            }
            qb.push(o.column);
            match o.direction {
                Direction::Asc => {
                    qb.push(" ASC");
                }
                Direction::Desc => {
                    qb.push(" DESC");
                }
            }
            match o.nulls {
                NullsOrder::First => {
                    qb.push(" NULLS FIRST");
                }
                NullsOrder::Last => {
                    qb.push(" NULLS LAST");
                }
                NullsOrder::Default => {}
            }
        }
    }

    if let Some(n) = qs.limit {
        qb.push(" LIMIT ");
        qb.push_bind(n);
    }
    if let Some(n) = qs.offset {
        qb.push(" OFFSET ");
        qb.push_bind(n);
    }
}

/// Build `SELECT [DISTINCT [ON (...)]] * FROM <table> [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
///
/// The queryset is borrowed, not consumed — terminal methods (`fetch_all`,
/// `fetch_one`, `first`) may need to mutate the queryset (e.g. `fetch_one`
/// overrides the user-set `limit` to 2 so it can distinguish single-row
/// success from multiple-row failure) before or after calling this builder.
pub(crate) fn build_select<'a, T: Model>(qs: &QuerySet<T>) -> QueryBuilder<'a, Postgres> {
    let mut qb = QueryBuilder::new("");
    match &qs.distinct {
        DistinctMode::None => {
            qb.push("SELECT * FROM ");
        }
        DistinctMode::Plain => {
            qb.push("SELECT DISTINCT * FROM ");
        }
        DistinctMode::On(cols) => {
            qb.push("SELECT DISTINCT ON (");
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    qb.push(", ");
                }
                qb.push(*c);
            }
            qb.push(") * FROM ");
        }
    }
    qb.push(T::table_name());
    push_tail(&mut qb, qs);
    qb
}

/// Build `SELECT COUNT(*) FROM <table> [WHERE ...]`.
///
/// `ORDER BY` / `LIMIT` / `OFFSET` are intentionally not emitted — they
/// don't affect the total row count and including them only slows the
/// query. `DISTINCT` semantics for COUNT (`SELECT COUNT(DISTINCT ...)`) are
/// a Phase 4+ concern: Phase 2 documents that `.distinct().count()`
/// currently counts non-distinct rows. Upgrade path tracked in the Phase 4
/// annotations backlog.
pub(crate) fn build_count<'a, T: Model>(qs: &QuerySet<T>) -> QueryBuilder<'a, Postgres> {
    let mut qb = QueryBuilder::new("SELECT COUNT(*) FROM ");
    qb.push(T::table_name());
    push_where(&mut qb, qs);
    qb
}

/// Build `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
///
/// `LIMIT 1` is inside the EXISTS subquery rather than being passed through
/// the queryset's `limit` slot: EXISTS returns a single boolean regardless
/// of how many rows match, so `LIMIT 1` here is a micro-optimization that
/// tells Postgres to stop scanning once one match is found.
pub(crate) fn build_exists<'a, T: Model>(qs: &QuerySet<T>) -> QueryBuilder<'a, Postgres> {
    let mut qb = QueryBuilder::new("SELECT EXISTS(SELECT 1 FROM ");
    qb.push(T::table_name());
    push_where(&mut qb, qs);
    qb.push(" LIMIT 1)");
    qb
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
        fn get<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create<'a>(
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn delete<'a>(
            self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<(), crate::DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'a>(
            &self,
            _e: impl sqlx::Executor<'a, Database = sqlx::Postgres>,
        ) -> impl std::future::Future<Output = Result<Self, crate::DjogiError>> + Send {
            async { unreachable!() }
        }
    }

    // `QueryBuilder::sql()` exposes the emitted SQL text — that is what we
    // assert on. Bind values don't appear in `.sql()`, they are tracked
    // separately and substituted as `$1`, `$2`, …; counting placeholders is
    // the unit-test-level proxy for "the right number of binds were made".

    #[test]
    fn select_no_filter_omits_where() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let qb = build_select(&qs);
        let sql = qb.sql().trim().to_string();
        assert_eq!(sql, "SELECT * FROM fakes");
    }

    #[test]
    fn select_with_leaf_filter_emits_where_with_one_bind() {
        let qs: QuerySet<Fake> =
            QuerySet::new().filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        let qb = build_select(&qs);
        let sql = qb.sql();
        assert!(sql.contains("WHERE a = $1"), "got: {sql}");
    }

    #[test]
    fn select_with_and_uses_parentheses() {
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))))
            .filter(|_| Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false))));
        let qb = build_select(&qs);
        let sql = qb.sql();
        // Flattened And(vec![a, b]) → "(a = $1 AND b = $2)"
        assert!(sql.contains("WHERE (a = $1 AND b = $2)"), "got: {sql}");
    }

    #[test]
    fn select_with_exclude_wraps_not() {
        let qs: QuerySet<Fake> = QuerySet::new()
            .exclude(|_| Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true))));
        let qb = build_select(&qs);
        let sql = qb.sql();
        assert!(sql.contains("WHERE NOT (a = $1)"), "got: {sql}");
    }

    #[test]
    fn select_distinct_plain_emits_distinct_keyword() {
        let qs: QuerySet<Fake> = QuerySet::new().distinct();
        let qb = build_select(&qs);
        assert!(qb.sql().contains("SELECT DISTINCT * FROM fakes"));
    }

    #[test]
    fn select_limit_offset_pushes_two_binds() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5);
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
        assert!(sql.contains("deleted_at IS NULL"), "got: {sql}");
        // No placeholder should appear — IS NULL is operator-only.
        assert!(!sql.contains('$'), "expected no binds, got: {sql}");
    }

    #[test]
    fn count_ignores_order_limit_offset() {
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5);
        let qb = build_count(&qs);
        let sql = qb.sql();
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
        let qb = build_exists(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
        assert!(sql.contains("ORDER BY title ASC NULLS LAST"), "got: {sql}");
    }

    #[test]
    fn distinct_on_emits_column_list() {
        // Hand-build the DistinctMode::On variant — skipping the typed
        // builder surface keeps this unit test independent of FieldRef
        // machinery (tested separately).
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.distinct = DistinctMode::On(vec!["title", "view_count"]);
        let qb = build_select(&qs);
        let sql = qb.sql();
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
        let qb = build_select(&qs);
        let sql = qb.sql();
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
}
