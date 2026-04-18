//! Ordering expressions — accumulated by `QuerySet::order_by`.
//!
//! # What
//!
//! An [`OrderExpr`] is the tiniest possible description of a single `ORDER BY`
//! clause element: a column name (`&'static str`, always a literal baked in by
//! the `#[model]` macro), a [`Direction`] (`ASC` / `DESC`), and an optional
//! [`NullsOrder`] position for explicit `NULLS FIRST` / `NULLS LAST`.
//!
//! # Why
//!
//! Ordering is emitted lazily along with the `WHERE`/`LIMIT`/`OFFSET` at
//! terminal-method time (Task 6). Keeping the representation a plain POD —
//! no SQL strings, no boxed allocations — means `QuerySet::clone()` stays
//! O(n) in the number of order columns, not in the length of an accumulated
//! SQL buffer. It also keeps the emitter's injection-safety review surface
//! confined to one file (`query::sql`).
//!
//! `FieldRef<M, V>` gains `.asc()` / `.desc()` inherent methods so callers
//! write `f.title.asc()` / `f.view_count.desc()` — the same typed-handle
//! ergonomics as `.eq(...)` / `.gte(...)`. `.nulls_first()` / `.nulls_last()`
//! modify an `OrderExpr` fluently after construction.
//!
//! # Where
//!
//! - Accumulated by [`crate::query::queryset::QuerySet::order_by`].
//! - Emitted to SQL by Task 6's `query::sql` module.

use crate::model::Model;
use crate::query::field::FieldRef;

/// Sort direction for a single `ORDER BY` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Direction {
    /// `ASC` — smallest first.
    Asc,
    /// `DESC` — largest first.
    Desc,
}

/// NULL positioning for an ordering expression.
///
/// `Default` lets the Postgres default win (NULLS LAST for ASC, NULLS FIRST
/// for DESC); pick `First` / `Last` explicitly for deterministic ordering
/// across dialects or documented row ordering contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NullsOrder {
    /// Postgres default — no `NULLS FIRST|LAST` clause emitted.
    Default,
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// A single `ORDER BY` element. Emitted verbatim by the SQL builder — the
/// column name is always a `&'static str` literal produced by the `#[model]`
/// macro, so it is never user-controlled input.
#[derive(Debug, Clone, Copy)]
pub struct OrderExpr {
    /// Column name (macro-baked literal — never user input).
    pub column: &'static str,
    /// Sort direction.
    pub direction: Direction,
    /// NULL positioning.
    pub nulls: NullsOrder,
}

impl OrderExpr {
    /// Force `NULLS FIRST` on this expression. Consumes and returns `Self`
    /// for fluent chaining: `f.title.asc().nulls_first()`.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn nulls_first(mut self) -> Self {
        self.nulls = NullsOrder::First;
        self
    }

    /// Force `NULLS LAST` on this expression. Consumes and returns `Self`
    /// for fluent chaining: `f.title.desc().nulls_last()`.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn nulls_last(mut self) -> Self {
        self.nulls = NullsOrder::Last;
        self
    }
}

impl<M: Model, V> FieldRef<M, V> {
    /// Ascending ordering for this column. `NULLS` position is left at the
    /// Postgres default (NULLS LAST for ASC); call `.nulls_first()` /
    /// `.nulls_last()` on the result to override.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn asc(self) -> OrderExpr {
        OrderExpr {
            column: self.column(),
            direction: Direction::Asc,
            nulls: NullsOrder::Default,
        }
    }

    /// Descending ordering for this column. `NULLS` position defaults to the
    /// Postgres default (NULLS FIRST for DESC); call `.nulls_last()` to
    /// override.
    #[must_use = "order expressions are inert until passed to `order_by`"]
    pub fn desc(self) -> OrderExpr {
        OrderExpr {
            column: self.column(),
            direction: Direction::Desc,
            nulls: NullsOrder::Default,
        }
    }
}

/// Accept a single `OrderExpr` where a `Vec<OrderExpr>` is expected —
/// `QuerySet::order_by(|f| f.title.asc())` closes over a single expression
/// and this `From` impl lifts it into the one-element vec the builder stores.
impl From<OrderExpr> for Vec<OrderExpr> {
    fn from(o: OrderExpr) -> Self {
        vec![o]
    }
}
