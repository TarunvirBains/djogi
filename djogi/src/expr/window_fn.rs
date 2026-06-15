//! Window-only ranking functions for annotated querysets.
//! `ROW_NUMBER`, `RANK`, and `DENSE_RANK` are window functions, not
//! aggregates. Djogi models them separately from [`super::AggregateExpr`] so
//! they cannot be used in aggregate-only terminals and so the generated SQL
//! always includes an `OVER (...)` clause.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::order::OrderExpr;

use super::window::WindowSpec;

/// One-shot comparison against an annotated window-function alias, lowered
/// to an outer-`WHERE` predicate over a derived table by
/// [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify).
/// PostgreSQL 18 has no `QUALIFY` clause, so a window-output filter has to
/// be applied in an outer scope where the alias is in scope as a column
/// reference. `QualifyCondition` captures that filter without ever
/// emitting the literal `QUALIFY` token.
/// The value payload is typed: `BIGINT`-returning windows (`RowNumber`,
/// `Rank`, `DenseRank`) produce an `i64` bind; `FLOAT8`-returning windows
/// (`PercentRankWindow`, `CumeDistWindow`) produce an `f64` bind.
/// The correct variant is selected by the window type's comparison helpers;
/// adopters do not construct `QualifyCondition` directly.
#[derive(Debug, Clone)]
#[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
pub struct QualifyCondition {
    pub(crate) alias: &'static str,
    pub(crate) op: QualifyOp,
    pub(crate) value: QualifyValue,
}

/// The typed bind value carried by a [`QualifyCondition`].
/// `BIGINT` window functions (`ROW_NUMBER`, `RANK`, `DENSE_RANK`) store
/// [`QualifyValue::Int`]; `FLOAT8` window functions (`PERCENT_RANK`,
/// `CUME_DIST`) store [`QualifyValue::Float`]. The correct variant is chosen
/// by the window type's comparison helpers — adopters never construct this
/// directly.
#[derive(Debug, Clone, Copy)]
pub(crate) enum QualifyValue {
    /// Integer threshold for `BIGINT`-returning window functions.
    Int(i64),
    /// Float threshold for `FLOAT8`-returning window functions
    /// (`PERCENT_RANK`, `CUME_DIST`). The value is bound as a Postgres
    /// `FLOAT8` parameter and compared against the window alias column.
    Float(f64),
}

/// Comparison operator family supported on window-output aliases.
/// Covers both `BIGINT`-returning windows (`RowNumber`, `Rank`, `DenseRank`)
/// and `FLOAT8`-returning windows (`PercentRankWindow`, `CumeDistWindow`).
/// The correct value type is carried separately in [`QualifyCondition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualifyOp {
    Lt,
    Lte,
    Eq,
    Gte,
    Gt,
}

impl QualifyOp {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            QualifyOp::Lt => "<",
            QualifyOp::Lte => "<=",
            QualifyOp::Eq => "=",
            QualifyOp::Gte => ">=",
            QualifyOp::Gt => ">",
        }
    }
}

impl QualifyCondition {
    pub(crate) fn push_outer_where(&self, acc: &mut SqlAccumulator) {
        acc.push_sql(self.alias);
        acc.push_sql(" ");
        acc.push_sql(self.op.sql());
        acc.push_sql(" ");
        match self.value {
            QualifyValue::Int(v) => acc.push_bind(v),
            QualifyValue::Float(v) => acc.push_bind(v),
        }
    }
}

/// Build a `QualifyCondition` for `BIGINT`-returning window functions
/// (`ROW_NUMBER`, `RANK`, `DENSE_RANK`).
/// # Panics
/// Panics when `alias` is `None`, i.e. when `.alias("…")` was not called
/// before the comparison helper.
fn build_qualify(alias: Option<&'static str>, op: QualifyOp, value: i64) -> QualifyCondition {
    QualifyCondition {
        alias: alias.expect(
            "qualify can only reference a window annotation that was registered with.alias(\"…\")",
        ),
        op,
        value: QualifyValue::Int(value),
    }
}

/// Build a `QualifyCondition` for `FLOAT8`-returning window functions
/// (`PERCENT_RANK`, `CUME_DIST`).
/// # Panics
/// Panics when `alias` is `None`, i.e. when `.alias("…")` was not called
/// before the comparison helper.
fn build_qualify_f64(alias: Option<&'static str>, op: QualifyOp, value: f64) -> QualifyCondition {
    QualifyCondition {
        alias: alias.expect(
            "qualify can only reference a window annotation that was registered with.alias(\"…\")",
        ),
        op,
        value: QualifyValue::Float(value),
    }
}

/// Sealing module — `Sealed` is private so adopters cannot implement
/// [`WindowRanking`] for their own types.
mod sealed_ranking {
    pub trait Sealed {}
}

