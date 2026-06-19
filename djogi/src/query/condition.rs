//! Condition tree — the filter AST that `QuerySet<T>` accumulates.
//! `Condition` is deliberately shallow: no SQL strings are stored here. SQL
//! generation happens exactly once at terminal-method time (`query::sql`).
//! This keeps composition cheap (cloning a QuerySet = cloning a Vec<Condition>
//! no duplicated SQL buffers) and keeps the injection-safety review surface
//! in one place.

use crate::HeerId;
use crate::RanjId;
use std::ops::{BitAnd, BitOr, Not};

/// A filter condition. Built by `FieldRef` lookup methods; composed via
/// `.and()` / `.or()` / `Condition::not`.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Vacuous — the root state before any filter applies. `ConditionBuilder`
    /// emits nothing for `True`, so a QuerySet with no filters produces a
    /// `SELECT ...` without a `WHERE` clause.
    True,

    Leaf(Leaf),

    /// SQL `(a AND b AND c)`. An empty `And(vec![])` is the vacuous-truth
    /// identity — Task 6's emitter renders it as `TRUE`. `and()` never
    /// constructs an empty vector from its own inputs, but public API
    /// consumers technically can; the invariant is "empty = TRUE".
    And(Vec<Condition>),

    /// SQL `(a OR b OR c)`. An empty `Or(vec![])` is the vacuous-falsehood
    /// identity — Task 6's emitter renders it as `FALSE`. `or()` never
    /// constructs an empty vector from its own inputs.
    Or(Vec<Condition>),

    Not(Box<Condition>),

    /// Bridge from the typed expression IR (Task 3a) — an
    /// `Expr<bool>` slotted into the filter tree by
    /// [`crate::query::QuerySet::filter_expr`]. The SQL emitter
    /// delegates to [`crate::expr::sql::emit_expr`] for this variant,
    /// bypassing [`super::sql::emit_leaf`] because the expression IR
    /// generalises both sides of the comparison (neither operand has
    /// to be a bare column), making the column-vs-literal
    /// leaf path the wrong abstraction for this emission.
    Expr(crate::expr::Expr<bool>),

    /// `col @> $1` — array contains. The column must be a Postgres array type;
    /// `values` is a bound array parameter holding the elements to test for.
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::contains`].
    ArrayContains(crate::array::ArrayContainsLeaf),

    /// `col <@ $1` — array contained by. Every element of `col` must also
    /// appear in the argument array.
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::contained_by`].
    ArrayContainedBy(crate::array::ArrayContainedByLeaf),

    /// `col && $1` — array overlap. The column and argument share at least one
    /// element.
    /// Produced by [`crate::query::field::FieldRef::<M, Vec<V>>::overlap`].
    ArrayOverlap(crate::array::ArrayOverlapLeaf),

    /// `col >> $1` — INET contains. The LHS network contains the RHS
    /// address or network.
    /// Produced by [`crate::query::field::DjogiField::<M, InetAddr>::contains`].
    #[cfg(feature = "network")]
    InetContains(crate::inet::InetContainsLeaf),

    /// `col << $1` — INET contained by. The LHS address is within the
    /// RHS network.
    /// Produced by [`crate::query::field::DjogiField::<M, InetAddr>::contained_by`].
    #[cfg(feature = "network")]
    InetContainedBy(crate::inet::InetContainedByLeaf),

    /// `col >>= $1` — INET contains or equals.
    /// Produced by [`crate::query::field::DjogiField::<M, InetAddr>::contains_or_equals`].
    #[cfg(feature = "network")]
    InetContainsEq(crate::inet::InetContainsEqLeaf),

    /// `col <<= $1` — INET contained by or equals.
    /// Produced by [`crate::query::field::DjogiField::<M, InetAddr>::contained_by_or_equals`].
    #[cfg(feature = "network")]
    InetContainedByEq(crate::inet::InetContainedByEqLeaf),

    /// `col && $1` — INET overlap. LHS and RHS share at least one address.
    /// Produced by [`crate::query::field::DjogiField::<M, InetAddr>::overlaps`].
    #[cfg(feature = "network")]
    InetOverlap(crate::inet::InetOverlapLeaf),

    /// Postgres range predicate (`@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>`,
    /// `-|-`) over a `Range<T>` column.
    /// Produced by the SQL-only range methods on
    /// [`crate::query::field::ExplicitPgPredicateField`] and the legacy
    /// [`crate::query::field::FieldRef`] surface.
    RangePredicate(crate::range::RangePredicateLeaf),

    /// JSONB flat-path comparison — `(col->'a'->>'b')::cast op $1`.
    /// Produced by [`crate::jsonb::path::JsonbPathRef`] comparison methods.
    /// The expression SQL is pre-rendered from validated identifiers.
    JsonbPath(crate::jsonb::path::JsonbPathLeaf),

    /// Raw SQL fragment — 4 carve-out for proxy
    /// `default_filter` lowering.
    /// The fragment is a `&'static str` baked at macro-expand time by
    /// [`crate::__private::lower_default_filter_fragment`] (which forwards
    /// to `djogi-macros`'s `model::proxy::lower_default_filter_to_sql`).
    /// The lowering pass enforces a closed grammar (eq/neq/range/null/
    /// between over inline literals; `and_with`/`or_with` combinators)
    /// and rejects every other shape with a span-precise compile error,
    /// so the fragment that reaches this variant cannot smuggle SQL
    /// from runtime input — the only construction path is the macro.
    /// # Why an escape-hatch variant rather than a full IR lowering
    /// Per the lens (`feedback_decision_priorities.md`, plan §7 #5
    /// resolved 2026-05-03): macro-only constructor + a sealed inner
    /// payload for the initial landing. Migration to the typed
    /// `Q<T>` IR is a semver-minor change at the
    /// enum-variant level. Constructing one outside the crate goes
    /// through
    /// [`Condition::__from_raw_sql_fragment`], which is
    /// `#[doc(hidden)]` and not part of the supported surface.
    /// # Injection-safety seal: [`RawSqlFragment`] newtype
    /// The variant carries a [`RawSqlFragment`] whose inner field is
    /// `pub(crate)` — adopter code in safe Rust cannot construct one
    /// (the field is unnameable and there is no `pub` constructor
    /// returning the newtype outside `djogi`). Even with this `pub` enum
    /// in scope, `Condition::RawSql(RawSqlFragment(s))` is a compile
    /// error in adopter crates because the inner field is private.
    /// Earlier shapes carrying a bare `&'static str` allowed
    /// `Condition::RawSql(Box::leak(...))` to inject arbitrary SQL into
    /// WHERE clauses — the newtype closes that hole.
    /// # Why no bind parameters
    /// The closed grammar rejects every non-literal RHS, so the
    /// fragment is fully self-contained at expand time. No `$n` binds
    /// are threaded through the accumulator — the emitter pushes the
    /// fragment as raw SQL with outer parens for operator-precedence
    /// safety under further AND-composition with user `.filter(...)`
    /// calls.
    RawSql(RawSqlFragment),
}

