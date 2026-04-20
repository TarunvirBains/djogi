//! Typed expression IR — the substrate for field-vs-field filters,
//! arithmetic assignments, aggregates, and subqueries.
//!
//! # What
//!
//! [`Expr<T>`] is a `PhantomData<fn() -> T>`-tagged wrapper around an
//! untyped [`node::ExprNode`] tree. Typed constructors (`Expr::literal`,
//! [`crate::query::field::FieldRef::as_expr`]) promote primitives and
//! columns into `Expr<T>`, and typed methods on `Expr<T>` compose them:
//!
//! - [`Expr::eq`] / [`Expr::neq`] / [`Expr::gt`] / [`Expr::gte`] /
//!   [`Expr::lt`] / [`Expr::lte`] — comparison, returning `Expr<bool>`.
//! - `impl Add/Sub/Mul/Div for Expr<T> where T: Numeric` — arithmetic
//!   in [`arithmetic`], gated on a sealed [`arithmetic::Numeric`] trait
//!   so only framework-blessed numeric types compose. The blessed set
//!   is `i16 / i32 / i64 / f32 / f64` for Phase 4; `Decimal` / `Interval`
//!   extend the trait later.
//!
//! The `Expr<bool>` produced by a comparison slots into the filter tree
//! via the [`crate::query::condition::Condition::Expr`] variant; the
//! [`crate::query::QuerySet::filter_expr`] entry point wires the closure
//! form to that bridge.
//!
//! # Why this shape
//!
//! Two constraints drove the design:
//!
//! 1. **Typed composition, untyped walk.** Users should not be able to
//!    build `Expr<String> + Expr<i32>` (nonsense addition) or compare
//!    `Expr<i64>.eq(Expr<String>)` (type-mismatched equality). The
//!    phantom `T` parameter on [`Expr<T>`] enforces those rules at
//!    compile time. But the SQL emitter doesn't care about `T` — it
//!    only needs to walk the enum and push bind parameters. The
//!    internal [`node::ExprNode`] is therefore untyped (no `T`
//!    parameter), and the emitter stays a single monomorphic function.
//!
//! 2. **Phase additivity.** Tasks 4 / 5 add `Case`, `Exists`, `Subquery`,
//!    `Aggregate`, and `OuterRef` variants. By storing the dynamic
//!    payload in `ExprNode`, those additions are one new variant + one
//!    new emitter arm + one new typed constructor per variant — no
//!    ripple through type-parameterised code.
//!
//! # Where
//!
//! - [`node::ExprNode`] / [`node::CmpOp`] — the untyped payload enum.
//! - [`literal`] — `impl From<T> for Expr<T>` for every bindable scalar.
//! - [`compare`] — comparison methods on `Expr<T>`.
//! - [`arithmetic`] — sealed `Numeric` + operator overloads.
//! - [`sql::emit_expr`] — the SQL emitter, matched exhaustively on
//!   `ExprNode` variants.
//! - [`crate::query::condition::Condition::Expr`] — filter-tree bridge.
//! - [`crate::query::QuerySet::filter_expr`] — closure entry point.
//!
//! # Example
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! // Field-vs-field comparison — not expressible with the Phase 2
//! // `filter(|f| f.col.eq(value))` API because the RHS is a literal
//! // there. `filter_expr` closes the gap.
//! let overdrawn = Account::objects()
//!     .filter_expr(|f| f.balance().as_expr().lt(f.overdraft_limit().as_expr()))
//!     .fetch_all(&mut ctx).await?;
//! ```

use crate::query::condition::FilterValue;
use std::marker::PhantomData;

pub mod aggregate;
pub(crate) mod arithmetic;
pub mod case;
pub(crate) mod compare;
pub(crate) mod literal;
pub(crate) mod node;
pub(crate) mod sql;
pub mod subquery;

pub use aggregate::AggregateExpr;
pub use case::{Case, CaseBuilder};
use node::ExprNode;
pub use subquery::{Exists, OuterRef, Subquery};

/// Typed expression handle — the public entry point for the IR.
///
/// Carries a `PhantomData<fn() -> T>` tag so the type parameter is
/// covariant and the struct is `Send + Sync` regardless of `T`'s own
/// markers. `T` never appears as an owned field — it only tags the
/// Rust-level type the underlying SQL expression evaluates to, which
/// lets comparisons and arithmetic enforce type discipline at compile
/// time while the emitter walks a type-erased enum.
///
/// Always returned by-value from composition methods; `#[must_use]`
/// because a dropped expression is usually a mistake (the user likely
/// meant to hand it to `filter_expr` / `set_expr` / similar).
///
/// `Clone` + `Debug` — the underlying [`ExprNode`] is also `Clone +
/// Debug`, so copies and diagnostics are cheap. `Expr` is **not**
/// `Copy`: expression trees contain boxed sub-nodes, and making the
/// whole struct `Copy` would hide the allocation cost of cloning a
/// deep tree.
#[must_use = "expressions are lazy — dropping one silently omits the predicate"]
#[derive(Clone, Debug)]
pub struct Expr<T> {
    pub(crate) node: ExprNode,
    pub(crate) _phantom: PhantomData<fn() -> T>,
}

impl<T> Expr<T>
where
    T: Into<Expr<T>>,
{
    /// Construct an `Expr<T>` from a Rust value.
    ///
    /// Every SQL-bindable scalar Djogi ships with has an `impl From<V>
    /// for Expr<V>` in [`literal`], which in turn means
    /// `V: Into<Expr<V>>`. This wrapper lets call sites read
    /// `Expr::literal(100i32)` instead of `Expr::<i32>::from(100i32)`
    /// — the inference direction matches the plan's pseudo-code and
    /// reads naturally alongside `field.as_expr()`.
    ///
    /// The `T: Into<Expr<T>>` bound on the impl block (rather than on
    /// the method's own generic parameter) means Rust infers `T`
    /// directly from the argument type at the call site. Turbofish
    /// (`Expr::literal::<i32>(100)`) is never needed in practice.
    pub fn literal(v: T) -> Expr<T> {
        v.into()
    }
}

impl<T> Expr<T> {
    /// Package an already-constructed `ExprNode` into `Expr<T>`. Crate-
    /// private so downstream code cannot fabricate an arbitrarily-typed
    /// expression by bypassing the typed constructors (`literal`,
    /// `FieldRef::as_expr`, operator overloads).
    ///
    /// Used internally by [`compare`] and [`arithmetic`] to wrap the
    /// newly-built node without repeating the `PhantomData` boilerplate
    /// at every call site.
    pub(crate) fn from_node(node: ExprNode) -> Self {
        Expr {
            node,
            _phantom: PhantomData,
        }
    }

    /// Build an `Expr<T>` directly from a [`FilterValue`]. Crate-private
    /// convenience for the `impl From<V> for Expr<V>` bridges in
    /// [`literal`] — every scalar impl calls this after mapping itself
    /// into the right `FilterValue` variant, so the literal constructors
    /// all route through the same typed seal.
    ///
    /// `T` is whatever the enclosing `impl<T> Expr<T>` block specialises
    /// to at the call site — the `From` impls in [`literal`] call this
    /// through `Expr::<Self>::from_literal(..)` after `Self == Expr<V>`
    /// nails `T` to the scalar type.
    pub(crate) fn from_literal(v: FilterValue) -> Self {
        Expr {
            node: ExprNode::Literal(v),
            _phantom: PhantomData,
        }
    }
}
