//! FTS query builder types — `FtsFieldRef` with `.matches` and `.rank`.
//! # What
//! `FtsFieldRef` is the typed handle returned by a macro-generated
//! `{Model}Fields::search` accessor on models that declare
//! `#[model(fts = { ... })]`. It carries the tsvector column name and the
//! Postgres text-search dictionary name baked in at codegen time, so call
//! sites only supply the user-facing `TsQuery` value.
//! Two operations are available:
//! - `.matches(q)` — builds a `Condition` leaf that emits
//! `<col> @@ to_tsquery('<dict>', $n)` in the WHERE clause.
//! - `.rank(q)` — builds an `Expr<f32>` that emits
//! `ts_rank(<col>, to_tsquery('<dict>', $n))` in ORDER BY / SELECT.
//! - `.rank_cd(q)` — same but uses `ts_rank_cd` (cover-density).
//! # Path routing
//! All emitted type paths go through `::djogi::*`. Macro output that
//! reaches this type does so via `::djogi::fts_query::FtsFieldRef`.
//! User code can import it via `djogi::prelude::*` (re-exported there).
//! # Usage example (user-facing)
//! ```ignore
//! use djogi::prelude::*;
//!
//! #[model(app = "library", fts = { source = "title, body", dictionary = "english" })]
//! pub struct Book {
//!     pub title: String,
//!     pub body: String,
//! }
//!
//! let hits = Book::objects()
//!     .filter(|b| b.search().matches(TsQuery::new("planet & earth")))
//!     .order_by(|b| b.search().rank(TsQuery::new("planet & earth")).desc())
//!     .fetch_all(&mut ctx).await?;
//! ```
//! # Note on dictionary embedding
//! The dictionary name in the emitted SQL is a literal (single-quoted),
//! not a bind parameter, because it is a Postgres configuration name
//! validated at macro parse time. It originates from
//! `#[model(fts = { dictionary = "english" })]` and passes through
//! byte-level identifier validation before landing in the `&'static str`
//! baked into the generated accessor. It is never user-controlled at runtime.

use crate::expr::Expr;
use crate::expr::node::ExprNode;
use crate::fts::TsQuery;
use crate::model::Model;
use crate::query::condition::Condition;
use std::marker::PhantomData;

/// Typed handle for a model's FTS column.
/// Returned by `{Model}Fields::search` on models that declare
/// `#[model(fts = { source = "...", dictionary = "..." })]`. Carry the
/// validated tsvector column name and dictionary name as `&'static str`s
/// baked in by the macro.
/// `Copy` — the struct is three static string pointers plus a phantom
/// marker; cheap to pass by value.
pub struct FtsFieldRef<M: Model> {
    /// The generated tsvector column name — typically `"search"`.
    pub(crate) column: &'static str,
    /// Postgres text-search config name — e.g. `"english"`.
    /// Validated at macro parse time; embedded literally into SQL.
    pub(crate) dictionary: &'static str,
    pub(crate) _m: PhantomData<fn() -> M>,
}

impl<M: Model> Copy for FtsFieldRef<M> {}
impl<M: Model> Clone for FtsFieldRef<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M: Model> std::fmt::Debug for FtsFieldRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FtsFieldRef({}, {})", self.column, self.dictionary)
    }
}

impl<M: Model> FtsFieldRef<M> {
    /// Construct an `FtsFieldRef`. This is the crate-private constructor
    /// called by macro-generated code; user code uses `{Model}Fields::search`.
    /// The column and dictionary strings must have been validated at macro
    /// parse time via byte-level identifier checks (see
    /// `djogi_macros::model::attrs::FtsSpec::parse_from_list`). They are
    /// `&'static str`s baked in the generated accessor — never user-controlled
    /// runtime strings.
    #[doc(hidden)]
    pub fn __new(column: &'static str, dictionary: &'static str) -> Self {
        FtsFieldRef {
            column,
            dictionary,
            _m: PhantomData,
        }
    }

    /// Build a `Condition` leaf that emits
    /// `<column> @@ to_tsquery('<dictionary>', $n)`.
    /// The query text is bound as a parameter; only the dictionary name
    /// is embedded literally (it was validated at macro parse time).
    pub fn matches(self, query: TsQuery) -> Condition {
        Condition::Expr(Expr::from_node(ExprNode::TsMatch {
            column: self.column,
            dictionary: self.dictionary,
            query_text: query.0,
        }))
    }

    /// Build an `Expr<f32>` that emits
    /// `ts_rank(<column>, to_tsquery('<dictionary>', $n))`.
    /// Use it in `.order_by(|b| b.search.rank(q).desc)` to sort by
    /// relevance. Higher scores mean higher relevance.
    /// Returns `Expr<f32>` — Postgres's `ts_rank` function returns a
    /// `float4` (which maps to Rust `f32`).
    pub fn rank(self, query: TsQuery) -> Expr<f32> {
        Expr::from_node(ExprNode::TsRank {
            column: self.column,
            dictionary: self.dictionary,
            query_text: query.0,
        })
    }