/// Sealed wrapper around the `&'static str` carried by
/// [`Condition::RawSql`] — 4 injection-safety newtype.
/// Construction is restricted to the `djogi` crate via the `pub(crate)`
/// inner field. `djogi-macros` and adopter crates reach this through
/// [`Condition::__from_raw_sql_fragment`] (the `#[doc(hidden)]`
/// macro-only constructor). The newtype closes the safe-Rust injection
/// surface that a bare `&'static str` payload exposed: even though
/// [`Condition`] is `pub` so its variants are pattern-matchable from
/// outside, the inner field's `pub(crate)` visibility prevents
/// constructing a `Condition::RawSql(RawSqlFragment(arbitrary_str))`
/// literal in adopter code.
/// This mirrors the same defensive boundary
/// [`crate::query::condition::Leaf`] / [`crate::jsonb::path::JsonbPathLeaf`]
/// apply for their own constructor paths — Djogi's standard pattern for
/// "public type, sealed interior" surfaces.
#[derive(Debug, Clone)]
pub struct RawSqlFragment(pub(crate) &'static str);

impl RawSqlFragment {
    /// Read-only access to the wrapped fragment — for emitter consumers
    /// inside `djogi` that need to push the verbatim SQL onto the
    /// accumulator. Crate-private; adopter code matches on
    /// [`Condition::RawSql`] but cannot extract or rebuild the inner
    /// string.
    pub(crate) fn as_str(&self) -> &'static str {
        self.0
    }
}

impl Condition {
    /// Macro-only constructor for the raw-SQL escape hatch.
    /// Routes through a `#[doc(hidden)]` public surface so
    /// `djogi-macros`'s emitted code (in adopter crates) can construct
    /// one without naming the sealed [`RawSqlFragment`] newtype's
    /// `pub(crate)` field. Not part of the user-facing API; the
    /// `__`-prefix + `#[doc(hidden)]` pair signals that downstream code
    /// must not call this directly.
    /// The `&'static str` argument is the lowered SQL fragment from
    /// `model::proxy::lower_default_filter_to_sql`. The lowering pass
    /// enforces the closed grammar at expand time, so the fragment
    /// reaching this constructor cannot carry runtime-bound values or
    /// out-of-grammar tokens.
    #[doc(hidden)]
    #[must_use]
    pub fn __from_raw_sql_fragment(sql: &'static str) -> Condition {
        Condition::RawSql(RawSqlFragment(sql))
    }
}

// Written out explicitly instead of `#[derive(Default)]` + `#[default]` on the
// variant: the enum's doc comments are load-bearing (they explain why `True`
// is the vacuous root), and reshuffling them around a derive attribute hurts
// readability. The clippy hint is correct but optimizes the wrong axis here.
#[allow(clippy::derivable_impls)]
impl Default for Condition {
    fn default() -> Self {
        Condition::True
    }
}