/// Comparison helpers shared by every typed window-only ranking function.
/// `RowNumber`, `Rank`, and `DenseRank` all return `BIGINT`, so the typed
/// `lt` / `lte` / `eq` / `gte` / `gt` lowering helpers are identical across
/// the three types — only the underlying SQL keyword differs. This trait
/// captures that shared surface as default methods so the per-type macro
/// expansion is a 3-line `impl WindowRanking` rather than 5 × 3 = 15
/// duplicate method bodies.
/// Sealed: only the `RowNumber` / `Rank` / `DenseRank` types in this module
/// implement it.
pub trait WindowRanking: sealed_ranking::Sealed {
    /// The output alias registered with [`alias`](#tymethod.alias)-equivalent
    /// builder methods. Returns `None` until an alias has been set; calling a
    /// comparison helper before `.alias("…")` panics with the same diagnostic
    /// across all three rank types.
    fn alias_name(&self) -> Option<&'static str>;

    /// `<alias> < value` — outer `WHERE` predicate over the derived table
    /// that wraps the annotated select. Lowering target for
    /// `.qualify(|w| w.lt(...))`.
    fn lt(&self, value: i64) -> QualifyCondition {
        build_qualify(self.alias_name(), QualifyOp::Lt, value)
    }

    /// `<alias> <= value` — see [`lt`](Self::lt) for lowering shape.
    fn lte(&self, value: i64) -> QualifyCondition {
        build_qualify(self.alias_name(), QualifyOp::Lte, value)
    }

    /// `<alias> = value` — see [`lt`](Self::lt) for lowering shape.
    fn eq(&self, value: i64) -> QualifyCondition {
        build_qualify(self.alias_name(), QualifyOp::Eq, value)
    }

    /// `<alias> >= value` — see [`lt`](Self::lt) for lowering shape.
    fn gte(&self, value: i64) -> QualifyCondition {
        build_qualify(self.alias_name(), QualifyOp::Gte, value)
    }

    /// `<alias> > value` — see [`lt`](Self::lt) for lowering shape.
    fn gt(&self, value: i64) -> QualifyCondition {
        build_qualify(self.alias_name(), QualifyOp::Gt, value)
    }
}

