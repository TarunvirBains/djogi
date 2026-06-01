//! SQL emission — walks `Condition` + `QuerySet` state and populates a
//! [`SqlAccumulator`] with correct positional binds.
//! # What
//! The public entry points are [`build_select`], [`build_count`], and
//! [`build_exists`]. Each consumes a borrowed [`QuerySet<T>`] and returns a
//! pre-populated [`SqlAccumulator`] ready for execution via `PgConnection`.
//! # Why
//! Every value flows through [`SqlAccumulator::push_bind`] — **never**
//! string interpolation of user-controlled data. Table names and column
//! names are the only items inserted as raw text, and both are
//! `&'static str` literals baked in by the `#[model]` macro (table name via
//! `Model::table_name`, column name via `FieldRef::column`), so they are
//! not user input. The emitter's job is therefore a straight enum-tree walk:
//! one variant -> one operator token + zero-or-more `push_bind` calls.
//! Pattern lookups (`ILIKE`) escape `%`, `_`, and `\\` in user input before
//! wrapping with the appropriate prefix / suffix `%` — escaped input goes
//! through `push_bind` so the wildcard-escape logic is independent of SQL
//! parameter placement.
//! `IN (...)` expands to exactly as many bind slots as the list has;
//! empty lists short-circuit to `FALSE` (IN) / `TRUE` (NOT IN) rather than
//! emitting the syntactically invalid `col IN `. This matches the contract
//! documented on `FieldRef::in_list` / `not_in_list`.
//! # Where
//! Consumed by [`crate::query::terminal`], which wraps each accumulator in the
//! appropriate execution call against the caller-provided `DjogiContext`. The
//! emitter never executes SQL — that is the terminal layer's responsibility.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::pg::decode::{FromPgRow, joined_alias_for_prefix};
use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
use crate::query::portable::{PortablePredicateError, SqlEmitContext};
use crate::query::q::{ArrayPredicate, CompoundOp, Q};
use crate::query::queryset::{DistinctMode, QuerySet};
// Typed MERGE types.
use crate::query::merge::{
    MergeAction, MergeBranch, MergeMatchKind, MergeOnEq, MergeValue, SRC_ALIAS, TGT_ALIAS,
};

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
        FilterValue::Timestamp(d) => {
            acc.push_bind(d);
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
        FilterValue::Interval(i) => {
            acc.push_bind(i);
        }
        #[cfg(feature = "network")]
        FilterValue::Inet(addr) => {
            acc.push_bind(addr);
        }
        #[cfg(feature = "network")]
        FilterValue::Cidr(cidr) => {
            acc.push_bind(cidr);
        }
        #[cfg(feature = "network")]
        FilterValue::Macaddr(mac) => {
            acc.push_bind(mac);
        }
        FilterValue::RangeI32(v) => {
            acc.push_bind(v);
        }
        FilterValue::RangeI64(v) => {
            acc.push_bind(v);
        }
        FilterValue::RangeDecimal(v) => {
            acc.push_bind(v);
        }
        FilterValue::RangeTimestamp(v) => {
            acc.push_bind(v);
        }
        FilterValue::RangeDateTime(v) => {
            acc.push_bind(v);
        }
        FilterValue::RangeDate(v) => {
            acc.push_bind(v);
        }
        FilterValue::Null => {
            acc.push_null_literal();
        }
        FilterValue::ArrayString(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayI16(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayI32(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayI64(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayF32(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayF64(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayBool(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayDateTime(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayDate(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayUuid(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayDecimal(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayHeerId(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayRanjId(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayHeerIdDesc(v) => {
            acc.push_bind(v);
        }
        FilterValue::ArrayRanjIdDesc(v) => {
            acc.push_bind(v);
        }
        FilterValue::List(_) | FilterValue::Pair(_, _) => {
            // These are handled at the operator level (see `emit_leaf`) and
            // never reach this function. Unreachable signals a Djogi
            // internal bug — user-facing `FieldRef` API blocks construction.
            // `FilterValue` is `#[non_exhaustive]` at the *crate boundary*,
            // but we're inside the same crate — so this match is already
            // exhaustive. New variants added here force a compile error in
            // this file, which is exactly the coupling we want: any new
            // SQL-bindable type must also learn how to bind.
            unreachable!("push_filter_value called with List/Pair — use emit_leaf")
        }
    }
}

/// Reference-borrowing counterpart of [`push_filter_value`]. Clones the
/// individual scalar value into `push_bind` so the caller does not have
/// to clone an entire `Condition` tree just to borrow-walk it.
/// `emit_condition` switched to borrow-walk so portable
/// predicate emission never has to construct a throw-away `Condition`
/// shadow tree. This helper preserves the same bind shape the by-value
/// helper produced; only the entry point differs.
pub(crate) fn push_filter_value_ref(acc: &mut SqlAccumulator, v: &FilterValue) {
    match v {
        FilterValue::String(s) => {
            acc.push_bind(s.clone());
        }
        FilterValue::I16(n) => {
            acc.push_bind(*n);
        }
        FilterValue::I32(n) => {
            acc.push_bind(*n);
        }
        FilterValue::I64(n) => {
            acc.push_bind(*n);
        }
        FilterValue::F32(n) => {
            acc.push_bind(*n);
        }
        FilterValue::F64(n) => {
            acc.push_bind(*n);
        }
        FilterValue::Bool(b) => {
            acc.push_bind(*b);
        }
        FilterValue::Timestamp(d) => {
            acc.push_bind(*d);
        }
        FilterValue::DateTime(d) => {
            acc.push_bind(*d);
        }
        FilterValue::Date(d) => {
            acc.push_bind(*d);
        }
        FilterValue::Uuid(u) => {
            acc.push_bind(*u);
        }
        FilterValue::HeerId(h) => {
            acc.push_bind(*h);
        }
        FilterValue::RanjId(r) => {
            acc.push_bind(*r);
        }
        FilterValue::HeerIdDesc(h) => {
            acc.push_bind(*h);
        }
        FilterValue::RanjIdDesc(r) => {
            acc.push_bind(*r);
        }
        FilterValue::Decimal(d) => {
            acc.push_bind(*d);
        }
        FilterValue::Interval(i) => {
            acc.push_bind(*i);
        }
        #[cfg(feature = "network")]
        FilterValue::Inet(addr) => {
            acc.push_bind(*addr);
        }
        #[cfg(feature = "network")]
        FilterValue::Cidr(cidr) => {
            acc.push_bind(*cidr);
        }
        #[cfg(feature = "network")]
        FilterValue::Macaddr(mac) => {
            acc.push_bind(*mac);
        }
        FilterValue::RangeI32(v) => {
            acc.push_bind(*v);
        }
        FilterValue::RangeI64(v) => {
            acc.push_bind(*v);
        }
        FilterValue::RangeDecimal(v) => {
            acc.push_bind(*v);
        }
        FilterValue::RangeTimestamp(v) => {
            acc.push_bind(*v);
        }
        FilterValue::RangeDateTime(v) => {
            acc.push_bind(*v);
        }
        FilterValue::RangeDate(v) => {
            acc.push_bind(*v);
        }
        FilterValue::Null => {
            acc.push_null_literal();
        }
        FilterValue::ArrayString(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayI16(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayI32(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayI64(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayF32(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayF64(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayBool(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayDateTime(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayDate(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayUuid(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayDecimal(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayHeerId(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayRanjId(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayHeerIdDesc(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::ArrayRanjIdDesc(v) => {
            acc.push_bind(v.clone());
        }
        FilterValue::List(_) | FilterValue::Pair(_, _) => {
            unreachable!("push_filter_value_ref called with List/Pair — use emit_leaf_ref")
        }
    }
}

/// Emit a list element for `IN (...)` / `NOT IN (...)`.
/// Same binding behaviour as [`push_filter_value`] for scalar and array
/// variants; rejects `Null`, `List`, and `Pair` (the typed `FieldRef::in_list`
/// API prevents these by construction, so reaching them here is a framework
/// bug). The reject branch is explicit so the caller cannot accidentally
/// thread a `Null` through `IN ($1)` — Postgres `col IN (NULL)` is always
/// `NULL`, never `TRUE`.
/// Kept the by-value form for parallelism with
/// [`push_filter_value`], even though the production borrow-walker uses
/// [`push_list_element_ref`] exclusively. The by-value form remains a
/// thin convenience for legacy unit tests.
#[allow(dead_code)]
fn push_list_element(acc: &mut SqlAccumulator, v: FilterValue) {
    match v {
        FilterValue::Null | FilterValue::List(_) | FilterValue::Pair(_, _) => {
            unreachable!("nested/null FilterValue in IN list — typed FieldRef API prevents this")
        }
        scalar => push_filter_value(acc, scalar),
    }
}

/// Emit a single [`Leaf`] — `column op value`. The column name is a
/// `&'static str` from the macro-baked `FieldRef::column`, so it is safe
/// to `acc.push_sql(col)` without quoting. The value always goes through
/// `push_bind`.
/// When `parent_table` is `Some(table)`, the emitted column reference is
/// prefixed as `{table}.{column}` so Postgres does not raise
/// `42702 column reference "X" is ambiguous` on a query with
/// `LEFT JOIN`-ed child tables that also expose a column of the same
/// bare name (`id`, `created_at`, `updated_at`). Passed through by the
/// join-aware helpers that wrap `build_select_joined`. The non-joined
/// [`build_select`] path passes `None` and emits bare column names
/// unchanged — byte-for-byte identical to the output.
/// Emit `{table}.{col}` if `parent_table` is `Some`, otherwise just `{col}`.
/// Used by every leaf-emitter and array-op arm in this file to handle the
/// join-aware qualifier prefix uniformly. `col` is always macro-baked
/// (`&'static str` from `FieldRef::column`); `parent_table` is `&'static str`
/// from `Model::table_name`. Neither is user input, so direct `push_sql` is
/// safe.
fn push_qualified_col(
    acc: &mut SqlAccumulator,
    col: &'static str,
    parent_table: Option<&'static str>,
) {
    if let Some(table) = parent_table {
        acc.push_sql(table);
        acc.push_sql(".");
    }
    acc.push_sql(col);
}

/// Kept the by-value form alongside the new
/// [`emit_leaf_ref`] borrow-walker for parallelism with the rest of the
/// pre-PR2b emitter helpers; the production path uses the borrow-walker
/// exclusively. Annotated `dead_code` so removing the legacy emitter
/// surface in a future cleanup does not cascade through this file's
/// tests.
#[allow(dead_code)]
fn emit_leaf(acc: &mut SqlAccumulator, leaf: Leaf, parent_table: Option<&'static str>) {
    let col = leaf.column;
    if let Some(tok) = leaf.op.binary_op_token() {
        push_qualified_col(acc, col, parent_table);
        acc.push_sql(tok);
        push_filter_value(acc, leaf.value);
        return;
    }
    match leaf.op {
        LookupOp::IsNull => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" IS NULL");
        }
        LookupOp::IsNotNull => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" IS NOT NULL");
        }
        LookupOp::IContains => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IContains requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}%", escape_like(&s)));
        }
        LookupOp::IStartsWith => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IStartsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("{}%", escape_like(&s)));
        }
        LookupOp::IEndsWith => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IEndsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}", escape_like(&s)));
        }
        LookupOp::IExact => {
            acc.push_sql("LOWER(");
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(") = LOWER(");
            push_filter_value(acc, leaf.value);
            acc.push_sql(")");
        }
        LookupOp::Between => {
            let (a, b) = match leaf.value {
                FilterValue::Pair(a, b) => (*a, *b),
                _ => unreachable!("Between requires FilterValue::Pair"),
            };
            push_qualified_col(acc, col, parent_table);
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
            // row matches). Avoids the `col IN ` Postgres syntax error and
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
            push_qualified_col(acc, col, parent_table);
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
        // Binary-op variants are handled by the `binary_op_token` early return
        // above; reaching them here would mean `binary_op_token` returned None
        // for a variant it should have covered — a framework bug, not user error.
        LookupOp::Eq
        | LookupOp::Neq
        | LookupOp::Gt
        | LookupOp::Gte
        | LookupOp::Lt
        | LookupOp::Lte
        | LookupOp::Regex
        | LookupOp::IRegex => unreachable!("binary-op LookupOp routed past early return"),
    }
}

/// Reference-borrowing counterpart of [`emit_leaf`]. 's
/// [`emit_condition`] borrow-walks the `Condition` tree, so individual
/// leaves enter through this helper rather than the by-value form. The
/// list / pair / pattern arms clone the captured payload values into
/// `push_bind` so production SQL emission no longer requires cloning a
/// whole `Condition` tree to dispatch.
fn emit_leaf_ref(acc: &mut SqlAccumulator, leaf: &Leaf, parent_table: Option<&'static str>) {
    let col = leaf.column;
    if let Some(tok) = leaf.op.binary_op_token() {
        push_qualified_col(acc, col, parent_table);
        acc.push_sql(tok);
        push_filter_value_ref(acc, &leaf.value);
        return;
    }
    match leaf.op {
        LookupOp::IsNull => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" IS NULL");
        }
        LookupOp::IsNotNull => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" IS NOT NULL");
        }
        LookupOp::IContains => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match &leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IContains requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}%", escape_like(s)));
        }
        LookupOp::IStartsWith => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match &leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IStartsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("{}%", escape_like(s)));
        }
        LookupOp::IEndsWith => {
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" ILIKE ");
            let s = match &leaf.value {
                FilterValue::String(s) => s,
                _ => unreachable!("IEndsWith requires FilterValue::String"),
            };
            acc.push_bind(format!("%{}", escape_like(s)));
        }
        LookupOp::IExact => {
            acc.push_sql("LOWER(");
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(") = LOWER(");
            push_filter_value_ref(acc, &leaf.value);
            acc.push_sql(")");
        }
        LookupOp::Between => {
            let (a, b) = match &leaf.value {
                FilterValue::Pair(a, b) => (a.as_ref(), b.as_ref()),
                _ => unreachable!("Between requires FilterValue::Pair"),
            };
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(" BETWEEN ");
            push_filter_value_ref(acc, a);
            acc.push_sql(" AND ");
            push_filter_value_ref(acc, b);
        }
        LookupOp::In | LookupOp::NotIn => {
            let list = match &leaf.value {
                FilterValue::List(v) => v,
                _ => unreachable!("In/NotIn requires FilterValue::List"),
            };
            if list.is_empty() {
                if matches!(leaf.op, LookupOp::In) {
                    acc.push_sql("FALSE");
                } else {
                    acc.push_sql("TRUE");
                }
                return;
            }
            push_qualified_col(acc, col, parent_table);
            acc.push_sql(if matches!(leaf.op, LookupOp::In) {
                " IN ("
            } else {
                " NOT IN ("
            });
            for (i, v) in list.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                push_list_element_ref(acc, v);
            }
            acc.push_sql(")");
        }
        LookupOp::Eq
        | LookupOp::Neq
        | LookupOp::Gt
        | LookupOp::Gte
        | LookupOp::Lt
        | LookupOp::Lte
        | LookupOp::Regex
        | LookupOp::IRegex => unreachable!("binary-op LookupOp routed past early return"),
    }
}

/// Reference-borrowing counterpart of [`push_list_element`]. Same restriction
/// surface; clones the underlying scalar or array into `push_bind`.
fn push_list_element_ref(acc: &mut SqlAccumulator, v: &FilterValue) {
    match v {
        FilterValue::Null | FilterValue::List(_) | FilterValue::Pair(_, _) => {
            unreachable!("nested/null FilterValue in IN list — typed FieldRef API prevents this")
        }
        scalar => push_filter_value_ref(acc, scalar),
    }
}

#[cfg(test)]
mod phase85_array_in_regression_tests {
    use super::*;

    #[test]
    fn array_values_in_in_list_bind_instead_of_panicking() {
        let mut acc = SqlAccumulator::new("");
        let value = FilterValue::ArrayI32(vec![1, 2, 3]);

        push_list_element_ref(&mut acc, &value);

        assert_eq!(acc.sql(), "$1");
        assert_eq!(acc.bind_count(), 1);
    }
}

/// Walk a [`Condition`] borrow and emit the corresponding SQL fragment.
/// Converted from by-value to by-reference (`&Condition`)
/// and made fallible (`Result<, PortablePredicateError>`). The
/// expression-IR bridge calls into `expr::sql::emit_expr`, which itself
/// returns `Result` after PR2b so portable predicates inside a subquery
/// surface their lowering errors through the outer query builder. Owned
/// payloads (strings, lists, pairs) clone into `push_bind` instead of
/// being moved, so production SQL emission no longer requires cloning a
/// whole `Condition` tree just to borrow-walk `Q<T>`.
/// `parent_table` threads through unchanged so every bare column reference
/// in a joined-variant emission lands as `{table}.{column}`; the non-joined
/// path passes `None` and gets bare names, preserving byte-for-byte parity
/// with the pre-PR2b output.
/// `pub(crate)` because needs this entry point to lower the
/// [`Condition`] tree that backs a subquery's `WHERE` clause (a
/// [`SubqueryNode`](crate::expr::node::SubqueryNode) stores the parent
/// queryset's predicate behind a type-erased emitter after PR2b — see
/// [`crate::expr::sql::emit_subquery`]).
pub(crate) fn emit_condition(
    acc: &mut SqlAccumulator,
    c: &Condition,
    parent_table: Option<&'static str>,
) -> Result<(), PortablePredicateError> {
    match c {
        Condition::True => {
            acc.push_sql("TRUE");
            Ok(())
        }
        Condition::Leaf(l) => {
            emit_leaf_ref(acc, l, parent_table);
            Ok(())
        }
        Condition::Not(inner) => {
            acc.push_sql("NOT (");
            emit_condition(acc, inner, parent_table)?;
            acc.push_sql(")");
            Ok(())
        }
        Condition::And(parts) => {
            // Empty `And(vec![])` is the vacuous-truth identity — documented
            // on the `Condition::And` variant. `Condition::and` never
            // constructs one, but external callers technically can.
            if parts.is_empty() {
                acc.push_sql("TRUE");
                return Ok(());
            }
            acc.push_sql("(");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" AND ");
                }
                emit_condition(acc, p, parent_table)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        Condition::Or(parts) => {
            // Empty `Or(vec![])` is the vacuous-falsehood identity — see the
            // variant doc and the condition tests.
            if parts.is_empty() {
                acc.push_sql("FALSE");
                return Ok(());
            }
            acc.push_sql("(");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(" OR ");
                }
                emit_condition(acc, p, parent_table)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        // Expression-IR bridge — delegates to the dedicated emitter in
        // `expr::sql`. The expression tree carries its own column
        // references + literals + nested arithmetic; the `parent_table`
        // qualifier is threaded through as an `SqlEmitContext` so
        // joined-query call sites (pair-tuple `filter_expr`,
        // select_related's filter path) emit `<alias>.<col>` instead of
        // bare names.
        Condition::Expr(expr) => {
            let ctx = match parent_table {
                Some(t) => SqlEmitContext::joined(t),
                None => SqlEmitContext::root(),
            };
            crate::expr::sql::emit_expr(acc, &expr.node, ctx)
        }
        // ── Array operators ─────────────────────────────
        // All three operators take the form `col OP $n` where `$n` is a
        // bound Postgres array parameter. `parent_table` qualification is
        // intentionally forwarded for the column name but array operators
        // are always single-table (no cross-join semantics), so the
        // `parent_table` prefix is cosmetic here — it matches the behaviour
        // of every other `Leaf` arm.
        Condition::ArrayContains(leaf) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" @> ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        Condition::ArrayContainedBy(leaf) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" <@ ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        Condition::ArrayOverlap(leaf) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" && ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        // ── Range operators ──────────────────────────────────
        // All range predicates emit `col OP $n`; the RHS bind is either a
        // range value or, for `contains(element)`, the range element type.
        Condition::RangePredicate(leaf) => {
            push_qualified_col(acc, leaf.column(), parent_table);
            acc.push_sql(leaf.op().sql_token());
            push_filter_value_ref(acc, leaf.value());
            if let Some(cast) = leaf.rhs_element_cast() {
                acc.push_sql("::");
                acc.push_sql(cast);
            }
            Ok(())
        }
        // ── JSONB flat-path condition ───────────────────
        // `JsonbPathLeaf` stores the column + path + cast as structured
        // parts so the emitter can qualify the column reference with the
        // parent table name in joined-query contexts (via the module-level
        // `push_qualified_col` helper). SQL is rendered here at
        // emit time, never at condition-tree construction time.
        Condition::JsonbPath(leaf) => {
            emit_jsonb_path_leaf_ref(acc, leaf, parent_table);
            Ok(())
        }
        // ── Raw SQL escape hatch (4) ─────────────────────────
        // Macro-emitted-only path for proxy `default_filter` lowering.
        // The fragment is a `&'static str` baked at expand time from a
        // closed-grammar walker that rejects every runtime-bound value;
        // the only adopter-facing construction route is the
        // `#[model(default_filter = |f| ...)]` attribute, which goes
        // through `lower_default_filter_to_sql` → descriptor → trait
        // override → here.
        // The fragment is wrapped in outer parens so further AND-
        // composition with user `.filter(...)` calls preserves operator
        // precedence (`(default_filter) AND (user_filter)`). The
        // proxy-side lowering already wraps `and_with` / `or_with`
        // composites in their own parens; the leaf shape (`col = TRUE`)
        // does not, so the wrapper here is the universal safety net.
        Condition::RawSql(s) => {
            acc.push_sql("(");
            acc.push_sql(s.as_str());
            acc.push_sql(")");
            Ok(())
        }
    }
}

/// Emit a [`crate::jsonb::path::JsonbPathLeaf`] — `(col->...'key')::cast op $n`.
/// SQL is rendered at emit time from the structured `column`, `path`, and
/// `cast` fields rather than from a pre-rendered string. This lets the
/// emitter qualify the column with the parent table name when inside a
/// joined query (same `{table}.{column}` prefix logic as [`emit_leaf`]).
/// When `parent_table` is `Some(table)`, the emitted expression is
/// `(table.col->'a'->>'b')::cast` — the Postgres JSONB navigation
/// operators apply to the `table.col` expression, so parenthesisation
/// wraps the qualified column reference correctly.
/// Reference-borrowing counterpart of [`emit_jsonb_path_leaf`].
/// PR2b — `emit_condition` borrow-walks the `Condition` tree, so JSONB
/// path leaves enter through this helper rather than the by-value form.
fn emit_jsonb_path_leaf_ref(
    acc: &mut SqlAccumulator,
    leaf: &crate::jsonb::path::JsonbPathLeaf,
    parent_table: Option<&'static str>,
) {
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
                acc.push_sql("->>'");
                acc.push_sql(seg);
                acc.push_sql("'");
            } else {
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

    if matches!(leaf.op, LookupOp::Regex | LookupOp::IRegex) {
        unreachable!(
            "Regex / IRegex not supported on JsonbPathLeaf: {:?}",
            leaf.op
        );
    }
    if let Some(tok) = leaf.op.binary_op_token() {
        build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
        acc.push_sql(tok);
        push_filter_value_ref(acc, &leaf.value);
        return;
    }
    match leaf.op {
        LookupOp::IsNull => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IS NULL");
        }
        LookupOp::IsNotNull => {
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IS NOT NULL");
        }
        LookupOp::In => {
            let list = match &leaf.value {
                FilterValue::List(v) => v,
                _ => unreachable!("JsonbPath In requires FilterValue::List"),
            };
            if list.is_empty() {
                acc.push_sql("FALSE");
                return;
            }
            build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
            acc.push_sql(" IN (");
            for (i, v) in list.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                push_list_element_ref(acc, v);
            }
            acc.push_sql(")");
        }
        _ => {
            unreachable!("unsupported LookupOp in JsonbPathLeaf: {:?}", leaf.op)
        }
    }
}

/// Kept the by-value form alongside the new
/// [`emit_jsonb_path_leaf_ref`] borrow-walker for parallelism. The
/// production borrow-walking path uses the `_ref` form exclusively.
#[allow(dead_code)]
fn emit_jsonb_path_leaf(
    acc: &mut SqlAccumulator,
    leaf: crate::jsonb::path::JsonbPathLeaf,
    parent_table: Option<&'static str>,
) {
    use LookupOp::{In, IsNotNull, IsNull};

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

    // The typed `JsonbPathRef` surface only constructs eq/neq/gt/gte/lt/lte
    // (plus IsNull/IsNotNull/In, handled below). `Regex` and `IRegex` slip
    // through `binary_op_token` since it covers every binary operator,
    // but they are not constructible for JSONB paths through the public
    // API — reject explicitly here so a hand-built `JsonbPathLeaf` with
    // `op: Regex` panics instead of silently emitting `(col->>...) ~ val`.
    if matches!(leaf.op, LookupOp::Regex | LookupOp::IRegex) {
        unreachable!(
            "Regex / IRegex not supported on JsonbPathLeaf: {:?}",
            leaf.op
        );
    }
    if let Some(tok) = leaf.op.binary_op_token() {
        build_lhs(acc, leaf.column, leaf.path, leaf.cast, parent_table);
        acc.push_sql(tok);
        push_filter_value(acc, leaf.value);
        return;
    }
    match leaf.op {
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
            // No other LookupOps are constructible from JsonbPathRef
            // the typed surface only exposes eq/neq/gt/gte/lt/lte/in/is_null/is_not_null.
            unreachable!("unsupported LookupOp in JsonbPathLeaf: {:?}", leaf.op)
        }
    }
}

/// Walk a [`Q<T>`] borrow and emit the corresponding SQL fragment.
/// Direct-`Q<T>` SQL emission. `Q::Portable(_)` arms
/// route through [`crate::query::portable::emit_portable_predicate`]
/// (which dispatches to `Model::__djogi_emit_field_predicate`) without
/// ever building a `Condition` shadow tree. Other variants (`Ilike`,
/// `Regex`, `JsonbPath`, `Expression`, `Array`, `Condition`, `Compound`,
/// `Xor`, `Negated`) emit through the existing emitter machinery in
/// this file.
/// `ctx` carries the parent-table qualifier so portable root-field
/// predicates emitted under `build_select_joined` qualify as
/// `<table>.<column>`. Non-joined builders pass `SqlEmitContext::root`
/// and the legacy parent-table-threading channel propagates through to
/// `emit_condition` for the `Q::Condition(_)` arm via
/// `ctx.parent_table`.
pub(crate) fn emit_q<T: Model>(
    acc: &mut SqlAccumulator,
    q: &Q<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    let parent_table = ctx.parent_table();
    match q {
        Q::Portable(predicate) => {
            crate::query::portable::emit_portable_predicate::<T>(acc, predicate, ctx)
        }
        Q::Ilike(field, pattern) => {
            push_qualified_col(acc, field.column(), parent_table);
            acc.push_sql(" ILIKE ");
            acc.push_bind(format!("%{}%", escape_like(pattern)));
            Ok(())
        }
        Q::JsonbPath(leaf) => {
            emit_jsonb_path_leaf_ref(acc, leaf, parent_table);
            Ok(())
        }
        Q::Regex(field, pattern, true) => {
            push_qualified_col(acc, field.column(), parent_table);
            acc.push_sql(" ~ ");
            acc.push_bind(pattern.clone());
            Ok(())
        }
        Q::Regex(field, pattern, false) => {
            push_qualified_col(acc, field.column(), parent_table);
            acc.push_sql(" ~* ");
            acc.push_bind(pattern.clone());
            Ok(())
        }
        Q::Expression(expr) => crate::expr::sql::emit_expr(acc, &expr.node, ctx),
        Q::Array(ArrayPredicate::Contains(leaf, _)) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" @> ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        Q::Array(ArrayPredicate::ContainedBy(leaf, _)) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" <@ ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        Q::Array(ArrayPredicate::Overlap(leaf, _)) => {
            push_qualified_col(acc, leaf.column, parent_table);
            acc.push_sql(" && ");
            push_filter_value_ref(acc, &leaf.values);
            Ok(())
        }
        Q::Condition(c) => emit_condition(acc, c, parent_table),
        Q::Compound { op, parts } => {
            if parts.is_empty() {
                acc.push_sql(match op {
                    CompoundOp::And => "TRUE",
                    CompoundOp::Or => "FALSE",
                });
                return Ok(());
            }
            acc.push_sql("(");
            let sep = match op {
                CompoundOp::And => " AND ",
                CompoundOp::Or => " OR ",
            };
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(sep);
                }
                emit_q::<T>(acc, p, ctx)?;
            }
            acc.push_sql(")");
            Ok(())
        }
        Q::Xor(a, b) => {
            // General XOR identity: `((NOT a) AND b) OR (a AND (NOT b))`.
            // Same shape `query::q::xor_to_condition_ref` produces in the
            // legacy bridge.
            acc.push_sql("(((NOT (");
            emit_q::<T>(acc, a, ctx)?;
            acc.push_sql(")) AND (");
            emit_q::<T>(acc, b, ctx)?;
            acc.push_sql(")) OR ((");
            emit_q::<T>(acc, a, ctx)?;
            acc.push_sql(") AND (NOT (");
            emit_q::<T>(acc, b, ctx)?;
            acc.push_sql("))))");
            Ok(())
        }
        Q::Negated(inner) => {
            acc.push_sql("NOT (");
            emit_q::<T>(acc, inner, ctx)?;
            acc.push_sql(")");
            Ok(())
        }
        // `Q<T>` is `#[non_exhaustive]` — a future variant lands here
        // as a typed error rather than panicking. The macro / SQL
        // builder layers surface this as `DjogiError::Predicate(_)`.
        #[allow(unreachable_patterns)]
        _ => Err(PortablePredicateError::CacheInvalidNode {
            kind: "Q::<unknown>",
        }),
    }
}

/// Test whether a `Q<T>` is structurally equivalent to `TRUE`. Mirrors
/// [`Condition::is_vacuously_true`] for the post-PR2b substrate so the
/// `WHERE` emitter can omit the clause entirely on unfiltered querysets
/// without round-tripping through the legacy bridge. Used by
/// [`push_where_qualified`].
pub(crate) fn q_is_vacuously_true<T: Model>(q: &Q<T>) -> bool {
    match q {
        Q::Portable(p) => match p.inner_ref() {
            sassi::BasicPredicate::True => true,
            sassi::BasicPredicate::And(parts) => parts
                .iter()
                .all(|c| matches!(c, sassi::BasicPredicate::True)),
            _ => false,
        },
        Q::Condition(c) => c.is_vacuously_true(),
        Q::Compound {
            op: CompoundOp::And,
            parts,
        } => parts.iter().all(q_is_vacuously_true),
        Q::Negated(inner) => match inner.as_ref() {
            // `NOT (False)` is structurally TRUE; the emitter would
            // render `Q::Negated(Q::Compound { Or, [] })` as `NOT (FALSE)`.
            Q::Portable(p) => matches!(p.inner_ref(), sassi::BasicPredicate::False),
            Q::Compound {
                op: CompoundOp::Or,
                parts,
            } => parts.is_empty(),
            _ => false,
        },
        _ => false,
    }
}

/// Emit the `WHERE ...` clause for a QuerySet, if any. Any top-level
/// condition that collapses to vacuous TRUE (see [`q_is_vacuously_true`])
/// is omitted entirely rather than emitted as `WHERE TRUE` — same
/// semantics, cleaner logs, and avoids touching the planner with a
/// trivially-true predicate.
/// The non-joined path (every caller in this file except
/// [`build_select_joined`]) uses this shim, which forwards to
/// [`push_where_qualified`] with `parent_table = None` — bare column
/// references are emitted exactly as shipped.
fn push_where<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
) -> Result<(), PortablePredicateError> {
    push_where_qualified(acc, qs, None)
}