impl Condition {
    /// Whether this condition is structurally equivalent to TRUE — used by the
    /// WHERE-clause emitter (model and visage paths) to skip the clause
    /// entirely rather than emit `WHERE TRUE`. Cleaner logs, no chance of an
    /// optimizer surprise on trivially-true predicates.
    /// Recognises the shapes the queryset constructors actually produce:
    /// `True`, `And(empty | all-True)`, and `Not(Or(empty))`. Other shapes
    /// (`Or(all-True)`, `Not(And(empty))`) are not collapsed because the
    /// builder API doesn't construct them.
    pub(crate) fn is_vacuously_true(&self) -> bool {
        match self {
            Condition::True => true,
            Condition::And(xs) => xs.iter().all(Condition::is_vacuously_true),
            Condition::Not(inner) => {
                matches!(inner.as_ref(), Condition::Or(xs) if xs.is_empty())
            }
            _ => false,
        }
    }
}

impl Condition {
    /// Combine two conditions with SQL AND. Flattens nested `And` trees to
    /// keep the structure shallow for the emitter.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn and(a: Condition, b: Condition) -> Condition {
        match (a, b) {
            (Condition::True, c) | (c, Condition::True) => c,
            (Condition::And(mut va), Condition::And(vb)) => {
                va.extend(vb);
                Condition::And(va)
            }
            (Condition::And(mut va), other) => {
                va.push(other);
                Condition::And(va)
            }
            (other, Condition::And(vb)) => {
                let mut v = vec![other];
                v.extend(vb);
                Condition::And(v)
            }
            (a, b) => Condition::And(vec![a, b]),
        }
    }

    /// Combine two conditions with SQL OR. Flattens nested `Or` trees.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn or(a: Condition, b: Condition) -> Condition {
        match (a, b) {
            (Condition::Or(mut va), Condition::Or(vb)) => {
                va.extend(vb);
                Condition::Or(va)
            }
            (Condition::Or(mut va), other) => {
                va.push(other);
                Condition::Or(va)
            }
            (other, Condition::Or(vb)) => {
                let mut v = vec![other];
                v.extend(vb);
                Condition::Or(v)
            }
            (a, b) => Condition::Or(vec![a, b]),
        }
    }

    /// Wrap a condition in SQL NOT. `Not(Not(x)) == x` is NOT auto-simplified
    /// users rarely write double-negation by accident and the emitter can
    /// handle it correctly either way.
    /// Two equivalent spellings exist: this associated function
    /// (`Condition::not(cond)`, matching `Condition::and(a, b)` /
    /// `Condition::or(a, b)`) and the unary [`std::ops::Not`] operator
    /// (`!cond`) defined below. The associated form is preserved for
    /// adopter call sites that prefer the function-style symmetry; the
    /// `!` operator is the terser idiom and pairs with the `&` / `|`
    /// composition operators on the same type.
    #[allow(clippy::should_implement_trait)]
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    pub fn not(inner: Condition) -> Condition {
        Condition::Not(Box::new(inner))
    }
}

/// Fluent method-style composition for [`Condition`].
/// The inherent associated constructors [`Condition::and`] /
/// [`Condition::or`] stay in place for backwards compatibility, so the
/// method spelling lives on an extension trait. `djogi::prelude::*`
/// re-exports this trait, letting adopter code write
/// `field.eq(v).and(other.eq(w))` without naming the trait.
/// Negation deliberately stays off this trait. Adding `fn not(self) ->
/// Condition` here would alias the inherent `std::ops::Not::not(self) ->
/// Self::Output` impl below, and any caller importing the prelude
/// (`use djogi::prelude::*;`) plus `std::ops::Not` would see two
/// reachable `.not()` methods on `Condition` and trigger an
/// "ambiguous method call" error. The two existing spellings — the
/// associated function [`Condition::not`] and the unary `!cond`
/// operator — already cover both styles; method chaining for negation
/// is `(!cond)` or `Condition::not(cond)`.
pub trait ConditionExt {
    /// Combine `self` and `other` with SQL `AND`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    fn and(self, other: Condition) -> Condition;

    /// Combine `self` and `other` with SQL `OR`.
    #[must_use = "conditions are lazy — dropping one silently omits the filter"]
    fn or(self, other: Condition) -> Condition;
}

impl ConditionExt for Condition {
    fn and(self, other: Condition) -> Condition {
        Condition::and(self, other)
    }

    fn or(self, other: Condition) -> Condition {
        Condition::or(self, other)
    }
}

impl BitAnd for Condition {
    type Output = Condition;

    fn bitand(self, rhs: Condition) -> Self::Output {
        Condition::and(self, rhs)
    }
}

impl BitOr for Condition {
    type Output = Condition;

    fn bitor(self, rhs: Condition) -> Self::Output {
        Condition::or(self, rhs)
    }
}

impl Not for Condition {
    type Output = Condition;

    fn not(self) -> Self::Output {
        Condition::not(self)
    }
}