macro_rules! define_window_rank_fn {
 ($type_name:ident, $sql_name:literal, $example_name:literal) => {
  #[doc = concat!(
   "`",
   $sql_name,
   "() OVER (...)` window-only annotation returning `i64`.\n\n",
   "# What\n\n",
   "Use this type inside [`QuerySet::annotate`](crate::query::QuerySet::annotate) ",
   "when each returned model row needs a per-partition ranking value. ",
   "The function is window-only: Djogi always emits `OVER (...)`; it never ",
   "lowers to a bare function call.\n\n",
   "# Why\n\n",
   "PostgreSQL 18 does not support a `QUALIFY` clause, so filters over ",
   "window outputs are lowered by [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify) ",
   "to an outer `WHERE` over a derived table. The alias supplied with ",
   "[`alias`](Self::alias) becomes the column name that outer filter can reference.\n\n",
   "# How\n\n",
   "Build the window with [`partition_by`](Self::partition_by), ",
   "[`order_by`](Self::order_by), and a required [`alias`](Self::alias). ",
   "The alias must be a plain unquoted PostgreSQL identifier.\n\n",
   "# Example\n\n",
   "```ignore\n",
   "use djogi::prelude::*;\n\n",
   "let rows = Elephant::objects()\n",
   ".annotate(|e| ", stringify!($type_name), "::new()\n",
   " .partition_by(e.herd_id())\n",
   " .order_by(e.score().desc())\n",
   " .alias(\"", $example_name, "\"))\n",
   ".qualify(|w| w.lte(3))\n",
   ".fetch_all(&mut ctx)\n",
   ".await?;\n",
   "# Ok::<_, djogi::DjogiError>(())\n",
   "```\n\n",
   "The SQL shape is a derived table rather than `QUALIFY`:\n\n",
   "```sql\n",
   "SELECT * FROM (\n",
   " SELECT t.id, ",
   $sql_name,
   "() OVER (PARTITION BY herd_id ORDER BY score DESC) AS ",
   $example_name,
   "\n",
   " FROM elephants AS t\n",
   ") AS __djogi_q\n",
   "WHERE ",
   $example_name,
   " <= $1\n",
   "```"
  )]
  #[must_use = "window functions are lazy annotations - dropping one omits the column"]
  #[derive(Debug, Clone, Default)]
  pub struct $type_name {
   pub(crate) window: WindowSpec,
   pub(crate) alias: Option<&'static str>,
  }

  impl $type_name {
   #[doc = concat!(
    "Construct an empty `",
    $sql_name,
    "() OVER ()` window annotation.\n\n",
    "# What\n\n",
    "The returned builder has no partitioning, ordering, frame, or alias yet.\n\n",
    "# Why\n\n",
    "The window spec starts empty so callers can opt into the exact ",
    "partition and order required by the ranking query. The function still ",
    "remains window-only because emission appends `OVER ()` even when no ",
    "builder methods are called.\n\n",
    "# How\n\n",
    "Chain [`partition_by`](Self::partition_by), [`order_by`](Self::order_by), ",
    "and the required [`alias`](Self::alias) before passing it to `.annotate(...)`.\n\n",
    "# Example\n\n",
    "```ignore\n",
    "let rank = ", stringify!($type_name), "::new()\n",
    ".order_by(fields.score().desc())\n",
    ".alias(\"", $example_name, "\");\n",
    "```"
   )]
   pub fn new() -> Self {
    Self {
     window: WindowSpec::default(),
     alias: None,
    }
   }

   /// Add a `PARTITION BY` column to this window function.
   /// # What
   /// The column comes from a typed [`FieldRef`], so it has already
   /// passed Djogi's identifier validation path.
   /// # Why
   /// Partitioning restarts the row numbering or ranking per group,
   /// which is the common "top N per parent" shape.
   /// # How
   /// Call this once per partition key before `.alias(...)`.
   /// ```ignore
   /// RowNumber::new()
   /// .partition_by(fields.herd_id())
   /// .order_by(fields.score().desc())
   /// .alias("rank");
   /// ```
   /// PR3: accepts `FieldRef<M, V>` or the post-flip root
   /// accessor return type `DjogiField<M, V>` through
   /// [`IntoSqlField`](crate::query::field::IntoSqlField).
   /// Window partitions are SQL-only emission boundaries.
   #[must_use = "window functions are immutable builders - use the returned value"]
   pub fn partition_by<M, V, S>(mut self, field: S) -> Self
   where
    M: Model,
    S: crate::query::field::IntoSqlField<M, V>,
   {
    self.window.partition_by.push(field.into_sql_field().column());
    self
   }

   /// Add an `ORDER BY` term to this window function.
   /// # What
   /// Accepts the [`OrderExpr`] produced by `FieldRef::asc()` or
   /// `FieldRef::desc()` and stores its column and direction in the
   /// window spec.
   /// # Why
   /// Ranking functions are only deterministic when the partition has
   /// an explicit order. Djogi keeps the order typed by reusing the
   /// same `OrderExpr` surface as `QuerySet::order_by`.
   /// # How
   /// Pass `field.asc()` or `field.desc()`:
   /// ```ignore
   /// Rank::new()
   /// .partition_by(fields.herd_id())
   /// .order_by(fields.score().desc())
   /// .alias("rank");
   /// ```
   /// # Panics
   /// Panics if called with a spatial-distance ordering. The current
   /// [`WindowSpec`] stores column-name ordering terms, which is enough
   /// for typed ranking functions but not expression-backed spatial
   /// ordering.
   #[must_use = "window functions are immutable builders - use the returned value"]
   pub fn order_by(mut self, order: OrderExpr) -> Self {
    push_order_expr(&mut self.window, order);
    self
   }

   /// Set the output alias used by annotation decode and outer filters.
   /// # What
   /// The alias is emitted as `AS <alias>` in the inner annotated
   /// select and becomes the column referenced by
   /// [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify).
   /// # Why
   /// PostgreSQL 18 has no `QUALIFY` clause, so Djogi lowers
   /// `.qualify(...)` into `SELECT * FROM (<annotated select>) AS
   /// __djogi_q WHERE <alias predicate>`. A stable alias is required
   /// for that outer predicate and for row decoding.
   /// # How
   /// Pass a plain unquoted PostgreSQL identifier:
   /// ```ignore
   /// DenseRank::new()
   /// .order_by(fields.score().desc())
   /// .alias("dense_rank");
   /// ```
   /// # Panics
   /// Panics when `alias` is empty, longer than PostgreSQL's usable
   /// identifier length, starts with an invalid byte, contains an
   /// invalid byte, or is a reserved PostgreSQL keyword. Also panics
   /// when the alias starts with the `__djogi_` framework-reserved
   /// prefix (e.g. `__djogi_q` would shadow the derived-table name
   /// used by qualify lowering; `__djogi_agg_N` would collide with
   /// the aggregate-tuple slot aliases used by row decode).
   /// User-chosen aliases SHOULD also avoid colliding with the
   /// model's own column names — the outer `WHERE <alias>` would
   /// then reference the underlying column instead of the window
   /// output. The framework cannot enforce this here because
   /// [`alias`](Self::alias) does not know the eventual `T`; it is
   /// the caller's responsibility to pick a non-colliding alias.
   #[must_use = "window functions are immutable builders - use the returned value"]
   pub fn alias(mut self, alias: &'static str) -> Self {
    crate::ident::assert_user_supplied_ident(alias, "window_alias");
    self.alias = Some(alias);
    self
   }

   pub(crate) fn alias_name(&self) -> Option<&'static str> {
    self.alias
   }

   pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
    acc.push_sql($sql_name);
    acc.push_sql("()");
    self.window.emit(acc);
    acc.push_sql(" AS ");
    acc.push_sql(
     self.alias
     .expect("window function annotations are checked before SQL emission"),
    );
   }

   // Inherent comparison wrappers — keep `use djogi::RowNumber; w.lte(3)`
   // working without forcing callers to also import [`WindowRanking`].
   // Each one-liner delegates to the trait's default body so the actual
   // qualify-lowering logic is written exactly once (in the trait).

   /// `<alias> < value` — see [`WindowRanking::lt`].
   pub fn lt(&self, value: i64) -> QualifyCondition {
    <Self as WindowRanking>::lt(self, value)
   }

   /// `<alias> <= value` — see [`WindowRanking::lte`].
   pub fn lte(&self, value: i64) -> QualifyCondition {
    <Self as WindowRanking>::lte(self, value)
   }

   /// `<alias> = value` — see [`WindowRanking::eq`].
   pub fn eq(&self, value: i64) -> QualifyCondition {
    <Self as WindowRanking>::eq(self, value)
   }

   /// `<alias> >= value` — see [`WindowRanking::gte`].
   pub fn gte(&self, value: i64) -> QualifyCondition {
    <Self as WindowRanking>::gte(self, value)
   }

   /// `<alias> > value` — see [`WindowRanking::gt`].
   pub fn gt(&self, value: i64) -> QualifyCondition {
    <Self as WindowRanking>::gt(self, value)
   }
  }

  impl sealed_ranking::Sealed for $type_name {}

  impl WindowRanking for $type_name {
   fn alias_name(&self) -> Option<&'static str> {
    self.alias
   }
  }
 };
}

