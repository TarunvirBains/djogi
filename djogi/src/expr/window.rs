//! Window function support — `OVER (PARTITION BY ... ORDER BY ... frame EXCLUDE ...)`.
//! # What
//! [`WindowSpec`] describes a SQL window clause. [`WindowBuilder`] is the fluent
//! builder handed to the user's `.over(|w| ...)` closure on
//! [`super::aggregate::AggregateExpr`]. An empty builder (`.over(|w| w)`) emits
//! `OVER ()` — identical to the default wrapping used by ungrouped `.annotate`
//! for value-aggregate annotations. Non-windowable aggregate kinds do not
//! expose `.over(...)` and are also rejected from the plain ungrouped annotate
//! path that would otherwise synthesize `OVER ()`.
//! Partition, order, and frame clauses are opt-in.
//! # Full surface
//! All Postgres window-frame variants are reachable:
//! - Frame kinds: ROWS, RANGE, GROUPS (see [`FrameKind`]).
//! - Frame bounds: UNBOUNDED PRECEDING, N PRECEDING, CURRENT ROW,
//!   N FOLLOWING, UNBOUNDED FOLLOWING (see [`FrameBound`]).
//! - Frame exclusion: EXCLUDE CURRENT ROW, EXCLUDE GROUP, EXCLUDE TIES,
//!   EXCLUDE NO OTHERS (see [`FrameExclude`]).
//! # Design note
//! `WindowBuilder::partition_by` and `WindowBuilder::order_by` both take a
//! [`crate::query::field::FieldRef`], which carries a validated `&'static str`
//! column name. Using `FieldRef` rather than a bare `&str` parameter keeps the
//! window clause in the same compile-time-typed idiom as the rest of the
//! Djogi query surface.

use crate::pg::accumulator::SqlAccumulator;
use crate::query::order::Direction;
use crate::query::portable::PortablePredicateError;

/// A window-function partition or order-by element — either a bare column
/// reference or an expression evaluated under a specific table alias.
///
/// Used internally by [`WindowSpec`] to store both plain-column window
/// elements (via [`WindowBuilder::partition_by`] / [`WindowBuilder::order_by`])
/// and expression-based elements (via the pair-tuple-aware
/// [`PairWindowExt::partition_by_pair_expr`](crate::query::joined::PairWindowExt::partition_by_pair_expr) /
/// [`PairWindowExt::order_by_pair_expr_asc`](crate::query::joined::PairWindowExt::order_by_pair_expr_asc) /
/// [`PairWindowExt::order_by_pair_expr_desc`](crate::query::joined::PairWindowExt::order_by_pair_expr_desc)
/// methods added in GH #302).
///
/// The `Expr` variant carries a boxed [`crate::expr::node::ExprNode`] plus
/// the table alias (e.g. `"l"` or `"r"`) under which the expression's fields
/// are qualified. At emit time, the entire expression is emitted directly inside
/// the window clause (e.g. `PARTITION BY l.col * 10`).
#[derive(Clone, Debug)]
pub(crate) enum WindowTerm {
    /// A bare column reference — emitted as-is.
    Column(&'static str),
    /// An expression emitted under an explicit alias.
    Expr {
        node: Box<crate::expr::node::ExprNode>,
        alias: &'static str,
    },
}

/// A fully specified window clause — partition, ordering, and optional frame.
/// Constructed via [`WindowBuilder`] and stored inside
/// [`crate::expr::node::ExprNode::Aggregate`] when the user calls
/// `.over(|w| ...)` on an [`super::aggregate::AggregateExpr`].
/// The `Default` impl produces an empty spec that emits `OVER ()`.
#[derive(Debug, Clone, Default)]
pub struct WindowSpec {
    pub(crate) partition_by: Vec<WindowTerm>,
    pub(crate) order_by: Vec<(WindowTerm, Direction)>,
    pub(crate) frame: Option<Frame>,
}

/// A window frame clause — `ROWS | RANGE | GROUPS BETWEEN <start> AND <end>
/// [EXCLUDE ...]`.
/// Attached to a [`WindowSpec`] by calling `.rows(...)`, `.range(...)`, or
/// `.groups(...)` on a [`WindowBuilder`], optionally followed by `.exclude(...)`.
#[derive(Debug, Clone)]
pub struct Frame {
    pub(crate) kind: FrameKind,
    pub(crate) start: FrameBound,
    pub(crate) end: FrameBound,
    pub(crate) exclude: Option<FrameExclude>,
}

/// The frame-unit keyword — determines whether the frame is measured in physical
/// rows, ordered values, or peer groups.
/// - `Rows` — physical row offsets from the current row.
/// - `Range` — logical value offsets from the current row's sort key.
/// - `Groups` — peer-group offsets; each peer group is a set of rows that
///   compare equal under the window's `ORDER BY`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum FrameKind {
    /// `ROWS` — physical row count offsets.
    Rows,
    /// `RANGE` — value-based offsets from the current sort key.
    Range,
    /// `GROUPS` — peer-group offsets (requires Postgres 11 or later).
    Groups,
}

/// A single endpoint of a window frame.
/// The `Preceding(n)` and `Following(n)` variants bind `n` as an `i64`
/// parameter so that the frame offset is never interpolated into the SQL
/// string — the value comes from user code and must be parameterised.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING` — the frame extends to the first row of the
    /// partition.
    UnboundedPreceding,
    /// `N PRECEDING` — the frame extends N rows, values, or peer groups
    /// before the current row. `n` is bound as a positional parameter
    /// (`$N PRECEDING`).
    /// # Panics
    /// Emission panics if `n > i64::MAX`. The frame offset is bound as
    /// Postgres `BIGINT`, so values above `i64::MAX` cannot round-trip;
    /// such a value is a programming error, not a runtime condition. The
    /// check uses `i64::try_from` so all build profiles panic rather than
    /// silently truncate.
    Preceding(u64),
    /// `CURRENT ROW` — the frame boundary is the current row itself.
    CurrentRow,
    /// `N FOLLOWING` — the frame extends N rows, values, or peer groups
    /// after the current row. `n` is bound as a positional parameter
    /// (`$N FOLLOWING`).
    /// # Panics
    /// Emission panics if `n > i64::MAX` — see [`FrameBound::Preceding`]
    /// for the rationale.
    Following(u64),
    /// `UNBOUNDED FOLLOWING` — the frame extends to the last row of the
    /// partition.
    UnboundedFollowing,
}