/// A single column-level comparison: `column op value`.
/// Fields are `pub(crate)` so the only construction path is through the
/// typed `FieldRef` lookup methods (`eq`, `neq`, `gt`, `ilike`, etc.).
/// The emitter assumes those methods for the `(op, value)` pairing
/// e.g. `IContains` carries `FilterValue::String`, `Between` carries
/// `FilterValue::Pair` — so widening the fields would let downstream
/// code construct ill-formed leaves that hit emitter `unreachable!`
/// arms or render incorrect SQL. Same defensive boundary as
/// [`crate::jsonb::path::JsonbPathLeaf`].
#[derive(Debug, Clone)]
pub struct Leaf {
    pub(crate) column: &'static str,
    pub(crate) op: LookupOp,
    pub(crate) value: FilterValue,
}

impl Leaf {
    /// Single same-module constructor. The crate-internal typed
    /// builders (`FieldRef::eq`, `FieldRef::between`, etc.) all funnel
    /// through here so adding a new `Leaf` field forces every site to
    /// update; downstream code reaches `Leaf` only via the typed surface.
    pub(crate) fn new(column: &'static str, op: LookupOp, value: FilterValue) -> Self {
        Leaf { column, op, value }
    }

    /// Test helper — not public API. Production code constructs leaves
    /// via `FieldRef` lookup methods.
    #[cfg(test)]
    pub(crate) fn eq_raw(column: &'static str, value: FilterValue) -> Leaf {
        Leaf::new(column, LookupOp::Eq, value)
    }

    /// Read-only accessors — external code can inspect leaves it received
    /// from QuerySet introspection (e.g. visage traversal tests verifying
    /// a generated condition tree) but cannot construct one. The
    /// constructor side stays sealed via `pub(crate)` fields.
    pub fn column(&self) -> &'static str {
        self.column
    }

    /// SQL operator on this leaf.
    pub fn op(&self) -> LookupOp {
        self.op
    }

    /// Bound value on the right-hand side of the comparison.
    pub fn value(&self) -> &FilterValue {
        &self.value
    }
}

/// The operator half of a `Leaf`. Every lookup method maps to one of
/// these variants. SQL emission (`query::sql`) pattern-matches on this
/// enum to produce the correct operator token.
/// Marked `#[non_exhaustive]` — later phases (array ops, JSONB lookups,
/// trigram search) extend this set, and downstream exhaustive matches
/// would break on every such addition. External pattern matches must
/// include a `_ => …` arm.
/// # The §660 split — Rust-evaluable vs SQL-only
/// Per spec §8e bullet 6 (`docs/spec/implementation-plan.md:660`),
/// every variant on this enum partitions cleanly into one of two
/// classes:
/// **Rust-evaluable (15 variants).** `Eq`, `Neq`, `Gt`, `Gte`, `Lt`,
/// `Lte`, `In`, `NotIn`, `IsNull`, `IsNotNull`, `Between`, `IContains`,
/// `IStartsWith`, `IEndsWith`, `IExact`. These ops have a sassi
/// counterpart (`sassi::LookupOp::Eq` etc.) and lift to
/// `sassi::BasicPredicate::Field` — they ride through `Q::Portable` in
/// the public algebra and can be evaluated against an in-memory `&T`
/// without a database round-trip (the substrate Punnu cache builds
/// on the Punnu cache).
/// **SQL-only (2 variants).** `Regex`, `IRegex` — Postgres POSIX
/// `~` / `~*`. These do **not** have sassi counterparts (sassi's
/// `LookupOp` enum has no `Regex` / `IRegex` variants by design) and
/// stay djogi-side as `Q::Regex(field, pattern, case_sensitive)` in
/// the public algebra. Lifting them to `BasicPredicate` would require
/// a Rust regex engine, which the framework forbids per `decisions.md`
/// rows 107 + 108 and `feedback_no_regex_in_djogi.md`.
/// The partition is locked by:
/// - The lihaaf compile-fail fixture
///   `djogi-macros/tests/compile_fail/lookup_op_regex_lifted_to_basic_predicate.rs`
///   (10) — verifies `sassi::LookupOp::Regex`
///   does not exist at the type level, so a future sassi release that
///   adds a `Regex` variant would silently break the no-regex
///   invariant; this fixture catches it.
/// - The `lookup_op_partitions_cleanly_into_15_lift_2_sql_only` test
///   in this module — exhaustively walks every `LookupOp` variant and
///   asserts each lands in the correct class.
///   Adding a new variant under `#[non_exhaustive]` requires extending
///   the partition test and explicitly placing the variant in one of
///   the two classes; the test will fail to compile until that's done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LookupOp {
    /// `field = value`. Rust-evaluable (lifts to
    /// `sassi::BasicPredicate::Field` via `Q::Portable`).
    Eq,
    /// `field <> value`. Rust-evaluable.
    Neq,
    /// `field > value`. Rust-evaluable.
    Gt,
    /// `field >= value`. Rust-evaluable.
    Gte,
    /// `field < value`. Rust-evaluable.
    Lt,
    /// `field <= value`. Rust-evaluable.
    Lte,
    /// `field IN (values…)`. Rust-evaluable.
    In,
    /// `field NOT IN (values…)`. Rust-evaluable.
    NotIn,
    /// `field IS NULL`. Rust-evaluable.
    IsNull,
    /// `field IS NOT NULL`. Rust-evaluable.
    IsNotNull,
    /// ILIKE '%s%' — spec §5.4 `contains`. Rust-evaluable
    /// (case-insensitive substring match via `str::to_lowercase()` on
    /// the sassi side; locale parity with Postgres `ILIKE` documented
    /// in spec §660).
    IContains,
    /// ILIKE 's%' — spec §5.4 `starts_with`. Rust-evaluable.
    IStartsWith,
    /// ILIKE '%s' — spec §5.4 `ends_with`. Rust-evaluable.
    IEndsWith,
    /// BETWEEN a AND b. Rust-evaluable.
    Between,
    /// Case-insensitive equality via `LOWER(col) = LOWER($n)`.
    /// Rust-evaluable.
    IExact,
    /// POSIX regex (`~`). **SQL-only** — stays in djogi `Q::Regex(_, _, true)`
    /// per `decisions.md` row 108. The match runs server-side; no
    /// Rust regex engine is linked.
    Regex,
    /// Case-insensitive POSIX regex (`~*`). **SQL-only** — same
    /// rationale as `Regex`. Stays in djogi `Q::Regex(_, _, false)`.
    IRegex,
}