define_window_rank_fn!(RowNumber, "ROW_NUMBER", "rank");
define_window_rank_fn!(Rank, "RANK", "rank");
define_window_rank_fn!(DenseRank, "DENSE_RANK", "dense_rank");

/// `PERCENT_RANK() OVER (...)` window-only annotation returning `f64`.
/// Zero-arg window function — the position of the
/// current row as a fraction in `[0.0, 1.0]` within its partition,
/// computed as `(rank - 1) / (total_rows - 1)`. First row is `0.0`;
/// last is `1.0`. Ties share the same fraction.
/// # Distinct from PercentRank in [`crate::expr::AggregateExpr`]
/// The hypothetical-set form (`f.col.percent_rank_of(value)`)
/// takes a literal value and answers "what fraction
/// would this hypothetical value have if inserted?". This window form
/// (`PercentRankWindow::new()` annotated on each row) gives every
/// returned row its actual fraction in the partition.
/// # Comparison helpers
/// `PERCENT_RANK` returns `f64`. Use `.lt(0.5)`, `.lte(0.9)`,
/// `.gte(0.9)`, `.gt(0.0)`, or `.eq(1.0)` inside a
/// [`.qualify(...)`](crate::query::AnnotatedQuerySet::qualify) closure
/// to filter on the computed fraction:
/// ```ignore
/// // Top half of each region by amount (ORDER BY amount DESC ⇒ rank 0 = highest).
/// let rows = Sale::objects()
/// .annotate(|f| PercentRankWindow::new()
///  .partition_by(f.region_id())
///  .order_by(f.amount().desc())
///  .alias("amount_pct"))
/// .qualify(|w| w.lt(0.5))
/// .fetch_all(&mut ctx).await?;
/// ```
/// The SQL shape is a derived table rather than `QUALIFY`:
/// ```sql
/// SELECT * FROM (
///  SELECT t.id,...,
///   PERCENT_RANK() OVER (PARTITION BY region_id ORDER BY amount DESC)
///    AS amount_pct
///  FROM sales AS t
/// ) AS __djogi_q
/// WHERE amount_pct < $1
/// ```
/// # Example
/// ```ignore
/// let rows = Sale::objects()
/// .annotate(|f| PercentRankWindow::new()
///  .partition_by(f.region_id())
///  .order_by(f.amount().desc())
///  .alias("amount_pct"))
/// .qualify(|w| w.gte(0.9))
/// .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone, Default)]
pub struct PercentRankWindow {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
}

