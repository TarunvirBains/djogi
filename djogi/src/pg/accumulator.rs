//! `SqlAccumulator` — a typed SQL builder with positional `$n` bind parameters.
//!
//! # What
//!
//! `SqlAccumulator` is the SQL construction layer inside Djogi. It accumulates:
//!
//! 1. An SQL string with `$1`, `$2`, ... placeholders for every bound value.
//! 2. A `Vec<Box<dyn postgres_types::ToSql + Sync + Send>>` carrying the bound
//!    values in positional order.
//!
//! The caller calls `into_parts()` to get `(String, Vec<Box<dyn ToSql...>>)`,
//! then executes the query via `tokio_postgres::Client::query` or similar.
//!
//! # Design rationale
//!
//! `postgres_types::ToSql` is the bind trait for tokio-postgres. `SqlAccumulator`
//! stores bound values as `Box<dyn ToSql + Sync + Send>` so the caller can push
//! heterogeneous types into one list without repeated dynamic dispatch at query
//! execution time. The accumulator owns the values for the lifetime of the query.
//!
//! # Parameter counter
//!
//! Postgres uses 1-indexed positional parameters (`$1`, `$2`, ...). Each call to
//! `push_bind` appends `$<next_param>` to the SQL string and increments the counter.
//! The accumulator is always created fresh per top-level query, so the counter
//! resets naturally. For nested subqueries that share the outer accumulator (e.g.
//! `SubqueryNode` in `expr::sql`), the counter continues incrementing globally —
//! matching `tokio_postgres::Client::query`'s parameter semantics.
//!
//! # SQL injection guarantee
//!
//! Only `push_sql` inserts raw text, and its callers are restricted to:
//! - SQL keywords (e.g. `" WHERE "`, `" ORDER BY "`, `" AND "`)
//! - `&'static str` table names and column names baked by `#[model]` macros
//!
//! User data always flows through `push_bind` as a parameterised value.
//! `push_null_literal` is the one special case — it appends the literal token
//! `NULL` (not a bind slot) because Postgres's three-valued logic means
//! `col = $1` with `NULL` bound is never `TRUE`, whereas `col IS NULL` is the
//! correct SQL for null-equality checks.

use postgres_types::ToSql;
use std::fmt::Write;

/// A positional-parameter SQL accumulator for Postgres.
///
/// Collects raw SQL fragments (keywords, identifiers) and typed bind values
/// into a `(String, Vec<Box<dyn ToSql + Sync + Send>>)` pair ready to be
/// dispatched via `tokio_postgres::Client::query` or `Client::execute`.
///
/// `#[doc(hidden)]` — internal SQL-emission substrate used by the
/// `#[model]` macro (via `::djogi::__private::pg::SqlAccumulator`) and
/// the framework's QuerySet emitter. Adopters reach raw SQL through
/// [`crate::context::DjogiContext::raw_query`] /
/// [`crate::context::DjogiContext::raw_execute`], which take a string
/// + bind slice directly — they never construct an accumulator.
#[doc(hidden)]
pub struct SqlAccumulator {
    /// The accumulated SQL text. Contains `$1`, `$2`, ... placeholders wherever
    /// `push_bind` was called.
    sql: String,

    /// Bound values in positional order matching the `$1`, `$2`, ... placeholders.
    binds: Vec<Box<dyn ToSql + Sync + Send>>,

    /// The next positional parameter index. Starts at 1 (Postgres is 1-indexed).
    next_param: u32,
}

impl SqlAccumulator {
    /// Create a new accumulator, pre-populated with an initial SQL fragment.
    ///
    /// The initial SQL is typically the static prefix of the query (e.g.
    /// `"SELECT * FROM users"`) — it must never contain user-controlled data.
    pub fn new(initial_sql: &str) -> Self {
        SqlAccumulator {
            sql: initial_sql.to_owned(),
            binds: Vec::new(),
            next_param: 1,
        }
    }

    /// Push a raw SQL fragment — keywords and identifiers only, never user data.
    ///
    /// Appends `s` to the accumulated SQL string without allocating a bind slot.
    /// The caller is responsible for ensuring `s` contains only trusted SQL text.
    pub fn push_sql(&mut self, s: &str) {
        self.sql.push_str(s);
    }

    /// Push one typed bind value. Appends `$<next_param>` to the SQL string,
    /// stores the value in the bind vector, and increments the parameter counter.
    pub fn push_bind<T>(&mut self, v: T)
    where
        T: ToSql + Sync + Send + 'static,
    {
        // `write!` against `String` writes the integer via `fmt::Write` —
        // no intermediate `String` allocation per `$n` slot.
        // `write!` into `String` cannot fail, so the result is discarded.
        let _ = write!(self.sql, "${}", self.next_param);
        self.binds.push(Box::new(v));
        self.next_param += 1;
    }

