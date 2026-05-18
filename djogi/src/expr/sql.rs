//! Expression-IR → SQL emitter.
//!
//! # What
//!
//! [`emit_expr`] walks an [`ExprNode`] tree and pushes the matching SQL
//! tokens + bind parameters onto a [`crate::pg::accumulator::SqlAccumulator`].
//! The entry point is called exactly once per `Condition::Expr` variant
//! by [`crate::query::sql::emit_condition`]; it recurses into itself for
//! nested arithmetic / comparison sub-trees.
//!
//! # Why a separate emitter?
//!
//! [`crate::query::sql::emit_leaf`] handles `column op literal` — the
//! left side is always a bare column name, the right side always a
//! literal. The expression IR generalises both sides: either can be a
//! column, a literal, or a nested arithmetic expression. A recursive
//! emitter is the natural fit; factoring it into its own function
//! keeps the leaf path (with its `parent_table` qualification + `ILIKE`
//! escape rules) un-entangled from the recursive walk.
//!
//! # Column references vs bind parameters
//!
//! - [`ExprNode::Field { column }`] — `acc.push_sql(*column)`. The column
//!   name is a `&'static str` validated at
//!   [`crate::query::field::FieldRef::new`] construction time against
//!   [`crate::ident::assert_plain_ident`]; no re-validation here.
//! - [`ExprNode::Literal(v)`] — delegates to
//!   [`crate::query::sql::push_filter_value`], which calls
//!   `acc.push_bind(v)` for every scalar variant. All user-supplied
//!   values flow through bind parameters; no string interpolation of
//!   user data.
//!
//! # `parent_table` qualification
//!
//! [`crate::query::sql::emit_condition`] takes a
//! `parent_table: Option<&'static str>` argument so `select_related`
//! joined queries qualify bare column references as `{table}.{col}`.
//! The expression IR now threads the same qualifier through
//! [`emit_expr`] via [`SqlEmitContext`]: the
//! [`ExprNode::Field { column }`] arm calls
//! [`SqlEmitContext::push_column`] so a pair-tuple joined query's
//! `filter_expr` emits `l.<col>` / `r.<col>` instead of bare names
//! when the WHERE clause is qualified for either pair side.
//!
//! Non-joined callers (the vast majority — single-Model `filter`,
//! aggregate scalar terminals, `update`, `delete`, etc.) pass
//! [`SqlEmitContext::root()`] which qualifies nothing, preserving
//! the bare-column emission Phase 2 shipped.

use crate::expr::node::{AggOp, CmpOp, ExprNode, SubqueryNode};
use crate::pg::accumulator::SqlAccumulator;
use crate::query::portable::{PortablePredicateError, SqlEmitContext};

/// Check whether an [`ExprNode`] tree contains any aggregate modifier
/// combination Postgres would reject or that Djogi cannot currently emit
/// correctly.
///
/// # Rejected combinations
///
/// - `COUNT(*)` with `distinct = true` — `COUNT(DISTINCT *)` is not valid SQL.
/// - `STRING_AGG(col, sep)` with `distinct = true` — Postgres requires an
///   explicit per-aggregate `ORDER BY` clause when DISTINCT is combined with
///   `STRING_AGG`; the check rejects only the no-`ORDER BY` shape.
/// - `COUNT(*)` with a per-aggregate `ORDER BY` — the emitter's `COUNT(*)`
///   branch has no column slot to attach that ordering to, so the modifier
///   would be silently dropped.
///
/// The type-state surface prevents non-value aggregate families from exposing
/// illegal modifiers. Debug builds additionally assert those invariants for
/// direct-IR construction paths so malformed internal nodes fail early during
/// tests/development.
///
/// # When to call
///
/// Terminal methods (`fetch_one`, `fetch_all`) call this before building the
/// SQL string so the caller gets a typed `DjogiError::UnsupportedAggregate`
/// rather than a cryptic Postgres syntax error.
pub(crate) fn check_aggregate_legality(node: &ExprNode) -> Result<(), crate::DjogiError> {
    match node {
        ExprNode::Aggregate {
            op,
            distinct,
            arg,
            arg2,
            filter,
            order_by,
            within_group_order_by,
            // `cast_to` is intentionally ignored — it is a framework-emitted
            // narrowing cast, never a user-supplied modifier. `window` is
            // bound for the debug-only type-state invariant checks below.
            window,
            cast_to: _,
        } => {
            // ── DISTINCT-shape rejections ─────────────────────────────
            if *distinct {
                match op {
                    AggOp::CountStar => {
                        return Err(crate::DjogiError::UnsupportedAggregate {
                            op: "COUNT(*)",
                            reason: "COUNT(DISTINCT *) is not valid SQL — \
                                     use COUNT(DISTINCT col) via FieldRef::count() instead",
                        });
                    }
                    AggOp::StringAgg(_) if order_by.is_empty() => {
                        // `STRING_AGG(DISTINCT col, sep)` requires a per-
                        // aggregate `ORDER BY` to disambiguate the output
                        // tail. With Cluster E T1, callers can chain
                        // `.order_by(f.other.asc())`; until they do, the
                        // combination is still ill-formed Postgres.
                        return Err(crate::DjogiError::UnsupportedAggregate {
                            op: "STRING_AGG",
                            reason: "STRING_AGG(DISTINCT col, sep) requires a per-aggregate \
                                     ORDER BY clause — chain `.order_by(other_field.asc())` \
                                     to disambiguate, otherwise Postgres rejects the call",
                        });
                    }
                    _ => {}
                }
            }

            // ── COUNT(*) + ORDER BY silent-drop guard (Codex round-1) ──
            // The COUNT(*) emitter hard-codes `COUNT(*)` and never renders
            // the `order_by` slot — chaining `.order_by(..)` on a
            // `count_star()` aggregate would silently drop the modifier.
            // Reject at fetch time so adopters see a typed error rather
            // than mysteriously-missing ordering. `COUNT(col ORDER BY ..)`
            // remains valid via `FieldRef::count()` which routes through
            // the unary-emit path.
            if matches!(op, AggOp::CountStar) && !order_by.is_empty() {
                return Err(crate::DjogiError::UnsupportedAggregate {
                    op: "COUNT(*)",
                    reason: "COUNT(*) does not accept a per-aggregate ORDER BY clause — \
                             COUNT counts every row and ordering inside the aggregate has \
                             no effect; chain ORDER BY at the QuerySet level instead, or \
                             use COUNT(col ORDER BY ...) via FieldRef::count()",
                });
            }

            // ── Debug-only direct-IR type-state firewall ───────────────
            // The typed API prevents these malformed shapes by withholding
            // the corresponding methods from the aggregate kind. These
            // assertions protect crate-internal direct-IR construction from
            // drifting out of sync with that public type-state surface.
            let ordered_or_hypothetical = matches!(
                op,
                AggOp::PercentileCont
                    | AggOp::PercentileDisc
                    | AggOp::Mode
                    | AggOp::HypotheticalRank
                    | AggOp::HypotheticalDenseRank
                    | AggOp::HypotheticalPercentRank
                    | AggOp::HypotheticalCumeDist
            );

            debug_assert!(
                !ordered_or_hypothetical || (!*distinct && order_by.is_empty() && window.is_none()),
                "ordered-set / hypothetical-set aggregate must not carry DISTINCT, \
                 per-aggregate ORDER BY, or window modifiers — the typed kind-state \
                 surface does not expose those methods"
            );

            // The typed `AggregateExpr::ordered_set` constructor always
            // populates `within_group_order_by` at construction time —
            // these ops are unreachable through the typed surface with an
            // empty target list. This defensive guard catches future
            // direct-IR construction (e.g. crate-internal helpers cloning
            // the `ExprNode::Aggregate { ... }` literal pattern without
            // routing through `ordered_set`, or `__bypass`-style callers
            // building nodes by hand). Behind `debug_assert!` so release
            // builds pay zero runtime cost while developer / CI builds
            // surface the malformed node immediately rather than emitting
            // invalid Postgres at fetch time.
            //
            // Kept distinct from the typed kind-state guard at the
            // `AggregateExpr` level (#89): the kind-state enforces "you
            // cannot build `f.col().sum().within_group_order_by(...)`";
            // this assertion enforces "if you somehow built an ordered-set
            // node, its WITHIN GROUP target must be populated."
            debug_assert!(
                !ordered_or_hypothetical || !within_group_order_by.is_empty(),
                "ordered-set / hypothetical-set aggregate must carry a non-empty \
                 within_group_order_by — typed constructor `AggregateExpr::ordered_set` \
                 populates this at build time; reaching this assertion indicates a \
                 direct-IR construction that bypassed the typed surface"
            );

            debug_assert!(
                !matches!(op, AggOp::Grouping)
                    || (!*distinct
                        && filter.is_none()
                        && order_by.is_empty()
                        && window.is_none()
                        && within_group_order_by.is_empty()),
                "GROUPING aggregate must not carry modifiers — the metadata kind-state \
                 surface does not expose DISTINCT, FILTER, ORDER BY, OVER, or \
                 WITHIN GROUP modifiers"
            );

            // Recurse into arg, arg2, and filter sub-trees in case there
            // are nested aggregates (unusual but structurally possible).
            // arg2 was added by T5; threading it through the walker keeps
            // future binary-aggregate sub-expressions inside legality
            // (Codex round-1 counter-signal).
            check_aggregate_legality(arg)?;
            if let Some(a2) = arg2 {
                check_aggregate_legality(a2)?;
            }
            if let Some(f) = filter {
                check_aggregate_legality(f)?;
            }
            Ok(())
        }
        // Recurse into compound expression nodes.
        ExprNode::Add(l, r) | ExprNode::Sub(l, r) | ExprNode::Mul(l, r) | ExprNode::Div(l, r) => {
            check_aggregate_legality(l)?;
            check_aggregate_legality(r)
        }
        ExprNode::Cmp { lhs, rhs, .. } => {
            check_aggregate_legality(lhs)?;
            check_aggregate_legality(rhs)
        }
        ExprNode::Case { arms, otherwise } => {
            for (cond, val) in arms {
                check_aggregate_legality(cond)?;
                check_aggregate_legality(val)?;
            }
            check_aggregate_legality(otherwise)
        }
        // GROUPING variadic carries no modifier slots under the #89
        // type-state migration, so `AggregateExpr` methods cannot build
        // invalid modifiers. The legality of the node itself is
        // trivially ok; walk the args in case a nested aggregate was
        // somehow constructed inside a column.
        ExprNode::GroupingVariadic { args } => {
            for a in args {
                check_aggregate_legality(a)?;
            }
            Ok(())
        }
        // Row aggregates (#92) carry no modifier slots at all — the typed
        // `RowAggregate<Out, K>` wrapper exposes no `.distinct()` /
        // `.filter()` / `.over()` / `.order_by()` /
        // `.within_group_order_by()` methods, so there is nothing for
        // this checker to reject. The arm exists for completeness and to
        // make the variant explicit on this walker's match.
        #[cfg(feature = "spatial")]
        ExprNode::RowAggregate { .. } => Ok(()),
        // Leaf nodes and variants with no sub-expressions are trivially valid.
        _ => Ok(()),
    }
}