/// `CUME_DIST() OVER (...)` window-only annotation returning `f64`.
/// Zero-arg window function — the cumulative
/// distribution: `(rows preceding or peer with current) / total_rows`
/// in the partition. Result is in `(0.0, 1.0]`. First-position rows
/// get `1/total`; last-position rows get `1.0`.
/// # Distinct from cume_dist_of in [`crate::expr::AggregateExpr`]
/// The hypothetical-set form (`f.col.cume_dist_of(value)`)
/// answers "what fraction would rank at-or-below this value?".
/// This window form gives every row its actual cume-dist position in
/// the partition.
/// # Comparison helpers
/// Same f64 qualify helpers as [`PercentRankWindow`] — `.lt(0.5)`,
/// `.lte(0.9)`, `.gte(0.1)`, `.gt(0.0)`, and `.eq(1.0)` inside a
/// [`.qualify(...)`](crate::query::AnnotatedQuerySet::qualify) closure.
/// ```ignore
/// // Rows in the top 10 % by cumulative distribution.
/// let rows = Sale::objects()
/// .annotate(|f| CumeDistWindow::new()
///  .partition_by(f.region_id())
///  .order_by(f.amount().asc())
///  .alias("cume_dist"))
/// .qualify(|w| w.gte(0.9))
/// .fetch_all(&mut ctx).await?;
/// ```
/// # Example
/// ```ignore
/// let rows = Sale::objects()
/// .annotate(|f| CumeDistWindow::new()
///  .partition_by(f.region_id())
///  .order_by(f.amount().asc())
///  .alias("cume_dist"))
/// .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone, Default)]
pub struct CumeDistWindow {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
}

macro_rules! impl_zero_arg_f64_window {
    ($type_name:ident, $sql_name:literal) => {
        impl $type_name {
            /// Construct an empty window annotation. Build via
            /// `partition_by` / `order_by` and finalize with `alias`.
            pub fn new() -> Self {
                Self {
                    window: WindowSpec::default(),
                    alias: None,
                }
            }

            /// Add a `PARTITION BY` column. PR3: accepts
            /// `FieldRef<M, V>` or `DjogiField<M, V>` through
            /// [`IntoSqlField`](crate::query::field::IntoSqlField).
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn partition_by<M, V, S>(mut self, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                self.window
                    .partition_by
                    .push(field.into_sql_field().column());
                self
            }

            /// Add an `ORDER BY` term. Spatial-distance orderings
            /// panic — pass a column `asc()` or `desc()`.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn order_by(mut self, order: OrderExpr) -> Self {
                push_order_expr(&mut self.window, order);
                self
            }

            /// Set the output alias. Required before SQL emission.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn alias(mut self, alias: &'static str) -> Self {
                crate::ident::assert_user_supplied_ident(alias, "window_alias");
                self.alias = Some(alias);
                self
            }

            pub(crate) fn alias_name(&self) -> Option<&'static str> {
                self.alias
            }

            pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
                acc.push_sql($sql_name);
                acc.push_sql("()");
                self.window.emit(acc);
                acc.push_sql(" AS ");
                acc.push_sql(
                    self.alias
                        .expect("window function annotations are checked before SQL emission"),
                );
            }

            // ── f64 qualify helpers ─────────────────────────────────────────────
            // `PERCENT_RANK` and `CUME_DIST` both return `FLOAT8`. These
            // inherent helpers let callers write `.qualify(|w| w.lt(0.5))`
            // without importing any additional trait. Each delegates to
            // `build_qualify_f64` (the f64 twin of the i64 `build_qualify`
            // used by the rank-family), which stores a `QualifyValue::Float`
            // bind slot in the resulting `QualifyCondition`.

            /// `<alias> < value` — outer `WHERE` predicate for this
            /// `FLOAT8`-returning window annotation.
            /// # Example
            /// ```ignore
            /// Sale::objects()
            /// .annotate(|f| PercentRankWindow::new()
            ///  .order_by(f.amount().desc())
            ///  .alias("pct"))
            /// .qualify(|w| w.lt(0.5))
            /// .fetch_all(&mut ctx).await?;
            /// ```
            #[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
            pub fn lt(&self, value: f64) -> QualifyCondition {
                build_qualify_f64(self.alias_name(), QualifyOp::Lt, value)
            }

            /// `<alias> <= value` — see [`Self::lt`] for the lowering shape.
            #[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
            pub fn lte(&self, value: f64) -> QualifyCondition {
                build_qualify_f64(self.alias_name(), QualifyOp::Lte, value)
            }

            /// `<alias> = value` — exact `FLOAT8` equality against the alias.
            /// # Note on floating-point equality
            /// `FLOAT8 = $1` has standard IEEE 754 precision caveats. This
            /// helper is reliable for the exact boundary values Postgres
            /// guarantees (`0.0` for the first `PERCENT_RANK` row, `1.0`
            /// for the last `CUME_DIST` row), but not for intermediate
            /// fractions. Prefer [`lt`](Self::lt) / [`lte`](Self::lte) /
            /// [`gte`](Self::gte) / [`gt`](Self::gt) for thresholds.
            #[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
            pub fn eq(&self, value: f64) -> QualifyCondition {
                build_qualify_f64(self.alias_name(), QualifyOp::Eq, value)
            }

            /// `<alias> >= value` — see [`Self::lt`] for the lowering shape.
            #[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
            pub fn gte(&self, value: f64) -> QualifyCondition {
                build_qualify_f64(self.alias_name(), QualifyOp::Gte, value)
            }

            /// `<alias> > value` — see [`Self::lt`] for the lowering shape.
            #[must_use = "qualify conditions are only meaningful once handed to.qualify(...)"]
            pub fn gt(&self, value: f64) -> QualifyCondition {
                build_qualify_f64(self.alias_name(), QualifyOp::Gt, value)
            }
        }
    };
}