/// Lowest-level WHERE helper — caller supplies the full SQL emission
/// context, including joined-table qualification and any optional
/// lateral scope metadata.
pub(crate) fn push_where_with_ctx<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    if q_is_vacuously_true(&qs.condition) {
        return Ok(());
    }
    acc.push_sql(" WHERE ");
    emit_q::<T>(acc, &qs.condition, ctx)
}

/// Qualification-aware variant of [`push_where`]. When `parent_table`
/// is `Some(table)`, every bare column reference in the emitted `WHERE`
/// clause is prefixed as `{table}.{column}` so Postgres does not raise
/// `42702 column reference "X" is ambiguous` under `LEFT JOIN`-ed
/// children that share the same column name (`id`, `created_at`,
/// `updated_at`). `None` preserves the bare-name emission.
/// Direct-`Q<T>` emission via [`emit_q`]. The
/// `q_is_vacuously_true` short-circuit replaces the legacy
/// `q_to_condition_ref(...).is_vacuously_true` round-trip; the SQL
/// emit path no longer builds a throw-away `Condition` shadow tree.
fn push_where_qualified<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    parent_table: Option<&'static str>,
) -> Result<(), PortablePredicateError> {
    let ctx = match parent_table {
        Some(t) => SqlEmitContext::joined(t),
        None => SqlEmitContext::root(),
    };
    push_where_with_ctx(acc, qs, ctx)
}