/// The exclusion clause for a window frame — controls which rows near the
/// current row are excluded from the frame.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum FrameExclude {
    /// `EXCLUDE CURRENT ROW` — exclude the current row from the frame.
    CurrentRow,
    /// `EXCLUDE GROUP` — exclude the current row and all peers (rows that
    /// compare equal under the window's `ORDER BY`) from the frame.
    Group,
    /// `EXCLUDE TIES` — exclude only the peers of the current row, keeping
    /// the current row itself in the frame.
    Ties,
    /// `EXCLUDE NO OTHERS` — exclude nothing (Postgres default; explicit for
    /// documentation at the call site).
    NoOthers,
}

/// Fluent builder for a [`WindowSpec`], handed to the `.over(|w| ...)` closure.
/// Every method consumes `self` and returns a new `WindowBuilder` so the
/// closures chain naturally: `.over(|w| w.partition_by(f.org_id()).order_by(f.created_at()))`.
/// An empty builder (`.over(|w| w)`) produces an empty [`WindowSpec`] that
/// emits `OVER ()`.
pub struct WindowBuilder(WindowSpec);

impl Default for WindowBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowBuilder {
    /// Construct an empty builder — corresponds to `OVER ()`.
    pub fn new() -> Self {
        Self(WindowSpec::default())
    }

    /// Add a `PARTITION BY` column to the window.
    /// The column name is taken from the validated `&'static str` inside
    /// the field handle, so no extra identifier validation is needed here.
    /// Calling this method multiple times appends columns in call order
    /// `PARTITION BY first_col, second_col, ...`.
    /// PR3: accepts both legacy `FieldRef<M, V>` and the post-flip root
    /// accessor return type `DjogiField<M, V>` through the sealed
    /// [`IntoSqlField`](crate::query::field::IntoSqlField) bridge — window
    /// partitions are SQL-only emission boundaries, not predicate
    /// boundaries, so the wrapper's column metadata flows through
    /// unchanged.
    pub fn partition_by<M, V, S>(mut self, f: S) -> Self
    where
        M: crate::model::Model,
        S: crate::query::field::IntoSqlField<M, V>,
    {
        self.0
            .partition_by
            .push(WindowTerm::Column(f.into_sql_field().column()));
        self
    }

    /// Add an `ORDER BY <col> ASC` term to the window.
    /// For descending order use [`WindowBuilder::order_by_desc`].
    /// Multiple calls append terms in call order
    /// `ORDER BY first_col ASC, second_col ASC, ...`.
    /// PR3: accepts `FieldRef<M, V>` or `DjogiField<M, V>` through
    /// `IntoSqlField`. Window ordering is a SQL-only emission boundary.
    pub fn order_by<M, V, S>(mut self, f: S) -> Self
    where
        M: crate::model::Model,
        S: crate::query::field::IntoSqlField<M, V>,
    {
        self.0.order_by.push((
            WindowTerm::Column(f.into_sql_field().column()),
            Direction::Asc,
        ));
        self
    }

    /// Add an `ORDER BY <col> DESC` term to the window.
    /// Multiple calls append terms in call order.
    /// PR3: accepts `FieldRef<M, V>` or `DjogiField<M, V>` through
    /// `IntoSqlField`. Window ordering is a SQL-only emission boundary.
    pub fn order_by_desc<M, V, S>(mut self, f: S) -> Self
    where
        M: crate::model::Model,
        S: crate::query::field::IntoSqlField<M, V>,
    {
        self.0.order_by.push((
            WindowTerm::Column(f.into_sql_field().column()),
            Direction::Desc,
        ));
        self
    }

    /// Set a `ROWS BETWEEN <start> AND <end>` frame clause.
    /// Replaces any previously set frame (rows, range, or groups). The
    /// exclusion clause, if any, is reset — call `.exclude(...)` after
    /// `.rows(...)` to set it.
    pub fn rows(mut self, start: FrameBound, end: FrameBound) -> Self {
        self.0.frame = Some(Frame {
            kind: FrameKind::Rows,
            start,
            end,
            exclude: None,
        });
        self
    }

    /// Set a `RANGE BETWEEN <start> AND <end>` frame clause.
    /// Replaces any previously set frame. Exclusion is reset; chain
    /// `.exclude(...)` to add it.
    pub fn range(mut self, start: FrameBound, end: FrameBound) -> Self {
        self.0.frame = Some(Frame {
            kind: FrameKind::Range,
            start,
            end,
            exclude: None,
        });
        self
    }

    /// Set a `GROUPS BETWEEN <start> AND <end>` frame clause.
    /// Replaces any previously set frame. Exclusion is reset; chain
    /// `.exclude(...)` to add it.
    pub fn groups(mut self, start: FrameBound, end: FrameBound) -> Self {
        self.0.frame = Some(Frame {
            kind: FrameKind::Groups,
            start,
            end,
            exclude: None,
        });
        self
    }

    /// Attach an `EXCLUDE ...` clause to the current frame.
    /// Has no effect if no frame has been set via `.rows(...)`, `.range(...)`,
    /// or `.groups(...)` — a frameless window cannot have an exclusion clause.
    pub fn exclude(mut self, ex: FrameExclude) -> Self {
        if let Some(f) = self.0.frame.as_mut() {
            f.exclude = Some(ex);
        }
        self
    }

    /// Finalise the builder and return the [`WindowSpec`].
    pub(crate) fn build(self) -> WindowSpec {
        self.0
    }
}