impl_zero_arg_f64_window!(PercentRankWindow, "PERCENT_RANK");
impl_zero_arg_f64_window!(CumeDistWindow, "CUME_DIST");

/// `NTILE(n) OVER (...)` window-only annotation returning `i32`.
/// Single-integer-arg window function — divides the
/// partition into `n` approximately-equal buckets and returns the
/// bucket number (1..=n) of each row. Useful for quartile (n=4),
/// quintile (n=5), or arbitrary equal-group bucketing.
/// # Example
/// ```ignore
/// // Quartile salary placement per department
/// let rows = Employee::objects()
/// .annotate(|f| NtileWindow::new(4)
///  .partition_by(f.dept_id())
///  .order_by(f.salary().desc())
///  .alias("salary_quartile"))
/// .fetch_all(&mut ctx).await?;
/// ```
/// `n` is bound as a literal in the SQL; values must fit in `i32`.
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone)]
pub struct NtileWindow {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
    pub(crate) buckets: i32,
}

impl NtileWindow {
    /// Construct an empty NTILE window annotation with the given
    /// bucket count `n`. `n` must be positive (Postgres rejects
    /// non-positive values at runtime).
    pub fn new(buckets: i32) -> Self {
        Self {
            window: WindowSpec::default(),
            alias: None,
            buckets,
        }
    }

    /// Add a `PARTITION BY` column. PR3: accepts `FieldRef<M, V>` or
    /// `DjogiField<M, V>` through
    /// [`IntoSqlField`](crate::query::field::IntoSqlField).
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn partition_by<M, V, S>(mut self, field: S) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V>,
    {
        self.window
            .partition_by
            .push(field.into_sql_field().column());
        self
    }

    /// Add an `ORDER BY` term.
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn order_by(mut self, order: OrderExpr) -> Self {
        push_order_expr(&mut self.window, order);
        self
    }

    /// Set the output alias.
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn alias(mut self, alias: &'static str) -> Self {
        crate::ident::assert_user_supplied_ident(alias, "window_alias");
        self.alias = Some(alias);
        self
    }

    pub(crate) fn alias_name(&self) -> Option<&'static str> {
        self.alias
    }

    pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
        acc.push_sql("NTILE(");
        acc.push_bind(self.buckets as i64);
        acc.push_sql(")");
        self.window.emit(acc);
        acc.push_sql(" AS ");
        acc.push_sql(
            self.alias
                .expect("window function annotations are checked before SQL emission"),
        );
    }
}