/// Walk an [`ExprNode`] and push the corresponding SQL fragment onto
/// `acc`. Leaves consume bind slots (via
/// [`crate::query::sql::push_filter_value`]); internal nodes push SQL
/// operator tokens and recurse.
///
/// # Invariants
///
/// - The input tree is always constructed via the typed [`Expr<T>`]
///   surface — user code cannot fabricate an `ExprNode` directly (the
///   enum is `pub(crate)`). That means every arm's operand types match
///   the operator's SQL semantics: `Cmp` operands are always
///   compatible-typed, `Add`/`Sub`/`Mul`/`Div` operands are always
///   [`super::arithmetic::Numeric`]. The emitter does not re-check;
///   the sealed trait + phantom-typed wrapper is the seal.
/// - `ExprNode::Field { column }`'s column string is always a
///   validated identifier (see
///   [`crate::query::field::FieldRef::new`]); safe to push under
///   [`SqlEmitContext::push_column`].
///
/// # `ctx`
///
/// Threads the parent-table qualifier through to every
/// [`ExprNode::Field`] arm. Joined-query call sites (the pair-tuple
/// emitter's `filter_expr` on either side, single-Model
/// `select_related`'s filter path) pass [`SqlEmitContext::joined(alias)`]
/// so a bare column reference renders as `<alias>.<column>`. Non-joined
/// call sites pass [`SqlEmitContext::root()`] for the historical
/// bare-column emission.
pub(crate) fn emit_expr(
    acc: &mut SqlAccumulator,
    node: &ExprNode,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match node {
        ExprNode::Field { column } => {
            // Qualified column reference — `ctx.push_column` prepends
            // the parent-table alias when set (joined / pair-tuple
            // paths) and emits bare otherwise (non-joined). The column
            // string is always validated at FieldRef construction.
            ctx.push_column(acc, column);
        }
        // Phase 8β T4.2 — raw SQL fragment escape hatch for
        // `#[computed(sql = "...")]`. Wrapped in outer parens for
        // operator-precedence stability under further composition with
        // arithmetic / comparison / aggregate nodes. The fragment is a
        // `&'static str` baked at macro expansion time; no bind values
        // are threaded through the accumulator.
        ExprNode::RawSql(s) => {
            acc.push_sql("(");
            acc.push_sql(s);
            acc.push_sql(")");
        }
        ExprNode::Literal(v) => {
            // `push_filter_value` consumes the value, so clone it — the
            // expression tree may be emitted more than once if, e.g., a
            // retry path re-emits the same `UpdateStmt`. All scalar
            // variants of `FilterValue` are cheap to clone (most are
            // `Copy` or carry a short `String`); the boxed `List`/`Pair`
            // shapes never reach here because `ExprNode::Literal` only
            // carries scalar values from the `impl From<V> for Expr<V>`
            // bridges.
            crate::query::sql::push_filter_value(acc, v.clone());
        }
        ExprNode::Add(lhs, rhs) => {
            emit_arith(acc, lhs, " + ", rhs, ctx)?;
        }
        ExprNode::Sub(lhs, rhs) => {
            emit_arith(acc, lhs, " - ", rhs, ctx)?;
        }
        ExprNode::Mul(lhs, rhs) => {
            emit_arith(acc, lhs, " * ", rhs, ctx)?;
        }
        ExprNode::Div(lhs, rhs) => {
            emit_arith(acc, lhs, " / ", rhs, ctx)?;
        }
        ExprNode::Cmp { op, lhs, rhs } => {
            emit_expr(acc, lhs, ctx)?;
            acc.push_sql(match op {
                CmpOp::Eq => " = ",
                CmpOp::Neq => " <> ",
                CmpOp::Gt => " > ",
                CmpOp::Gte => " >= ",
                CmpOp::Lt => " < ",
                CmpOp::Lte => " <= ",
            });
            emit_expr(acc, rhs, ctx)?;
        }
        ExprNode::Aggregate {
            op,
            arg,
            arg2,
            filter,
            cast_to: _,
            distinct,
            window: _,
            order_by,
            within_group_order_by,
        } => {
            // Bare aggregate emission — keyword, optional DISTINCT, argument,
            // closing paren, optional FILTER clause. The `cast_to` field is
            // intentionally ignored here; the narrowing cast lives at
            // the terminal layer (see
            // [`crate::query::sql::emit_aggregate_with_cast`] and
            // [`crate::query::sql::emit_aggregate_with_window_and_cast`])
            // because its placement depends on whether the aggregate
            // is used as a SELECT scalar (`(AGG(..))::TY`) or inside
            // the annotate SELECT list with a window function
            // (`(AGG(..) OVER ())::TY`). Keeping this arm bare means
            // nested aggregates don't accidentally pick up a cast
            // they never asked for.
            //
            // `CountStar` is the only branch that emits a bare `*`
            // inside the parens and deliberately skips the recursive
            // `emit_expr(arg)` call — the `arg` slot on the typed
            // wrapper carries an inert placeholder for that variant,
            // never a real column reference.
            //
            // The `distinct` flag is checked by
            // `check_aggregate_legality` before emission — rejected
            // combinations (CountStar + DISTINCT, StringAgg + DISTINCT
            // without ORDER BY, CountStar + ORDER BY)
            // never reach this arm at fetch time; they fail earlier.
            // At the bare-emit level (unit tests, nested contexts) the
            // flag is still emitted verbatim so tests can verify the
            // flag round-trips correctly.
            // CountStar is special — emits `COUNT(*)` with no DISTINCT
            // hook (the legality check rejects DISTINCT + CountStar
            // upstream of the emitter). StringAgg is special — takes a
            // bound separator after the column expression.
            // Every other variant emits `<KEYWORD>([DISTINCT] <expr>)`
            // through `emit_unary_agg`.
            match op {
                AggOp::CountStar => acc.push_sql("COUNT(*)"),
                AggOp::StringAgg(sep) => {
                    // STRING_AGG with DISTINCT requires a per-aggregate
                    // ORDER BY, enforced by `check_aggregate_legality` at
                    // fetch time. With T1, callers chain
                    // `.order_by(...)` and the `order_by` slot below
                    // emits the well-formed Postgres syntax.
                    // `sep.clone()` is required because `sep` is
                    // `&String` here and `push_bind` takes owned values.
                    acc.push_sql("STRING_AGG(");
                    if *distinct {
                        acc.push_sql("DISTINCT ");
                    }
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(", ");
                    acc.push_bind(sep.clone());
                    push_aggregate_order_by(acc, order_by);
                    acc.push_sql(")");
                }
                AggOp::Count => emit_unary_agg(acc, "COUNT(", *distinct, arg, order_by, ctx)?,
                AggOp::Sum => emit_unary_agg(acc, "SUM(", *distinct, arg, order_by, ctx)?,
                AggOp::Avg => emit_unary_agg(acc, "AVG(", *distinct, arg, order_by, ctx)?,
                AggOp::Min => emit_unary_agg(acc, "MIN(", *distinct, arg, order_by, ctx)?,
                AggOp::Max => emit_unary_agg(acc, "MAX(", *distinct, arg, order_by, ctx)?,
                AggOp::ArrayAgg => {
                    emit_unary_agg(acc, "ARRAY_AGG(", *distinct, arg, order_by, ctx)?
                }
                // JSONB_AGG (not JSON_AGG) — Djogi standardises on JSONB
                // for all JSON wire and storage. See docs/spec/decisions.md.
                AggOp::JsonAgg => emit_unary_agg(acc, "JSONB_AGG(", *distinct, arg, order_by, ctx)?,
                // BOOL_AND / BOOL_OR accept DISTINCT (no-op for booleans
                // but valid Postgres syntax).
                AggOp::BoolAnd => emit_unary_agg(acc, "BOOL_AND(", *distinct, arg, order_by, ctx)?,
                AggOp::BoolOr => emit_unary_agg(acc, "BOOL_OR(", *distinct, arg, order_by, ctx)?,
                // EVERY is the SQL-standard alias for BOOL_AND. Both produce
                // identical results in Postgres; the IR carries the alias
                // separately so the emitter renders the keyword the user
                // wrote (call to `.every()` → `EVERY(col)`, never silently
                // rewritten to `BOOL_AND(col)`).
                AggOp::Every => emit_unary_agg(acc, "EVERY(", *distinct, arg, order_by, ctx)?,
                // Bitwise integer aggregates — Postgres returns the
                // operand's own integer type, so no narrowing cast.
                AggOp::BitAnd => emit_unary_agg(acc, "BIT_AND(", *distinct, arg, order_by, ctx)?,
                AggOp::BitOr => emit_unary_agg(acc, "BIT_OR(", *distinct, arg, order_by, ctx)?,
                AggOp::BitXor => emit_unary_agg(acc, "BIT_XOR(", *distinct, arg, order_by, ctx)?,
                // Statistics aggregates — Postgres returns NUMERIC for
                // integer inputs and DOUBLE PRECISION for float; the
                // typed surface narrows everywhere via the cast slot
                // (`::DOUBLE PRECISION`) emitted at the terminal layer.
                // STDDEV / VARIANCE are Postgres aliases for
                // STDDEV_SAMP / VAR_SAMP respectively; preserved as
                // distinct keywords so the emitter honours the caller's
                // spelling (matching the EVERY/BOOL_AND alias treatment).
                AggOp::StddevPop => {
                    emit_unary_agg(acc, "STDDEV_POP(", *distinct, arg, order_by, ctx)?
                }
                AggOp::StddevSamp => {
                    emit_unary_agg(acc, "STDDEV_SAMP(", *distinct, arg, order_by, ctx)?
                }
                AggOp::Stddev => emit_unary_agg(acc, "STDDEV(", *distinct, arg, order_by, ctx)?,
                AggOp::VarPop => emit_unary_agg(acc, "VAR_POP(", *distinct, arg, order_by, ctx)?,
                AggOp::VarSamp => emit_unary_agg(acc, "VAR_SAMP(", *distinct, arg, order_by, ctx)?,
                AggOp::Variance => emit_unary_agg(acc, "VARIANCE(", *distinct, arg, order_by, ctx)?,
                // Binary (two-arg) aggregates — `arg` carries y / key,
                // `arg2` carries x / value. Routed through the shared
                // `emit_binary_agg` helper which handles the
                // `KEYWORD(arg, arg2 [ORDER BY ...])` shape uniformly.
                // The `expect` here is a structural invariant: the
                // typed `binary_agg` constructor always populates
                // `arg2: Some(_)` for these op variants, and no other
                // construction path reaches this arm.
                AggOp::CovarPop => emit_binary_agg(
                    acc,
                    "COVAR_POP(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("CovarPop aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::CovarSamp => emit_binary_agg(
                    acc,
                    "COVAR_SAMP(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("CovarSamp aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::Corr => emit_binary_agg(
                    acc,
                    "CORR(",
                    *distinct,
                    arg,
                    arg2.as_deref().expect("Corr aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                // Regression family — every variant takes (y, x) and
                // returns DOUBLE PRECISION except REGR_COUNT (BIGINT).
                // The cast slot picks up the per-variant return type;
                // emission shape is uniform across all ten through the
                // shared `emit_binary_agg` helper.
                AggOp::RegrAvgx => emit_binary_agg(
                    acc,
                    "REGR_AVGX(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrAvgx aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrAvgy => emit_binary_agg(
                    acc,
                    "REGR_AVGY(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrAvgy aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrCount => emit_binary_agg(
                    acc,
                    "REGR_COUNT(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrCount aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrIntercept => emit_binary_agg(
                    acc,
                    "REGR_INTERCEPT(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrIntercept aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrR2 => emit_binary_agg(
                    acc,
                    "REGR_R2(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrR2 aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrSlope => emit_binary_agg(
                    acc,
                    "REGR_SLOPE(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrSlope aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrSxx => emit_binary_agg(
                    acc,
                    "REGR_SXX(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrSxx aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrSxy => emit_binary_agg(
                    acc,
                    "REGR_SXY(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrSxy aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::RegrSyy => emit_binary_agg(
                    acc,
                    "REGR_SYY(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("RegrSyy aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                // JSON-object aggregates — both binary, both build an
                // object from (key, value) pairs. JSON_OBJECT_AGG
                // returns `json`; JSONB_OBJECT_AGG returns `jsonb`.
                // Carried as separate AggOp variants so the emitter
                // honours the caller's choice (matching the
                // EVERY/BOOL_AND alias treatment from T3).
                AggOp::JsonObjectAgg => emit_binary_agg(
                    acc,
                    "JSON_OBJECT_AGG(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("JsonObjectAgg aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                AggOp::JsonbObjectAgg => emit_binary_agg(
                    acc,
                    "JSONB_OBJECT_AGG(",
                    *distinct,
                    arg,
                    arg2.as_deref()
                        .expect("JsonbObjectAgg aggregate must have arg2 set"),
                    order_by,
                    ctx,
                )?,
                // GROUPING(col) — single-column form. The variadic
                // GROUPING(c1, c2, …, cN) bitmask form routes through
                // `ExprNode::GroupingVariadic` instead (added in #94).
                // The structural shape (one arg, no separator) matches
                // every other unary aggregate, so routes through
                // `emit_unary_agg`.
                AggOp::Grouping => emit_unary_agg(acc, "GROUPING(", *distinct, arg, order_by, ctx)?,
                // Ordered-set aggregates (Cluster E T7) — emit
                // `OP(arg) WITHIN GROUP (ORDER BY target)`. The arg
                // slot carries the function-call literal (percentile
                // fraction); the target lives in within_group_order_by.
                AggOp::PercentileCont => {
                    acc.push_sql("PERCENTILE_CONT(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                AggOp::PercentileDisc => {
                    acc.push_sql("PERCENTILE_DISC(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                AggOp::Mode => {
                    // MODE() takes no function arguments — the arg slot
                    // is a sentinel placeholder that the emitter ignores
                    // on this branch (parallel to CountStar).
                    acc.push_sql("MODE() WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                // Hypothetical-set aggregates (Cluster E T8) — same
                // shape as ordered-set, with the function-call literal
                // being the hypothetical value (matching the WITHIN
                // GROUP target column's type).
                AggOp::HypotheticalRank => {
                    acc.push_sql("RANK(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                AggOp::HypotheticalDenseRank => {
                    acc.push_sql("DENSE_RANK(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                AggOp::HypotheticalPercentRank => {
                    acc.push_sql("PERCENT_RANK(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                AggOp::HypotheticalCumeDist => {
                    acc.push_sql("CUME_DIST(");
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql(") WITHIN GROUP (ORDER BY ");
                    emit_within_group_target(acc, within_group_order_by);
                    acc.push_sql(")");
                }
                // PostGIS spatial aggregates — fused two-call shape with
                // inner `::geometry` cast on the column and outer
                // `::geography` cast on the result. The DISTINCT keyword,
                // when set, lands inside `ST_Collect(...)` because that
                // is the actual aggregating step; `ST_Centroid` is a
                // post-aggregate scalar wrapper that doesn't admit
                // DISTINCT directly. Same for ORDER BY (T1) — it
                // applies to ST_Collect's input ordering, which only
                // affects the output for non-commutative outer wrappers
                // (centroid is commutative, but the IR carries the
                // clause uniformly so future PostGIS aggregates with
                // order-sensitive outer wrappers can reuse the slot).
                #[cfg(feature = "spatial")]
                AggOp::SpatialCentroid => emit_spatial_unary_agg(
                    acc,
                    "ST_Collect(",
                    "ST_Centroid(",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                // Cluster E round-5 BLOCK-2 closure: ConvexHull
                // migrated from SpatialExpr::ConvexHull to a proper
                // AggOp envelope so `.distinct()` / `.filter()` /
                // `.over()` / `.order_by()` all compose uniformly.
                // Same wrapped shape as SpatialCentroid.
                #[cfg(feature = "spatial")]
                AggOp::SpatialConvexHull => emit_spatial_unary_agg(
                    acc,
                    "ST_Collect(",
                    "ST_ConvexHull(",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialCollect => emit_spatial_unary_agg(
                    acc,
                    "ST_Collect(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                // T13 — region / bounding-box aggregates. `ST_Extent` /
                // `ST_3DExtent` return `box2d` / `box3d` respectively,
                // neither of which casts directly to `geography`. The
                // two-step cast chain `::geometry::geography` is
                // well-defined PostGIS: the first cast produces a
                // four-vertex Polygon footprint, the second moves it
                // onto the geography substrate.
                #[cfg(feature = "spatial")]
                AggOp::SpatialUnion => emit_spatial_unary_agg(
                    acc,
                    "ST_Union(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialExtent => emit_spatial_unary_agg(
                    acc,
                    "ST_Extent(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialExtent3D => emit_spatial_unary_agg(
                    acc,
                    "ST_3DExtent(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                // T14 — line / polygon aggregates. `ST_MakeLine` is
                // order-sensitive (the per-aggregate ORDER BY controls
                // the LineString's vertex sequence). `ST_Collect`
                // (used as the portable fallback for `ST_PolygonAgg`,
                // which is PostGIS 3.5+) is order-insensitive but
                // emits the order_by slot uniformly so the IR stays
                // composable across the family.
                #[cfg(feature = "spatial")]
                AggOp::SpatialMakeLine => emit_spatial_unary_agg(
                    acc,
                    "ST_MakeLine(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialLineAgg => emit_spatial_unary_agg(
                    acc,
                    "ST_LineAgg(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialPolygonAgg => emit_spatial_unary_agg(
                    // Portable fallback for ST_PolygonAgg (PostGIS 3.5+);
                    // ST_Collect produces an equivalent MultiPolygon
                    // for polygon-typed inputs. If Djogi ever raises
                    // its PostGIS floor to 3.5 the keyword swaps to
                    // "ST_PolygonAgg("; the surrounding shape stays.
                    acc,
                    "ST_Collect(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                // T15 — clustering aggregates. Both return `geometry[]`
                // at the Postgres level; the trailing `::geography[]`
                // cast moves the array's element type onto the
                // geography substrate. ST_ClusterIntersecting fits the
                // unary helper; ST_ClusterWithin's bound distance
                // argument is hand-rolled below.
                #[cfg(feature = "spatial")]
                AggOp::SpatialClusterIntersecting => emit_spatial_unary_agg(
                    acc,
                    "ST_ClusterIntersecting(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialClusterWithin(distance) => {
                    // Hand-rolled — binary signature with bound distance.
                    // The unary helper doesn't fit because the second
                    // arg is a parameter-bound f64, not a column ref or
                    // a fixed token. Codex T22 BLOCK-1: FILTER attaches
                    // to the inner ST_ClusterWithin aggregate before
                    // the outer ::geography[] cast (which is appended
                    // by the post-arm `outer_cast_suffix(op)` push).
                    let needs_filter_paren = filter.is_some();
                    if needs_filter_paren {
                        acc.push_sql("(");
                    }
                    acc.push_sql("ST_ClusterWithin(");
                    if *distinct {
                        acc.push_sql("DISTINCT ");
                    }
                    emit_expr(acc, arg, ctx)?;
                    acc.push_sql("::geometry, ");
                    acc.push_bind(*distance);
                    push_aggregate_order_by(acc, order_by);
                    acc.push_sql(")");
                    if let Some(cond) = filter.as_deref() {
                        acc.push_sql(" FILTER (WHERE ");
                        emit_expr(acc, cond, ctx)?;
                        acc.push_sql(")");
                    }
                    if needs_filter_paren {
                        acc.push_sql(")");
                    }
                    // outer ::geography[] cast appended by post-arm
                    // outer_cast_suffix(op) — matches the placement
                    // discipline shared with emit_spatial_unary_agg.
                }
                // T16 — mem_union / polygonize. Same `<col>::geometry`
                // → `geography` cast discipline.
                #[cfg(feature = "spatial")]
                AggOp::SpatialMemUnion => emit_spatial_unary_agg(
                    acc,
                    "ST_MemUnion(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
                #[cfg(feature = "spatial")]
                AggOp::SpatialPolygonize => emit_spatial_unary_agg(
                    acc,
                    "ST_Polygonize(",
                    "",
                    "",
                    *distinct,
                    arg,
                    order_by,
                    filter.as_deref(),
                    ctx,
                )?,
            }
            // Postgres `AGG(...) FILTER (WHERE <cond>)` runs the
            // filter inside the aggregate's per-row scan — the
            // aggregate ignores rows where `cond` is false. `filter`
            // is None on the bare call site; Some(cond) when
            // `AggregateExpr::filter(...)` was chained.
            //
            // Note: STRING_AGG with FILTER is valid Postgres syntax and
            // works here because the FILTER clause attaches to the whole
            // aggregate function expression after the closing paren.
            //
            // Codex T22 BLOCK-1: spatial aggregates that emit an outer
            // cast (e.g. `::geography`) must place FILTER *before* the
            // cast. The per-aggregate spatial arms above handle FILTER
            // inline via `emit_spatial_unary_agg`; this generic
            // post-arm block fires only for non-spatial aggregates
            // where the FILTER suffix attaches to the bare aggregate.
            if !op_emits_outer_cast(op)
                && let Some(cond) = filter
            {
                acc.push_sql(" FILTER (WHERE ");
                emit_expr(acc, cond, ctx)?;
                acc.push_sql(")");
            }

            // Codex T22 round-3 BLOCK-1: append the outer cast suffix
            // (e.g. `::geography`) for spatial aggregates here, AFTER
            // FILTER. The per-aggregate spatial arms above pass `""`
            // for the cast slot so the cast lives in exactly one
            // place. The windowed-emission path
            // (`emit_aggregate_inner`) pops this suffix and re-appends
            // it after OVER so the placement is
            // `(AGG(..) FILTER (...) OVER (...))::geography` — valid
            // Postgres aggregate syntax.
            if let Some(suffix) = outer_cast_suffix(op) {
                acc.push_sql(suffix);
            }
        }
        ExprNode::Case { arms, otherwise } => {
            // CASE WHEN <cond> THEN <val> ... ELSE <default> END.
            //
            // `otherwise` is required by construction (the typed builder
            // surface [`super::case::CaseBuilder::otherwise`] produces
            // the `Expr<V>` only after the caller supplies a default)
            // — no `Option` / `if let` branch here, the ELSE arm is
            // always rendered. This matches the plan's Step 3 decision
            // that a CASE with no matching arm must never silently
            // produce NULL against a column the user expected to be
            // non-null.
            acc.push_sql("CASE ");
            for (cond, val) in arms {
                acc.push_sql("WHEN ");
                emit_expr(acc, cond, ctx)?;
                acc.push_sql(" THEN ");
                emit_expr(acc, val, ctx)?;
                acc.push_sql(" ");
            }
            acc.push_sql("ELSE ");
            emit_expr(acc, otherwise, ctx)?;
            acc.push_sql(" END");
        }
        ExprNode::Exists(sub) => {
            // EXISTS (<subquery>) — subquery renders SELECT 1 because
            // EXISTS only cares about row presence. `emit_subquery`
            // special-cases the `None` select_column arm for exactly
            // this reason; the [`super::subquery::Exists`] typed
            // constructor is the sole producer of this shape and always
            // sets `select_column = None`.
            acc.push_sql("EXISTS (");
            emit_subquery(acc, sub)?;
            acc.push_sql(")");
        }
        ExprNode::Subquery(sub) => {
            // Scalar subquery — must be wrapped in parens so it slots
            // into arithmetic / comparison positions without re-parsing
            // the outer expression. `emit_subquery` handles the
            // `SELECT <col> FROM ... WHERE ...` body; the outer parens
            // here are structural.
            acc.push_sql("(");
            emit_subquery(acc, sub)?;
            acc.push_sql(")");
        }
        ExprNode::ArrayLength { column } => {
            // array_length(column, 1) — dimension is always 1 (Djogi arrays
            // are 1-dimensional; multi-dimensional arrays are not supported).
            acc.push_sql("array_length(");
            acc.push_sql(column);
            acc.push_sql(", 1)");
        }
        ExprNode::CurrentYear => {
            // EXTRACT(YEAR FROM CURRENT_DATE) returns Postgres `numeric`; the
            // typed `Expr<i32>` wrapper at the public surface promises an
            // `i32` decode, so the explicit `::INTEGER` cast narrows the
            // result here. No bind parameter — the year is read from the
            // server clock, never user-supplied.
            acc.push_sql("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER");
        }
        ExprNode::OuterRef { column } => {
            // Outer-scope column reference — emitted unqualified.
            // Postgres resolves the name against the enclosing query
            // scope when the inner `FROM` list has no matching column.
            // Same-named collisions between inner and outer scope
            // trigger `42702 column reference "X" is ambiguous`; the
            // typed surface flags this limitation on
            // [`super::subquery::OuterRef`]. The qualified form is
            // available via [`ExprNode::OuterRefColumn`] for callers
            // that need to disambiguate.
            //
            // Column strings reach this arm only via the sealed macro
            // entry point
            // [`super::subquery::__macro_support::__make_outer_ref`]
            // (macro-emitted code) or the crate-private
            // [`super::subquery::OuterRef::new`] (test / internal
            // helpers) — both run through
            // [`crate::ident::assert_plain_ident`] before the value
            // lands here. Safe to push as a raw SQL token.
            acc.push_sql(column);
        }
        ExprNode::OuterRefColumn { table, column } => {
            // Table-qualified outer-scope column reference — emits
            // `<table>.<column>` to disambiguate same-named columns
            // across the inner and outer scopes (every Djogi model
            // carries `id` / `created_at` / `updated_at`, so framework
            // columns collide on every M2M correlation).
            //
            // Both strings come from [`crate::ident::assert_plain_ident`]-
            // validated identifiers (table from `Model::table_name()`
            // declarations, column from sealed macro entry points).
            // Safe to push as raw SQL tokens.
            acc.push_sql(table);
            acc.push_sql(".");
            acc.push_sql(column);
        }
        ExprNode::IntervalLiteral { microseconds } => {
            // INTERVAL '{N} microseconds' — the full precision of
            // `time::Duration` as a Postgres interval literal. The
            // microsecond count was saturating-clamped to i64 at ExprNode
            // construction time via `expr::literal::saturating_micros`
            // (Durations outside ±292,277 years encode as i64::MAX/MIN).
            // The formatted string consists solely of a decimal integer and
            // the ASCII keyword "microseconds" — no user-controlled text
            // reaches this arm, so pushing raw SQL is safe.
            let literal = format!("INTERVAL '{microseconds} microseconds'");
            acc.push_sql(&literal);
        }

        ExprNode::TsMatch {
            column,
            dictionary,
            query_text,
        } => {
            // `<col> @@ to_tsquery('<dictionary>', $n)`. Column and dictionary
            // identifiers are validated at construction; query_text is user-
            // supplied so it always rides as a bound parameter.
            emit_ts(acc, "", column, dictionary, query_text, " @@ ", ")");
        }
        ExprNode::TsRank {
            column,
            dictionary,
            query_text,
        } => {
            // `ts_rank(<col>, to_tsquery('<dictionary>', $n))`. Standard
            // relevance score — higher = more relevant.
            emit_ts(acc, "ts_rank(", column, dictionary, query_text, ", ", "))");
        }
        ExprNode::TsRankCd {
            column,
            dictionary,
            query_text,
        } => {
            // `ts_rank_cd(...)`. Cover-density variant; weighs term proximity
            // more heavily than `ts_rank`.
            emit_ts(
                acc,
                "ts_rank_cd(",
                column,
                dictionary,
                query_text,
                ", ",
                "))",
            );
        }

        // ── pg_trgm (gated on `trgm` feature) ─────────────────────────────
        #[cfg(feature = "trgm")]
        ExprNode::TrgmSimilarTo { column, pattern } => {
            // <col> % $n
            // The `%` operator is the indexable strategy member of
            // `gin_trgm_ops` / `gist_trgm_ops`; emitting this form rather
            // than `similarity(...) >= ...` is what makes a GIN/GiST
            // trgm index actually accelerate the predicate. The
            // threshold for `%` comes from the session GUC
            // `pg_trgm.similarity_threshold` (default 0.3); per-query
            // numeric thresholds go through `TrgmSimilarityScore`
            // composed inside `filter_expr`.
            //
            // column: validated identifier; routed through
            // `ctx.push_column` so joined / self-pair callers qualify
            // the reference as `<alias>.<col>` instead of a bare name.
            // pattern: user-supplied — always a bind parameter.
            ctx.push_column(acc, column);
            acc.push_sql(" % ");
            acc.push_bind(pattern.to_owned());
        }
        #[cfg(feature = "trgm")]
        ExprNode::TrgmSimilarityScore { column, pattern } => {
            // similarity(<col>, $n)
            // Returns f64 per row. Composed via the typed `Expr<T>`
            // comparison API inside `filter_expr` for per-query numeric
            // thresholds. NOT index-accelerated by the trgm opclasses —
            // those target the operator family, not the function form.
            //
            // column: routed through `ctx.push_column` for the same
            // joined-query qualification as `TrgmSimilarTo` above.
            // Follow-up: the FTS arms (`TsMatch` / `TsRank` / `TsRankCd`)
            // have the same bare-push bug via `emit_ts`; fix is tracked
            // separately so this commit stays narrow to #147.
            acc.push_sql("similarity(");
            ctx.push_column(acc, column);
            acc.push_sql(", ");
            acc.push_bind(pattern.to_owned());
            acc.push_sql(")");
        }

        // ── Spatial (gated on `spatial` feature) ───────────────────────────
        #[cfg(feature = "spatial")]
        ExprNode::Spatial(s) => {
            // Delegate entirely to `SpatialExpr::emit`, which handles all
            // bind-parameter placement for PostGIS functions.
            s.emit(acc);
        }

        ExprNode::GroupingVariadic { args } => {
            // `GROUPING(c1, c2, …, cN)` — variadic bitmask form.
            // Args are framework-validated identifiers (every element is
            // `ExprNode::Field { column }` produced by `grouping_of`,
            // which assert_plain_ident-validates every entry); routed
            // through `emit_expr` recursively so a future select_related
            // pass picks up the parent-table qualifier via `ctx`.
            //
            // GROUPING does not accept any aggregate modifier — DISTINCT,
            // ORDER BY, FILTER, OVER all produce Postgres syntax errors —
            // so this arm renders only the bare function call. The typed
            // metadata kind-state omits those modifiers for the single-arg
            // form, and `check_aggregate_legality` debug-asserts the same
            // invariant for direct-IR construction. The variadic form has
            // no modifier slots in the IR by design.
            acc.push_sql("GROUPING(");
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    acc.push_sql(", ");
                }
                emit_expr(acc, a, ctx)?;
            }
            acc.push_sql(")");
        }

        // ── Row-shape aggregate (Phase 8.5 Cluster F #92) ──────────────────
        //
        // `ST_AsMVT(<row_alias>, $1, $2, $3, $4)` / `ST_AsGeobuf(<row_alias>, $1)`.
        // The row-alias reference is the special `__djogi_row` identifier
        // that the terminal builders splice into the surrounding FROM
        // clause as the alias of a derived-table wrapper. Every other
        // argument is pushed through `push_bind` so layer names, extents,
        // and geometry-column names cannot be SQL-injected even if a
        // future builder mistakenly accepted dynamic input.
        //
        // The `columns` slot on the IR variant is informational at
        // v0.1.0 (PostGIS resolves column references from the runtime
        // record type, not from the SQL emitted here). Future row
        // aggregates that need column projection slot in here without
        // an IR rev.
        #[cfg(feature = "spatial")]
        ExprNode::RowAggregate { op, columns: _ } => {
            emit_row_aggregate(acc, op);
        }
    }
    Ok(())
}

/// Emit a row-shape aggregate function call. The function reads the
/// row argument from the framework-fixed `__djogi_row` alias the
/// terminal builder splices into the wrapping `FROM (...) AS __djogi_row`
/// clause; every other argument is bound through the accumulator.
///
/// Spatial-only — the row-aggregate IR variant itself is gated on
/// `feature = "spatial"`, so reaching this helper without the feature
/// is structurally impossible.
#[cfg(feature = "spatial")]
fn emit_row_aggregate(acc: &mut SqlAccumulator, op: &crate::expr::node::RowAggOp) {
    use crate::expr::node::RowAggOp;
    match op {
        RowAggOp::AsMvt {
            layer_name,
            extent,
            geom_name,
            feature_id_name,
        } => {
            // ST_AsMVT(row record, layer_name text, extent integer,
            //          geom_name text, feature_id_name text)
            //
            // The PostGIS signature is variadic at the SQL level; the
            // typed emitter always passes layer_name + extent + geom_name
            // (so the emission stays explicit across PostGIS versions
            // that might rev defaults) and skips the trailing feature_id
            // argument when the typed surface left it `None` so PostGIS
            // falls through to its `NULL` default.
            acc.push_sql("ST_AsMVT(__djogi_row, ");
            acc.push_bind(layer_name.clone());
            acc.push_sql(", ");
            acc.push_bind(*extent);
            acc.push_sql(", ");
            acc.push_bind(geom_name.clone());
            if let Some(fid) = feature_id_name {
                acc.push_sql(", ");
                acc.push_bind(fid.clone());
            }
            acc.push_sql(")");
        }
        RowAggOp::AsGeobuf { geom_name } => {
            // ST_AsGeobuf(row anyelement, geom_name text)
            //
            // PostGIS's Geobuf surface takes just the geometry column
            // name — no extent / no feature id slot. Both arguments are
            // emitted explicitly so the call shape stays stable.
            acc.push_sql("ST_AsGeobuf(__djogi_row, ");
            acc.push_bind(geom_name.clone());
            acc.push_sql(")");
        }
    }
}

/// Emit a Postgres FTS expression — `<prefix><col><sep>to_tsquery('<dictionary>', $n)<suffix>`.
///
/// Three [`ExprNode`] variants share this shape:
///
/// - `TsMatch` — `<col> @@ to_tsquery('<dict>', $n)` (`prefix=""`,
///   `sep=" @@ "`, `suffix=")"`).
/// - `TsRank` — `ts_rank(<col>, to_tsquery('<dict>', $n))` (`prefix="ts_rank("`,
///   `sep=", "`, `suffix="))"`).
/// - `TsRankCd` — same shape with `ts_rank_cd(`.
///
/// `column` is a `&'static str` macro-validated via `assert_plain_ident`.
/// `dictionary` is byte-level validated at attribute parse time; embedded
/// literally as a single-quoted string. `query_text` is user-supplied
/// and always rides through `push_bind`.
fn emit_ts(
    acc: &mut SqlAccumulator,
    prefix: &'static str,
    column: &str,
    dictionary: &str,
    query_text: &str,
    sep: &'static str,
    suffix: &'static str,
) {
    acc.push_sql(prefix);
    acc.push_sql(column);
    acc.push_sql(sep);
    acc.push_sql("to_tsquery('");
    acc.push_sql(dictionary);
    acc.push_sql("', ");
    acc.push_bind(query_text.to_owned());
    acc.push_sql(suffix);
}

/// Emit `<KEYWORD_OPENER>[DISTINCT ]<expr>)`.
///
/// `keyword_opener` is the SQL function name plus opening paren — e.g.
/// `"SUM("`, `"ARRAY_AGG("`. Centralises the eight unary aggregate emit
/// arms (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `ARRAY_AGG`, `JSONB_AGG`,
/// `BOOL_AND`, `BOOL_OR`) so the `[DISTINCT]` placement and parens stay
/// uniform. `STRING_AGG` and `COUNT(*)` are special-cased upstream.
fn emit_unary_agg(
    acc: &mut SqlAccumulator,
    keyword_opener: &'static str,
    distinct: bool,
    arg: &ExprNode,
    order_by: &[crate::query::order::OrderExpr],
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    acc.push_sql(keyword_opener);
    if distinct {
        acc.push_sql("DISTINCT ");
    }
    emit_expr(acc, arg, ctx)?;
    push_aggregate_order_by(acc, order_by);
    acc.push_sql(")");
    Ok(())
}

/// Emit `<KEYWORD_OPENER>[DISTINCT ]<arg>, <arg2>[ ORDER BY ...])`.
///
/// `keyword_opener` is the SQL function name plus opening paren — e.g.
/// `"COVAR_POP("`, `"REGR_SLOPE("`, `"JSONB_OBJECT_AGG("`. Centralises
/// every binary aggregate emit arm so the comma-separator placement
/// and per-aggregate ORDER BY tail stay uniform.
///
/// # Argument convention
///
/// For the stats / regression family, `arg` is `y` (dependent variable)
/// and `arg2` is `x` (independent variable). For JSON-object aggregates,
/// `arg` is the key and `arg2` is the value. The Postgres function
/// signatures all share the `KEYWORD(first, second)` shape, so the
/// helper does not need to distinguish.
fn emit_binary_agg(
    acc: &mut SqlAccumulator,
    keyword_opener: &'static str,
    distinct: bool,
    arg: &ExprNode,
    arg2: &ExprNode,
    order_by: &[crate::query::order::OrderExpr],
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    acc.push_sql(keyword_opener);
    if distinct {
        acc.push_sql("DISTINCT ");
    }
    emit_expr(acc, arg, ctx)?;
    acc.push_sql(", ");
    emit_expr(acc, arg2, ctx)?;
    push_aggregate_order_by(acc, order_by);
    acc.push_sql(")");
    Ok(())
}

/// Emit a per-aggregate `ORDER BY <ord1>, <ord2>, ...` tail when
/// `order_by` is non-empty. Renders inside the aggregate's parens —
/// the caller pushes the closing paren after this returns.
///
/// Aggregates do not participate in the join / parent-table-qualifier
/// flow that `QuerySet`-level ordering uses, so the qualifier slot is
/// always `None` here. Each `OrderExpr::emit` call writes its column
/// reference without table qualification.
fn push_aggregate_order_by(acc: &mut SqlAccumulator, order_by: &[crate::query::order::OrderExpr]) {
    if order_by.is_empty() {
        return;
    }
    acc.push_sql(" ORDER BY ");
    for (i, o) in order_by.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        o.emit(acc, None);
    }
}

/// Emit the `<ord1>, <ord2>, ...` body of a `WITHIN GROUP (ORDER BY ...)`
/// clause for ordered-set / hypothetical-set aggregates. Cluster E T7
/// added this for `PERCENTILE_CONT` / `PERCENTILE_DISC` / `MODE`; T8
/// hypothetical-set aggregates reuse it.
///
/// The caller writes `WITHIN GROUP (ORDER BY ` and the closing `)`
/// around this helper's output, identical to how
/// [`push_aggregate_order_by`] is sandwiched inside the aggregate
/// parens. The typed `AggregateExpr::ordered_set` constructor populates this
/// list before emission for ordered-set / hypothetical-set aggregates; debug
/// builds also assert that direct-IR construction did not bypass that
/// invariant.
fn emit_within_group_target(acc: &mut SqlAccumulator, targets: &[crate::query::order::OrderExpr]) {
    for (i, t) in targets.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        t.emit(acc, None);
    }
}

/// Emit the canonical PostGIS-aggregate emission shape with correct
/// FILTER placement. `FILTER (WHERE ...)` attaches to the *inner*
/// aggregate (`ST_Collect` / `ST_Union` / `ST_Extent` / etc.) before
/// any outer wrapper (`ST_Centroid`) or outer cast (`::geography`),
/// matching Postgres syntax for aggregate-with-FILTER expressions.
///
/// Codex T22 BLOCK-1: the previous shape emitted `ST_Collect(arg)::geography
/// FILTER (WHERE ...)` which is invalid Postgres. Postgres aggregate
/// syntax requires `(<aggregate-call> FILTER (WHERE <cond>))::cast`
/// or `<wrapper>(<aggregate-call> FILTER (WHERE <cond>))::cast` for
/// double-wrap shapes like `ST_Centroid(ST_Collect(...))`.
///
/// # Parameters
///
/// - `inner_keyword` — the actual aggregate function call's opening,
///   e.g. `"ST_Collect("` / `"ST_Union("` / `"ST_Extent("`. The FILTER
///   clause attaches to this call's close paren, before any outer
///   wrapper.
/// - `outer_wrap_open` — empty for single-wrap aggregates;
///   `"ST_Centroid("` for the double-wrap centroid case where
///   `ST_Centroid` is a scalar post-aggregate wrapper around
///   `ST_Collect`. The wrapper's close paren is emitted after FILTER.
/// - `outer_close_and_cast` — the cast suffix without leading close
///   paren, e.g. `"::geography"` / `"::geometry::geography"` /
///   `"::geography[]"`. The helper inserts the necessary close
///   parens (for outer_wrap or for the FILTER-paren) before this
///   suffix.
/// - `filter` — FILTER (WHERE ...) clause from the aggregate's typed
///   surface. Emitted between the inner-aggregate close paren and the
///   outer-wrapper close paren (or before the outer cast for unary
///   aggregates).
///
/// # Emission shapes
///
/// - **Unary, no filter:** `ST_Union(arg::geometry)::geography`
/// - **Unary, with filter:**
///   `(ST_Union(arg::geometry) FILTER (WHERE ...))::geography`
/// - **Double-wrap (Centroid), no filter:**
///   `ST_Centroid(ST_Collect(arg::geometry))::geography`
/// - **Double-wrap with filter:**
///   `ST_Centroid(ST_Collect(arg::geometry) FILTER (WHERE ...))`
///
/// The outer FILTER block in the `emit_expr` Aggregate arm is gated on
/// `op_emits_outer_cast(op)` so spatial aggregates handle FILTER inline
/// here; the post-arm block fires only for non-cast-wrapping aggregates
/// (the standard SUM/COUNT/etc. family that lives at the bare-emission
/// boundary).
///
/// Codex T22 round-3 BLOCK-1: `outer_close_and_cast` is now always
/// `""` — the outer `::geography` cast is appended by the post-arm
/// `outer_cast_suffix(op)` push so the windowed-emission path can
/// splice OVER between the bare aggregate body and the cast. The
/// parameter is retained as an empty placeholder for caller clarity
/// (the call sites now read as a uniform `"", "", ""` triple of
/// inner / outer-wrap / cast slots).
#[cfg(feature = "spatial")]
#[allow(clippy::too_many_arguments)]
fn emit_spatial_unary_agg(
    acc: &mut SqlAccumulator,
    inner_keyword: &'static str,
    outer_wrap_open: &'static str,
    outer_close_and_cast: &'static str,
    distinct: bool,
    arg: &ExprNode,
    order_by: &[crate::query::order::OrderExpr],
    filter: Option<&ExprNode>,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    let has_outer_wrap = !outer_wrap_open.is_empty();
    // Parens needed for FILTER attachment ONLY when there's no outer
    // wrapper (which already provides the binding context). Centroid's
    // ST_Centroid(...) wrapper makes the outer parens redundant.
    let needs_filter_paren = filter.is_some() && !has_outer_wrap;

    if needs_filter_paren {
        acc.push_sql("(");
    }
    acc.push_sql(outer_wrap_open);
    acc.push_sql(inner_keyword);
    if distinct {
        acc.push_sql("DISTINCT ");
    }
    emit_expr(acc, arg, ctx)?;
    acc.push_sql("::geometry");
    push_aggregate_order_by(acc, order_by);
    acc.push_sql(")"); // close inner aggregate (ST_Collect / ST_Union / etc.)

    if let Some(cond) = filter {
        acc.push_sql(" FILTER (WHERE ");
        emit_expr(acc, cond, ctx)?;
        acc.push_sql(")");
    }

    if has_outer_wrap {
        acc.push_sql(")"); // close outer wrap (e.g., ST_Centroid)
    }
    if needs_filter_paren {
        acc.push_sql(")"); // close the parens around aggregate-with-FILTER
    }

    acc.push_sql(outer_close_and_cast);
    Ok(())
}

/// Outer cast suffix string for aggregates whose result requires a
/// scalar `::TYPE` cast appended after the aggregate-call body — and
/// after any FILTER / OVER modifier. Returns `None` for aggregates
/// whose return type already decodes directly (no cast needed) or
/// whose cast lives at the terminal layer via `cast_to`.
///
/// Spatial aggregates always cast their result onto the `geography`
/// substrate so the typed decode tunnel matches the
/// [`crate::geo::GeoPoint`] family. The cast must attach AFTER the
/// FILTER and OVER modifiers — Postgres's aggregate-call grammar
/// places `FILTER` and `OVER` on the bare aggregate expression before
/// any post-call scalar wrapper (per
/// <https://www.postgresql.org/docs/current/sql-expressions.html#SYNTAX-AGGREGATES>).
/// `emit_aggregate_inner` (in `query/sql.rs`) splices OVER between
/// the bare body and this suffix; the bare emitter emits both
/// adjacently for the no-window case.
///
/// Future aggregates with outer scalar casts must opt in here. Adding
/// a variant without listing it makes the cast attach in the wrong
/// position relative to FILTER / OVER.
#[cfg(feature = "spatial")]
pub(crate) fn outer_cast_suffix(op: &AggOp) -> Option<&'static str> {
    match op {
        AggOp::SpatialCentroid
        | AggOp::SpatialConvexHull
        | AggOp::SpatialCollect
        | AggOp::SpatialUnion
        | AggOp::SpatialMakeLine
        | AggOp::SpatialLineAgg
        | AggOp::SpatialPolygonAgg
        | AggOp::SpatialMemUnion
        | AggOp::SpatialPolygonize => Some("::geography"),
        // box2d / box3d → geometry → geography (two-step cast).
        AggOp::SpatialExtent | AggOp::SpatialExtent3D => Some("::geometry::geography"),
        // ST_ClusterIntersecting / ST_ClusterWithin return `geometry[]`.
        AggOp::SpatialClusterIntersecting | AggOp::SpatialClusterWithin(_) => Some("::geography[]"),
        _ => None,
    }
}

#[cfg(not(feature = "spatial"))]
pub(crate) fn outer_cast_suffix(_op: &AggOp) -> Option<&'static str> {
    None
}

/// Returns true when [`outer_cast_suffix`] is `Some` for this op.
/// Equivalent to `outer_cast_suffix(op).is_some()`; kept as a named
/// predicate because the post-arm FILTER block reads more naturally
/// with a boolean.
fn op_emits_outer_cast(op: &AggOp) -> bool {
    outer_cast_suffix(op).is_some()
}

/// Emit a [`SubqueryNode`] body — `SELECT <col or 1> FROM <table>
/// [WHERE <condition>]`.
///
/// Shared by both [`ExprNode::Exists`] and [`ExprNode::Subquery`] — they
/// differ only in (a) whether the node carries a `select_column` and
/// (b) whether the outer arm wraps the result in `EXISTS (..)` / `(..)`.
/// `emit_subquery` itself renders the common body; the outer wrap lives
/// at the arm level.
///
/// # Why `emit_condition` and not a second [`emit_expr`] walk?
///
/// The subquery's `WHERE` clause is a [`Condition`] tree (not an
/// [`ExprNode`]) because it was built through
/// [`crate::query::QuerySet::filter`] / [`crate::query::QuerySet::filter_expr`]
/// — those accumulate `Condition` with a full `LookupOp` vocabulary
/// (ILIKE, BETWEEN, IS NULL, IN list, …). Reusing
/// [`crate::query::sql::emit_condition`] lets every lookup op compose
/// inside a subquery without a parallel `ExprNode`-side emitter, which
/// would duplicate every `LookupOp` arm and drift over time. The tradeoff
/// is that [`super::subquery::Exists::new`] / [`super::subquery::Subquery::new`]
/// clone the inner queryset's condition tree at construction time; that
/// clone is cheap (the tree is shallow Vec<_> + enum variants) and the
/// correlated-subquery build sites are a hot-path outlier, not the norm.
fn emit_subquery(
    acc: &mut SqlAccumulator,
    node: &SubqueryNode,
) -> Result<(), PortablePredicateError> {
    acc.push_sql("SELECT ");
    match &node.select_column {
        Some(col) => {
            // `col` is a `&'static str` from
            // [`crate::query::field::FieldRef::column`], validated at
            // `FieldRef::new` construction time. Safe to push raw.
            acc.push_sql(col);
        }
        None => {
            // EXISTS path — the constant 1 stands in for "some value"
            // because EXISTS ignores column values entirely. Using the
            // literal `1` rather than `*` avoids an unnecessary SELECT-
            // list expansion on the planner side and matches the
            // idiomatic Postgres form for EXISTS subqueries.
            acc.push_sql("1");
        }
    }
    acc.push_sql(" FROM ");
    // Table name is always `<T as Model>::table_name()` from the typed
    // surface — macro-baked, never user input.
    acc.push_sql(node.table);
    if let Some(predicate) = &node.where_clause {
        acc.push_sql(" WHERE ");
        // Phase 8eta PR2b — `where_clause` now stores a typed-erased
        // [`crate::expr::node::ErasedSubqueryPredicate`] handle so
        // expression subqueries carry full `Q<T>` predicates without
        // round-tripping through `q_to_condition`. The handle's
        // `emit` method drives `query::sql::emit_q::<T>(...)` under
        // `SqlEmitContext::root()` (the subquery's own table is the
        // primary `FROM` source — qualified emission stays out of
        // scope here, matching the pre-PR2b `parent_table = None`
        // contract).
        predicate.emit(acc)?;
    }
    Ok(())
}

/// Emit an arithmetic binary node with parens around any arithmetic
/// sub-expression.
///
/// SQL precedence binds `*` / `/` tighter than `+` / `-`, so a
/// Rust-built tree like `Mul(Add(a, b), c)` would silently re-parse as
/// `a + (b * c)` if we emitted `a + b * c`. Wrapping every arithmetic
/// sub-expression in explicit parens preserves the structural grouping
/// the user wrote. The outer arm still picks up its own operator
/// between the two wrapped sides.
///
/// Non-arithmetic operands (field refs, literals, comparisons,
/// aggregates) don't need wrapping — they're already single tokens or
/// already self-parenthesised — so the wrap is gated on the sub-node's
/// discriminant.
fn emit_arith(
    acc: &mut SqlAccumulator,
    lhs: &ExprNode,
    op: &'static str,
    rhs: &ExprNode,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    emit_wrapped_if_arith(acc, lhs, ctx)?;
    acc.push_sql(op);
    emit_wrapped_if_arith(acc, rhs, ctx)?;
    Ok(())
}

fn emit_wrapped_if_arith(
    acc: &mut SqlAccumulator,
    node: &ExprNode,
    ctx: SqlEmitContext,
) -> Result<(), PortablePredicateError> {
    match node {
        ExprNode::Add(..) | ExprNode::Sub(..) | ExprNode::Mul(..) | ExprNode::Div(..) => {
            acc.push_sql("(");
            emit_expr(acc, node, ctx)?;
            acc.push_sql(")");
        }
        _ => emit_expr(acc, node, ctx)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Emitter unit tests — assert the generated SQL text for each
    //! `ExprNode` variant combination the public API can produce.
    //! `SqlAccumulator::sql()` exposes the text with bind placeholders as
    //! `$1`, `$2`, … so we can count the bind slots without actually
    //! running the query.
    //!
    //! The tests reach `FieldRef::new` via its `pub(crate)` constructor
    //! — expr lives in the same crate, so direct construction is fine.
    //! Column strings (`"view_count"`, `"author_id"`, etc.) satisfy
    //! `assert_plain_ident` so the seal on `__make_field_ref` is not
    //! exercised here; it has its own unit coverage in
    //! `query::field::__macro_support::tests`.
    use super::*;
    use crate::Expr;
    use crate::descriptor::ModelDescriptor;
    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::field::FieldRef;

    // Inert local model — only `table_name` and the trait bounds
    // matter for unit tests that never run SQL. Mirrors the `Fake`
    // stub in `query::field::tests` and `query::sql::tests`.
    struct Post;
    impl crate::model::__sealed::Sealed for Post {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Post {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "posts"
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
    fn emit_field_vs_literal_eq() {
        // `view_count.as_expr().eq(Expr::literal(100))` must emit
        // `view_count = $1` — one bind slot for the literal, bare
        // column reference for the field side.
        let f: FieldRef<Post, i32> = FieldRef::new("view_count");
        let expr = f.as_expr().eq(Expr::literal(100i32));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert!(sql.contains("view_count = $1"), "got: {sql}");
    }

    #[test]
    fn emit_field_vs_field_eq() {
        // `author_id.as_expr().eq(editor_id.as_expr())` must emit
        // `author_id = editor_id` with zero bind slots — both sides
        // are column references.
        let a: FieldRef<Post, i64> = FieldRef::new("author_id");
        let b: FieldRef<Post, i64> = FieldRef::new("editor_id");
        let expr = a.as_expr().eq(b.as_expr());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(sql.trim(), "author_id = editor_id");
        assert!(!sql.contains('$'), "no binds expected, got: {sql}");
    }

    #[test]
    fn emit_arithmetic_add_literal() {
        // `view_count.as_expr() + Expr::literal(1)` must emit
        // `view_count + $1` — one bind slot for the literal RHS,
        // bare `+` operator token between the two operands.
        let f: FieldRef<Post, i32> = FieldRef::new("view_count");
        let expr = f.as_expr() + Expr::literal(1i32);
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert!(sql.contains("view_count + $1"), "got: {sql}");
    }

    #[test]
    fn emit_arithmetic_mixed_precedence_wraps_inner_ops() {
        // Rust-level `(a + b) * c` builds `Mul(Add(a, b), c)`. Without
        // grouping, the emitter would produce `a + b * c` which SQL
        // binds as `a + (b * c)` — the opposite of what the user wrote.
        // Every arithmetic sub-expression must be wrapped in explicit
        // parens to preserve the structural grouping.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = (a.as_expr() + b.as_expr()) * c.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(sql.trim(), "(a + b) * c", "got: {sql}");
    }

    // ── Phase 8β T4.2 — __raw_sql_fragment escape hatch ────────────────────

    /// `Expr::<f64>::__raw_sql_fragment("base_price * (1.0 + tax_rate)")`
    /// emits the fragment verbatim, wrapped in outer parens so any
    /// further composition (`.eq(...)`, arithmetic, aggregate) preserves
    /// operator precedence.
    #[test]
    fn raw_sql_fragment_emits_verbatim_with_outer_parens() {
        let expr: Expr<f64> = Expr::__raw_sql_fragment("base_price * (1.0 + tax_rate)");
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(sql.trim(), "(base_price * (1.0 + tax_rate))");
    }

    /// `__raw_sql_fragment(...).gte(literal(100))` composes through the
    /// Cmp emitter — the fragment side keeps its outer parens, the
    /// literal side binds as `$1`, and the comparison operator
    /// renders between them.
    #[test]
    fn raw_sql_fragment_composes_with_compare() {
        let expr = Expr::<f64>::__raw_sql_fragment("base_price * (1.0 + tax_rate)")
            .gte(Expr::literal(100.0_f64));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert!(
            sql.contains("(base_price * (1.0 + tax_rate)) >= $1"),
            "got: {sql}",
        );
    }

    #[test]
    fn emit_arithmetic_mixed_precedence_wraps_rhs_ops() {
        // `a + (b - c)` builds `Add(a, Sub(b, c))`. Rust already picked
        // the grouping; the emitter must echo it. Without the rhs
        // wrap the SQL would be `a + b - c` which re-parses left-
        // associative as `(a + b) - c` — wrong.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = a.as_expr() + (b.as_expr() - c.as_expr());
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(sql.trim(), "a + (b - c)", "got: {sql}");
    }

    #[test]
    fn emit_arithmetic_flat_left_associative_no_spurious_parens() {
        // `a + b + c` builds `Add(Add(a, b), c)` via left-to-right
        // evaluation of the `+` operator. The inner `Add(a, b)` IS an
        // arithmetic sub-expression, so the emitter wraps it. That's
        // structurally accurate — `(a + b) + c` and `a + b + c` are
        // semantically identical, and the explicit grouping hurts
        // nothing. The test pins the shipped behaviour so a future
        // optimisation that elides parens for associative peers
        // (same-op chains) has an explicit decision point.
        let a: FieldRef<Post, i32> = FieldRef::new("a");
        let b: FieldRef<Post, i32> = FieldRef::new("b");
        let c: FieldRef<Post, i32> = FieldRef::new("c");
        let expr = a.as_expr() + b.as_expr() + c.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(sql.trim(), "(a + b) + c", "got: {sql}");
    }

    // ── T20: Expr::current_year() — Cluster C C2 ──────────────────────────────

    /// `Expr::current_year()` emits the bare
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER` token stream with no
    /// bind parameters — the year is read from the server clock, never
    /// supplied by the caller.
    #[test]
    fn emit_current_year_no_binds() {
        let expr = Expr::current_year();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &expr.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert_eq!(
            sql.trim(),
            "EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER",
            "got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            0,
            "current_year takes no user input — must bind zero params; got {}",
            acc.bind_count()
        );
    }

    /// `Expr::current_year() - field.as_expr()` — the canonical age
    /// expression — must lower to
    /// `EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER - <col>` via the existing
    /// `Expr<i32>` arithmetic IR. The arithmetic emitter wraps each side
    /// in parens because `CurrentYear` is a non-trivial expression token
    /// (it contains its own `::` cast operator), so the canonical SQL
    /// shape includes the structural parens that pin the operator
    /// precedence.
    #[test]
    fn emit_current_year_minus_field_age_expression() {
        let f: FieldRef<Post, i32> = FieldRef::new("estimated_birth_year");
        let age = Expr::current_year() - f.as_expr();
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &age.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        // The token stream must reference both halves and the subtraction
        // operator. Existing arithmetic emitter wraps each side in parens
        // when it is itself an expression token; the bare column on the
        // RHS does not get wrapped, but the LHS's `EXTRACT(...)::INTEGER`
        // form contains an arithmetic-adjacent `::` cast so the emitter's
        // structural-parens contract makes the layout deterministic.
        assert!(
            sql.contains("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER"),
            "expected current_year token, got: {sql}"
        );
        assert!(
            sql.contains(" - estimated_birth_year"),
            "expected subtraction with column on RHS, got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            0,
            "neither side binds a parameter; got {}",
            acc.bind_count()
        );
    }

    /// The age expression composes with comparisons — `(year - col).gte(15)`
    /// must yield a bound RHS literal alongside the year/column tokens.
    #[test]
    fn emit_current_year_age_with_gte_threshold() {
        let f: FieldRef<Post, i32> = FieldRef::new("estimated_birth_year");
        let predicate = (Expr::current_year() - f.as_expr()).gte(Expr::literal(15i32));
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &predicate.node, SqlEmitContext::root())
            .expect("expression should lower to SQL");
        let sql = acc.sql();
        assert!(
            sql.contains("EXTRACT(YEAR FROM CURRENT_DATE)::INTEGER"),
            "got: {sql}"
        );
        assert!(sql.contains("estimated_birth_year"), "got: {sql}");
        assert!(sql.contains(" >= $1"), "got: {sql}");
        assert_eq!(
            acc.bind_count(),
            1,
            "exactly one bind for the threshold; got {}",
            acc.bind_count()
        );
    }

    // ── Phase 8.5 #147 — trgm joined-context column qualification ─────────
    //
    // Regression guard for BLOCK-2: both trgm emitter arms used bare
    // `acc.push_sql(column)` before this fix, which silently dropped the
    // table alias in joined / self-pair query contexts. These tests emit
    // each variant under `SqlEmitContext::joined("u")` and assert the
    // column appears as `u.<col>` — the form that prevents Postgres
    // "column reference is ambiguous" errors when the same column name
    // exists on both sides of a JOIN.

    /// `TrgmSimilarTo` under a joined context must emit `<alias>.<col> % $1`.
    ///
    /// Without the `ctx.push_column` fix the column would be bare, producing
    /// an ambiguous column reference when used inside a pair-tuple or
    /// select_related joined query.
    #[cfg(feature = "trgm")]
    #[test]
    fn trgm_similar_to_joined_context_qualifies_column() {
        use crate::expr::node::ExprNode;
        let node = ExprNode::TrgmSimilarTo {
            column: "bio",
            pattern: "rust".to_owned(),
        };
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &node, SqlEmitContext::joined("u"))
            .expect("TrgmSimilarTo emission must succeed");
        let sql = acc.sql();
        assert!(
            sql.contains("u.bio % $1"),
            "joined context must qualify column as `u.bio`; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "exactly one bind (pattern); got {}",
            acc.bind_count()
        );
    }

    /// `TrgmSimilarityScore` under a joined context must emit
    /// `similarity(<alias>.<col>, $1)`.
    ///
    /// Without the `ctx.push_column` fix the column inside `similarity()`
    /// would be bare, ambiguous in joined contexts.
    #[cfg(feature = "trgm")]
    #[test]
    fn trgm_similarity_score_joined_context_qualifies_column() {
        use crate::expr::node::ExprNode;
        let node = ExprNode::TrgmSimilarityScore {
            column: "bio",
            pattern: "rust".to_owned(),
        };
        let mut acc = SqlAccumulator::new("");
        emit_expr(&mut acc, &node, SqlEmitContext::joined("u"))
            .expect("TrgmSimilarityScore emission must succeed");
        let sql = acc.sql();
        assert!(
            sql.contains("similarity(u.bio, $1)"),
            "joined context must qualify column as `u.bio`; got: {sql}"
        );
        assert_eq!(
            acc.bind_count(),
            1,
            "exactly one bind (pattern); got {}",
            acc.bind_count()
        );
    }
}