    /// Build an `Expr<f32>` that emits
    /// `ts_rank_cd(<column>, to_tsquery('<dictionary>', $n))`.
    /// Uses the cover-density ranking algorithm, which weights proximity of
    /// matching terms more heavily than `ts_rank`. Useful when positional
    /// adjacency of search terms is relevant to relevance judgements.
    pub fn rank_cd(self, query: TsQuery) -> Expr<f32> {
        Expr::from_node(ExprNode::TsRankCd {
            column: self.column,
            dictionary: self.dictionary,
            query_text: query.0,
        })
    }
}

/// Macro-support module — re-exports the `FtsFieldRef` constructor under a
/// `#[doc(hidden)]` path that macro-emitted code uses.
/// Macro output calls `::djogi::fts_query::__macro_support::__make_fts_ref::<M>(col, dict)`;
/// user code never needs this path. The indirection keeps the internal
/// constructor (`__new`) visually separate from user-facing API while still
/// being reachable from the emitted code.
#[doc(hidden)]
pub mod __macro_support {
    use super::FtsFieldRef;
    use crate::ident::assert_plain_ident;
    use crate::model::Model;

    /// Construct an `FtsFieldRef` for `column` on model `M`.
    /// Called only from macro-emitted code. `column` must be a plain ASCII
    /// identifier; `dictionary` was validated at macro parse time. The
    /// `assert_plain_ident` call is a crate-internal runtime sanity check
    /// that fires if the macro somehow emits a malformed column name.
    #[doc(hidden)]
    #[inline]
    pub fn __make_fts_ref<M: Model>(
        column: &'static str,
        dictionary: &'static str,
    ) -> FtsFieldRef<M> {
        assert_plain_ident(column, "fts column");
        FtsFieldRef::__new(column, dictionary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    // Minimal test-only model stub.
    struct FakeBook;
    impl crate::model::__sealed::Sealed for FakeBook {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for FakeBook {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "book"
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

    #[test]
    fn fts_field_ref_matches_emits_at_operator() {
        let fref = FtsFieldRef::<FakeBook>::__new("search", "english");
        let cond = fref.matches(TsQuery::new("planet & earth"));
        // The condition must be a Condition::Expr wrapping a TsMatch node.
        match cond {
            Condition::Expr(expr) => match &expr.node {
                ExprNode::TsMatch {
                    column,
                    dictionary,
                    query_text,
                } => {
                    assert_eq!(*column, "search");
                    assert_eq!(*dictionary, "english");
                    assert_eq!(query_text, "planet & earth");
                }
                other => panic!("expected TsMatch, got {other:?}"),
            },
            other => panic!("expected Condition::Expr, got {other:?}"),
        }
    }

    #[test]
    fn fts_field_ref_rank_emits_ts_rank_node() {
        let fref = FtsFieldRef::<FakeBook>::__new("search", "english");
        let expr = fref.rank(TsQuery::new("planet & earth"));
        match &expr.node {
            ExprNode::TsRank {
                column,
                dictionary,
                query_text,
            } => {
                assert_eq!(*column, "search");
                assert_eq!(*dictionary, "english");
                assert_eq!(query_text, "planet & earth");
            }
            other => panic!("expected TsRank, got {other:?}"),
        }
    }

    #[test]
    fn fts_matches_sql_emission() {
        // Exercise the full emission path through emit_expr to verify the
        // rendered SQL shape before any database round-trip.
        use crate::expr::sql::emit_expr;
        use crate::pg::accumulator::SqlAccumulator;
        use crate::query::portable::SqlEmitContext;

        let fref = FtsFieldRef::<FakeBook>::__new("search", "english");
        let cond = fref.matches(TsQuery::new("planet & earth"));

        let Condition::Expr(expr) = cond else {
            panic!("expected Condition::Expr");
        };

        let mut acc = SqlAccumulator::new("book");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("FTS expression should lower to SQL");
        let sql = acc.sql();

        assert!(
            sql.contains("search @@ to_tsquery('english',"),
            "expected @@ operator in SQL, got: {sql}"
        );
        assert!(sql.contains("$1"), "expected bind parameter $1, got: {sql}");
    }

    #[test]
    fn fts_rank_sql_emission() {
        use crate::expr::sql::emit_expr;
        use crate::pg::accumulator::SqlAccumulator;
        use crate::query::portable::SqlEmitContext;

        let fref = FtsFieldRef::<FakeBook>::__new("search", "english");
        let rank_expr = fref.rank(TsQuery::new("planet & earth"));

        let mut acc = SqlAccumulator::new("book");
        emit_expr(&mut acc, &rank_expr.node, SqlEmitContext::root())
            .expect("FTS expression should lower to SQL");
        let sql = acc.sql();

        assert!(
            sql.contains("ts_rank(search, to_tsquery('english',"),
            "expected ts_rank SQL, got: {sql}"
        );
        assert!(sql.contains("$1"), "expected bind parameter $1, got: {sql}");
    }
}