/// Shared shape for column-argument window functions: `LEAD`, `LAG`,
/// `FIRST_VALUE`, `LAST_VALUE`.
/// Each takes a column reference as the primary argument; `LEAD` /
/// `LAG` additionally take an optional offset (default 1) and an
/// optional default value (returned when the offset row is past the
/// partition boundary).
/// All four return the column's own type at the typed surface;
/// adopters decode into `V`. Today the typed wrapper around these
/// stores the column name as a `&'static str` and the SQL emitter
/// emits `<KEYWORD>(<col>) OVER (...)` (or
/// `<KEYWORD>(<col>, <offset>, <default>) OVER (...)` for LEAD/LAG).
/// # Why a separate macro from the rank family
/// Rank-family windows (`ROW_NUMBER`, `RANK`, `DENSE_RANK`) take no
/// arguments and return `i64`. Column-argument windows take one or
/// more arguments and return the column's own type. Different
/// signatures, different emission shape; separate macros keep each
/// surface tight.
macro_rules! define_column_arg_window_fn {
    ($type_name:ident, $sql_name:literal) => {
        #[must_use = "window functions are lazy annotations - dropping one omits the column"]
        #[derive(Debug, Clone)]
        pub struct $type_name<V> {
            pub(crate) window: WindowSpec,
            pub(crate) alias: Option<&'static str>,
            pub(crate) target_column: &'static str,
            pub(crate) _out: std::marker::PhantomData<fn() -> V>,
        }

        impl<V> $type_name<V> {
            /// Construct a window annotation reading from `target`
            /// the column whose value is reported per row by this
            /// window function. The typed wrapper carries `V` for
            /// row-decode at fetch time. PR3: accepts `FieldRef<M, V>`
            /// or `DjogiField<M, V>` through
            /// [`IntoSqlField`](crate::query::field::IntoSqlField).
            pub fn new<M, S>(target: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                Self {
                    window: WindowSpec::default(),
                    alias: None,
                    target_column: target.into_sql_field().column(),
                    _out: std::marker::PhantomData,
                }
            }

            /// Add a `PARTITION BY` column. PR3: accepts
            /// `FieldRef<M, V>` or `DjogiField<M, V>` through
            /// [`IntoSqlField`](crate::query::field::IntoSqlField).
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn partition_by<M, V2, S>(mut self, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V2>,
            {
                self.window
                    .partition_by
                    .push(field.into_sql_field().column());
                self
            }

            /// Add an `ORDER BY` term.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn order_by(mut self, order: OrderExpr) -> Self {
                push_order_expr(&mut self.window, order);
                self
            }

            /// Set the output alias.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn alias(mut self, alias: &'static str) -> Self {
                crate::ident::assert_user_supplied_ident(alias, "window_alias");
                self.alias = Some(alias);
                self
            }

            pub(crate) fn alias_name(&self) -> Option<&'static str> {
                self.alias
            }

            pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
                acc.push_sql($sql_name);
                acc.push_sql("(");
                acc.push_sql(self.target_column);
                acc.push_sql(")");
                self.window.emit(acc);
                acc.push_sql(" AS ");
                acc.push_sql(
                    self.alias
                        .expect("window function annotations are checked before SQL emission"),
                );
            }
        }
    };
}

define_column_arg_window_fn!(FirstValueWindow, "FIRST_VALUE");
define_column_arg_window_fn!(LastValueWindow, "LAST_VALUE");

/// `LEAD(col [, offset [, default]]) OVER (...)` window function.
/// Returns the value of `col` from the row `offset` rows AFTER the
/// current row in the partition (default offset is 1). When the
/// computed row is past the partition's tail, returns `default` if
/// supplied, else SQL NULL.
/// # Example
/// ```ignore
/// // Compare each event to the next event in the same session
/// let rows = Event::objects()
/// .annotate(|f| LeadWindow::new(f.timestamp())
///  .partition_by(f.session_id())
///  .order_by(f.timestamp().asc())
///  .alias("next_ts"))
/// .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone)]
pub struct LeadWindow<V> {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
    pub(crate) target_column: &'static str,
    pub(crate) offset: Option<i64>,
    pub(crate) _out: std::marker::PhantomData<fn() -> V>,
}

/// `LAG(col [, offset [, default]]) OVER (...)` window function.
/// Symmetric to [`LeadWindow`] — returns the value `offset` rows
/// BEFORE the current row in the partition (default offset 1).
/// Useful for "compare to previous" delta computations.
/// # Example
/// ```ignore
/// // Compute revenue delta day-over-day
/// let rows = Day::objects()
/// .annotate(|f| LagWindow::new(f.revenue())
///  .order_by(f.date().asc())
///  .alias("prev_revenue"))
/// .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone)]
pub struct LagWindow<V> {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
    pub(crate) target_column: &'static str,
    pub(crate) offset: Option<i64>,
    pub(crate) _out: std::marker::PhantomData<fn() -> V>,
}

macro_rules! impl_lead_lag {
    ($type_name:ident, $sql_name:literal) => {
        impl<V> $type_name<V> {
            /// Construct with the target column. Default offset is 1
            /// (next/previous row); chain `.offset(n)` to override.
            /// PR3: accepts `FieldRef<M, V>` or `DjogiField<M, V>`
            /// through
            /// [`IntoSqlField`](crate::query::field::IntoSqlField).
            pub fn new<M, S>(target: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V>,
            {
                Self {
                    window: WindowSpec::default(),
                    alias: None,
                    target_column: target.into_sql_field().column(),
                    offset: None,
                    _out: std::marker::PhantomData,
                }
            }

            /// Override the row offset. Postgres default is 1; pass
            /// any positive integer (LEAD looks N rows ahead, LAG
            /// looks N rows back). Negative offsets reverse the
            /// direction (Postgres permits this); stick to positive
            /// integers for clarity.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn offset(mut self, n: i64) -> Self {
                self.offset = Some(n);
                self
            }

            /// Add a `PARTITION BY` column. PR3: accepts
            /// `FieldRef<M, V>` or `DjogiField<M, V>` through
            /// [`IntoSqlField`](crate::query::field::IntoSqlField).
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn partition_by<M, V2, S>(mut self, field: S) -> Self
            where
                M: Model,
                S: crate::query::field::IntoSqlField<M, V2>,
            {
                self.window
                    .partition_by
                    .push(field.into_sql_field().column());
                self
            }

            /// Add an `ORDER BY` term. Lead/Lag are deterministic
            /// only when the partition has an explicit order.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn order_by(mut self, order: OrderExpr) -> Self {
                push_order_expr(&mut self.window, order);
                self
            }

            /// Set the output alias.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn alias(mut self, alias: &'static str) -> Self {
                crate::ident::assert_user_supplied_ident(alias, "window_alias");
                self.alias = Some(alias);
                self
            }

            pub(crate) fn alias_name(&self) -> Option<&'static str> {
                self.alias
            }

            pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
                acc.push_sql($sql_name);
                acc.push_sql("(");
                acc.push_sql(self.target_column);
                if let Some(off) = self.offset {
                    acc.push_sql(", ");
                    acc.push_bind(off);
                }
                acc.push_sql(")");
                self.window.emit(acc);
                acc.push_sql(" AS ");
                acc.push_sql(
                    self.alias
                        .expect("window function annotations are checked before SQL emission"),
                );
            }
        }
    };
}