impl WindowSpec {
    /// Whether every term stored in this window spec is unambiguous under a
    /// self-join — i.e. every emitted column reference is pair-side-qualified
    /// (`l.<col>` or `r.<col>`).
    /// # What
    /// Inspects [`Self::partition_by`] and the term part of every
    /// [`Self::order_by`] entry, and returns `true` iff every term is
    /// unambiguous:
    /// - [`WindowTerm::Column`] entries must start with `"l."` or `"r."` — the
    ///   exact prefixes that
    ///   [`PairWindowExt::partition_by_pair`](crate::query::joined::PairWindowExt::partition_by_pair),
    ///   [`PairWindowExt::order_by_pair_asc`](crate::query::joined::PairWindowExt::order_by_pair_asc),
    ///   and
    ///   [`PairWindowExt::order_by_pair_desc`](crate::query::joined::PairWindowExt::order_by_pair_desc)
    ///   produce.
    /// - [`WindowTerm::Expr`] entries carry an explicit pair alias, but only
    ///   those `ExprNode` variants whose emit arms route every column through
    ///   `ctx.push_column` actually emit that alias. The allow-list predicate
    ///   [`is_allowed_window_expr_node`] vets the node; a denied variant
    ///   (bare-column scalar function, raw fragment, aggregate, subquery,
    ///   spatial, etc.) makes the spec not pair-qualified.
    ///
    /// A vacuous window (no partition, no order) returns `true` — `OVER ()`
    /// references no columns and is unambiguous.
    /// # Why
    /// The joined-annotation safety gate in
    /// [`JoinedAnnotatedQuerySet::fetch_all`](crate::query::JoinedAnnotatedQuerySet::fetch_all)
    /// rejects window slots whose emitted SQL could resolve to either
    /// pair side. Bare column references (e.g. `PARTITION BY id`) are
    /// ambiguous in self-join contexts where both `l` and `r` carry an
    /// `id` column. Pair-qualified references (`PARTITION BY l.id`)
    /// are unambiguous by construction.
    /// # How
    /// For [`WindowTerm::Column`]: plain column idents are validated by
    /// [`crate::ident::assert_plain_ident`] to contain only ASCII
    /// alphanumerics and underscore, so a non-pair-aware `partition_by`
    /// call can never store a string containing `.`. Only the
    /// `intern_alias_column` helper inside [`crate::query::joined`]
    /// composes `"<alias>.<col>"` strings, and it only emits the
    /// `LEFT_ALIAS` (`"l"`) / `RIGHT_ALIAS` (`"r"`) prefixes. So a
    /// `starts_with("l.")` / `starts_with("r.")` check is a faithful
    /// proxy for "this column was registered through `PairWindowExt`".
    ///
    /// For [`WindowTerm::Expr`]: the alias is supplied explicitly by the
    /// pair-aware builder, but the alias only reaches the SQL for variants
    /// that emit columns through `ctx.push_column`, so the per-node allow-list
    /// [`is_allowed_window_expr_node`] is the faithful witness there.
    ///
    /// This method is the per-instance safety witness for window slots
    /// whose `is_joined_safe` impl forwards here.
    pub(crate) fn is_pair_qualified(&self) -> bool {
        let is_term_pair_qualified = |term: &WindowTerm| match term {
            WindowTerm::Column(s) => s.starts_with("l.") || s.starts_with("r."),
            // An Expr entry carries an explicit pair alias, but only those
            // `ExprNode` variants whose emit arms route every column
            // reference through `ctx.push_column` actually emit the alias
            // (`l.<col>` / `r.<col>`). Variants that push a bare column,
            // a verbatim raw fragment, or emit without a `SqlEmitContext`
            // would produce ambiguous SQL in a self-join, so they are
            // rejected by the allow-list gate. See
            // [`is_allowed_window_expr_node`].
            WindowTerm::Expr { node, .. } => is_allowed_window_expr_node(node),
        };
        self.partition_by.iter().all(is_term_pair_qualified)
            && self
                .order_by
                .iter()
                .all(|(term, _)| is_term_pair_qualified(term))
    }

    /// Emit ` OVER (PARTITION BY ... ORDER BY ... frame EXCLUDE ...)` onto
    /// `acc`. The leading space is part of the emission — callers append
    /// this directly after the aggregate function call and optional FILTER
    /// clause.
    ///
    /// # Errors
    /// Returns [`PortablePredicateError`] when a [`WindowTerm::Expr`] node
    /// fails to emit via [`crate::expr::sql::emit_expr`]. The two reachable
    /// cases are:
    /// - [`PortablePredicateError::EmptyVariadicFunction`] — an empty
    ///   `COALESCE()` / `LEAST()` / `GREATEST()` in the window expression.
    /// - [`PortablePredicateError::LateralOuterRefOutOfScope`] — a lateral outer
    ///   ref ([`crate::expr::node::ExprNode::OuterRefAlias`]) in the window
    ///   expression, emitted outside a lateral-inner scope.
    ///
    /// Both shapes are rejected upstream for a *well-formed* query: the
    /// joined-only [`WindowSpec::is_pair_qualified`] gate (via
    /// [`is_allowed_window_expr_node`]) denies them, and every annotate terminal
    /// additionally runs
    /// [`crate::query::annotate::check_window_annotation_legality`] (via
    /// [`WindowSpec::has_empty_variadic_term`] /
    /// [`WindowSpec::has_lateral_outer_ref_term`]). Those static walkers cannot
    /// inspect a type-erased subquery predicate nested inside a window
    /// expression, so propagating the error here — rather than unwrapping — is
    /// what keeps emission panic-free on inputs the walkers cannot reach, and is
    /// defense in depth against any future gate/emitter drift. Plain
    /// [`WindowTerm::Column`] terms and the frame clause never fail.
    pub(crate) fn emit(&self, acc: &mut SqlAccumulator) -> Result<(), PortablePredicateError> {
        acc.push_sql(" OVER (");
        let mut spacer = false;

        if !self.partition_by.is_empty() {
            acc.push_sql("PARTITION BY ");
            for (i, term) in self.partition_by.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                match term {
                    WindowTerm::Column(c) => acc.push_sql(c),
                    WindowTerm::Expr { node, alias } => crate::expr::sql::emit_expr(
                        acc,
                        node,
                        crate::query::portable::SqlEmitContext::joined(alias),
                    )?,
                }
            }
            spacer = true;
        }