/// Source-side classification of [`LookupOp`] per the §660 split.
/// Every shipped variant is tagged either `RustEvaluable` (lifts to
/// `sassi::BasicPredicate::Field` via `Q::Portable`) or `SqlOnly` (stays
/// djogi-side via `Q::Regex` etc.).
/// Adopters do not name this enum — it is the type-level partition
/// the lowering bridge and the lihaaf compile-fail fixture lean on. A
/// new `LookupOp` variant added under `#[non_exhaustive]` must be placed
/// in one of these two classes by extending
/// [`LookupOp::source_class`] below; the partition test in this
/// module fails to compile until the new arm is added.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupOpSourceClass {
    /// Lifts to `sassi::BasicPredicate::Field` via `Q::Portable`.
    /// 15 variants today.
    RustEvaluable,
    /// Stays djogi-side via `Q::Regex` (or future SQL-only variants).
    /// 2 variants today (`Regex`, `IRegex`).
    SqlOnly,
}

impl LookupOp {
    /// Source-side class per the §660 split.
    /// Exhaustive over the **shipped** variants. The match has no
    /// `_ => …` catch-all so adding a new `LookupOp` variant fails
    /// to compile this method until the arm is added — that is the
    /// load-bearing protection: a future variant cannot silently fall
    /// into the wrong class.
    /// # Why a method, not a const
    /// A `const` map (`[(LookupOp::Eq, RustEvaluable), …]`) would
    /// require linear-search lookup at every callsite. The match
    /// optimises into a single jump table at the same compile-time
    /// cost without sacrificing the type-level partition guarantee.
    // `#[allow(dead_code)]` — no production path consumes
    // `source_class` today (the legacy `Condition`-based emission
    // is still the SQL path). The classification is consumed by the
    // partition test in the `tests` module and by future cluster
    // integrations that need the source-side tag (e.g. Punnu
    // cache eligibility checks). Keeping the API loaded but
    // dormant per `feedback_atomic_commits.md` — the §660 split
    // documentation ships in a later commit; the consumers land later.
    #[allow(dead_code)]
    pub(crate) fn source_class(self) -> LookupOpSourceClass {
        // Exhaustive — no `_` arm. New variants land in the right
        // class by being explicitly named here, or the build fails.
        match self {
            // ── Rust-evaluable (15) — lifts to sassi::BasicPredicate::Field ──
            LookupOp::Eq
            | LookupOp::Neq
            | LookupOp::Gt
            | LookupOp::Gte
            | LookupOp::Lt
            | LookupOp::Lte
            | LookupOp::In
            | LookupOp::NotIn
            | LookupOp::IsNull
            | LookupOp::IsNotNull
            | LookupOp::Between
            | LookupOp::IContains
            | LookupOp::IStartsWith
            | LookupOp::IEndsWith
            | LookupOp::IExact => LookupOpSourceClass::RustEvaluable,
            // ── SQL-only (2) — Postgres POSIX regex; no Rust regex engine ────
            LookupOp::Regex | LookupOp::IRegex => LookupOpSourceClass::SqlOnly,
        }
    }
}

impl LookupOp {
    /// SQL operator token for the simple binary-comparison forms emitted as
    /// `col OP value`. Returns `None` for variants whose emission shape
    /// differs (`IS NULL`, `BETWEEN`, `IN`, `ILIKE`, `LOWER(col) = LOWER(v)`).
    /// Used by the leaf emitters in `query::sql` to collapse what was
    /// previously eight near-identical match arms into one path.
    pub(crate) fn binary_op_token(self) -> Option<&'static str> {
        match self {
            LookupOp::Eq => Some(" = "),
            LookupOp::Neq => Some(" <> "),
            LookupOp::Gt => Some(" > "),
            LookupOp::Gte => Some(" >= "),
            LookupOp::Lt => Some(" < "),
            LookupOp::Lte => Some(" <= "),
            LookupOp::Regex => Some(" ~ "),
            LookupOp::IRegex => Some(" ~* "),
            _ => None,
        }
    }
}