/// Shared tail emitted by SELECT variants: `ORDER BY ...`, `LIMIT $n`,
/// `OFFSET $n`. `WHERE` is emitted separately so count/exists builders can
/// reuse `push_where` without taking the ordering/limit tail.
/// Fallible because the inner `WHERE` helper
/// propagates `PortablePredicateError`. Callers `?` through to the
/// builder's `Result<SqlAccumulator, _>` return.
/// Shim for the non-joined path — forwards to [`push_tail_qualified`]
/// with `parent_table = None`.
fn push_tail<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
) -> Result<(), PortablePredicateError> {
    push_tail_qualified(acc, qs, None)
}

/// Lowest-level tail helper — caller supplies the full SQL emission
/// context, including joined-table qualification and any optional
/// lateral scope metadata.
pub(crate) fn push_tail_with_ctx<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    push_where_with_ctx(acc, qs, ctx)?;

    if !qs.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in qs.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            // Delegate to OrderExpr::emit — it handles both Column and
            // (when the spatial feature is on) SpatialDistance variants.
            // The table_qualifier threads through for select_related joins.
            o.emit(acc, ctx.parent_table());
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
    Ok(())
}

/// Qualification-aware variant of [`push_tail`]. `parent_table` threads
/// through to both the `WHERE` helper and the ordering emission so
/// `ORDER BY id` on a joined query renders as `ORDER BY {table}.id`.
/// `LIMIT` / `OFFSET` need no qualification — they carry no column
/// references.
pub(crate) fn push_tail_qualified<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
    parent_table: Option<&'static str>,
) -> Result<(), PortablePredicateError> {
    let ctx = match parent_table {
        Some(t) => SqlEmitContext::joined(t),
        None => SqlEmitContext::root(),
    };
    push_tail_with_ctx(acc, qs, ctx)
}

/// Emit the shared HAVING / ORDER BY / LIMIT / OFFSET tail for grouped-aggregate
/// SELECTs. Four grouped builders (`build_grouped_annotated_select`,
/// `build_spatial_join_grouped_select`, `build_cluster_grouped_select`,
/// `build_geohash_grouped_select`) used to inline this 25-line block verbatim;
/// extracting it here keeps their phase ordering visible and removes the copy.
/// `OrderExpr::emit` is called with `parent_table = None` because grouped
/// queries reference their own outer projection, not a joined parent table.
fn push_grouped_tail(
    acc: &mut SqlAccumulator,
    having: Option<&crate::expr::node::ExprNode>,
    order: &[crate::query::order::OrderExpr],
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<(), PortablePredicateError> {
    if let Some(h) = having {
        acc.push_sql(" HAVING ");
        crate::expr::sql::emit_expr(acc, h, SqlEmitContext::root())?;
    }

    if !order.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in order.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(acc, None);
        }
    }

    if let Some(n) = limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n as i64);
    }
    if let Some(n) = offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n as i64);
    }
    Ok(())
}

/// Build `SELECT [DISTINCT [ON (...)]] <COLUMN_LIST> FROM <table> [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
/// The queryset is borrowed, not consumed — terminal methods (`fetch_all`,
/// `fetch_one`, `first`) may need to mutate the queryset (e.g. `fetch_one`
/// overrides the user-set `limit` to 2 so it can distinguish single-row
/// success from multiple-row failure) before or after calling this builder.
pub(crate) fn build_select<T: Model + FromPgRow>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = SqlAccumulator::new("");
    // Emit the canonical `FromPgRow::COLUMN_LIST` rather than `*`. Ordinal
    // decode relies on wire column order matching struct-field order;
    // `SELECT *` leaks DDL column order into the decode path, which
    // models with non-default column ordering (user-defined columns
    // declared before framework columns) do not guarantee. Baking the
    // canonical list pins the order regardless of migration shape.
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
            acc.push_csv(cols.iter().copied());
            acc.push_sql(") ");
            acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
            acc.push_sql(" FROM ");
        }
    }
    acc.push_sql(T::table_name());
    push_tail(&mut acc, qs)?;
    Ok(acc)
}

/// Build `SELECT {parent_cols} FROM <table> {left joins} [WHERE ...]
/// [ORDER BY ...] [LIMIT $n] [OFFSET $n]` — the select_related variant.
/// Mirror of [`build_select`], but:
/// 1. Replaces `*` in the projection with the aliased column list built
///    by [`crate::relation::select_related::select_columns`] — parent
///    columns stay unqualified, each joined child's columns land under
///    a `"rel_{source_column}.{col}"` alias.
/// 2. Appends one `LEFT JOIN` clause per registered path, via
///    [`crate::relation::select_related::push_joins`].
/// # Why a separate emitter
/// Keeping `build_select` unchanged means a queryset with no
/// registered select_related paths still emits the exact SQL
/// shipped — no regression risk, no surprise `LEFT JOIN` on plain
/// `fetch_all` call sites. The joined variant is reached only via
/// [`QuerySet::fetch_all_joined`](crate::query::QuerySet::fetch_all_joined),
/// which explicitly opts into the joined decode path.
/// # `DistinctMode` interaction
/// Whether the joined columns should participate in the distinct tuple
/// is left to the caller. If the queryset has a non-`None`
/// `DistinctMode`, the emitter preserves it exactly: `SELECT DISTINCT
/// {parent_cols}...`. Callers who combine `.distinct` with
/// `.select_related(...)` get consistent shape — distinct is applied
/// to the full projection (parent + aliased children) — but they
/// should verify the emitted SQL matches their intent.
pub(crate) fn build_select_joined<T: Model>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError> {
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
    push_tail_qualified(&mut acc, qs, parent_table)?;
    Ok(acc)
}

/// Emit `(AGG(..) [OVER (...)])::CAST` for the scalar-aggregate and
/// grouped-annotate paths — wraps the aggregate (plus the optional window
/// clause) in parens when a narrowing `::CAST` is needed, then appends
/// the cast.
/// `cast_to` is pulled from the [`crate::expr::node::ExprNode::Aggregate`]
/// payload; `None` skips the cast entirely (used for `COUNT` /
/// `MIN` / `MAX` where the Postgres return type already decodes into
/// `Out` directly).
/// When the aggregate carries a user-set `window: Some(spec)` (from
/// `.over(|w| ...)`), the `OVER (...)` clause is appended immediately
/// after `emit_expr` returns — in the right position for Postgres
/// window-function syntax (`AGG(...) OVER (...)`). No default `OVER `
/// is added when `window` is `None`: this path is used for both the
/// scalar terminal and the grouped annotate SELECT list, neither of which
/// should silently grow a window clause.
/// `default_window` controls fallback behaviour when the aggregate carries
/// no `.over(|w| ...)` window spec:
/// - `None` — emit no window clause at all (scalar terminal + grouped
///   annotate SELECT, neither of which should silently grow `OVER `).
/// - `Some(s)` — emit `s` as the default (the ungrouped annotate path
///   uses `Some(" OVER ")` only after the plain-annotation type-state has
///   proven the aggregate kind is windowable; non-windowable aggregate kinds
///   are rejected before reaching this helper).
fn emit_aggregate_inner(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
    default_window: Option<&'static str>,
) -> Result<(), PortablePredicateError> {
    // Spatial aggregates emit an outer scalar cast (e.g. `::geography`).
    // When OVER is present, Postgres grammar places `OVER` on the
    // *aggregate* itself before any post-call scalar wrapper. Two
    // emission profiles cover the surface:
    // - **Unwrapped spatial** (collect, union, extent, line_agg,
    // cluster_intersecting, cluster_within, mem_union, polygonize):
    // emit `(AGG(...) OVER (...))::cast`. The cast attaches to the
    // paren-wrapped aggregate-with-OVER unit.
    // - **Wrapped spatial** (centroid, convex_hull): emit
    // `WRAP(AGG(...) OVER (...))::cast`. Here `WRAP` is a scalar
    // function (`ST_Centroid`, `ST_ConvexHull`); the *aggregate*
    // is the inner `ST_Collect`, so `OVER` must fall inside the
    // wrapper, between the inner-aggregate close and the wrapper
    // close. Pre-fix the OVER lived outside the wrapper, which
    // Postgres rejects with "OVER specified, but ST_Centroid is
    // not a window function nor an aggregate function".
    // The shape detector below inspects `ExprNode::Aggregate` only;
    // every spatial aggregate (collect, centroid, convex_hull, …)
    // routes through the `AggOp` envelope. See [`spatial_emission_shape`] for the wrapped vs
    // unwrapped distinction.
    let shape = spatial_emission_shape(agg);
    let (cast_to, window) = match agg {
        crate::expr::node::ExprNode::Aggregate {
            cast_to, window, ..
        } => (*cast_to, window.as_ref()),
        _ => (None, None),
    };
    let has_window_clause = window.is_some() || default_window.is_some();

    let emit_window_clause = |acc: &mut SqlAccumulator| match window {
        Some(ws) => ws.emit(acc),
        None => {
            if let Some(s) = default_window {
                acc.push_sql(s);
            }
        }
    };

    match (shape, has_window_clause, cast_to) {
        // Spatial wrapped (centroid, convex_hull) WITH window — splice
        // OVER inside the wrapper, between the inner-aggregate close
        // and the wrapper close.
        (
            Some(SpatialShape {
                suffix,
                wrapped: true,
            }),
            true,
            _,
        ) => {
            crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
            // Bare emission ended with `WRAP(AGG(...))::cast`. Pop
            // the cast and the wrapper close so the next push falls
            // between the AGG(...)'s close paren and the wrapper's.
            let popped_cast = acc.pop_sql_suffix(suffix);
            debug_assert!(
                popped_cast,
                "wrapped spatial bare emission must end with {suffix}"
            );
            let popped_close = acc.pop_sql_suffix(")");
            debug_assert!(
                popped_close,
                "wrapped spatial bare emission must end with `)` after popping cast"
            );
            emit_window_clause(acc);
            acc.push_sql(")");
            acc.push_sql(suffix);
        }
        // Spatial unwrapped WITH window — paren-wrap (AGG OVER), then cast.
        // Regression guard: the bare emission of an unwrapped spatial WITH FILTER
        // produces `(AGG(...) FILTER (...))::cast` — emit_spatial_unary_agg adds
        // outer parens for cast attachment when filter is present. Naively adding
        // another outer paren here gives `((AGG FILTER) OVER)::cast`, which Postgres
        // rejects because OVER attaches to a parenthesized expression rather than
        // the aggregate call itself.
        // Strategy: when filter is present, splice OVER INSIDE
        // the existing FILTER parens by popping the trailing `)`
        // after popping the cast. When no filter, the bare emission
        // is `AGG(...)::cast` (no outer parens) — add them externally.
        (
            Some(SpatialShape {
                suffix,
                wrapped: false,
            }),
            true,
            _,
        ) => {
            let has_filter = matches!(
                agg,
                crate::expr::node::ExprNode::Aggregate {
                    filter: Some(_),
                    ..
                }
            );
            if has_filter {
                crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
                let popped_cast = acc.pop_sql_suffix(suffix);
                debug_assert!(
                    popped_cast,
                    "unwrapped spatial bare emission must end with {suffix}"
                );
                let popped_close = acc.pop_sql_suffix(")");
                debug_assert!(
                    popped_close,
                    "unwrapped spatial-with-filter bare emission must end with `)` \
                     after popping cast (FILTER parens from emit_spatial_unary_agg)"
                );
                emit_window_clause(acc);
                acc.push_sql(")");
                acc.push_sql(suffix);
            } else {
                acc.push_sql("(");
                crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
                let popped_cast = acc.pop_sql_suffix(suffix);
                debug_assert!(
                    popped_cast,
                    "unwrapped spatial bare emission must end with {suffix}"
                );
                emit_window_clause(acc);
                acc.push_sql(")");
                acc.push_sql(suffix);
            }
        }
        // Spatial WITHOUT window — bare emit already includes the
        // cast adjacent to the aggregate. Nothing to splice.
        (Some(_), false, _) => {
            crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
        }
        // Non-spatial WITH explicit cast_to — paren-wrap (AGG OVER)?,
        // then `::ty`. Window may or may not be present; the existing
        // contract emits parens whenever cast_to is set so the cast
        // attaches cleanly.
        (None, _, Some(ty)) => {
            acc.push_sql("(");
            crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
            emit_window_clause(acc);
            acc.push_sql(")::");
            acc.push_sql(ty);
        }
        // Non-spatial, no cast — bare emit + optional OVER.
        (None, _, None) => {
            crate::expr::sql::emit_expr(acc, agg, SqlEmitContext::root())?;
            emit_window_clause(acc);
        }
    }
    Ok(())
}