    /// Push a list of bind values separated by commas, for `IN (...)` / `NOT IN (...)` lists.
    ///
    /// Each element gets its own `$n` slot. The caller is responsible for emitting
    /// the opening `(` before calling this and `)` after — this method emits only
    /// the comma-separated `$n, $m, ...` list, not the surrounding parentheses.
    ///
    /// Empty iterators are a no-op. Callers that need `IN ()` short-circuit
    /// behaviour (which is a Postgres syntax error) should check `is_empty()` and
    /// emit `FALSE` or `TRUE` before calling this.
    pub fn push_list_binds<T, I>(&mut self, iter: I)
    where
        T: ToSql + Sync + Send + 'static,
        I: IntoIterator<Item = T>,
    {
        let mut first = true;
        for v in iter {
            if !first {
                self.sql.push_str(", ");
            }
            first = false;
            self.push_bind(v);
        }
    }

    /// Push an iterator of string fragments separated by `, ` — for column
    /// lists, GROUPING SETS, and any place an SQL emitter walks an iterator
    /// of identifiers under a parenthesized comma-separated shape.
    ///
    /// Each item is `push_sql`'d directly; the caller is responsible for
    /// ensuring items contain only trusted SQL text (column names baked by
    /// `#[model]` / `FieldRef::column()` — `&'static str` either way).
    /// Empty iterators are a no-op.
    pub fn push_csv<'a, I: IntoIterator<Item = &'a str>>(&mut self, items: I) {
        let mut first = true;
        for s in items {
            if !first {
                self.sql.push_str(", ");
            }
            first = false;
            self.sql.push_str(s);
        }
    }

    /// Push the literal token `NULL` — NOT a bind slot.
    ///
    /// Used for `IS NULL` / `IS NOT NULL` operator emission and for
    /// `FilterValue::Null` in the condition emitter. Postgres's three-valued
    /// logic means `col = $1` (with `NULL` bound as a parameter) is always
    /// `FALSE` (not `NULL`) — the correct way to check for null is the SQL
    /// keyword `NULL` in the context of `IS NULL` / `IS NOT NULL` clauses.
    ///
    /// Does NOT increment the parameter counter.
    pub fn push_null_literal(&mut self) {
        self.sql.push_str("NULL");
    }

    /// Splice another accumulator's accumulated SQL and binds into this one.
    ///
    /// The other accumulator's `$1`, `$2`, ... placeholders are renumbered to
    /// continue this accumulator's `next_param` sequence so the merged SQL
    /// stays positional. Used by emitters that wrap an inner-built SQL in an
    /// outer scope (e.g. derived-table qualify lowering — see
    /// `build_annotated_select_for_fetch`).
    ///
    /// Renumbering is a textual rewrite over `$N` runs in `other`'s SQL — `$N`
    /// only appears in trusted positional-bind sites because every emitter
    /// routes user data through `push_bind`, never `push_sql`. ASCII `$`
    /// outside that role does not occur in any current emitter; if a future
    /// emitter introduces literal `$` text it must use `push_bind` (no
    /// scenario for a literal `$` in trusted SQL today).
    pub fn extend_with(&mut self, other: SqlAccumulator) {
        let SqlAccumulator {
            sql: other_sql,
            binds: other_binds,
            next_param: _,
        } = other;
        let offset = self.next_param - 1;
        if offset == 0 {
            self.sql.push_str(&other_sql);
        } else {
            // Reserve up front so multiple realloc-doublings don't kick in
            // for long inner SQL — renumbering grows by at most 1 byte per
            // placeholder ($9 -> $19 etc.), so `other_sql.len() +
            // other_binds.len()` is a safe upper bound.
            self.sql.reserve(other_sql.len() + other_binds.len());

            // Slice-based flush: walk byte indices, and whenever a
            // `$<digits>` run starts, push the contiguous run BEFORE it as
            // one `push_str`, then write the renumbered placeholder, then
            // resume from after the digits. Avoids per-byte `push(c as
            // char)` while keeping the renumbering logic single-pass.
            let bytes = other_sql.as_bytes();
            let mut start = 0;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    if start < i {
                        self.sql.push_str(&other_sql[start..i]);
                    }
                    let mut j = i + 1;
                    let mut n: u32 = 0;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        n = n * 10 + (bytes[j] - b'0') as u32;
                        j += 1;
                    }
                    let _ = write!(self.sql, "${}", n + offset);
                    i = j;
                    start = j;
                } else {
                    i += 1;
                }
            }
            if start < bytes.len() {
                self.sql.push_str(&other_sql[start..]);
            }
        }
        self.next_param += other_binds.len() as u32;
        self.binds.extend(other_binds);
    }

    /// Consume the accumulator and return the `(sql_text, binds_vec)` pair.
    ///
    /// The binds vec is in positional order matching the `$1`, `$2`, ... slots
    /// in the SQL text. The caller uses [`as_params`] to reborrow the boxed
    /// vec as the `&[&(dyn ToSql + Sync)]` slice `tokio_postgres` expects:
    ///
    /// ```ignore
    /// let (sql, binds) = acc.into_parts();
    /// let params = djogi::pg::accumulator::as_params(&binds);
    /// conn.query(&sql, &params).await?
    /// ```
    pub fn into_parts(self) -> (String, Vec<Box<dyn ToSql + Sync + Send>>) {
        (self.sql, self.binds)
    }

    /// Read-only view of the accumulated SQL text.
    ///
    /// Used by unit tests that assert SQL shape without executing, and by
    /// any internal helper that needs to inspect the current SQL string.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Number of bind slots pushed so far (`next_param - 1`).
    ///
    /// Used by tests and by helpers that need to know the current `$n` position
    /// before composing a subquery or conditional clause.
    pub fn bind_count(&self) -> u32 {
        self.next_param - 1
    }
}