        if !self.order_by.is_empty() {
            if spacer {
                acc.push_sql(" ");
            }
            acc.push_sql("ORDER BY ");
            for (i, (term, d)) in self.order_by.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                match term {
                    WindowTerm::Column(c) => acc.push_sql(c),
                    WindowTerm::Expr { node, alias } => crate::expr::sql::emit_expr(
                        acc,
                        node,
                        crate::query::portable::SqlEmitContext::joined(alias),
                    )?,
                }
                acc.push_sql(match d {
                    Direction::Asc => " ASC",
                    Direction::Desc => " DESC",
                });
            }
            spacer = true;
        }

        if let Some(frame) = &self.frame {
            if spacer {
                acc.push_sql(" ");
            }
            acc.push_sql(match frame.kind {
                FrameKind::Rows => "ROWS BETWEEN ",
                FrameKind::Range => "RANGE BETWEEN ",
                FrameKind::Groups => "GROUPS BETWEEN ",
            });
            emit_bound(acc, frame.start);
            acc.push_sql(" AND ");
            emit_bound(acc, frame.end);
            if let Some(ex) = frame.exclude {
                acc.push_sql(match ex {
                    FrameExclude::CurrentRow => " EXCLUDE CURRENT ROW",
                    FrameExclude::Group => " EXCLUDE GROUP",
                    FrameExclude::Ties => " EXCLUDE TIES",
                    FrameExclude::NoOthers => " EXCLUDE NO OTHERS",
                });
            }
        }

        acc.push_sql(")");
        Ok(())
    }

    /// Whether any [`WindowTerm::Expr`] stored in this spec carries an empty
    /// variadic (`COALESCE()` / `LEAST()` / `GREATEST()` with zero operands)
    /// anywhere in its node tree.
    /// # What
    /// Walks every partition and order term; for each [`WindowTerm::Expr`]
    /// node it asks [`crate::expr::sql::expr_node_has_empty_variadic`] whether
    /// the subtree contains an empty variadic. Plain [`WindowTerm::Column`]
    /// terms carry no node tree and are always `false`.
    /// # Why
    /// [`Self::emit`] returns [`PortablePredicateError::EmptyVariadicFunction`]
    /// for exactly this shape (an empty variadic is the only emit failure). The
    /// annotation legality gate
    /// ([`crate::query::annotate::IntoAggregateTuple::check_legality`], called
    /// by every annotate terminal before SQL build) consults this so the empty
    /// variadic surfaces as a typed error on the single-Model, grouped, and
    /// joined paths alike — independent of the joined-only
    /// [`Self::is_pair_qualified`] gate, which runs only on the joined terminal.
    /// Without this, a single-Model `annotate` never calls `is_pair_qualified`,
    /// so an empty-variadic window term reached [`Self::emit`] and panicked at
    /// the `.expect()` in the window-function emitter.
    pub(crate) fn has_empty_variadic_term(&self) -> bool {
        let term_has_empty_variadic = |term: &WindowTerm| match term {
            WindowTerm::Column(_) => false,
            WindowTerm::Expr { node, .. } => crate::expr::sql::expr_node_has_empty_variadic(node),
        };
        self.partition_by.iter().any(term_has_empty_variadic)
            || self
                .order_by
                .iter()
                .any(|(term, _)| term_has_empty_variadic(term))
    }

    /// Whether any [`WindowTerm::Expr`] stored in this spec carries a lateral
    /// outer reference ([`crate::expr::node::ExprNode::OuterRefAlias`])
    /// anywhere in its node tree.
    /// # What
    /// A thin `.is_some()` wrapper over [`Self::find_lateral_outer_ref_term`].
    /// Plain [`WindowTerm::Column`] terms carry a `&'static str` column name —
    /// never an `OuterRefAlias` node — and are always `false`.
    /// # Why
    /// A lateral outer ref is only legal inside a lateral-inner lowering scope.
    /// On a single-Model window annotation there is no such scope, so
    /// [`Self::emit`] (via the `OuterRefAlias` arm of
    /// [`crate::expr::sql::emit_expr`]) returns
    /// [`crate::query::PortablePredicateError::LateralOuterRefOutOfScope`],
    /// which the window-function emitter unwraps with `.expect()` — a panic.
    /// The annotation legality gate
    /// ([`crate::query::annotate::check_window_annotation_legality`], called by
    /// every annotate terminal before SQL build) consults this so the misuse
    /// surfaces as a typed error on the single-Model, grouped, and joined paths
    /// alike, instead of reaching that `.expect()`. Mirrors
    /// [`Self::has_empty_variadic_term`].
    pub(crate) fn has_lateral_outer_ref_term(&self) -> bool {
        self.find_lateral_outer_ref_term().is_some()
    }

    /// Find the first lateral outer reference
    /// ([`crate::expr::node::ExprNode::OuterRefAlias`]) carried by any
    /// [`WindowTerm::Expr`] in this spec, returning its `(column,
    /// source_model)` diagnostic pair.
    /// # What
    /// Walks partition terms then order terms, left to right; for each
    /// [`WindowTerm::Expr`] node it defers to
    /// [`crate::expr::sql::find_lateral_outer_ref`] and returns the first
    /// match. Plain [`WindowTerm::Column`] terms carry a column name string,
    /// never an `OuterRefAlias`, so they never match. Returns `None` when no
    /// window term carries a lateral outer ref.
    /// # Why
    /// [`crate::expr::sql::check_aggregate_legality`] builds a typed
    /// [`crate::query::PortablePredicateError::LateralOuterRefOutOfScope`] that
    /// names the offending column/model; when the lateral ref is carried only
    /// by an aggregate's `.over(...)` window term, this supplies that pair.
    /// [`Self::has_lateral_outer_ref_term`] is the boolean `.is_some()` view
    /// used by [`crate::query::annotate::check_window_annotation_legality`].
    pub(crate) fn find_lateral_outer_ref_term(&self) -> Option<(&'static str, &'static str)> {
        let term_find_lateral_outer_ref = |term: &WindowTerm| match term {
            WindowTerm::Column(_) => None,
            WindowTerm::Expr { node, .. } => crate::expr::sql::find_lateral_outer_ref(node),
        };
        self.partition_by
            .iter()
            .find_map(&term_find_lateral_outer_ref)
            .or_else(|| {
                self.order_by
                    .iter()
                    .find_map(|(term, _)| term_find_lateral_outer_ref(term))
            })
    }
}

fn emit_bound(acc: &mut SqlAccumulator, bound: FrameBound) {
    match bound {
        FrameBound::UnboundedPreceding => acc.push_sql("UNBOUNDED PRECEDING"),
        FrameBound::Preceding(n) => {
            let n = i64::try_from(n)
                .unwrap_or_else(|_| panic!("FrameBound::Preceding(n = {n}) overflows i64"));
            acc.push_bind(n);
            acc.push_sql(" PRECEDING");
        }
        FrameBound::CurrentRow => acc.push_sql("CURRENT ROW"),
        FrameBound::Following(n) => {
            let n = i64::try_from(n)
                .unwrap_or_else(|_| panic!("FrameBound::Following(n = {n}) overflows i64"));
            acc.push_bind(n);
            acc.push_sql(" FOLLOWING");
        }
        FrameBound::UnboundedFollowing => acc.push_sql("UNBOUNDED FOLLOWING"),
    }
}