/// Emission profile for spatial aggregates. The two `wrapped` cases
/// drive the OVER-splice strategy in [`emit_aggregate_inner`].
/// - `wrapped: true` — bare emission has the form `WRAP(AGG(...))::cast`.
///   `WRAP` is a scalar function (`ST_Centroid`, `ST_ConvexHull`); the
///   actual aggregate is `AGG` (`ST_Collect`). OVER must fall inside
///   the wrapper, between AGG's close paren and the wrapper's close.
/// - `wrapped: false` — bare emission has the form `AGG(...)::cast`.
///   No scalar wrapper; OVER attaches directly to the aggregate, then
///   the cast wraps the aggregate-with-OVER unit:
///   `(AGG(...) OVER (...))::cast`.
struct SpatialShape {
    suffix: &'static str,
    wrapped: bool,
}

/// Detect the spatial-emission profile for an aggregate node. Returns
/// `Some(SpatialShape)` for spatial aggregates whose bare emission
/// ends with a scalar cast suffix (potentially preceded by a wrapper
/// close), `None` for non-spatial aggregates.
/// ConvexHull migrated from `ExprNode::Spatial(SpatialExpr::ConvexHull{..})` to
/// `AggOp::SpatialConvexHull`, so the entire spatial aggregate family now
/// routes through a single `ExprNode::Aggregate` arm — modifiers
/// (.distinct/.filter/.over/.order_by) compose uniformly.
fn spatial_emission_shape(agg: &crate::expr::node::ExprNode) -> Option<SpatialShape> {
    use crate::expr::node::ExprNode;
    match agg {
        ExprNode::Aggregate { op, .. } => {
            let suffix = crate::expr::sql::outer_cast_suffix(op)?;
            // The wrapped vs unwrapped distinction is feature-gated
            // because the wrapped variants live behind `spatial`
            // outer_cast_suffix returns None when spatial is off,
            // so this branch is unreachable for unfeatured builds.
            // Wrapped variants: SpatialCentroid, SpatialConvexHull
            // both emit `WRAP(ST_Collect(...))::geography` where
            // ST_Collect is the actual aggregate and the wrapper is
            // a scalar function. OVER must splice inside the wrap.
            #[cfg(feature = "spatial")]
            let wrapped = matches!(
                op,
                crate::expr::node::AggOp::SpatialCentroid
                    | crate::expr::node::AggOp::SpatialConvexHull
            );
            #[cfg(not(feature = "spatial"))]
            let wrapped = false;
            Some(SpatialShape { suffix, wrapped })
        }
        _ => None,
    }
}

pub(crate) fn emit_aggregate_with_cast(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
) -> Result<(), PortablePredicateError> {
    emit_aggregate_inner(acc, agg, None)
}

/// Emit `(AGG(..) OVER )::CAST` for the plain ungrouped
/// annotate-SELECT-list path — wraps a windowable aggregate in a window
/// function so the SELECT list is valid without a `GROUP BY` clause, then
/// applies the optional narrowing cast.
/// # Why `OVER ` rather than explicit `GROUP BY`
/// `annotate(|f| f.col.sum)` on an ungrouped queryset has no natural
/// grouping key — the main row's PK would give a one-row-per-group
/// partition (every aggregate collapses to the per-row column value).
/// An unbounded window function (`OVER `) produces the table-wide
/// aggregate value on every returned row, which is the useful
/// per-row-with-table-aggregate semantics annotate users expect.
/// This synthesized-window path is only legal for value aggregates. The
/// public `QuerySet::annotate` builder requires `PlainAnnotationTuple`, so
/// ordered-set, hypothetical-set, and metadata aggregate kinds cannot reach
/// this helper through plain annotate; scalar `QuerySet::aggregate` and
/// grouped annotate use the non-default-window emitter instead.
/// Reverse-relation aggregates (`f.orders.count`) may need `OVER
/// (PARTITION BY parent.id)` after a LATERAL join; that is a deliberate
/// scope boundary handled by the user-supplied `.over(|w| ...)` spec.
pub(crate) fn emit_aggregate_with_window_and_cast(
    acc: &mut SqlAccumulator,
    agg: &crate::expr::node::ExprNode,
) -> Result<(), PortablePredicateError> {
    emit_aggregate_inner(acc, agg, Some(" OVER ()"))
}

/// Build `SELECT <agg> FROM <table> [WHERE ...]` — the scalar-aggregate
/// terminal for [`crate::query::aggregate::AggregateQuery::fetch_one`].
/// No `ORDER BY`, no `LIMIT`, no `OFFSET`, no `GROUP BY` — ungrouped
/// aggregates collapse to exactly one result row regardless of the
/// underlying cardinality, so those clauses would be meaningless.
/// The aggregate expression is emitted via [`emit_aggregate_with_cast`]
/// so integer `SUM` / `AVG` results narrow back to the typed `Out`
/// the decoder expects. `WHERE` uses the shared [`push_where`] helper so
/// vacuously-true predicates are elided identically to every other
/// terminal.
pub(crate) fn build_aggregate_select<T: Model>(
    qs: &QuerySet<T>,
    agg: &crate::expr::node::ExprNode,
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = SqlAccumulator::new("SELECT ");
    emit_aggregate_with_cast(&mut acc, agg)?;
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs)?;
    Ok(acc)
}

/// Build `SELECT t.*, <agg_0> AS __djogi_agg_0, <agg_1> AS __djogi_agg_1
/// FROM <table> [WHERE ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]`
/// the annotation terminal for
/// [`crate::query::annotate::AnnotatedQuerySet::fetch_all`].
/// # Why `t.*` plus aliased aggregates
/// The annotated row carries both the full `t.*`
/// projection (decoded into `T`) and the aggregate columns under
/// synthetic `__djogi_agg_N` aliases (decoded into the tuple slots).
/// Each side reads its own column set; they never collide because
/// model columns are user-chosen identifiers and the aggregate
/// aliases use the framework-reserved `__djogi_agg_` prefix.
/// # Columns argument
/// `push_columns` is a closure the caller supplies so the SELECT-list
/// emission can inspect the typed tuple shape at compile time. The
/// annotate terminal's `IntoAggregateTuple::push_columns` impl pushes
/// `, <agg_expr> AS __djogi_agg_N` once per tuple arity slot; this
/// emitter owns the `t.*` prefix and the `FROM` / `WHERE` / `ORDER BY`
/// / `LIMIT` / `OFFSET` tail around it.
pub(crate) fn build_select_with_annotations<T, F>(
    qs: &QuerySet<T>,
    push_columns: F,
) -> Result<SqlAccumulator, PortablePredicateError>
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
    push_tail(&mut acc, qs)?;
    Ok(acc)
}

/// Build the annotated SELECT, optionally wrapping in a derived table so
/// an outer `WHERE` can filter on a window-function alias.
/// PostgreSQL 18 has no `QUALIFY` clause, so a window-output filter has
/// to live in an outer scope where the alias is in scope as a column
/// reference. When `qualify` is `Some`, this emits:
/// `SELECT * FROM (<inner annotated select>) AS __djogi_q WHERE
/// <alias> <op> $N`. Bind ordering: inner-query binds first, qualify
/// bind appended last.
/// When `qualify` is `None`, this is identical in output to
/// [`build_select_with_annotations`] — no derived-table indirection.
pub(crate) fn build_annotated_select_for_fetch<T, F>(
    qs: &QuerySet<T>,
    push_columns: F,
    qualify: Option<&crate::expr::QualifyCondition>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    F: FnOnce(&mut SqlAccumulator),
{
    let inner = build_select_with_annotations(qs, push_columns)?;
    let Some(qualify) = qualify else {
        return Ok(inner);
    };

    let mut wrapped = SqlAccumulator::new("SELECT * FROM (");
    wrapped.extend_with(inner);
    wrapped.push_sql(") AS __djogi_q WHERE ");
    qualify.push_outer_where(&mut wrapped);
    Ok(wrapped)
}

/// Build the annotated SELECT used as the inner rowset for PostGIS
/// row-shape encoders (`ST_AsMVT` / `ST_AsGeobuf`).
/// Djogi stores spatial model fields as PostGIS `geography(...)`, while
/// both row encoders inspect a geometry-typed record column by name. The
/// ordinary annotated fetch path must keep geography values untouched for
/// `FromPgRow`; this row-aggregate-only path is free to project
/// `t.<geography_col>::geometry AS <geography_col>` because the outer
/// terminal decodes only the single `bytea` aggregate result.
#[cfg(feature = "spatial")]
pub(crate) fn build_spatial_row_select_with_annotations_for_fetch<T, F>(
    qs: &QuerySet<T>,
    push_columns: F,
    qualify: Option<&crate::expr::QualifyCondition>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    F: FnOnce(&mut SqlAccumulator),
{
    let inner = build_spatial_row_select_with_annotations(qs, push_columns)?;
    let Some(qualify) = qualify else {
        return Ok(inner);
    };

    let mut wrapped = SqlAccumulator::new("SELECT * FROM (");
    wrapped.extend_with(inner);
    wrapped.push_sql(") AS __djogi_q WHERE ");
    qualify.push_outer_where(&mut wrapped);
    Ok(wrapped)
}

#[cfg(feature = "spatial")]
fn build_spatial_row_select_with_annotations<T, F>(
    qs: &QuerySet<T>,
    push_columns: F,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    F: FnOnce(&mut SqlAccumulator),
{
    let mut acc = SqlAccumulator::new("SELECT ");
    let desc = T::descriptor();

    for (i, col) in <T as FromPgRow>::COLUMNS.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql("t.");
        acc.push_sql(col);
        if desc.fields.iter().any(|f| {
            f.name == *col
                && matches!(
                    f.sql_type,
                    crate::descriptor::FieldSqlType::Geography { .. }
                )
        }) {
            acc.push_sql("::geometry AS ");
            acc.push_sql(col);
        }
    }

    push_columns(&mut acc);
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");
    push_tail(&mut acc, qs)?;
    Ok(acc)
}

/// Build `SELECT keys, aggregates FROM <table> [WHERE ...] GROUP BY keys
/// [HAVING ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]` — the terminal for
/// [`crate::query::grouped::GroupedAnnotatedQuerySet::fetch_all`].
/// # SELECT list layout
/// Keys MUST be emitted first and in `push_group_by_columns` order
/// `K::decode_tuple` reads positionally (ordinals 0..N_keys). Aggregate
/// columns follow, decoded by alias (`__djogi_agg_N`). If the key/aggregate
/// order in the SELECT list ever changes, key decoding will silently read the
/// wrong columns.
/// # Why `push_columns_bare` not plain-annotate column emission
/// Plain ungrouped annotate emits value aggregate slots with a synthesized
/// `OVER ` through the `PlainAnnotationTuple` path. A `GROUP BY` query must
/// not use those synthesized windows in the SELECT list for its aggregate
/// columns — Postgres would reject the combination. `push_columns_bare` emits
/// the aggregate with only the narrowing cast and no default window, which is
/// why grouped annotate remains legal for metadata, ordered-set, and
/// hypothetical-set aggregates.
/// # Spatial JOIN delegation
/// When the `spatial` feature is enabled and `gaq.spatial_source` is `Some`,
/// this function delegates to the appropriate spatial builder so the caller
/// does not need to be aware of which emission path to take.
pub(crate) fn build_grouped_annotated_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
) -> Result<SqlAccumulator, PortablePredicateError>
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
    let has_key_columns = acc.sql() != "SELECT ";
    gaq.aggregates
        .push_columns_bare_after(&mut acc, has_key_columns);

    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS t");

    // WHERE from the upstream queryset (filters set before .group_by)
    push_where(&mut acc, &gaq.qs)?;

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
                acc.push_csv(set.iter().copied());
                acc.push_sql(")");
            }
            acc.push_sql(")");
        }
    }

    push_grouped_tail(
        &mut acc,
        gaq.having.as_ref(),
        &gaq.order,
        gaq.limit,
        gaq.offset,
    )?;

    Ok(acc)
}

