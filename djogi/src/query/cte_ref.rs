//! `CteRef` — a runtime-typed reference to a column projected by a CTE
//! term, for use in `CteQuerySet::filter_expr` cross-CTE predicates.
//! # What
//! [`CteRef`] names a `(cte_alias, column)` pair and promotes to a typed
//! [`Expr<V>`](crate::expr::Expr) via [`CteRef::as_expr`]. The `V` is
//! **caller-asserted** — `CteRef` cannot check at compile time that the named
//! column exists on the CTE term or that its type is `V`, because a CTE term
//! stores only a name + body recipe (no typed column bag). The runtime guard
//! is identifier validation: both strings are checked at construction.
//! # Why runtime-typed (not a generated accessor)
//! A compile-time-checked `{Model}CteRef::column()` accessor would require
//! the `#[derive(Model)]` macro to emit a per-CTE typed bag. That is a
//! larger surface deferred to a follow-up; this minimal form unblocks
//! cross-CTE filtering today.
//! # Limitation
//! The named CTE alias must be in the consumer query's `FROM` clause —
//! Postgres requires a `WITH` term to appear in `FROM` before its columns
//! are referenceable (a merely-declared term raises `42P01`). When the
//! `CteRef` names the `from_cte` target itself, it is already in `FROM`.
//! When it names a *sibling* term, bring that term into the consumer
//! `FROM` with [`crate::query::CteQuerySet::also_from_cte`] — which emits
//! `FROM <from_cte target>, <sibling>` (an implicit cross join). Adding a
//! multi-row sibling produces a cartesian product, so prefer single-row
//! anchor terms. The general `FROM <model> m, <cte> a` shape (a base model
//! table cross-joined with a CTE in the consumer `FROM`) is not produced
//! by this surface — `also_from_cte` adds only declared CTE terms.

use crate::DjogiError;
use crate::expr::Expr;
use crate::expr::node::ExprNode;
use crate::ident::check_user_supplied_ident;

/// A validated `(cte_alias, column)` reference. Build with
/// [`CteRef::new`]; promote with [`CteRef::as_expr`].
#[derive(Debug, Clone, Copy)]
#[must_use = "a CteRef is inert until promoted with .as_expr()"]
pub struct CteRef {
    alias: &'static str,
    column: &'static str,
}

impl CteRef {
    /// Build a CTE column reference from a CTE term name and a column
    /// name. Both are validated as user-supplied identifiers (non-empty,
    /// ≤ 63 bytes, ASCII letter/underscore first, no `__djogi_` prefix,
    /// not a reserved keyword).
    /// # Errors
    /// [`DjogiError::Validation`] if either string fails identifier
    /// validation.
    /// # Example
    /// ```ignore
    /// let anchor_geo = CteRef::new("anchor", "geo")?.as_expr::<GeoPoint>();
    /// ```
    pub fn new(alias: &'static str, column: &'static str) -> Result<Self, DjogiError> {
        check_user_supplied_ident(alias, true)
            .map_err(|e| DjogiError::Validation(format!("invalid CTE alias {alias:?}: {e:?}")))?;
        check_user_supplied_ident(column, true)
            .map_err(|e| DjogiError::Validation(format!("invalid CTE column {column:?}: {e:?}")))?;
        Ok(Self { alias, column })
    }

    /// Promote to a typed [`Expr<V>`](crate::expr::Expr). `V` is
    /// caller-asserted — the type must match the CTE column's Postgres type,
    /// or the query fails at fetch time with a Postgres / decode error, not a
    /// Rust compile error. See the module limitation note.
    pub fn as_expr<V>(self) -> Expr<V> {
        Expr::from_node(ExprNode::CteAliasField {
            alias: self.alias,
            column: self.column,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CteRef;
    use crate::expr::sql::emit_expr;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::portable::SqlEmitContext;

    #[test]
    fn cte_ref_as_expr_emits_alias_dot_column() {
        let r = CteRef::new("anchor", "geo").expect("valid idents");
        let e = r.as_expr::<i64>();
        let mut qb = SqlAccumulator::new("");
        emit_expr(&mut qb, &e.node, SqlEmitContext::root()).expect("cte ref emission");
        assert_eq!(qb.sql().trim(), "anchor.geo", "got: {}", qb.sql());
    }

    #[test]
    fn cte_ref_rejects_invalid_alias() {
        let r = CteRef::new("1bad", "geo");
        assert!(r.is_err(), "leading-digit alias must be rejected");
    }

    #[test]
    fn cte_ref_rejects_djogi_prefixed_column() {
        let r = CteRef::new("anchor", "__djogi_internal");
        assert!(r.is_err(), "__djogi_-prefixed column must be rejected");
    }
}
