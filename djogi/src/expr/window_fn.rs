//! Window-only ranking functions for annotated querysets.
//!
//! `ROW_NUMBER`, `RANK`, and `DENSE_RANK` are window functions, not
//! aggregates. Djogi models them separately from [`super::AggregateExpr`] so
//! they cannot be used in aggregate-only terminals and so the generated SQL
//! always includes an `OVER (...)` clause.

use crate::model::Model;
use crate::pg::accumulator::SqlAccumulator;
use crate::query::field::FieldRef;
use crate::query::order::OrderExpr;

use super::window::WindowSpec;

/// One-shot comparison against an annotated window-function alias, lowered
/// to an outer-`WHERE` predicate over a derived table by
/// [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify).
///
/// PostgreSQL 18 has no `QUALIFY` clause, so a window-output filter has to
/// be applied in an outer scope where the alias is in scope as a column
/// reference. `QualifyCondition` captures that filter without ever
/// emitting the literal `QUALIFY` token.
#[derive(Debug, Clone)]
#[must_use = "qualify conditions are only meaningful once handed to .qualify(...)"]
pub struct QualifyCondition {
    pub(crate) alias: &'static str,
    pub(crate) op: QualifyOp,
    pub(crate) value: i64,
}

/// Comparison operator family supported on window-output aliases.
///
/// Constrained to the typed `i64` ranking outputs — `RowNumber`, `Rank`,
/// and `DenseRank` all return `BIGINT`, so a single integer comparator
/// set covers every v0.1.0 use case.
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
        acc.push_bind(self.value);
    }
}

fn build_qualify(alias: Option<&'static str>, op: QualifyOp, value: i64) -> QualifyCondition {
    QualifyCondition {
        alias: alias.expect(
            "qualify can only reference a window annotation that was registered with .alias(\"…\")",
        ),
        op,
        value,
    }
}

/// Sealing module — `Sealed` is private so adopters cannot implement
/// [`WindowRanking`] for their own types.
mod sealed_ranking {
    pub trait Sealed {}
}

/// Comparison helpers shared by every typed window-only ranking function.
///
/// `RowNumber`, `Rank`, and `DenseRank` all return `BIGINT`, so the typed
/// `lt` / `lte` / `eq` / `gte` / `gt` lowering helpers are identical across
/// the three types — only the underlying SQL keyword differs. This trait
/// captures that shared surface as default methods so the per-type macro
/// expansion is a 3-line `impl WindowRanking` rather than 5 × 3 = 15
/// duplicate method bodies.
///
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
            "    .annotate(|e| ", stringify!($type_name), "::new()\n",
            "        .partition_by(e.herd_id())\n",
            "        .order_by(e.score().desc())\n",
            "        .alias(\"", $example_name, "\"))\n",
            "    .qualify(|w| w.lte(3))\n",
            "    .fetch_all(&mut ctx)\n",
            "    .await?;\n",
            "# Ok::<_, djogi::DjogiError>(())\n",
            "```\n\n",
            "The SQL shape is a derived table rather than `QUALIFY`:\n\n",
            "```sql\n",
            "SELECT * FROM (\n",
            "    SELECT t.id, ",
            $sql_name,
            "() OVER (PARTITION BY herd_id ORDER BY score DESC) AS ",
            $example_name,
            "\n",
            "    FROM elephants AS t\n",
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
                "    .order_by(fields.score().desc())\n",
                "    .alias(\"", $example_name, "\");\n",
                "```"
            )]
            pub fn new() -> Self {
                Self {
                    window: WindowSpec::default(),
                    alias: None,
                }
            }

            /// Add a `PARTITION BY` column to this window function.
            ///
            /// # What
            ///
            /// The column comes from a typed [`FieldRef`], so it has already
            /// passed Djogi's identifier validation path.
            ///
            /// # Why
            ///
            /// Partitioning restarts the row numbering or ranking per group,
            /// which is the common "top N per parent" shape.
            ///
            /// # How
            ///
            /// Call this once per partition key before `.alias(...)`.
            ///
            /// ```ignore
            /// RowNumber::new()
            ///     .partition_by(fields.herd_id())
            ///     .order_by(fields.score().desc())
            ///     .alias("rank");
            /// ```
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn partition_by<M, V>(mut self, field: FieldRef<M, V>) -> Self
            where
                M: Model,
            {
                self.window.partition_by.push(field.column());
                self
            }

            /// Add an `ORDER BY` term to this window function.
            ///
            /// # What
            ///
            /// Accepts the [`OrderExpr`] produced by `FieldRef::asc()` or
            /// `FieldRef::desc()` and stores its column and direction in the
            /// window spec.
            ///
            /// # Why
            ///
            /// Ranking functions are only deterministic when the partition has
            /// an explicit order. Djogi keeps the order typed by reusing the
            /// same `OrderExpr` surface as `QuerySet::order_by`.
            ///
            /// # How
            ///
            /// Pass `field.asc()` or `field.desc()`:
            ///
            /// ```ignore
            /// Rank::new()
            ///     .partition_by(fields.herd_id())
            ///     .order_by(fields.score().desc())
            ///     .alias("rank");
            /// ```
            ///
            /// # Panics
            ///
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
            ///
            /// # What
            ///
            /// The alias is emitted as `AS <alias>` in the inner annotated
            /// select and becomes the column referenced by
            /// [`AnnotatedQuerySet::qualify`](crate::query::AnnotatedQuerySet::qualify).
            ///
            /// # Why
            ///
            /// PostgreSQL 18 has no `QUALIFY` clause, so Djogi lowers
            /// `.qualify(...)` into `SELECT * FROM (<annotated select>) AS
            /// __djogi_q WHERE <alias predicate>`. A stable alias is required
            /// for that outer predicate and for row decoding.
            ///
            /// # How
            ///
            /// Pass a plain unquoted PostgreSQL identifier:
            ///
            /// ```ignore
            /// DenseRank::new()
            ///     .order_by(fields.score().desc())
            ///     .alias("dense_rank");
            /// ```
            ///
            /// # Panics
            ///
            /// Panics when `alias` is empty, longer than PostgreSQL's usable
            /// identifier length, starts with an invalid byte, contains an
            /// invalid byte, or is a reserved PostgreSQL keyword. Also panics
            /// when the alias starts with the `__djogi_` framework-reserved
            /// prefix (e.g. `__djogi_q` would shadow the derived-table name
            /// used by qualify lowering; `__djogi_agg_N` would collide with
            /// the aggregate-tuple slot aliases used by row decode).
            ///
            /// User-chosen aliases SHOULD also avoid colliding with the
            /// model's own column names — the outer `WHERE <alias>` would
            /// then reference the underlying column instead of the window
            /// output. The framework cannot enforce this here because
            /// [`alias`](Self::alias) does not know the eventual `T`; it is
            /// the caller's responsibility to pick a non-colliding alias.
            #[must_use = "window functions are immutable builders - use the returned value"]
            pub fn alias(mut self, alias: &'static str) -> Self {
                crate::ident::assert_plain_ident(alias, "window_alias");
                assert!(
                    !alias.starts_with("__djogi_"),
                    "window alias \"{alias}\" is reserved (the `__djogi_` prefix is used for framework-internal identifiers like the qualify derived-table alias `__djogi_q` and the aggregate-tuple slot aliases `__djogi_agg_N`)"
                );
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