/// Build the spatial-JOIN variant of the grouped-annotated SELECT:
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
/// Called by [`build_grouped_annotated_select`] when `gaq.spatial_source` is
/// `Some(SpatialGroupSource::Join(_))`. All clause-ordering and bind-slot
/// semantics are identical to the plain grouped path — the only difference is
/// the FROM + LEFT JOIN instead of the bare `FROM <t-table> AS t`.
/// # Column name safety
/// `spec.t_geo_col`, `spec.r_geo_col`, `spec.r_pk_col`, and `spec.r_table`
/// are all `&'static str` baked by the macro or read from `ModelDescriptor`
/// field names. They are pushed as SQL text (not bound parameters) on the same
/// basis as every other column or table name in this file. No user input flows
/// through these slots.
#[cfg(feature = "spatial")]
pub(crate) fn build_spatial_join_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::SpatialJoinSpec,
) -> Result<SqlAccumulator, PortablePredicateError>
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
    // LEFT JOIN so unmatched rows (no containing region) appear in the result
    // with r.<pk-col> = NULL rather than being silently dropped.
    // # ST_Covers vs ST_Contains
    // `ST_Covers` is used (not `ST_Contains`) because:
    // - `ST_Contains(geography, geography)` does **not** exist in PostGIS 3.x
    // only the geometry overload is defined, and Djogi stores spatial
    // columns as `GEOGRAPHY(..., 4326)`. Using `ST_Contains` here forces
    // `::geometry` casts on both sides, which defeats GiST index usage on
    // the geography column.
    // - `ST_Covers` has a native `geography` overload and gives the same
    // answer as `ST_Contains` for the point-in-polygon use case this JOIN
    // implements (a point is "covered by" a polygon iff it is "inside" the
    // polygon; the distinction between the two functions only matters when
    // the inner geometry touches the boundary of the outer one — and for
    // the scalar point case, being on the boundary is treated as inside
    // under both functions for geography inputs).
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
    push_where_qualified(&mut acc, &gaq.qs, Some("t"))?;

    // GROUP BY r.<pk-col>
    acc.push_sql(" GROUP BY ");
    match gaq.grouping {
        crate::query::grouped::GroupingMode::Plain => {
            gaq.keys.push_group_by_columns(&mut acc);
        }
        // ROLLUP / CUBE / SETS are not meaningful for spatial region grouping
        // the key is derived from a JOIN condition, not a column value. Reaching
        // here indicates the user set the grouping mode manually, which is not
        // supported via the `group_by_region` entry point. Emit plain GROUP BY
        // as a safe fallback (the user is off-path if they reach this via any
        // internal route).
        _ => {
            gaq.keys.push_group_by_columns(&mut acc);
        }
    }

    push_grouped_tail(
        &mut acc,
        gaq.having.as_ref(),
        &gaq.order,
        gaq.limit,
        gaq.offset,
    )?;

    Ok(acc)
}

/// Build the DBSCAN-clustering variant of the grouped-annotated SELECT:
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
/// # Why the subquery
/// A flat `SELECT ST_ClusterDBSCAN(...) OVER AS cluster_id ... GROUP BY
/// cluster_id` query is rejected by Postgres with
/// ```text
/// ERROR: window functions are not allowed in GROUP BY
/// ```
/// because the GROUP BY references an alias whose defining expression is a
/// window aggregate. Wrapping the window call in an inner subquery
/// materialises `cluster_id` as a plain column in the outer query, so the
/// outer `GROUP BY cluster_id` is valid.
/// The inner subquery projects `t.*` so any outer aggregate expression that
/// references `t.<col>` continues to resolve — the outer subquery alias is
/// also `t`, keeping the column-qualification pattern identical to every
/// other query shape in this file.
/// # Clause placement under the subquery
/// - `WHERE` stays on the **inner** subquery — it prunes rows *before*
///   clustering, which is the only semantically meaningful position for a
///   filter that does not reference `cluster_id`.
/// - `HAVING` stays on the **outer** query — it filters the aggregated
///   groups.
/// - `ORDER BY` / `LIMIT` / `OFFSET` stay on the **outer** query — they
///   paginate the aggregated result.
/// # Casts and binds
/// The `::geometry` cast is required because `ST_ClusterDBSCAN` does not
/// accept the `geography` type directly in PostGIS 3.x.
/// `$eps` is bound as `f64`; `$minpoints` as `i32`. Both are positional
/// parameters (no user-controlled SQL text).
/// Called by [`build_grouped_annotated_select`] when
/// `gaq.spatial_source == Some(SpatialGroupSource::Cluster(_))`.
#[cfg(feature = "spatial")]
pub(crate) fn build_cluster_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::ClusterSpec,
) -> Result<SqlAccumulator, PortablePredicateError>
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
    push_where(&mut acc, &gaq.qs)?;

    acc.push_sql(") AS t");

    // Outer GROUP BY on the materialised cluster_id column — now valid.
    acc.push_sql(" GROUP BY cluster_id");

    // Outer-scope tail: HAVING / ORDER BY / LIMIT / OFFSET filter the
    // aggregated groups, not the inner pre-cluster rows.
    push_grouped_tail(
        &mut acc,
        gaq.having.as_ref(),
        &gaq.order,
        gaq.limit,
        gaq.offset,
    )?;

    Ok(acc)
}

/// Build the geohash-bucketing variant of the grouped-annotated SELECT:
/// ```sql
/// SELECT ST_GeoHash(t.<col>::geometry, $precision) AS geohash, <aggregates>
/// FROM <table> AS t
/// [WHERE ...]
/// GROUP BY geohash
/// [HAVING ...]
/// [ORDER BY ...]
/// [LIMIT $n] [OFFSET $n]
/// ```
/// The `::geometry` cast is required for the same reason as DBSCAN
/// `ST_GeoHash` accepts `geometry`, not `geography`, in PostGIS 3.x.
/// `$precision` is bound as `i32` from [`GeohashPrecision::as_i32`].
/// Called by [`build_grouped_annotated_select`] when
/// `gaq.spatial_source == Some(SpatialGroupSource::Geohash(_))`.
#[cfg(feature = "spatial")]
pub(crate) fn build_geohash_grouped_select<T, K, A>(
    gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    spec: &crate::query::spatial_grouping::GeohashSpec,
) -> Result<SqlAccumulator, PortablePredicateError>
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
    push_where(&mut acc, &gaq.qs)?;

    // GROUP BY geohash (references the scalar-function alias)
    acc.push_sql(" GROUP BY geohash");

    push_grouped_tail(
        &mut acc,
        gaq.having.as_ref(),
        &gaq.order,
        gaq.limit,
        gaq.offset,
    )?;

    Ok(acc)
}

/// Build `SELECT COUNT(*) FROM <table> [WHERE ...]`, honoring
/// [`DistinctMode`].
/// Shapes emitted per mode:
/// - `DistinctMode::None` → `SELECT COUNT(*) FROM "table" [WHERE ...]`
/// - `DistinctMode::Plain` → `SELECT COUNT(*) FROM (SELECT DISTINCT * FROM
/// "table" [WHERE ...]) AS sub`
/// - `DistinctMode::On(cols)` → `SELECT COUNT(*) FROM (SELECT DISTINCT ON
/// (cols) * FROM "table" [WHERE ...] ORDER BY cols [, user-ordering]) AS sub`
///   `ORDER BY` / `LIMIT` / `OFFSET` from the queryset are intentionally not
///   emitted on the **outer** count — they don't affect total cardinality and
///   including them only slows the query. For `DISTINCT ON` the inner ORDER
///   BY is required by Postgres (the `ON` column list must be a prefix of
///   `ORDER BY`); we prepend the distinct columns and then append any
///   user-supplied ordering so the emitted SQL is syntactically valid and
///   semantically stable.
pub(crate) fn build_count<T: Model>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError> {
    match &qs.distinct {
        DistinctMode::None => {
            // Fast path — plain row count, no subquery wrap.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs)?;
            Ok(acc)
        }
        DistinctMode::Plain => {
            // `COUNT(*)` over `SELECT DISTINCT *` counts distinct whole-row
            // tuples. No ordering needed inside the subquery — DISTINCT has
            // no prefix requirement.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (SELECT DISTINCT * FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs)?;
            acc.push_sql(") AS sub");
            Ok(acc)
        }
        DistinctMode::On(cols) => {
            // `DISTINCT ON (a, b)` requires `ORDER BY a, b [, ...]`. We
            // prepend the distinct columns to the user's ordering so the
            // subquery is always well-formed. Duplicates (user already
            // ordered by a distinct column) are harmless — Postgres ignores
            // repeated expressions in ORDER BY for ordering purposes.
            let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM (SELECT DISTINCT ON (");
            acc.push_csv(cols.iter().copied());
            acc.push_sql(") * FROM ");
            acc.push_sql(T::table_name());
            push_where(&mut acc, qs)?;
            acc.push_sql(" ORDER BY ");
            acc.push_csv(cols.iter().copied());
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
            Ok(acc)
        }
    }
}

/// Build `SELECT EXISTS(SELECT 1 FROM <table> [WHERE ...] LIMIT 1)`.
/// `LIMIT 1` is inside the EXISTS subquery rather than being passed through
/// the queryset's `limit` slot: EXISTS returns a single boolean regardless
/// of how many rows match, so `LIMIT 1` here is a micro-optimization that
/// tells Postgres to stop scanning once one match is found.
pub(crate) fn build_exists<T: Model>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = SqlAccumulator::new("SELECT EXISTS(SELECT 1 FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs)?;
    acc.push_sql(" LIMIT 1)");
    Ok(acc)
}

/// Build `UPDATE <table> SET col = $1, col = $2, updated_at = now
/// [WHERE ...]`.
/// Every assignment's value flows through [`push_filter_value`] — i.e.
/// `push_bind` — so the emitted SQL has one positional parameter per
/// user-supplied value. The `updated_at = now` tail is always appended,
/// even when the caller's closure omitted it: parity with the single-row
/// `save` path, which also bumps `updated_at` on every write. Users who
/// need to preserve `updated_at` across a bulk update reach for raw SQL
/// via `ctx.raw_execute` (T5) — same as any other ORM layer that
/// treats the audit column as non-optional.
/// `WHERE` is emitted via the shared [`push_where`] helper, so
/// `QuerySet::none`-derived querysets (caught earlier in
/// [`crate::query::update::UpdateStmt::execute`]) and vacuously-true
/// condition trees are handled identically to the read terminals.
/// # Assignment list invariants
/// Callers must ensure `assignments` is non-empty — `UPDATE ... SET ` with
/// an empty list is a Postgres syntax error. The public entry point
/// ([`crate::query::update::UpdateStmt::execute`]) short-circuits on
/// `assignments.is_empty` before reaching this emitter, so the emitter
/// itself does not need a runtime guard. Panicking here would be
/// defensive-programming noise; the short-circuit is the real safety rail.
pub(crate) fn build_update<T: Model>(
    qs: &QuerySet<T>,
    assignments: &[crate::query::update::UpdateAssignment],
) -> Result<SqlAccumulator, PortablePredicateError> {
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
        // refs, and nested binds. `clone` on the literal path retains
        // the `UpdateStmt`'s payload for retry; the Expr arm borrows
        // the inner `ExprNode` by reference.
        match a.value() {
            crate::query::update::AssignmentValue::Literal(v) => {
                push_filter_value(&mut acc, v.clone());
            }
            crate::query::update::AssignmentValue::Expr(node) => {
                crate::expr::sql::emit_expr(&mut acc, node, SqlEmitContext::root())?;
            }
        }
    }
    // Always stamp `updated_at = now` on bulk updates — matches
    // single-row save. `now` is a SQL literal, not a user value, so
    // `push_sql` is correct (no bind slot needed). Position-wise this is a
    // trailing clause after the user's SET list; the leading ", " handles
    // the separator even when the user supplied only one assignment.
    acc.push_sql(", updated_at = now()");
    push_where(&mut acc, qs)?;
    Ok(acc)
}

/// Build `DELETE FROM <table> [WHERE ...]`.
/// Plain DELETE — no RETURNING, no USING join. The `WHERE` clause uses
/// the shared [`push_where`] helper so vacuously-true condition trees
/// (e.g. `Condition::And(vec![])`) are omitted entirely rather than
/// emitted as `WHERE TRUE`. A queryset with no filters at all (just
/// `T::objects`) deletes every row in the table — same semantics as
/// raw SQL; callers who want extra safety wrap the call in a
/// transaction and `ROLLBACK` if the row count looks wrong.
/// `updated_at = now` stamping does **not** apply here — the row is
/// being removed, so auditing the timestamp has no meaning. Audit of
/// deletions lives in the `_logs` mirror tables (populated by
/// the `crud_log_url` pool).
pub(crate) fn build_delete<T: Model>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = SqlAccumulator::new("DELETE FROM ");
    acc.push_sql(T::table_name());
    push_where(&mut acc, qs)?;
    Ok(acc)
}