/// Reborrow a `&[Box<dyn ToSql + Sync + Send>]` as a `Vec<&(dyn ToSql + Sync)>`.
///
/// The query layer accumulates bind values as `Box<dyn ToSql + Sync + Send>`
/// for storage flexibility, but `tokio_postgres::Client::query` takes
/// `&[&(dyn ToSql + Sync)]`. Every terminal in `query::*` performs the same
/// reborrow; centralising it here keeps the call sites uniform.
pub fn as_params(binds: &[Box<dyn ToSql + Sync + Send>]) -> Vec<&(dyn ToSql + Sync)> {
    binds
        .iter()
        .map(|b| b.as_ref() as &(dyn ToSql + Sync))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accumulator_starts_empty() {
        let acc = SqlAccumulator::new("SELECT 1");
        assert_eq!(acc.sql(), "SELECT 1");
        assert_eq!(acc.bind_count(), 0);
    }

    #[test]
    fn push_sql_appends_raw_text() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t");
        acc.push_sql(" WHERE active = ");
        acc.push_sql("TRUE");
        assert_eq!(acc.sql(), "SELECT * FROM t WHERE active = TRUE");
        assert_eq!(acc.bind_count(), 0);
    }

    #[test]
    fn push_bind_inserts_positional_placeholder() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE id = ");
        acc.push_bind(42_i64);
        assert_eq!(acc.sql(), "SELECT * FROM t WHERE id = $1");
        assert_eq!(acc.bind_count(), 1);

        acc.push_sql(" AND name = ");
        acc.push_bind("alice".to_owned());
        assert_eq!(acc.sql(), "SELECT * FROM t WHERE id = $1 AND name = $2");
        assert_eq!(acc.bind_count(), 2);
    }

    #[test]
    fn push_list_binds_produces_comma_separated_params() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE id IN (");
        acc.push_list_binds([1_i32, 2, 3]);
        acc.push_sql(")");
        assert_eq!(acc.sql(), "SELECT * FROM t WHERE id IN ($1, $2, $3)");
        assert_eq!(acc.bind_count(), 3);
    }

    #[test]
    fn push_list_binds_empty_is_noop() {
        let mut acc = SqlAccumulator::new("SELECT 1");
        acc.push_list_binds(std::iter::empty::<i32>());
        assert_eq!(acc.sql(), "SELECT 1");
        assert_eq!(acc.bind_count(), 0);
    }

    #[test]
    fn push_null_literal_does_not_allocate_slot() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE col IS ");
        acc.push_null_literal();
        assert_eq!(acc.sql(), "SELECT * FROM t WHERE col IS NULL");
        assert_eq!(acc.bind_count(), 0);
    }

    #[test]
    fn into_parts_returns_sql_and_binds() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE id = ");
        acc.push_bind(99_i64);
        let (sql, binds) = acc.into_parts();
        assert_eq!(sql, "SELECT * FROM t WHERE id = $1");
        assert_eq!(binds.len(), 1);
    }

    #[test]
    fn extend_with_renumbers_inner_dollar_n_relative_to_outer_offset() {
        let mut inner = SqlAccumulator::new("SELECT * FROM t WHERE id = ");
        inner.push_bind(7_i64);
        inner.push_sql(" AND name = ");
        inner.push_bind("alice");

        let mut outer = SqlAccumulator::new("SELECT * FROM (");
        outer.push_sql("inline ");
        outer.push_bind(99_i32);
        outer.push_sql(", ");
        outer.extend_with(inner);
        outer.push_sql(") WHERE rank <= ");
        outer.push_bind(3_i32);

        let (sql, binds) = outer.into_parts();
        assert!(
            sql.starts_with("SELECT * FROM (inline $1, SELECT * FROM t WHERE id = $2"),
            "got: {sql}"
        );
        assert!(sql.contains("AND name = $3"), "got: {sql}");
        assert!(sql.ends_with("WHERE rank <= $4"), "got: {sql}");
        assert_eq!(binds.len(), 4);
    }

    #[test]
    fn extend_with_at_offset_zero_preserves_inner_dollar_numbers() {
        let mut inner = SqlAccumulator::new("SELECT a = ");
        inner.push_bind(1_i64);
        inner.push_sql(", b = ");
        inner.push_bind(2_i64);

        let mut outer = SqlAccumulator::new("");
        outer.extend_with(inner);
        let (sql, binds) = outer.into_parts();
        assert_eq!(sql, "SELECT a = $1, b = $2");
        assert_eq!(binds.len(), 2);
    }

    #[test]
    fn bind_count_tracks_push_bind_calls() {
        let mut acc = SqlAccumulator::new("");
        assert_eq!(acc.bind_count(), 0);
        acc.push_bind(1_i32);
        assert_eq!(acc.bind_count(), 1);
        acc.push_bind(2_i32);
        assert_eq!(acc.bind_count(), 2);
        acc.push_null_literal(); // does not increment
        assert_eq!(acc.bind_count(), 2);
    }

    // ── Injection-safety tests ────────────────────────────────────────────────
    //
    // These three tests verify the SQL-injection safety contract of
    // `SqlAccumulator`. Each test documents a distinct injection-safety
    // invariant that the accumulator must uphold regardless of the value
    // supplied by the caller.

    /// Bound values must never appear verbatim in the SQL text — only
    /// positional `$n` placeholders may appear. A caller supplying a
    /// value that looks like SQL (e.g. `"'; DROP TABLE users; --"`) must
    /// not see that text in the emitted SQL string.
    #[test]
    fn push_bind_never_leaks_into_sql_text() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE name = ");
        acc.push_bind("'; DROP TABLE users; --".to_owned());
        let sql = acc.sql();
        // The SQL string must contain the placeholder, not the raw value.
        assert!(
            sql.contains("$1"),
            "expected $1 placeholder in SQL, got: {sql}"
        );
        assert!(
            !sql.contains("DROP"),
            "user-supplied value leaked into SQL text: {sql}"
        );
        // The bind vector carries the actual value; SQL carries only the placeholder.
        let (sql_out, binds) = acc.into_parts();
        assert_eq!(sql_out, "SELECT * FROM t WHERE name = $1");
        assert_eq!(binds.len(), 1);
    }

    /// `push_list_binds` must emit exactly one `$n` placeholder per element,
    /// comma-separated, and never inline any element's text into the SQL.
    #[test]
    fn push_list_binds_emits_one_placeholder_per_element() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE id IN (");
        // Supply values that look like SQL — none should appear in the SQL text.
        acc.push_list_binds(["1 OR 1=1".to_owned(), "2".to_owned(), "3".to_owned()]);
        acc.push_sql(")");
        let sql = acc.sql();
        assert_eq!(
            sql, "SELECT * FROM t WHERE id IN ($1, $2, $3)",
            "expected exactly three placeholders, got: {sql}"
        );
        assert!(
            !sql.contains("OR"),
            "user-supplied value leaked into SQL text: {sql}"
        );
        assert_eq!(acc.bind_count(), 3);
    }

    /// `push_null_literal` emits the SQL keyword `NULL` directly — NOT a
    /// bind slot. The parameter counter must not increment, and no extra
    /// `$n` placeholder must appear.
    #[test]
    fn push_null_literal_emits_sql_null_not_placeholder() {
        let mut acc = SqlAccumulator::new("SELECT * FROM t WHERE col IS ");
        acc.push_null_literal();
        let sql = acc.sql();
        assert_eq!(
            sql, "SELECT * FROM t WHERE col IS NULL",
            "expected literal NULL in SQL, got: {sql}"
        );
        // No bind slot allocated — the counter must still be zero.
        assert_eq!(
            acc.bind_count(),
            0,
            "push_null_literal must not allocate a bind slot"
        );
        let (_, binds) = acc.into_parts();
        assert!(
            binds.is_empty(),
            "no bind values expected after push_null_literal"
        );
    }
}