// SAFETY-CRITICAL: This allow-list predicate is the per-node half of the
// self-join window safety gate. A `WindowTerm::Expr` is emitted under a pair
// alias (`l` / `r`) via `SqlEmitContext::joined(alias)`. Only `ExprNode`
// variants whose emit arms route EVERY column reference through
// `ctx.push_column` produce alias-qualified SQL (`l.<col>` / `r.<col>`) inside
// a self-join. Any variant whose emit arm pushes a bare column name (e.g.
// `array_length(<col>, 1)`, `ts_rank(<col>, ...)`), emits a verbatim raw
// fragment (`RawSql`), or emits without taking a `SqlEmitContext` at all
// (`Spatial`, `RowAggregate`) yields ambiguous SQL that Postgres rejects with
// `42702 column reference is ambiguous` — or, worse, silently resolves to the
// wrong pair side. Those variants are DENIED here.
//
// The match MUST remain exhaustive — do NOT add a `_ => false` wildcard. The
// `ExprNode` enum is `#[non_exhaustive]`, so an exhaustive same-crate match is
// what turns a future variant addition into a COMPILE ERROR that forces a
// deliberate allow/deny decision, instead of silently defaulting a new variant
// to DENIED (a `_ => false` wildcard) and shipping a column the gate never
// vetted. Structural variants (arithmetic, boolean, coalesce, comparison,
// case) recurse into EVERY child so a denied node smuggled inside an allowed
// wrapper — e.g. `Add(Field, Subquery)` — still returns `false`.
//
// This mirrors the `expr_node_contains_excluded` walker in
// `crate::query::insert_select`, which enforces the analogous exhaustive,
// no-wildcard, child-recursing discipline for the EXCLUDED-reference gate.
pub(crate) fn is_allowed_window_expr_node(node: &crate::expr::node::ExprNode) -> bool {
    use crate::expr::node::ExprNode;
    match node {
        // ── ALLOWED leaves: emit arms reference no ambiguous bare column ──
        // `Field` routes through `ctx.push_column`, so it qualifies as
        // `<alias>.<col>` under a joined context.
        ExprNode::Field { .. } => true,
        // `Literal` / `CurrentYear` / `Now` / `IntervalLiteral` emit no column
        // reference at all — they are context-free server-side constants.
        ExprNode::Literal(_)
        | ExprNode::CurrentYear
        | ExprNode::Now
        | ExprNode::IntervalLiteral { .. } => true,

        // ── ALLOWED structural: recurse into EVERY child ──
        // Binary arithmetic / boolean nodes thread `ctx` into both operands
        // (`emit_arith` / the boolean arms), so each side is qualified only
        // if its own subtree is allowed.
        ExprNode::Add(lhs, rhs)
        | ExprNode::Sub(lhs, rhs)
        | ExprNode::Mul(lhs, rhs)
        | ExprNode::Div(lhs, rhs)
        | ExprNode::And(lhs, rhs)
        | ExprNode::Or(lhs, rhs) => {
            is_allowed_window_expr_node(lhs) && is_allowed_window_expr_node(rhs)
        }
        // Unary wrappers recurse into the single inner node.
        ExprNode::Not(inner) | ExprNode::IsNull(inner) | ExprNode::IsNotNull(inner) => {
            is_allowed_window_expr_node(inner)
        }
        // `Coalesce` / `Least` / `Greatest` recurse into every operand.
        // An EMPTY operand list is rejected outright: `COALESCE()` / `LEAST()`
        // / `GREATEST()` with zero arguments is invalid SQL that Postgres
        // refuses with a syntax error, and the emitter (`emit_expr`) returns
        // `PortablePredicateError::EmptyVariadicFunction` for it. Because
        // `Iterator::all` is vacuously `true` on an empty iterator, the
        // recurse-into-every-operand check alone would let an empty variadic
        // pass this gate and reach emission, where it previously panicked.
        // Rejecting it here means the empty variadic is caught at the same
        // fetch-time `is_joined_safe()` boundary as every other disallowed
        // window expression, surfacing as `DjogiError::Validation` before any
        // SQL is built.
        ExprNode::Coalesce(operands) | ExprNode::Least(operands) | ExprNode::Greatest(operands) => {
            !operands.is_empty() && operands.iter().all(is_allowed_window_expr_node)
        }
        // `Cmp` recurses into both operands.
        ExprNode::Cmp { lhs, rhs, .. } => {
            is_allowed_window_expr_node(lhs) && is_allowed_window_expr_node(rhs)
        }
        // `Case` recurses into every arm condition, every arm value, AND the
        // mandatory `otherwise` default — a denied node in any slot poisons
        // the whole CASE.
        ExprNode::Case { arms, otherwise } => {
            arms.iter().all(|(cond, val)| {
                is_allowed_window_expr_node(cond) && is_allowed_window_expr_node(val)
            }) && is_allowed_window_expr_node(otherwise)
        }

        // ── DENIED leaves: emit arms push a bare column or verbatim text ──
        // `RawSql` pushes the fragment verbatim inside parens with no
        // qualification, and is adopter-constructible via
        // `Expr::__raw_sql_fragment`.
        ExprNode::RawSql(_) => false,
        // `ArrayLength` emits `array_length(<col>, 1)` with a bare column.
        ExprNode::ArrayLength { .. } => false,
        // Full-text-search arms emit the column bare through `emit_ts`.
        ExprNode::TsMatch { .. } | ExprNode::TsRank { .. } | ExprNode::TsRankCd { .. } => false,
        // Aggregate / grouping nodes are not row-scalar; they are illegal in
        // a PARTITION BY / ORDER BY key regardless of qualification.
        ExprNode::Aggregate { .. } | ExprNode::GroupingVariadic { .. } => false,
        // Subquery nodes open a separate scope; their emission cannot be
        // pair-qualified from the outer window context.
        ExprNode::Exists(_) | ExprNode::Subquery(_) => false,
        ExprNode::InSubquery { .. } | ExprNode::QuantifiedSubquery { .. } => false,
        // Outer / lateral / EXCLUDED references belong to other emit contexts
        // (correlated subqueries, INSERT ... ON CONFLICT) and never produce a
        // pair-alias-qualified column in a window slot. `CteAliasField` emits a
        // CTE-alias-qualified column (`<alias>.<col>`), not the pair-alias the
        // window context would supply, so it is equally unresolvable here.
        ExprNode::OuterRef { .. }
        | ExprNode::OuterRefColumn { .. }
        | ExprNode::OuterRefAlias { .. }
        | ExprNode::CteAliasField { .. }
        | ExprNode::Excluded { .. } => false,

        // ── Trigram (gated): ALLOWED — emit arms route through `ctx.push_column` ──
        #[cfg(feature = "trgm")]
        ExprNode::TrgmSimilarTo { .. } | ExprNode::TrgmSimilarityScore { .. } => true,

        // ── Spatial (gated): DENIED — `SpatialExpr::emit` / row aggregates
        // take no `SqlEmitContext`, so columns emit unqualified ──
        #[cfg(feature = "spatial")]
        ExprNode::Spatial(_) => false,
        #[cfg(feature = "spatial")]
        ExprNode::RowAggregate { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `WindowSpec::emit` — each variant combination produces
    //! the expected SQL fragment. These tests construct `WindowSpec` values
    //! directly (bypassing `WindowBuilder`) to isolate the emitter from the
    //! builder; round-trip tests through `.over(|w| ...)` live in
    //! `expr::aggregate::tests`.

    use super::*;
    use crate::pg::accumulator::SqlAccumulator;

    fn emit(spec: &WindowSpec) -> String {
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
        acc.sql().to_string()
    }

    // ── Empty spec ────────────────────────────────────────────────────────

    #[test]
    fn empty_spec_emits_over_parens() {
        let spec = WindowSpec::default();
        assert_eq!(emit(&spec), " OVER ()");
    }

    // ── PARTITION BY ─────────────────────────────────────────────────────

    #[test]
    fn partition_by_single_column() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("org_id")],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (PARTITION BY org_id)");
    }

    #[test]
    fn partition_by_two_columns() {
        let spec = WindowSpec {
            partition_by: vec![
                WindowTerm::Column("org_id"),
                WindowTerm::Column("department_id"),
            ],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (PARTITION BY org_id, department_id)");
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    #[test]
    fn order_by_asc() {
        let spec = WindowSpec {
            order_by: vec![(WindowTerm::Column("created_at"), Direction::Asc)],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (ORDER BY created_at ASC)");
    }

    #[test]
    fn order_by_desc() {
        let spec = WindowSpec {
            order_by: vec![(WindowTerm::Column("amount"), Direction::Desc)],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (ORDER BY amount DESC)");
    }

    #[test]
    fn order_by_asc_and_desc() {
        let spec = WindowSpec {
            order_by: vec![
                (WindowTerm::Column("created_at"), Direction::Asc),
                (WindowTerm::Column("amount"), Direction::Desc),
            ],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (ORDER BY created_at ASC, amount DESC)");
    }

    // ── PARTITION BY + ORDER BY ───────────────────────────────────────────

    #[test]
    fn partition_and_order_by_separated_by_space() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("org_id")],
            order_by: vec![(WindowTerm::Column("created_at"), Direction::Asc)],
            ..Default::default()
        };
        assert_eq!(
            emit(&spec),
            " OVER (PARTITION BY org_id ORDER BY created_at ASC)"
        );
    }

    // ── ROWS frame ───────────────────────────────────────────────────────

    #[test]
    fn rows_unbounded_preceding_to_current_row() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::CurrentRow,
                exclude: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            emit(&spec),
            " OVER (ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
        );
    }

    #[test]
    fn rows_preceding_n_binds_as_parameter() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::Preceding(3),
                end: FrameBound::CurrentRow,
                exclude: None,
            }),
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
        let sql = acc.sql().to_string();
        // The offset 3 is bound as $1; the FOLLOWING keyword is a SQL token.
        assert!(
            sql.contains("ROWS BETWEEN $1 PRECEDING AND CURRENT ROW"),
            "got: {sql}"
        );
    }

    #[test]
    fn rows_current_row_to_following_n_binds_as_parameter() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::CurrentRow,
                end: FrameBound::Following(2),
                exclude: None,
            }),
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("ROWS BETWEEN CURRENT ROW AND $1 FOLLOWING"),
            "got: {sql}"
        );
    }

    // ── Frame bound overflow guards ──────────────────────────────────────

    #[test]
    #[should_panic(expected = "FrameBound::Preceding(n = 18446744073709551615) overflows i64")]
    fn frame_preceding_over_i64_max_panics() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::Preceding(u64::MAX),
                end: FrameBound::CurrentRow,
                exclude: None,
            }),
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
    }

    #[test]
    #[should_panic(expected = "FrameBound::Following(n = 18446744073709551615) overflows i64")]
    fn frame_following_over_i64_max_panics() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::CurrentRow,
                end: FrameBound::Following(u64::MAX),
                exclude: None,
            }),
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
    }

    // ── RANGE frame ──────────────────────────────────────────────────────

    #[test]
    fn range_unbounded_both_sides() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Range,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::UnboundedFollowing,
                exclude: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            emit(&spec),
            " OVER (RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"
        );
    }

    // ── GROUPS frame ─────────────────────────────────────────────────────

    #[test]
    fn groups_current_row_to_following_n() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Groups,
                start: FrameBound::CurrentRow,
                end: FrameBound::Following(1),
                exclude: None,
            }),
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("GROUPS BETWEEN CURRENT ROW AND $1 FOLLOWING"),
            "got: {sql}"
        );
    }

    // ── EXCLUDE variants ─────────────────────────────────────────────────

    #[test]
    fn exclude_current_row() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::CurrentRow,
                exclude: Some(FrameExclude::CurrentRow),
            }),
            ..Default::default()
        };
        let sql = emit(&spec);
        assert!(sql.contains("EXCLUDE CURRENT ROW"), "got: {sql}");
    }

    #[test]
    fn exclude_group() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::UnboundedFollowing,
                exclude: Some(FrameExclude::Group),
            }),
            ..Default::default()
        };
        let sql = emit(&spec);
        assert!(sql.contains("EXCLUDE GROUP"), "got: {sql}");
    }

    #[test]
    fn exclude_ties() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::UnboundedFollowing,
                exclude: Some(FrameExclude::Ties),
            }),
            ..Default::default()
        };
        let sql = emit(&spec);
        assert!(sql.contains("EXCLUDE TIES"), "got: {sql}");
    }

    #[test]
    fn exclude_no_others() {
        let spec = WindowSpec {
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::UnboundedPreceding,
                end: FrameBound::UnboundedFollowing,
                exclude: Some(FrameExclude::NoOthers),
            }),
            ..Default::default()
        };
        let sql = emit(&spec);
        assert!(sql.contains("EXCLUDE NO OTHERS"), "got: {sql}");
    }

    // ── All-combined ─────────────────────────────────────────────────────

    #[test]
    fn full_composition_partition_order_rows_exclude() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("org_id")],
            order_by: vec![(WindowTerm::Column("created_at"), Direction::Asc)],
            frame: Some(Frame {
                kind: FrameKind::Rows,
                start: FrameBound::Preceding(3),
                end: FrameBound::CurrentRow,
                exclude: Some(FrameExclude::Ties),
            }),
        };
        let mut acc = SqlAccumulator::new("");
        spec.emit(&mut acc)
            .expect("test window specs use only well-formed terms");
        let sql = acc.sql().to_string();
        assert!(
            sql.contains("PARTITION BY org_id"),
            "missing PARTITION BY: {sql}"
        );
        assert!(
            sql.contains("ORDER BY created_at ASC"),
            "missing ORDER BY: {sql}"
        );
        assert!(
            sql.contains("ROWS BETWEEN $1 PRECEDING AND CURRENT ROW"),
            "missing ROWS frame: {sql}"
        );
        assert!(sql.contains("EXCLUDE TIES"), "missing EXCLUDE: {sql}");
    }

    // ── is_pair_qualified ────────────────────────────────────────────────

    #[test]
    fn is_pair_qualified_true_for_empty_spec() {
        // A vacuous `OVER ()` window references no columns — trivially
        // unambiguous in joined contexts.
        let spec = WindowSpec::default();
        assert!(
            spec.is_pair_qualified(),
            "empty WindowSpec must be vacuously pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_true_for_left_qualified_partition() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("l.id")],
            ..Default::default()
        };
        assert!(
            spec.is_pair_qualified(),
            "`PARTITION BY l.id` must be pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_true_for_right_qualified_partition() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("r.id")],
            ..Default::default()
        };
        assert!(
            spec.is_pair_qualified(),
            "`PARTITION BY r.id` must be pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_true_for_mixed_sides() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("l.id"), WindowTerm::Column("r.name")],
            order_by: vec![(WindowTerm::Column("l.score"), Direction::Desc)],
            ..Default::default()
        };
        assert!(
            spec.is_pair_qualified(),
            "mixed l./r. qualifiers across partition+order must be pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_bare_partition() {
        // The exact emission the blocker calls out: `partition_by(l.id())`
        // through the non-pair-aware path stores a bare `"id"`.
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("id")],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "bare `PARTITION BY id` must NOT be pair-qualified — ambiguous in self-joins"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_bare_order_by() {
        let spec = WindowSpec {
            order_by: vec![(WindowTerm::Column("score"), Direction::Desc)],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "bare `ORDER BY score DESC` must NOT be pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_false_when_any_entry_is_bare() {
        // One bare slot poisons the whole spec — matches the AND logic in
        // the joined-annotation gate.
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("l.id"), WindowTerm::Column("name")],
            order_by: vec![(WindowTerm::Column("r.score"), Direction::Desc)],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "any single bare column must disqualify the whole spec"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_unrelated_prefix() {
        // Defense-in-depth: a column literally named with a `.` prefix
        // cannot occur today (idents are validated to be alnum/underscore
        // only), but assert the check rejects anything that's not l./r.
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("la.path_count")],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "`la.` prefix (closure-pair alias) is not a pair-window-qualified prefix"
        );
    }

    // ── WindowTerm::Expr support ─────────────────────────────────────────

    #[test]
    fn partition_by_pair_expr_emits_qualified_field() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![crate::expr::window::WindowTerm::Expr {
                node: Box::new(ExprNode::Field { column: "score" }),
                alias: "l",
            }],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (PARTITION BY l.score)");
    }

    #[test]
    fn order_by_pair_expr_desc_emits_qualified_arithmetic() {
        use crate::expr::node::ExprNode;
        use crate::query::condition::FilterValue;
        let spec = WindowSpec {
            order_by: vec![(
                crate::expr::window::WindowTerm::Expr {
                    node: Box::new(ExprNode::Mul(
                        Box::new(ExprNode::Field { column: "score" }),
                        Box::new(ExprNode::Literal(FilterValue::I32(10))),
                    )),
                    alias: "l",
                },
                Direction::Desc,
            )],
            ..Default::default()
        };
        assert_eq!(emit(&spec), " OVER (ORDER BY l.score * $1 DESC)");
    }

    #[test]
    fn is_pair_qualified_true_for_expr_term() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![crate::expr::window::WindowTerm::Expr {
                node: Box::new(ExprNode::Field { column: "id" }),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(
            spec.is_pair_qualified(),
            "Expr term must be pair-qualified by construction"
        );
    }

    #[test]
    fn is_pair_qualified_false_when_bare_column_mixed_with_expr() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![
                crate::expr::window::WindowTerm::Column("bare_col"),
                crate::expr::window::WindowTerm::Expr {
                    node: Box::new(ExprNode::Field { column: "id" }),
                    alias: "l",
                },
            ],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "bare Column entry poisons the spec"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_denied_array_length() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::ArrayLength { column: "tags" }),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "ArrayLength emits bare column, must not be pair-qualified"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_denied_raw_sql() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::RawSql("custom_expr(col)")),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "RawSql emits verbatim fragment, must not be pair-qualified"
        );
    }

    // ── Empty variadic (COALESCE/LEAST/GREATEST with no operands) ─────────
    //
    // An empty variadic emits invalid SQL (`COALESCE()` / `LEAST()` /
    // `GREATEST()`), which Postgres rejects. `emit_expr` surfaces this as
    // `PortablePredicateError::EmptyVariadicFunction`. Two guarantees are
    // asserted: (1) the fetch-time `is_pair_qualified()` gate rejects the
    // shape so it never reaches SQL build via a well-formed query, and (2)
    // `WindowSpec::emit` propagates the error instead of panicking, as
    // defense in depth if the gate and emitter ever drift.

    #[test]
    fn is_pair_qualified_false_for_empty_coalesce_partition() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::Coalesce(Vec::new())),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "empty COALESCE emits invalid SQL — must be rejected at the gate, not \
             pass vacuously via `Iterator::all` on an empty operand list"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_empty_least_order_by() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Least(Vec::new())),
                    alias: "r",
                },
                Direction::Asc,
            )],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "empty LEAST in ORDER BY must be rejected at the gate"
        );
    }

    #[test]
    fn is_pair_qualified_false_for_empty_greatest_order_by() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Greatest(Vec::new())),
                    alias: "l",
                },
                Direction::Desc,
            )],
            ..Default::default()
        };
        assert!(
            !spec.is_pair_qualified(),
            "empty GREATEST in ORDER BY must be rejected at the gate"
        );
    }

    #[test]
    fn is_pair_qualified_true_for_non_empty_coalesce() {
        use crate::expr::node::ExprNode;
        // Regression guard: rejecting the EMPTY case must not reject a
        // well-formed non-empty variadic whose operands are all allowed.
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::Coalesce(vec![
                    ExprNode::Field { column: "score" },
                    ExprNode::Field { column: "rank" },
                ])),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(
            spec.is_pair_qualified(),
            "a non-empty COALESCE of allowed fields must remain pair-qualified"
        );
    }

    #[test]
    fn emit_returns_err_for_empty_coalesce_partition() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::Coalesce(Vec::new())),
                alias: "l",
            }],
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        let result = spec.emit(&mut acc);
        assert!(
            matches!(result, Err(PortablePredicateError::EmptyVariadicFunction)),
            "empty COALESCE partition must propagate EmptyVariadicFunction, \
             not panic; got {result:?}"
        );
    }

    #[test]
    fn emit_returns_err_for_empty_least_order_by() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Least(Vec::new())),
                    alias: "r",
                },
                Direction::Asc,
            )],
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        let result = spec.emit(&mut acc);
        assert!(
            matches!(result, Err(PortablePredicateError::EmptyVariadicFunction)),
            "empty LEAST ordering must propagate EmptyVariadicFunction, \
             not panic; got {result:?}"
        );
    }

    #[test]
    fn emit_returns_err_for_empty_greatest_order_by() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Greatest(Vec::new())),
                    alias: "l",
                },
                Direction::Desc,
            )],
            ..Default::default()
        };
        let mut acc = SqlAccumulator::new("");
        let result = spec.emit(&mut acc);
        assert!(
            matches!(result, Err(PortablePredicateError::EmptyVariadicFunction)),
            "empty GREATEST ordering must propagate EmptyVariadicFunction, \
             not panic; got {result:?}"
        );
    }

    // ── has_empty_variadic_term ──────────────────────────────────────────
    //
    // The fetch-time witness consulted by `check_legality` on every annotate
    // terminal (independent of the joined-only `is_pair_qualified` gate), so an
    // empty-variadic window term is rejected before it reaches `emit`.

    #[test]
    fn has_empty_variadic_term_true_for_empty_coalesce_partition() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::Coalesce(Vec::new())),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(spec.has_empty_variadic_term());
    }

    #[test]
    fn has_empty_variadic_term_true_for_empty_least_order_by() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Least(Vec::new())),
                    alias: "r",
                },
                Direction::Asc,
            )],
            ..Default::default()
        };
        assert!(spec.has_empty_variadic_term());
    }

    #[test]
    fn has_empty_variadic_term_false_for_plain_columns() {
        // Plain column terms carry no node tree — never an empty variadic.
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Column("l.id")],
            order_by: vec![(WindowTerm::Column("r.score"), Direction::Desc)],
            ..Default::default()
        };
        assert!(!spec.has_empty_variadic_term());
    }

    #[test]
    fn has_empty_variadic_term_false_for_non_empty_expr_term() {
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(ExprNode::Coalesce(vec![
                    ExprNode::Field { column: "score" },
                    ExprNode::Field { column: "rank" },
                ])),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(!spec.has_empty_variadic_term());
    }

    #[test]
    fn has_empty_variadic_term_false_for_empty_spec() {
        // A vacuous `OVER ()` window has no terms and so no empty variadic.
        assert!(!WindowSpec::default().has_empty_variadic_term());
    }

    // ── has_lateral_outer_ref_term / find_lateral_outer_ref_term ──────────
    //
    // The fetch-time witness consulted by `check_window_annotation_legality`
    // so a lateral outer ref (`OuterRef::as_lateral_outer_expr()`,
    // `ExprNode::OuterRefAlias`) in a window term is rejected before it
    // reaches `emit` and the window-function slot `.expect()`.

    /// Build the `OuterRefAlias` node `as_lateral_outer_expr()` produces. The
    /// `model_type` / `model_name` fields take any concrete type's identity —
    /// these walkers never compare the `TypeId` (that happens at emit time).
    fn lateral_outer_ref(column: &'static str) -> crate::expr::node::ExprNode {
        struct Marker;
        crate::expr::node::ExprNode::OuterRefAlias {
            alias: "l",
            column,
            model_type: std::any::TypeId::of::<Marker>(),
            model_name: std::any::type_name::<Marker>(),
        }
    }

    #[test]
    fn has_lateral_outer_ref_term_true_for_partition_ref() {
        let spec = WindowSpec {
            partition_by: vec![WindowTerm::Expr {
                node: Box::new(lateral_outer_ref("owner_id")),
                alias: "l",
            }],
            ..Default::default()
        };
        assert!(spec.has_lateral_outer_ref_term());
        assert_eq!(
            spec.find_lateral_outer_ref_term().map(|(c, _)| c),
            Some("owner_id"),
            "partition-term lateral ref must be found and name its column"
        );
    }

    #[test]
    fn has_lateral_outer_ref_term_true_for_order_by_ref() {
        let spec = WindowSpec {
            order_by: vec![(
                WindowTerm::Expr {
                    node: Box::new(lateral_outer_ref("sort_key")),
                    alias: "r",
                },
                Direction::Desc,
            )],
            ..Default::default()
        };
        assert_eq!(
            spec.find_lateral_outer_ref_term().map(|(c, _)| c),
            Some("sort_key")
        );
    }

    #[test]
    fn has_lateral_outer_ref_term_false_for_plain_and_empty() {
        // Plain column terms and clean expression terms carry no lateral ref.
        use crate::expr::node::ExprNode;
        let spec = WindowSpec {
            partition_by: vec![
                WindowTerm::Column("l.id"),
                WindowTerm::Expr {
                    node: Box::new(ExprNode::Field { column: "score" }),
                    alias: "l",
                },
            ],
            ..Default::default()
        };
        assert!(!spec.has_lateral_outer_ref_term());
        assert!(spec.find_lateral_outer_ref_term().is_none());

        // A vacuous `OVER ()` window has no terms.
        assert!(!WindowSpec::default().has_lateral_outer_ref_term());
    }
}