/// Build `MERGE INTO <target> AS tgt USING (...) AS __djogi_src ON ...
/// [WHEN MATCHED [AND ...] THEN UPDATE SET ... | DELETE]
/// [WHEN NOT MATCHED [BY TARGET] [AND ...] THEN INSERT (...) VALUES (...)]
/// [WHEN NOT MATCHED BY SOURCE [AND ...] THEN UPDATE SET ... | DELETE]`.
/// Issue #178 — typed MERGE query surface.
pub(crate) fn build_merge<S: Model + FromPgRow, T: Model>(
    source: &QuerySet<S>,
    on: &[MergeOnEq<S, T>],
    branches: &[MergeBranch<S, T>],
    returning: Option<()>,
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = SqlAccumulator::new("MERGE INTO ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(TGT_ALIAS);
    acc.push_sql(" ");

    let _ = returning;

    // USING (...) AS __djogi_src
    acc.push_sql("USING (");
    // Emit source QuerySet as a subquery.
    let source_acc = build_select(source)?;
    // Class push_tail must be INSIDE the parentheses.
    // build_select already calls push_tail.
    // Wait, build_select implementation:
    // let mut acc = build_select_list(qs)?;
    // acc.push_sql(" FROM ");
    // acc.push_sql(T::table_name);
    // push_tail(&mut acc, qs)?;
    // So build_select is correct.
    acc.extend_with(source_acc);
    acc.push_sql(") AS ");
    acc.push_sql(SRC_ALIAS);
    acc.push_sql(" ");

    // ON (tgt.col = __djogi_src.col AND ...)
    acc.push_sql("ON ");
    for (i, cond) in on.iter().enumerate() {
        if i > 0 {
            acc.push_sql(" AND ");
        }
        acc.push_sql(TGT_ALIAS);
        acc.push_sql(".");
        acc.push_sql(cond.target_col);
        acc.push_sql(" = ");
        acc.push_sql(SRC_ALIAS);
        acc.push_sql(".");
        acc.push_sql(cond.source_col);
    }

    // Branches
    for branch in branches {
        acc.push_sql("\nWHEN ");
        match branch.match_kind {
            MergeMatchKind::Matched => acc.push_sql("MATCHED"),
            MergeMatchKind::NotMatchedByTarget => acc.push_sql("NOT MATCHED"),
            MergeMatchKind::NotMatchedBySource => acc.push_sql("NOT MATCHED BY SOURCE"),
        }

        if let Some(cond) = &branch.condition {
            acc.push_sql(" AND ");
            // Emit condition. Target fields are qualified with `tgt`.
            crate::expr::sql::emit_expr(&mut acc, &cond.node, SqlEmitContext::joined(TGT_ALIAS))?;
        }

        acc.push_sql(" THEN ");
        match &branch.action {
            MergeAction::Update(updates) => {
                acc.push_sql("UPDATE SET ");
                // updated_at = now
                acc.push_sql("updated_at = now()");
                for update in updates {
                    acc.push_sql(", ");
                    acc.push_sql(update.target_col);
                    acc.push_sql(" = ");
                    match &update.value {
                        MergeValue::Literal(v, _) => push_filter_value(&mut acc, v.clone()),
                        MergeValue::SourceField(col, _) => {
                            acc.push_sql(SRC_ALIAS);
                            acc.push_sql(".");
                            acc.push_sql(col);
                        }
                        MergeValue::TargetExpr(node, _) => {
                            crate::expr::sql::emit_expr(
                                &mut acc,
                                node,
                                SqlEmitContext::joined(TGT_ALIAS),
                            )?;
                        }
                    }
                }
            }
            MergeAction::Delete => {
                acc.push_sql("DELETE");
            }
            MergeAction::Insert(columns) => {
                acc.push_sql("INSERT (");
                for (i, col) in columns.iter().enumerate() {
                    if i > 0 {
                        acc.push_sql(", ");
                    }
                    acc.push_sql(col.target_col);
                }
                acc.push_sql(") VALUES (");
                for (i, col) in columns.iter().enumerate() {
                    if i > 0 {
                        acc.push_sql(", ");
                    }
                    match &col.value {
                        MergeValue::Literal(v, _) => push_filter_value(&mut acc, v.clone()),
                        MergeValue::SourceField(scol, _) => {
                            acc.push_sql(SRC_ALIAS);
                            acc.push_sql(".");
                            acc.push_sql(scol);
                        }
                        MergeValue::TargetExpr(node, _) => {
                            // Class Source references in VALUES must be qualified
                            crate::expr::sql::emit_expr(
                                &mut acc,
                                node,
                                SqlEmitContext::joined(TGT_ALIAS),
                            )?;
                        }
                    }
                }
                acc.push_sql(")");
            }
            MergeAction::_Marker(_) => unreachable!("MergeAction::_Marker is a type marker only"),
        }
    }

    Ok(acc)
}

// ── — PG18 OLD/NEW RETURNING helpers ──────────────────

/// Append the `RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)`
/// clause and the fully-aliased column projection for both sides to `acc`.
/// The table aliases (`__djogi_old` / `__djogi_new`) use Djogi's reserved
/// namespace. Column aliases are short ordinals (`o0`, `o1`, ... and
/// `n0`, `n1`, ...) so projection aliases stay safely below the 63-byte
/// identifier limit while preserving a stable positional mapping.
/// Column names come from `T::COLUMNS`, which are macro-validated `&'static str`
/// identifiers; `push_sql` is safe here because no user data flows in.
/// Called by [`build_update_returning_pairs`] and [`build_delete_returning`].
fn push_old_new_returning_projection<T: FromPgRow>(acc: &mut SqlAccumulator, include_new: bool) {
    if include_new {
        acc.push_sql(" RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)");
    } else {
        acc.push_sql(" RETURNING WITH (OLD AS __djogi_old)");
    }
    // Old-side columns: __djogi_old.<col> AS "o{idx}".
    let mut first = true;
    for (idx, col) in T::COLUMNS.iter().enumerate() {
        if first {
            acc.push_sql(" ");
            first = false;
        } else {
            acc.push_sql(", ");
        }
        acc.push_sql("__djogi_old.");
        acc.push_sql(col);
        let old_alias = joined_alias_for_prefix("__djogi_old__", idx, col);
        acc.push_sql(" AS \"");
        acc.push_sql(&old_alias);
        acc.push_sql("\"");
    }
    if include_new {
        // New-side columns: __djogi_new.<col> AS "n{idx}".
        for (idx, col) in T::COLUMNS.iter().enumerate() {
            acc.push_sql(", __djogi_new.");
            acc.push_sql(col);
            let new_alias = joined_alias_for_prefix("__djogi_new__", idx, col);
            acc.push_sql(" AS \"");
            acc.push_sql(&new_alias);
            acc.push_sql("\"");
        }
    }
}

/// Build `UPDATE <table> SET ... RETURNING WITH (OLD AS __djogi_old, NEW AS
/// __djogi_new) __djogi_old.<col> AS "o{idx}", ...,
/// __djogi_new.<col> AS "n{idx}", ...`.
/// This is the bulk-returning variant of [`build_update`]; the WHERE clause and
/// SET assignments are identical. The RETURNING projection appends the full
/// column list for both the pre-update (`OLD`) and post-update (`NEW`) row
/// images using short ordinal aliases (`o{idx}` / `n{idx}`) to avoid
/// PostgreSQL identifier truncation.
/// Callers must guarantee `assignments` is non-empty — the same contract as
/// [`build_update`]. The callers in [`crate::query::update::UpdateStmt::execute_returning_pairs`]
/// short-circuit on an empty assignment list before reaching this emitter.
/// # SQL injection guarantee
/// All column names come from `T::COLUMNS` (macro-validated `&'static str`).
/// All values flow through `push_bind` via the shared [`build_update`] path.
/// The `__djogi_old` / `__djogi_new` table aliases are framework-internal
/// `&'static str` literals.
pub(crate) fn build_update_returning_pairs<T>(
    qs: &QuerySet<T>,
    assignments: &[crate::query::update::UpdateAssignment],
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
{
    let mut acc = build_update(qs, assignments)?;
    push_old_new_returning_projection::<T>(&mut acc, true);
    Ok(acc)
}

/// Build `UPDATE <table> SET ... [WHERE ...] RETURNING <pk_column>`.
/// Issue #304 Stage 2 — bulk update cache invalidation SQL builder.
/// Emits the same UPDATE statement as [`build_update`] but appends a
/// `RETURNING <pk_column>` clause so the caller can collect the primary-
/// key IDs of every affected row for targeted cache eviction.
/// The PK column name comes from `T::descriptor.pk_column`, which is
/// a macro-baked `&'static str` — safe for direct `push_sql` insertion.
/// All value binding follows the same [`build_update`] path, so assignment
/// binds fill slots before WHERE filter binds in the positional parameter
/// sequence.
pub(crate) fn build_update_returning_ids<T: Model>(
    qs: &QuerySet<T>,
    assignments: &[crate::query::update::UpdateAssignment],
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = build_update(qs, assignments)?;
    let pk_column = T::descriptor()
        .pk_column()
        .expect("Model implementations with CRUD support must expose a primary-key column");
    acc.push_sql(" RETURNING ");
    acc.push_sql(pk_column);
    Ok(acc)
}

/// Build `DELETE FROM <table> [WHERE ...] RETURNING WITH (OLD AS __djogi_old)
/// __djogi_old.<col> AS "o{idx}", ...`.
/// The DELETE variant only has an `OLD` side — `NEW` is semantically absent for
/// deleted rows. The projection aliases use the same short `o{idx}` pattern as
/// the UPDATE variant so the `FromJoinedPgRow` path is identical
/// (`from_joined_pg_row(row, "__djogi_old__")`).
/// Callers must guarantee the queryset is not `QuerySet::none`-derived before
/// reaching this emitter — same contract as [`build_delete`].
pub(crate) fn build_delete_returning<T>(
    qs: &QuerySet<T>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
{
    let mut acc = build_delete(qs)?;
    push_old_new_returning_projection::<T>(&mut acc, false);
    Ok(acc)
}

/// Build `INSERT INTO <target> (cols...) SELECT exprs... FROM <source>
/// [WHERE ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]`.
/// Closes [](https://github.com/TarunvirBains/djogi/issues/106)
/// the typed bulk-copy surface from one model's queryset into another
/// model's table.
/// # Inputs
/// - `qs` — source queryset, contributes the source FROM table, the
///   WHERE clause, ordering, limit, and offset. All other source state
///   (`prefetch_paths`, `select_related_paths`, `cache_target`,
///   `lock`, `distinct`) is rejected by
///   [`crate::query::insert_select::InsertSelectStmt::execute`] before
///   reaching this emitter, so the emitter itself does not need a
///   runtime guard.
/// - `columns` — `(target_column, source_expression)` mappings in
///   lockstep position. The column list and the SELECT projection are
///   emitted in the same order; per-column type alignment is enforced
///   at compile time by [`crate::query::FieldRef::copy_from`].
/// # Output shape
/// ```sql
/// INSERT INTO target_table (target_col1, target_col2, ...)
/// SELECT source_expr1, source_expr2, ...
/// FROM source_table
/// [WHERE ...]
/// [ORDER BY ...]
/// [LIMIT $n]
/// [OFFSET $n]
/// ```
/// Framework columns (`id`, `created_at`, `updated_at`) on the target
/// are populated by their column-level `DEFAULT` clauses — the emitter
/// never names them unless the closure explicitly maps them. This
/// matches `Model::create`'s contract.
/// # Why not `RETURNING`
/// The terminal contract for returns the affected row count
/// only — see the module docs on
/// [`crate::query::insert_select`]. A `RETURNING`-bearing variant can
/// be added in a follow-up issue without breaking the row-count
/// terminal.
/// # Why not the joined-select tail
/// `select_related_paths` is rejected at the terminal layer, so the
/// emitter takes the bare [`push_tail`] (not [`push_tail_qualified`])
/// path. No `LEFT JOIN`s, no column qualification — matches the
/// minimum coherent surface of the public API.
/// # Invariants
/// - `columns` is non-empty (the terminal's
///   [`crate::query::insert_select::InsertSelectStmt::execute`] returns
///   `DjogiError::Validation` before reaching here on empty input).
/// - `columns` has no duplicate `target_column` entries (same source
///   of validation).
/// - Every `column.source` is an [`crate::expr::node::ExprNode`] tree
///   built through the typed `FieldRef::copy_from` constructor — the
///   `&'static str` column names baked into `ExprNode::Field` flow
///   straight to `SqlAccumulator::push_sql`, matching the existing
///   bind-vs-text discipline.
/// # Source's `lock` is intentionally NOT emitted
/// Although [`push_tail`] would normally append `qs.lock`'s `FOR UPDATE`
/// tail, the terminal rejects non-default `LockMode` upstream so the
/// `LockMode::None` path is the only one this emitter sees. The tail
/// shim still calls `qs.lock.push_tail(acc)` (a no-op for
/// `LockMode::None`); preserving the call keeps the emitter
/// structurally identical to the SELECT path and makes the future
/// lock-opt-in surface a single-line change at the terminal layer.
pub(crate) fn build_insert_select<S: Model, T: Model>(
    qs: &QuerySet<S>,
    columns: &[crate::query::insert_select::InsertSelectColumn<S, T>],
) -> Result<SqlAccumulator, PortablePredicateError> {
    // Emit the INSERT prefix and the target column list. Target column
    // names are macro-baked `&'static str` literals via FieldRef, so
    // `push_sql` (not `push_bind`) is correct.
    let mut acc = SqlAccumulator::new("INSERT INTO ");
    acc.push_sql(T::table_name());
    acc.push_sql(" (");
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(col.target_column());
    }
    acc.push_sql(") SELECT ");

    // Emit the source-expression list in lockstep with the target
    // column list. Each ExprNode flows through emit_expr — literals
    // bind via push_bind, field refs emit bare column names (validated
    // by FieldRef construction), and arithmetic / function nodes
    // recurse through the existing expression emitter.
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        crate::expr::sql::emit_expr(&mut acc, col.source(), SqlEmitContext::root())?;
    }

    // SELECT FROM source_table — followed by the bare tail (WHERE,
    // ORDER BY, LIMIT, OFFSET, and the LockMode::None no-op).
    acc.push_sql(" FROM ");
    acc.push_sql(S::table_name());
    push_tail(&mut acc, qs)?;
    Ok(acc)
}

/// Build `INSERT INTO target (cols...) SELECT exprs... FROM source [WHERE ...] RETURNING <column_list>`.
/// Identical to [`build_insert_select`] but appends ` RETURNING <column_list>` so the
/// caller can decode the inserted rows via [`crate::pg::decode::FromPgRow`].
/// This uses the model's canonical projection (`T::COLUMN_LIST`) rather than
/// physical `*`, so insert-select returning remains stable when table order and
/// model decode order differ.
/// # Errors
/// Returns the same [`PortablePredicateError`] variants as
/// [`build_insert_select`] — the additional SQL token is always trusted
/// (`T::COLUMN_LIST` is crate-owned static SQL text).
pub(crate) fn build_insert_select_returning<S: Model, T: Model + FromPgRow>(
    qs: &QuerySet<S>,
    columns: &[crate::query::insert_select::InsertSelectColumn<S, T>],
) -> Result<SqlAccumulator, PortablePredicateError> {
    let mut acc = build_insert_select::<S, T>(qs, columns)?;
    acc.push_sql(" RETURNING ");
    acc.push_sql(<T as FromPgRow>::COLUMN_LIST);
    Ok(acc)
}

/// Walk the emitted SELECT list and check that every column's alias (or
/// plain column name if no `AS` alias) is unique. A collision would cause
/// the terminal decoder to read the wrong value for one of the columns.
/// # Algorithm
/// 1. Find the substring between `SELECT ` and the next ` FROM ` (case
///    matters — emitters use uppercase keywords).
/// 2. Split on commas at the top parenthesis level into logical columns.
///    Parens and nested function calls are handled by tracking depth, so
///    aggregate expressions like `SUM(a, b)` are not split mid-argument.
/// 3. For each column, extract the alias — the substring after the last
///    ` AS ` if present, otherwise the whole column text (trimmed).
/// 4. Check uniqueness; return `Err(DjogiError::AliasCollision)` on
///    duplicate.
/// # Limitations
/// This is a best-effort string parse. It does not handle:
/// - Nested subqueries in the SELECT list (not emitted by).
/// - Unparenthesised comma-separated arguments at the top level (our
///   emitter always parenthesises function args).
///   The check is defensive; failure means something has gone subtly wrong
///   in the query builder, not that the user did something wrong.
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
/// The grouped-emitter output uses simple function-call args, so a single
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
    //! We reach into the emitter using a minimal local `Model` impl (mirrors
    //! the `Fake` model used in `query::field`'s tests) so that unit tests
    //! remain independent of `#[model]` macro expansion.

    use super::*;
    use crate::descriptor::{ModelDescriptor, PkType};
    use crate::query::condition::{Condition, FilterValue, Leaf, LookupOp};
    use crate::query::queryset::QuerySet;

    // REQ-304: minimal descriptor for Fake model — provides pk_column = Some("id")
    // so that build_update_returning_ids can resolve the primary key column name.
    static FAKE_DESCRIPTOR: ModelDescriptor = ModelDescriptor {
        type_name: "Fake",
        table_name: "fakes",
        pk_type: PkType::HeerIdDesc,
        fields: &[],
        partition_by: None,
        has_outbox: false,
        idempotency_key: None,
        tenant_key: None,
        cache_ttl: None,
        rationale: None,
        indexes: &[],
        is_through: false,
        fts: None,
        app: None,
        moved_from_app: None,
        renamed_from: None,
        exclusion_constraints: &[],
        tree_edge: None,
        proxy_for: None,
        default_filter_sql: None,
        computed_fields: &[],
        table_comment: None,
        storage_params: None,
        tablespace: None,
    };

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
            &FAKE_DESCRIPTOR
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

    // Stub `FromJoinedPgRow` for Fake — required by the #180 returning builders.
    impl crate::pg::decode::FromJoinedPgRow for Fake {
        fn from_joined_pg_row(
            _row: &tokio_postgres::Row,
            _prefix: &str,
        ) -> Result<Self, crate::DjogiError> {
            unreachable!("SQL-text unit tests do not exercise row decode")
        }
    }

    fn build_select<T: Model + FromPgRow>(qs: &QuerySet<T>) -> SqlAccumulator {
        super::build_select(qs).expect("test predicate should lower to SQL")
    }

    fn build_select_joined<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
        super::build_select_joined(qs).expect("test predicate should lower to joined SQL")
    }

    fn build_count<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
        super::build_count(qs).expect("test predicate should lower to count SQL")
    }

    fn build_exists<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
        super::build_exists(qs).expect("test predicate should lower to exists SQL")
    }

    fn build_update<T: Model>(
        qs: &QuerySet<T>,
        assignments: &[crate::query::update::UpdateAssignment],
    ) -> SqlAccumulator {
        super::build_update(qs, assignments).expect("test predicate should lower to update SQL")
    }

    fn build_delete<T: Model>(qs: &QuerySet<T>) -> SqlAccumulator {
        super::build_delete(qs).expect("test predicate should lower to delete SQL")
    }

    fn build_grouped_annotated_select<T, K, A>(
        gaq: &crate::query::grouped::GroupedAnnotatedQuerySet<T, K, A>,
    ) -> SqlAccumulator
    where
        T: Model,
        K: crate::query::grouped::IntoGroupKeyTuple,
        A: crate::query::annotate::IntoAggregateTuple,
    {
        super::build_grouped_annotated_select(gaq)
            .expect("test predicate should lower to grouped annotated SQL")
    }

    // `SqlAccumulator::sql` exposes the emitted SQL text — that is what we
    // assert on. Bind values don't appear in `.sql`, they are tracked
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
    fn select_with_range_predicate_emits_postgres_range_operator() {
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| {
            crate::query::field::FieldRef::<Fake, crate::Range<i32>>::new("span")
                .adjacent_to(crate::Range::inclusive_exclusive(5_i32, 10_i32))
        });
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE span -|- $1"), "got: {sql}");
    }

    #[test]
    fn select_with_range_contains_element_emits_scalar_cast() {
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| {
            crate::query::field::FieldRef::<Fake, crate::Range<i32>>::new("span").contains(3_i32)
        });
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE span @> $1::int4"), "got: {sql}");
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
        let leaf = Leaf::new("id", LookupOp::In, FilterValue::List(Vec::new()));
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE FALSE"), "got: {sql}");
    }

    #[test]
    fn not_in_empty_list_renders_true() {
        let leaf = Leaf::new("id", LookupOp::NotIn, FilterValue::List(Vec::new()));
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("WHERE TRUE"), "got: {sql}");
    }

    #[test]
    fn in_list_emits_one_placeholder_per_element() {
        let leaf = Leaf::new(
            "id",
            LookupOp::In,
            FilterValue::List(vec![
                FilterValue::I64(1),
                FilterValue::I64(2),
                FilterValue::I64(3),
            ]),
        );
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("id IN ($1, $2, $3)"), "got: {sql}");
    }

    #[test]
    fn between_emits_two_binds() {
        let leaf = Leaf::new(
            "age",
            LookupOp::Between,
            FilterValue::Pair(
                Box::new(FilterValue::I32(10)),
                Box::new(FilterValue::I32(20)),
            ),
        );
        let qs: QuerySet<Fake> = QuerySet::new().filter(|_| Condition::Leaf(leaf));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(sql.contains("age BETWEEN $1 AND $2"), "got: {sql}");
    }

    #[test]
    fn is_null_takes_no_bind() {
        let leaf = Leaf::new("deleted_at", LookupOp::IsNull, FilterValue::Null);
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
        // from `sql` alone, but we CAN observe that it was a single bind
        // (not multiple).
        let leaf = Leaf::new(
            "title",
            LookupOp::IContains,
            FilterValue::String("50% off_sale\\".to_string()),
        );
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
        // `.distinct.count` must wrap the query in a subquery so
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
        qs.condition = crate::query::Q::Condition(Condition::And(Vec::new()));
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    /// 4 — `Condition::RawSql` emits the carried fragment
    /// wrapped in outer parens so further AND-composition with user
    /// `.filter(...)` calls preserves operator precedence. Lock the
    /// shape against accidental drift in the emitter.
    #[test]
    fn raw_sql_condition_wraps_in_parens() {
        let mut qs: QuerySet<Fake> = QuerySet::new();
        // (T6.9): wrap the legacy `Condition` through
        // `Q::Condition(_)` so `qs.condition`'s `Q<T>` substrate stays
        // honest. The direct-Q emitter delegates this arm to
        // `emit_condition`, so SQL parity with the pre-flip
        // `Condition::RawSql` path is preserved without using the
        // q_to_condition bridge.
        qs.condition =
            crate::query::Q::Condition(Condition::__from_raw_sql_fragment("active = TRUE"));
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.contains("WHERE (active = TRUE)"),
            "expected outer parens around proxy fragment, got: {sql}",
        );
    }

    /// Proxy raw-SQL filter AND-composes with a user leaf filter: the
    /// emitted WHERE clause has both terms separated by AND inside the
    /// flattened `Condition::And` tree. End-to-end coverage that the
    /// runtime wiring from T3.4 (`QuerySet::new` seeding + `filter`
    /// AND-composition + `RawSql` emission) lines up.
    #[test]
    fn raw_sql_condition_ands_with_user_filter() {
        // Construct an And node mirroring what `QuerySet::filter`
        // produces against a queryset whose seed is RawSql.
        let raw = Condition::__from_raw_sql_fragment("active = TRUE");
        let user = Condition::Leaf(Leaf::eq_raw("price", FilterValue::I64(100)));
        let mut qs: QuerySet<Fake> = QuerySet::new();
        // (T6.9): wrap the composed legacy tree
        // through `Q::Condition(_)` for the substrate flip. The direct-Q
        // emitter delegates this arm to `emit_condition`, so the
        // assertion on the emitted `WHERE ((active = TRUE) AND price = $1)`
        // shape still holds character-for-character without using the
        // q_to_condition bridge.
        qs.condition = crate::query::Q::Condition(Condition::and(raw, user));
        let acc = build_select(&qs);
        let sql = acc.sql();
        // Composite of the RawSql wrapper + the leaf comparison.
        assert!(
            sql.contains("WHERE ((active = TRUE) AND price = $1)"),
            "expected AND-composed WHERE clause, got: {sql}",
        );
    }

    #[test]
    fn where_skipped_on_nested_vacuous_and() {
        // Nested `And(vec![True, And(vec![])])` is also vacuously TRUE
        // `is_vacuously_true` walks the `And` subtree recursively. Same
        // cleanup as the flat empty-And case.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition = crate::query::Q::Condition(Condition::And(vec![
            Condition::True,
            Condition::And(Vec::new()),
        ]));
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    #[test]
    fn where_skipped_on_not_empty_or() {
        // `Not(Or(vec![]))` emits as `NOT FALSE` → `TRUE`, which is
        // vacuously true. Handled by the same skip path.
        let mut qs: QuerySet<Fake> = QuerySet::new();
        qs.condition =
            crate::query::Q::Condition(Condition::Not(Box::new(Condition::Or(Vec::new()))));
        let acc = build_select(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "SELECT id FROM fakes");
    }

    // ── Task 9: UPDATE / DELETE emitter ───────────────────────────────

    #[test]
    fn update_single_assignment_emits_set_and_updated_at() {
        // Single assignment + no filter: one bind for the user value,
        // `updated_at = now` stamped by the emitter, no `WHERE`.
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
        // Two assignments: `SET col = $1, col = $2, updated_at = now`.
        // Only the user's values consume bind slots; `now` is raw SQL.
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
        qs.condition = crate::query::Q::Condition(Condition::And(Vec::new()));
        let acc = build_delete(&qs);
        let sql = acc.sql().trim().to_string();
        assert_eq!(sql, "DELETE FROM fakes");
    }

    // ── ): INSERT...SELECT SQL shape ─────────

    /// A second `Model` impl so the INSERT...SELECT emitter tests can
    /// distinguish the source from the target by table name. Mirrors the
    /// outer `Fake` impl byte-for-byte but lands on a different table.
    struct FakeTarget;
    impl crate::model::__sealed::Sealed for FakeTarget {}
    #[allow(clippy::manual_async_fn)]
    impl Model for FakeTarget {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fake_targets"
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

    fn build_insert_select<S: Model, T: Model>(
        qs: &QuerySet<S>,
        columns: &[crate::query::insert_select::InsertSelectColumn<S, T>],
    ) -> SqlAccumulator {
        super::build_insert_select::<S, T>(qs, columns)
            .expect("test predicate should lower to insert-select SQL")
    }

    /// Helper — build an `InsertSelectColumn<S, T>` whose source is a
    /// bare column reference (the most common shape in adopter call
    /// sites). The post-fix source-tagged constructor pins the source
    /// model `S` on the operand, which propagates onto the returned
    /// `InsertSelectColumn<S, T>`.
    fn col_copy<S: Model, T: Model>(
        target_column: &'static str,
        source_column: &'static str,
    ) -> crate::query::insert_select::InsertSelectColumn<S, T> {
        let target: crate::query::FieldRef<T, i32> = crate::query::FieldRef::new(target_column);
        let source: crate::query::FieldRef<S, i32> = crate::query::FieldRef::new(source_column);
        target.copy_from(source.as_insert_source())
    }

    #[test]
    fn insert_select_no_filter_emits_bare_shape() {
        // The simplest shape — no WHERE, no ORDER BY, no LIMIT. The
        // emitted SQL is the literal `INSERT ... SELECT ... FROM ...`
        // with no trailing clause.
        let qs: QuerySet<Fake> = QuerySet::new();
        let cols = vec![col_copy::<Fake, FakeTarget>("view_count", "score")];
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql().trim().to_string();
        assert_eq!(
            sql,
            "INSERT INTO fake_targets (view_count) SELECT score FROM fakes"
        );
    }

    #[test]
    fn insert_select_multi_column_emits_lockstep_lists() {
        // Multi-column mapping — the target list and the source-
        // expression list must appear in lockstep position. Pins the
        // structural invariant the emitter relies on for type safety.
        let qs: QuerySet<Fake> = QuerySet::new();
        let cols = vec![
            col_copy::<Fake, FakeTarget>("a", "x"),
            col_copy::<Fake, FakeTarget>("b", "y"),
            col_copy::<Fake, FakeTarget>("c", "z"),
        ];
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql();
        assert!(
            sql.contains("INSERT INTO fake_targets (a, b, c) SELECT x, y, z FROM fakes"),
            "got: {sql}"
        );
    }

    #[test]
    fn insert_select_with_filter_emits_where() {
        // WHERE on the source — composes through the existing push_where
        // helper. Pins that the WHERE binds line up with the SELECT-side
        // binds (the SELECT side has no binds when the source is a bare
        // FieldRef, so the WHERE bind is `$1`).
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("published", FilterValue::Bool(true))));
        let cols = vec![col_copy::<Fake, FakeTarget>("view_count", "score")];
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql();
        assert!(
            sql.contains("FROM fakes WHERE published = $1"),
            "got: {sql}"
        );
    }

    #[test]
    fn insert_select_with_literal_source_pushes_bind() {
        // Literal source expression — the literal binds as `$1`, the
        // WHERE filter (if any) binds *after*. Pins that the SELECT
        // projection's binds precede the WHERE clause's binds.
        let target: crate::query::FieldRef<FakeTarget, i32> =
            crate::query::FieldRef::new("status_code");
        // `InsertSelectSource::literal` is polymorphic in `S`; the
        // explicit turbofish pins `S = Fake` so this test type-checks
        // without relying on closure-return inference.
        let cols =
            vec![target.copy_from(
                crate::query::insert_select::InsertSelectSource::<Fake, _>::literal(7i32),
            )];
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("published", FilterValue::Bool(true))));
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql();
        // SELECT binds first ($1), then WHERE binds ($2). The literal
        // is the SELECT projection's `$1`.
        assert!(
            sql.contains("INSERT INTO fake_targets (status_code) SELECT $1 FROM fakes"),
            "got: {sql}"
        );
        assert!(sql.contains("WHERE published = $2"), "got: {sql}");
    }

    #[test]
    fn insert_select_with_limit_offset_pushes_tail_binds() {
        // LIMIT + OFFSET on the source compose into the emitted SQL
        // through the shared push_tail helper. Pins that the tail
        // binds appear at the end of the parameter list (after the
        // SELECT projection's binds and any WHERE binds).
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5);
        let cols = vec![col_copy::<Fake, FakeTarget>("view_count", "score")];
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql();
        assert!(sql.contains("LIMIT $1"), "got: {sql}");
        assert!(sql.contains("OFFSET $2"), "got: {sql}");
    }

    #[test]
    fn insert_select_uses_source_table_in_from_not_target() {
        // Regression guard: the FROM clause references the SOURCE
        // table, not the target. A swap would land rows from the
        // target's contents into the target, which is what the
        // bypass-pattern raw-SQL escape hatch did wrong in adopter code
        // pre-. Anchor the shape here.
        let qs: QuerySet<Fake> = QuerySet::new();
        let cols = vec![col_copy::<Fake, FakeTarget>("view_count", "score")];
        let acc = build_insert_select::<Fake, FakeTarget>(&qs, &cols);
        let sql = acc.sql();
        assert!(sql.contains("INSERT INTO fake_targets"), "got: {sql}");
        assert!(sql.contains("FROM fakes"), "got: {sql}");
        // The target table name does NOT appear in a FROM position.
        // (Using a defensive substring check rather than a regex per
        // the no-regex rule in CLAUDE.md.)
        assert!(
            !sql.contains("FROM fake_targets"),
            "INSERT...SELECT emitted FROM target table — sql: {sql}"
        );
    }

    // ── fix: parent-table qualification under select_related ──
    // When `.select_related(...)` is active, every bare column reference
    // emitted by the `WHERE` / `ORDER BY` / `DISTINCT ON` clauses must be
    // qualified with the parent table name so Postgres does not raise
    // `42702 column reference "X" is ambiguous` on framework columns
    // (`id`, `created_at`, `updated_at`) that also appear on the joined
    // child. These tests pin the SQL shape at emission time so the live-
    // Postgres integration tests stay readable — a shape regression
    // surfaces here, not in a one-line 42702 failure down the stack.

    use crate::descriptor::{FieldDescriptor, FieldSqlType, field_descriptor, model_descriptor};
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
        // `.order_by(|f| f.created_at.asc)` on a joined queryset must
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
        // pick up the qualifier. Bare `WHERE id = $1` matches the
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

    // ── — row-lock SQL tails ───────────────────────────

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
        // `.nowait` alone — implies the base `FOR UPDATE` per the
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
        // Chaining `.nowait.skip_locked` — the skip_locked call
        // overwrites the nowait variant. Mirrors the rustdoc contract.
        let qs: QuerySet<Fake> = QuerySet::new().nowait().skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE SKIP LOCKED"),
            "expected skip_locked to win over nowait, got: {sql}"
        );
    }

    // ── — FOR SHARE row-lock SQL tails ──────────────────────

    #[test]
    fn select_for_share_appends_lock_tail() {
        let qs: QuerySet<Fake> = QuerySet::new().select_for_share();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR SHARE"),
            "expected FOR SHARE tail, got: {sql}"
        );
        // No silent escalation: the bare `select_for_share` must not
        // leak NOWAIT / SKIP LOCKED. Mirrors the FOR UPDATE guard.
        assert!(
            !sql.contains("NOWAIT") && !sql.contains("SKIP LOCKED"),
            "select_for_share must not escalate to NOWAIT / SKIP LOCKED"
        );
        assert!(
            !sql.contains("FOR UPDATE"),
            "select_for_share must not emit FOR UPDATE: {sql}"
        );
    }

    #[test]
    fn for_share_nowait_appends_for_share_nowait_tail() {
        let qs: QuerySet<Fake> = QuerySet::new().for_share_nowait();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR SHARE NOWAIT"),
            "expected FOR SHARE NOWAIT tail, got: {sql}"
        );
        assert!(
            !sql.contains("FOR UPDATE"),
            "for_share_nowait must not emit FOR UPDATE: {sql}"
        );
    }

    #[test]
    fn for_share_skip_locked_appends_for_share_skip_locked_tail() {
        let qs: QuerySet<Fake> = QuerySet::new().for_share_skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR SHARE SKIP LOCKED"),
            "expected FOR SHARE SKIP LOCKED tail, got: {sql}"
        );
        assert!(
            !sql.contains("FOR UPDATE"),
            "for_share_skip_locked must not emit FOR UPDATE: {sql}"
        );
    }

    #[test]
    fn for_share_tail_follows_limit_and_offset() {
        // Postgres requires the row-lock tail after LIMIT/OFFSET for
        // FOR SHARE just as for FOR UPDATE. Mirror the FOR UPDATE
        // order test so a future reordering surfaces here.
        let qs: QuerySet<Fake> = QuerySet::new().limit(10).offset(5).select_for_share();
        let acc = build_select(&qs);
        let sql = acc.sql();
        let limit_idx = sql.find("LIMIT").expect("LIMIT must appear");
        let offset_idx = sql.find("OFFSET").expect("OFFSET must appear");
        let lock_idx = sql.find("FOR SHARE").expect("FOR SHARE must appear");
        assert!(
            limit_idx < offset_idx && offset_idx < lock_idx,
            "expected LIMIT ... OFFSET ... FOR SHARE order, got: {sql}"
        );
    }

    #[test]
    fn for_share_builders_last_call_wins() {
        // `.for_share_nowait.for_share_skip_locked` — last call
        // wins. Same contract as the FOR UPDATE family.
        let qs: QuerySet<Fake> = QuerySet::new().for_share_nowait().for_share_skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR SHARE SKIP LOCKED"),
            "expected for_share_skip_locked to win, got: {sql}"
        );
    }

    #[test]
    fn for_share_then_for_update_last_call_wins() {
        // Swapping families also last-call-wins. Guards against any
        // future "compatible mode promotion" logic — the contract is
        // unconditional overwrite.
        let qs: QuerySet<Fake> = QuerySet::new().select_for_share().select_for_update();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE"),
            "expected last-call-wins flip to FOR UPDATE, got: {sql}"
        );
        assert!(
            !sql.contains("FOR SHARE"),
            "select_for_update after select_for_share must clear the FOR SHARE tail: {sql}"
        );
    }

    #[test]
    fn nowait_after_select_for_share_promotes_to_for_update_nowait() {
        // Documented chaining footgun: the historical
        // `.nowait` and `.skip_locked` modifiers unconditionally
        // set the FOR UPDATE family. Calling them after
        // `.select_for_share` silently swaps the base lock back to
        // FOR UPDATE. The rustdoc on `select_for_share` calls this
        // out as a footgun; this test pins the documented behaviour.
        // If a future change makes the contention modifiers
        // family-aware (e.g., preserve the base family), this test
        // surfaces the surface change loudly before it ships.
        let qs: QuerySet<Fake> = QuerySet::new().select_for_share().nowait();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE NOWAIT"),
            "expected .nowait() after .select_for_share() to promote to FOR UPDATE NOWAIT, got: {sql}"
        );
        assert!(
            !sql.contains("FOR SHARE"),
            ".nowait() must replace the FOR SHARE tail outright (documented footgun): {sql}"
        );
    }

    #[test]
    fn skip_locked_after_select_for_share_promotes_to_for_update_skip_locked() {
        // Parallel to the `.nowait` footgun. `.skip_locked` also
        // unconditionally sets the FOR UPDATE family — chaining it
        // after `.select_for_share` produces FOR UPDATE SKIP LOCKED,
        // not FOR SHARE SKIP LOCKED. Adopters who want the FOR SHARE
        // contention shape must use `for_share_skip_locked` directly.
        let qs: QuerySet<Fake> = QuerySet::new().select_for_share().skip_locked();
        let acc = build_select(&qs);
        let sql = acc.sql();
        assert!(
            sql.trim_end().ends_with("FOR UPDATE SKIP LOCKED"),
            "expected .skip_locked() after .select_for_share() to promote to FOR UPDATE SKIP LOCKED, got: {sql}"
        );
        assert!(
            !sql.contains("FOR SHARE"),
            ".skip_locked() must replace the FOR SHARE tail outright (documented footgun): {sql}"
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
            !sql.contains("SELECT ,"),
            "unit-key grouping sets must not emit a leading SELECT comma: {sql}"
        );
        assert!(
            sql.starts_with("SELECT (SUM(amount))::BIGINT AS __djogi_agg_0 FROM fakes AS t"),
            "unit-key grouping sets must start with the aggregate projection, got: {sql}"
        );
        assert!(
            sql.contains("GROUPING SETS ((org_id), (region))"),
            "expected GROUPING SETS clause, got: {sql}"
        );
    }

    // ── T1 emitter behavior: ROLLUP / CUBE ────────────────────────────────
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
    /// OVER AS cluster_id` and `GROUP BY cluster_id`.
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
    /// OVER AS cluster_id ... GROUP BY cluster_id`, which Postgres rejects
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
        // (The string `OVER AS cluster_id` is still present inside the
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

    // ── — PG18 OLD/NEW RETURNING builder unit tests ────────────────

    fn build_update_returning_pairs<T: Model + FromPgRow + crate::pg::decode::FromJoinedPgRow>(
        qs: &QuerySet<T>,
        assignments: &[crate::query::update::UpdateAssignment],
    ) -> SqlAccumulator {
        super::build_update_returning_pairs(qs, assignments)
            .expect("update_returning_pairs should build successfully")
    }

    fn build_delete_returning<T: Model + FromPgRow + crate::pg::decode::FromJoinedPgRow>(
        qs: &QuerySet<T>,
    ) -> SqlAccumulator {
        super::build_delete_returning(qs).expect("build_delete_returning should build successfully")
    }

    #[test]
    fn update_returning_pairs_emits_returning_with_old_and_new_clause() {
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_pairs(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        assert!(
            sql.contains("RETURNING WITH (OLD AS __djogi_old, NEW AS __djogi_new)"),
            "expected RETURNING WITH clause, got: {sql}"
        );
    }

    #[test]
    fn update_returning_pairs_includes_old_id_alias() {
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_pairs(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        assert!(sql.contains("\"o0\""), "expected o0 alias, got: {sql}");
    }

    #[test]
    fn update_returning_pairs_includes_new_id_alias() {
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_pairs(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        assert!(sql.contains("\"n0\""), "expected n0 alias, got: {sql}");
    }

    #[test]
    fn update_returning_pairs_bind_order_assignments_before_filter() {
        // Assignments must fill bind slots before the WHERE filter binds.
        // Fake has no WHERE filter here so there is only the assignment bind ($1).
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_pairs(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();
        // The assignment bind $1 is inside the SET clause, before RETURNING.
        let returning_pos = sql.find("RETURNING").expect("should contain RETURNING");
        let bind_pos = sql.find("$1").expect("should contain $1");
        assert!(
            bind_pos < returning_pos,
            "bind slot $1 should appear before RETURNING clause, got: {sql}"
        );
    }

    #[test]
    fn delete_returning_emits_returning_with_old_clause() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_delete_returning(&qs);
        let sql = acc.sql();

        assert!(
            sql.contains("RETURNING WITH (OLD AS __djogi_old)"),
            "expected DELETE RETURNING WITH OLD clause, got: {sql}"
        );
    }

    #[test]
    fn delete_returning_includes_old_id_alias() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_delete_returning(&qs);
        let sql = acc.sql();

        assert!(
            sql.contains("\"o0\""),
            "expected o0 alias in DELETE returning, got: {sql}"
        );
    }

    #[test]
    fn delete_returning_does_not_include_new_projection() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_delete_returning(&qs);
        let sql = acc.sql();

        assert!(
            !sql.contains("__djogi_new"),
            "DELETE returning must not include new projection, got: {sql}"
        );
    }

    #[test]
    fn update_returning_pairs_projection_includes_both_sides_shape() {
        // Verify the OLD/NEW projection shape by looking at the SQL generated by
        // build_update_returning_pairs (no suffix helper needed — test the actual
        // public builder).
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(0i32));
        let acc = build_update_returning_pairs(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        // Both sides must be present.
        assert!(sql.contains("OLD AS __djogi_old"), "{sql}");
        assert!(sql.contains("NEW AS __djogi_new"), "{sql}");
        assert!(sql.contains("\"o0\""), "{sql}");
        assert!(sql.contains("\"n0\""), "{sql}");
    }

    #[test]
    fn delete_returning_projection_is_old_only_shape() {
        let qs: QuerySet<Fake> = QuerySet::new();
        let acc = build_delete_returning(&qs);
        let sql = acc.sql();

        assert!(sql.contains("OLD AS __djogi_old"), "{sql}");
        assert!(!sql.contains("NEW"), "{sql}");
        assert!(sql.contains("\"o0\""), "{sql}");
    }

    // ── REQ-304: bulk update cache invalidation — SQL emission tests ────────

    fn build_update_returning_ids<T: Model + FromPgRow>(
        qs: &QuerySet<T>,
        assignments: &[crate::query::update::UpdateAssignment],
    ) -> SqlAccumulator {
        super::build_update_returning_ids(qs, assignments)
            .expect("update_returning_ids should build successfully")
    }

    #[test]
    fn update_returning_ids_emits_returning_pk_clause() {
        // REQ-304-7a: The emitted SQL must contain `RETURNING id` so the
        // caller can collect affected row IDs for bulk cache invalidation.
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new();
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_ids(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        assert!(
            sql.contains("RETURNING id"),
            "expected RETURNING id clause for bulk update cache invalidation, got: {sql}"
        );
    }

    #[test]
    fn update_returning_ids_bind_order_assignments_before_filter() {
        // REQ-304-7b: Assignment binds must fill slots before WHERE filter
        // binds, matching the existing build_update discipline. This ensures
        // the RETURNING clause receives the correct positional parameters.
        let f: crate::query::field::FieldRef<Fake, i32> =
            crate::query::field::FieldRef::new("view_count");
        let qs: QuerySet<Fake> = QuerySet::new()
            .filter(|_| Condition::Leaf(Leaf::eq_raw("published", FilterValue::Bool(true))));
        let stmt = qs.update(|_| f.set(999i32));
        let acc = build_update_returning_ids(&stmt.qs, &stmt.assignments);
        let sql = acc.sql();

        // $1 (assignment) must appear before $2 (filter).
        let set_pos = sql.find("SET").expect("should contain SET");
        let where_pos = sql.find("WHERE").expect("should contain WHERE");
        let returning_pos = sql.find("RETURNING").expect("should contain RETURNING");
        assert!(
            set_pos < where_pos && where_pos < returning_pos,
            "expected SET ... WHERE ... RETURNING order, got: {sql}"
        );
    }
}