impl_lead_lag!(LeadWindow, "LEAD");
impl_lead_lag!(LagWindow, "LAG");

/// `NTH_VALUE(col, n) OVER (...)` window function.
/// Returns the value of `col` from the `n`-th row of the window frame
/// (1-indexed). When the frame has fewer than `n` rows, returns SQL
/// NULL. Useful for "third-best per group" patterns.
/// # Example
/// ```ignore
/// // Score of the third-highest entry per category
/// let rows = Entry::objects()
/// .annotate(|f| NthValueWindow::new(f.score(), 3)
///  .partition_by(f.category_id())
///  .order_by(f.score().desc())
///  .alias("third_best"))
/// .fetch_all(&mut ctx).await?;
/// ```
#[must_use = "window functions are lazy annotations - dropping one omits the column"]
#[derive(Debug, Clone)]
pub struct NthValueWindow<V> {
    pub(crate) window: WindowSpec,
    pub(crate) alias: Option<&'static str>,
    pub(crate) target_column: &'static str,
    pub(crate) n: i64,
    pub(crate) _out: std::marker::PhantomData<fn() -> V>,
}

impl<V> NthValueWindow<V> {
    /// Construct with the target column and the 1-indexed position
    /// `n`. Postgres rejects `n <= 0` at runtime. PR3: accepts
    /// `FieldRef<M, V>` or `DjogiField<M, V>` through
    /// [`IntoSqlField`](crate::query::field::IntoSqlField).
    pub fn new<M, S>(target: S, n: i64) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V>,
    {
        Self {
            window: WindowSpec::default(),
            alias: None,
            target_column: target.into_sql_field().column(),
            n,
            _out: std::marker::PhantomData,
        }
    }

    /// Add a `PARTITION BY` column. PR3: accepts `FieldRef<M, V>` or
    /// `DjogiField<M, V>` through
    /// [`IntoSqlField`](crate::query::field::IntoSqlField).
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn partition_by<M, V2, S>(mut self, field: S) -> Self
    where
        M: Model,
        S: crate::query::field::IntoSqlField<M, V2>,
    {
        self.window
            .partition_by
            .push(field.into_sql_field().column());
        self
    }

    /// Add an `ORDER BY` term.
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn order_by(mut self, order: OrderExpr) -> Self {
        push_order_expr(&mut self.window, order);
        self
    }

    /// Set the output alias.
    #[must_use = "window functions are immutable builders - use the returned value"]
    pub fn alias(mut self, alias: &'static str) -> Self {
        crate::ident::assert_user_supplied_ident(alias, "window_alias");
        self.alias = Some(alias);
        self
    }

    pub(crate) fn alias_name(&self) -> Option<&'static str> {
        self.alias
    }

    pub(crate) fn push_annotated_column(&self, acc: &mut SqlAccumulator) {
        acc.push_sql("NTH_VALUE(");
        acc.push_sql(self.target_column);
        acc.push_sql(", ");
        acc.push_bind(self.n);
        acc.push_sql(")");
        self.window.emit(acc);
        acc.push_sql(" AS ");
        acc.push_sql(
            self.alias
                .expect("window function annotations are checked before SQL emission"),
        );
    }
}

fn push_order_expr(window: &mut WindowSpec, order: OrderExpr) {
    match order {
        OrderExpr::Column {
            column, direction, ..
        } => {
            window.order_by.push((column, direction));
        }
        #[cfg(feature = "spatial")]
        OrderExpr::SpatialDistance { .. } => {
            panic!("window ranking order_by accepts field asc()/desc() orderings only")
        }
    }
}