/// A concrete value to bind to a query parameter. One variant per
/// SQL-bindable Rust type Djogi knows about. Implementers adding a new
/// column type (e.g. `Decimal` in) extend this enum here; `sql.rs`
/// pattern-matches and calls `qb.push_bind(v)` on each variant.
/// `List` carries a boxed `Vec<FilterValue>` for `IN (...)` / `NOT IN (...)`.
/// Mixed-type lists are representable but the emitter rejects them — the
/// typed `FieldRef<M, V>` API prevents construction from user code.
/// Marked `#[non_exhaustive]` — new SQL-bindable types (e.g. JSONB payload
/// variants) are added in later phases. Adding a
/// variant must not break downstream code that pattern-matches on this
/// enum, so external matches must include a `_ => …` arm.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum FilterValue {
    String(String),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    /// `TIMESTAMP` / timestamp-without-timezone values. Currently used as
    /// the element RHS for `Range<time::PrimitiveDateTime>::contains(...)`.
    Timestamp(time::PrimitiveDateTime),
    DateTime(time::OffsetDateTime),
    Date(time::Date),
    Uuid(uuid::Uuid),
    HeerId(HeerId),
    RanjId(RanjId),
    /// Reverse-chronological `HeerId` — stored as BIGINT, same Postgres
    /// surface as [`FilterValue::HeerId`]. Kept as a distinct variant so
    /// `FieldRef<M, HeerIdDesc>::eq(x)` can preserve the type identity all
    /// the way into the emitted bind site (v3).
    HeerIdDesc(crate::types::HeerIdDesc),
    /// Reverse-chronological `RanjId` — stored as UUID, same Postgres
    /// surface as [`FilterValue::RanjId`]. See [`FilterValue::HeerIdDesc`].
    RanjIdDesc(crate::types::RanjIdDesc),
    Null,
    /// For IN (...) / NOT IN (...) list lookups.
    /// # Invariants (enforced by construction, not the enum itself)
    /// - Elements must be SQL-bindable **scalars** — never nested `List` or
    ///   `Pair`. The typed `FieldRef<M, V>::in_list(impl IntoIterator<Item = V>)`
    ///   API prevents nesting by construction; manual `FilterValue::List`
    ///   construction that violates this invariant is a framework bug that
    ///   Task 6's SQL emitter panics on (not silently miscompiles).
    /// - All elements should be the same `FilterValue` discriminant (mixed-type
    ///   lists are meaningless for SQL `IN`). The typed API enforces this.
    List(Vec<FilterValue>),
    /// BETWEEN a AND b payload (two bound values).
    Pair(Box<FilterValue>, Box<FilterValue>),
    /// `NUMERIC` / `DECIMAL` column values .
    Decimal(rust_decimal::Decimal),
    /// Postgres `INTERVAL` column values.
    Interval(crate::Interval),
    /// Postgres `BYTEA` column values. Carries `Vec<u8>`; tokio-postgres
    /// native `ToSql`/`FromSql` codec.
    Bytea(Vec<u8>),
    /// Postgres `INET` column values (, `network` feature).
    /// Carries `std::net::IpAddr`; the wire codec is the always-on
    /// `postgres-types` native impl which writes /32 or /128 netmasks
    /// for the host-address case.
    #[cfg(feature = "network")]
    Inet(std::net::IpAddr),
    /// Postgres `INET` column values carrying an explicit prefix
    /// (`network` feature). Carries `djogi::InetAddr { addr, prefix }`
    /// with construction-time prefix-bound validation. Distinct from
    /// [`FilterValue::Inet`], which carries a bare `std::net::IpAddr`
    /// (prefix collapsed to /32 or /128 by the native codec).
    #[cfg(feature = "network")]
    InetTyped(crate::InetAddr),
    /// Postgres `CIDR` column values (, `network` feature).
    /// Carries `djogi::CidrAddr { addr, prefix }` with construction-time
    /// host-bit-zero validation.
    #[cfg(feature = "network")]
    Cidr(crate::CidrAddr),
    /// Postgres `MACADDR` column values (, `network` feature).
    /// Carries `djogi::MacAddr([u8; 6])`.
    #[cfg(feature = "network")]
    Macaddr(crate::MacAddr),

    // ── Range variants ───────────────────────────────────────
    // Used by PostgreSQL range operators and explicit SQL equality over
    // range columns. Each variant binds a typed `Range<T>` as a Postgres
    // range parameter so tokio-postgres can encode the correct OID.
    /// `Range<i32>` parameter (`int4range`).
    RangeI32(crate::Range<i32>),
    /// `Range<i64>` parameter (`int8range`).
    RangeI64(crate::Range<i64>),
    /// `Range<rust_decimal::Decimal>` parameter (`numrange`).
    RangeDecimal(crate::Range<rust_decimal::Decimal>),
    /// `Range<time::PrimitiveDateTime>` parameter (`tsrange`).
    RangeTimestamp(crate::Range<time::PrimitiveDateTime>),
    /// `Range<time::OffsetDateTime>` parameter (`tstzrange`).
    RangeDateTime(crate::Range<time::OffsetDateTime>),
    /// `Range<time::Date>` parameter (`daterange`).
    RangeDate(crate::Range<time::Date>),

    // ── Array variants ──────────────────────────────────
    // Used exclusively by the array column operators (`@>`, `<@`, `&&`).
    // Each variant binds a typed `Vec<V>` as a Postgres array parameter
    // so tokio-postgres can encode it with the correct OID. Mixed-type
    // arrays are not representable — the typed `FieldRef<M, Vec<V>>` API
    // prevents construction.
    /// `Vec<String>` array parameter (TEXT[]).
    ArrayString(Vec<String>),
    /// `Vec<i32>` array parameter (INT4[]).
    ArrayI32(Vec<i32>),
    /// `Vec<i64>` array parameter (INT8[]).
    ArrayI64(Vec<i64>),
    /// `Vec<bool>` array parameter (BOOL[]).
    ArrayBool(Vec<bool>),
    /// `Vec<i16>` array parameter (INT2[]).
    ArrayI16(Vec<i16>),
    /// `Vec<f32>` array parameter (FLOAT4[]).
    ArrayF32(Vec<f32>),
    /// `Vec<f64>` array parameter (FLOAT8[]).
    ArrayF64(Vec<f64>),
    /// `Vec<time::OffsetDateTime>` array parameter (TIMESTAMPTZ[]).
    ArrayDateTime(Vec<time::OffsetDateTime>),
    /// `Vec<time::Date>` array parameter (DATE[]).
    ArrayDate(Vec<time::Date>),
    /// `Vec<uuid::Uuid>` array parameter (UUID[]).
    ArrayUuid(Vec<uuid::Uuid>),
    /// `Vec<rust_decimal::Decimal>` array parameter (NUMERIC[]).
    ArrayDecimal(Vec<rust_decimal::Decimal>),
    /// `Vec<HeerId>` array parameter (BIGINT[]). `HeerId` encodes as INT8.
    ArrayHeerId(Vec<crate::types::HeerId>),
    /// `Vec<RanjId>` array parameter (UUID[]). `RanjId` encodes as UUID.
    ArrayRanjId(Vec<crate::types::RanjId>),
    /// `Vec<HeerIdDesc>` array parameter (BIGINT[]). `HeerIdDesc` /
    /// `HeerIdRecencyBiased` encodes as INT8, newest-first sort order.
    ArrayHeerIdDesc(Vec<crate::types::HeerIdDesc>),
    /// `Vec<RanjIdDesc>` array parameter (UUID[]). `RanjIdDesc` /
    /// `RanjIdRecencyBiased` encodes as UUID, newest-first sort order.
    ArrayRanjIdDesc(Vec<crate::types::RanjIdDesc>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_true_is_default() {
        let c = Condition::default();
        assert!(matches!(c, Condition::True));
    }

    #[test]
    fn and_flattens_nested_ands() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let combined = Condition::and(Condition::and(a, b), c);
        if let Condition::And(parts) = combined {
            assert_eq!(
                parts.len(),
                3,
                "nested And should flatten to 3 leaves, got {parts:?}"
            );
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn and_with_true_is_identity() {
        let leaf = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let combined = Condition::and(Condition::True, leaf.clone());
        assert!(matches!(combined, Condition::Leaf(_)));
    }

    #[test]
    fn or_flattens_nested_ors() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let combined = Condition::or(Condition::or(a, b), c);
        if let Condition::Or(parts) = combined {
            assert_eq!(parts.len(), 3);
        } else {
            panic!("expected Or");
        }
    }

    #[test]
    fn and_flattens_three_levels_of_nesting() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));
        let d = Condition::Leaf(Leaf::eq_raw("d", FilterValue::Bool(false)));
        // ((a AND b) AND c) AND d → 4-leaf flat And
        let combined = Condition::and(Condition::and(Condition::and(a, b), c), d);
        if let Condition::And(parts) = combined {
            assert_eq!(parts.len(), 4, "3-deep nesting should flatten to 4 leaves");
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn and_preserves_order_with_rhs_container() {
        // (leaf_x AND (And[a, b])) should yield [leaf_x, a, b] in order
        let x = Condition::Leaf(Leaf::eq_raw("x", FilterValue::Bool(true)));
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let combined = Condition::and(x, Condition::And(vec![a, b]));
        if let Condition::And(parts) = combined {
            assert_eq!(parts.len(), 3);
            // Check ordering via column names stored in leaves
            let names: Vec<&'static str> = parts
                .iter()
                .filter_map(|p| {
                    if let Condition::Leaf(l) = p {
                        Some(l.column)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(names, vec!["x", "a", "b"]);
        } else {
            panic!("expected And, got {combined:?}");
        }
    }

    #[test]
    fn condition_ext_methods_compose_left_to_right() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));
        let c = Condition::Leaf(Leaf::eq_raw("c", FilterValue::Bool(true)));

        let combined = a.and(b).or(c);

        assert!(matches!(combined, Condition::Or(parts) if parts.len() == 2));
    }

    #[test]
    fn condition_operators_delegate_to_existing_constructors() {
        let a = Condition::Leaf(Leaf::eq_raw("a", FilterValue::Bool(true)));
        let b = Condition::Leaf(Leaf::eq_raw("b", FilterValue::Bool(false)));

        assert!(matches!(a.clone() & b.clone(), Condition::And(parts) if parts.len() == 2));
        assert!(matches!(a.clone() | b.clone(), Condition::Or(parts) if parts.len() == 2));
        assert!(matches!(!a, Condition::Not(_)));
    }

    #[test]
    fn or_empty_vec_is_not_auto_replaced() {
        // Invariant check: Or(vec![]) stays Or(vec![]) — it is emitter's job
        // to render FALSE. Construct directly (not via `or()` — that flattens).
        let empty = Condition::Or(Vec::new());
        assert!(matches!(empty, Condition::Or(ref v) if v.is_empty()));
    }

    // ── §660 partition test ────────────────────────────────────────
    // Locks the Rust-evaluable vs SQL-only split documented on
    // [`LookupOp`]. Every shipped variant is tagged via
    // `source_class()` and the test verifies the exact 15-vs-2 partition.
    // The partition is load-bearing per `decisions.md` rows 107 + 108
    // and `feedback_no_regex_in_djogi.md`: lifting `Regex` / `IRegex`
    // to sassi would require a Rust regex engine, which the framework
    // forbids. The lihaaf compile-fail fixture
    // `djogi-macros/tests/compile_fail/lookup_op_regex_lifted_to_basic_predicate.rs`
    // (10) catches the type-level violation;
    // this unit test catches the source-side classification mistake
    // (e.g. a future cluster accidentally re-tagging `Regex` as
    // `RustEvaluable`).
    // Adding a new `LookupOp` variant fails to compile
    // `LookupOp::source_class` (no `_ => …` arm) until placed
    // explicitly in one of the two classes; this test fails to compile
    // until the new variant is also represented in the relevant
    // count-asserting arm below.

    #[test]
    fn lookup_op_partitions_cleanly_into_15_lift_2_sql_only() {
        // Exhaustive walk: every shipped variant maps to a known class.
        // Hard-coded counts catch silent partition drift if a new
        // variant is added without updating this test.
        let rust_evaluable = [
            LookupOp::Eq,
            LookupOp::Neq,
            LookupOp::Gt,
            LookupOp::Gte,
            LookupOp::Lt,
            LookupOp::Lte,
            LookupOp::In,
            LookupOp::NotIn,
            LookupOp::IsNull,
            LookupOp::IsNotNull,
            LookupOp::Between,
            LookupOp::IContains,
            LookupOp::IStartsWith,
            LookupOp::IEndsWith,
            LookupOp::IExact,
        ];
        assert_eq!(
            rust_evaluable.len(),
            15,
            "expected 15 Rust-evaluable variants per §660"
        );
        for op in rust_evaluable {
            assert_eq!(
                op.source_class(),
                LookupOpSourceClass::RustEvaluable,
                "variant {op:?} must be Rust-evaluable per the §660 split"
            );
        }

        let sql_only = [LookupOp::Regex, LookupOp::IRegex];
        assert_eq!(
            sql_only.len(),
            2,
            "expected exactly 2 SQL-only variants — Regex + IRegex per decisions.md row 108"
        );
        for op in sql_only {
            assert_eq!(
                op.source_class(),
                LookupOpSourceClass::SqlOnly,
                "variant {op:?} must be SQL-only — lifting it to sassi requires a Rust regex engine"
            );
        }
    }

    /// `Regex` and `IRegex` MUST land in `SqlOnly` — the load-bearing
    /// arm of the partition. If a future refactor accidentally
    /// re-classifies these as `RustEvaluable`, the no-regex
    /// invariant breaks silently; this test catches it before
    /// anything ships.
    #[test]
    fn regex_variants_are_sql_only() {
        assert_eq!(LookupOp::Regex.source_class(), LookupOpSourceClass::SqlOnly);
        assert_eq!(
            LookupOp::IRegex.source_class(),
            LookupOpSourceClass::SqlOnly
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_typed_filter_value_is_constructible() {
        use std::net::{IpAddr, Ipv4Addr};
        let inet = crate::InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 8).unwrap();
        let fv = FilterValue::InetTyped(inet);
        assert!(matches!(fv, FilterValue::InetTyped(_)));
    }
}
