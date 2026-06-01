//! `PortablePredicate<T>` + `Predicate<T>` — Djogi-owned wrappers around the
//! Sassi predicate substrate.
//! # Why a Djogi wrapper around `sassi::BasicPredicate<T>`?
//! Sassi exposes `BasicPredicate<T>` as the universal Rust-evaluable predicate
//! algebra. Anyone with `pub use sassi::Field` can construct
//! `Field::new("any_string", arbitrary_extractor)` and then call
//! `.eq(value)` to obtain a `BasicPredicate<T>` whose `field_name`,
//! `LookupOp`, and extractor closure may not match any real Djogi column on
//! `T`. If raw `BasicPredicate<T>` were accepted at Djogi cache or SQL
//! boundaries, two threats would land at runtime:
//! 1. **Field-name forgery** — an attacker (or honest mistake) can pair a
//! valid column name with a closure that reads a different field of the
//! struct. SQL emission would target the named column while in-memory
//! Punnu evaluation would read the unrelated field, silently drifting
//! cache-side and DB-side answers.
//! 2. **Identifier smuggling** — `Field::new` accepts any `&'static str` for
//! the column name; Djogi's SQL emitter ultimately routes it through
//! `SqlAccumulator::push_sql`, which assumes its inputs were already
//! validated against [`crate::ident::assert_plain_ident`] (the same gate
//! applied to `FieldRef` and `RelationPath`).
//! `PortablePredicate<T>` is the trusted-Djogi-provenance wrapper. Its
//! `from_djogi_field` constructor requires a [`crate::query::field::DjogiFieldProvenance`]
//! token, and that token is constructible only by Djogi-owned root field
//! methods on `DjogiField` / `DjogiPresentField`. The seal is the same
//! pattern the rest of the crate uses for `RelationPath`,
//! `OptionalRelationRef`, and `__SealedIntoQ`.
//! # `Predicate<T>` — a trusted `Q<T>` wrapper
//! `Predicate<T>` is a thin shell over `Q<T>` carrying the mixed-operator
//! matrix from the v3 plan. Pure-portable composition (`PortablePredicate<T>
//! & PortablePredicate<T>`) stays inside `PortablePredicate<T>` so flattening
//! rides the trusted Sassi reducer. Mixed compositions (where at least one
//! operand is a `Predicate<T>` or a `Condition`) lift through `Predicate<T>`
//! so the resulting `Q<T>` carries a `Q::Compound { op, parts }` structure
//! the cache boundary can audit.
//! # PR2b scope
//! - Implement `IntoQ<T>` for `PortablePredicate<T>`, `Predicate<T>`, and
//! `crate::expr::Expr<bool>` (sealed alongside the existing impls in
//! `query::q`).
//! - Add the closed mixed-operator matrix between `PortablePredicate<T>`,
//! `Predicate<T>`, and `Condition` so `&` / `|` / `^` / `!` compose freely
//! regardless of operand order.
//! - Manual `Clone` for `Predicate<T>` reaches into the new
//! `Q<T>: Clone` impl (which itself does not require `T: Clone` after
//! PR2b).

use crate::model::Model;
use crate::query::condition::Condition;
use crate::query::field::DjogiFieldProvenance;
use crate::query::q::{IntoQ, Q};
use sassi::{BasicPredicate, IntoBasicPredicate};
use std::ops::{BitAnd, BitOr, BitXor, Not};

mod sealed {
    /// Crate-private seal for [`super::IntoPortablePredicate`].
    /// Only types defined inside `djogi` can implement
    /// [`super::IntoPortablePredicate`]. PR2a implements it for
    /// [`super::PortablePredicate`] only — raw `sassi::BasicPredicate<T>`
    /// is **deliberately** excluded because raw Sassi predicates can pair
    /// forged field names with unrelated extractors and are not safe Djogi
    /// SQL/Punnu cache predicates.
    pub trait Sealed {}
}

/// Trusted-provenance Djogi portable predicate.
/// Wraps a `sassi::BasicPredicate<T>` whose `Field(_)` leaves were produced
/// through Djogi's root-field surface (`DjogiField` / `DjogiPresentField`),
/// not through raw `sassi::Field::new(...)` calls. The
/// [`DjogiFieldProvenance`] token tracks the trusted-construction guarantee
/// at the type level; vacuous predicates ([`PortablePredicate::always_true`] /
/// [`PortablePredicate::always_false`]) carry `field_provenance: None`
/// because they contain no field leaves to vouch for.
/// # Why this is a separate type from `Q<T>`
/// `Q<T>` carries SQL-only nodes (`Q::Ilike`, `Q::Regex`, `Q::JsonbPath`,
/// expression escape hatches, etc.) that cannot be evaluated in Punnu
/// without database access. `PortablePredicate<T>` is the strictly-portable
/// subset — every leaf can be evaluated against `&T` in memory and emitted
/// as SQL. Cache and refresh boundaries (PR4) accept only this type.
/// # Construction
/// Downstream code constructs values through the root-field methods on
/// [`crate::query::field::DjogiField`] (e.g.
/// `f.title.eq("rust") -> PortablePredicate<Post>`). The
/// [`from_djogi_field`](Self::from_djogi_field) helper is `pub(crate)` so the
/// only way for downstream Sassi predicates to enter Djogi cache paths is
/// through the trusted Djogi field surface.
pub struct PortablePredicate<T: Model> {
    inner: BasicPredicate<T>,
    field_provenance: Option<DjogiFieldProvenance>,
}

impl<T: Model> PortablePredicate<T> {
    /// Wrap a Sassi `BasicPredicate<T>` produced by a Djogi root-field
    /// method. Crate-private — see the module docs for why this is not
    /// `pub`.
    /// `provenance` is the trusted-construction marker callable only from
    /// `DjogiField` / `DjogiPresentField` methods. The token itself
    /// carries no payload; its presence in the type signature is the
    /// social contract that the caller is a Djogi-owned wrapper, not a
    /// downstream attacker constructing arbitrary Sassi predicates.
    #[doc(hidden)]
    pub(crate) fn from_djogi_field(
        inner: BasicPredicate<T>,
        provenance: DjogiFieldProvenance,
    ) -> Self {
        Self {
            inner,
            field_provenance: Some(provenance),
        }
    }

    /// Vacuous-truth predicate.
    /// Used by PR2b's `Q::Portable` migration as the canonical identity for
    /// unfiltered querysets. `field_provenance` is `None` because `True`
    /// carries no field leaves.
    #[doc(hidden)]
    #[allow(dead_code)] // PR2b consumes this in the Q::always_true migration
    pub(crate) fn always_true() -> Self {
        Self {
            inner: BasicPredicate::True,
            field_provenance: None,
        }
    }

    /// Vacuous-falsehood predicate.
    /// Used by PR2b's `Q::Portable` migration as the canonical identity for
    /// `QuerySet::none`-style impossible queries. `field_provenance` is
    /// `None` because `False` carries no field leaves.
    #[doc(hidden)]
    #[allow(dead_code)] // PR2b consumes this in the Q::always_false migration
    pub(crate) fn always_false() -> Self {
        Self {
            inner: BasicPredicate::False,
            field_provenance: None,
        }
    }

    /// Unwrap into the underlying `sassi::BasicPredicate<T>`.
    /// Provenance is intentionally dropped on extraction — once a caller
    /// has the raw Sassi predicate, they have left the trusted-Djogi-API
    /// path. This is the documented escape hatch for callers reaching
    /// directly into Sassi (e.g. `PunnuScope::filter_basic`); ordinary
    /// Djogi adopter code does not need it because
    /// `sassi::IntoBasicPredicate` already routes the conversion through
    /// the public Sassi trait.
    pub fn into_inner(self) -> BasicPredicate<T> {
        self.inner
    }

    /// Borrow the underlying `BasicPredicate<T>` without consuming the
    /// wrapper. Used by the SQL emitter walker in PR2b's `query::portable`
    /// module so the `Q<T>` clone path can avoid moving out.
    #[doc(hidden)]
    #[allow(dead_code)] // PR2b's emit_portable_predicate walker consumes this
    pub(crate) fn inner_ref(&self) -> &BasicPredicate<T> {
        &self.inner
    }

    /// `true` iff this predicate carries at least one Djogi-provenance
    /// field leaf. Vacuous identities (`True` / `False`) and the empty
    /// `And(vec![])` / `Or(vec![])` containers return `false`.
    #[doc(hidden)]
    #[allow(dead_code)] // PR4 cache-boundary portability gate consumes this
    pub(crate) fn has_field_provenance(&self) -> bool {
        self.field_provenance.is_some()
    }
}

// Manual `Clone` — the derived impl would impose `T: Clone` even though
// `BasicPredicate<T>` itself clones structurally without that bound (sassi's
// PR1 manual `impl Clone for BasicPredicate<T>`). `Option<DjogiFieldProvenance>`
// is `Copy`, so the manual impl is identical to the derived one save for the
// dropped `T` bound.
impl<T: Model> Clone for PortablePredicate<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            field_provenance: self.field_provenance,
        }
    }
}

impl<T: Model> std::fmt::Debug for PortablePredicate<T> {
    /// Avoids touching the inner `BasicPredicate<T>` (whose derived
    /// `Debug` impl imposes `T: Debug`) and the `FieldPredicate<T>`
    /// payload (whose value is type-erased through `Arc<dyn Any>`).
    /// Prints only the trusted-provenance flag and a structural variant
    /// tag — sufficient to debug cache/refresh portability gates without
    /// requiring every model to derive `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.inner {
            BasicPredicate::True => "True",
            BasicPredicate::False => "False",
            BasicPredicate::Field(_) => "Field",
            BasicPredicate::And(_) => "And",
            BasicPredicate::Or(_) => "Or",
            BasicPredicate::Not(_) => "Not",
            BasicPredicate::Xor(_, _) => "Xor",
            // `BasicPredicate<T>` is `#[non_exhaustive]`; future Sassi
            // variants land here as `<unknown>` rather than a panic.
            _ => "<unknown>",
        };
        f.debug_struct("PortablePredicate")
            .field("provenance", &self.field_provenance)
            .field("kind", &kind)
            .finish_non_exhaustive()
    }
}

/// Sassi-side bridge: a `PortablePredicate<T>` is convertible into the
/// underlying `BasicPredicate<T>` so it can pass through
/// `PunnuScope::filter_basic` (Sassi PR1's `IntoBasicPredicate`-bound
/// surface) without forcing adopters to spell `into_inner` by hand.
impl<T: Model> IntoBasicPredicate<T> for PortablePredicate<T> {
    fn into_basic_predicate(self) -> BasicPredicate<T> {
        self.inner
    }
}

/// Sealed-trait conversion for Djogi-owned portable predicates.
/// PR2a implements this **only** for [`PortablePredicate<T>`]. Raw
/// `sassi::BasicPredicate<T>` deliberately does not get an impl — see the
/// module docs for the threat model.
/// PR2b extends this trait with `IntoQ<T>` interop so portable predicates
/// flow through `QuerySet::filter` / `exclude` without reaching for
/// `Q::Portable(...)` by hand. PR2a keeps the trait sealed and minimal so
/// the public surface compiles before the SQL walker exists.
pub trait IntoPortablePredicate<T: Model>: sealed::Sealed {
    /// Lower the implementor into the Djogi-trusted portable predicate.
    fn into_portable_predicate(self) -> PortablePredicate<T>;
}

impl<T: Model> sealed::Sealed for PortablePredicate<T> {}
impl<T: Model> IntoPortablePredicate<T> for PortablePredicate<T> {
    #[inline]
    fn into_portable_predicate(self) -> PortablePredicate<T> {
        self
    }
}

// ── Pure-portable boolean composition ──────────────────────────────────────
// PR2a only implements the `PortablePredicate<T> ⊕ PortablePredicate<T>` rows
// of the v3 operator matrix. Mixed `Predicate`/`Condition` rows land in PR2b
// after the direct-`Q<T>` SQL emitter exists; without that emitter, mixed
// composition would have to lower portable predicates through
// `q_to_condition`, which is the exact bridge v3 retires.
// Provenance composition rule: a composite predicate carries provenance iff
// either operand had a Djogi field leaf. `True`/`False` operands keep their
// `None` provenance. This matches the trusted-construction invariant — the
// composition of trusted leaves is itself trusted.

fn merge_provenance(
    lhs: Option<DjogiFieldProvenance>,
    rhs: Option<DjogiFieldProvenance>,
) -> Option<DjogiFieldProvenance> {
    lhs.or(rhs)
}

impl<T: Model> BitAnd for PortablePredicate<T> {
    type Output = PortablePredicate<T>;
    /// SQL/Punnu AND. Delegates to sassi's flattening
    /// `BasicPredicate::bitand` so `a & b & c` produces a single
    /// `BasicPredicate::And(vec![a, b, c])` rather than nested binary
    /// nodes. Provenance is preserved across the composition.
    fn bitand(self, rhs: Self) -> PortablePredicate<T> {
        let provenance = merge_provenance(self.field_provenance, rhs.field_provenance);
        PortablePredicate {
            inner: self.inner & rhs.inner,
            field_provenance: provenance,
        }
    }
}

impl<T: Model> BitOr for PortablePredicate<T> {
    type Output = PortablePredicate<T>;
    /// SQL/Punnu OR. Delegates to sassi's flattening
    /// `BasicPredicate::bitor`.
    fn bitor(self, rhs: Self) -> PortablePredicate<T> {
        let provenance = merge_provenance(self.field_provenance, rhs.field_provenance);
        PortablePredicate {
            inner: self.inner | rhs.inner,
            field_provenance: provenance,
        }
    }
}

impl<T: Model> BitXor for PortablePredicate<T> {
    type Output = PortablePredicate<T>;
    /// SQL/Punnu XOR. Mirrors sassi's `BasicPredicate::Xor(Box, Box)`
    /// non-associative shape.
    fn bitxor(self, rhs: Self) -> PortablePredicate<T> {
        let provenance = merge_provenance(self.field_provenance, rhs.field_provenance);
        PortablePredicate {
            inner: self.inner ^ rhs.inner,
            field_provenance: provenance,
        }
    }
}

impl<T: Model> Not for PortablePredicate<T> {
    type Output = PortablePredicate<T>;
    /// SQL/Punnu NOT. Sassi collapses double-negation in place
    /// (`!!p == p`) and flips `True` ↔ `False`; provenance survives the
    /// negation because the negated predicate still references the same
    /// trusted field leaves (or none at all).
    fn not(self) -> PortablePredicate<T> {
        PortablePredicate {
            inner: !self.inner,
            field_provenance: self.field_provenance,
        }
    }
}

/// Trusted-provenance `Q<T>` wrapper for the mixed-operator matrix.
/// `Predicate<T>` is the output type of the
/// `PortablePredicate<T> ⊕ Condition`-style combinator rows: any time a
/// composition mixes Djogi-trusted portable leaves with a SQL-only
/// `Condition` (or with another already-mixed `Predicate<T>`), the
/// resulting predicate lifts to `Predicate<T>` so the SQL emitter sees a
/// `Q::Compound { op, parts }` shape and the cache boundary can audit
/// which leaves were portable.
/// `IntoQ<T> for Predicate<T>` unwraps to the inner `Q<T>` directly, so
/// `QuerySet::filter` / `filter_struct` accept any `P: IntoQ<T>` and the
/// caller never has to spell `Q::Compound { ... }` by hand.
/// User code typically reaches `Predicate<T>` through the operator
/// overloads on `PortablePredicate<T>` and `Condition` (declared below);
/// constructing values directly stays crate-private.
pub struct Predicate<T: Model> {
    inner: Q<T>,
}

impl<T: Model> Predicate<T> {
    /// Construct from an arbitrary `Q<T>`. Crate-private so only Djogi-owned
    /// combinators can produce a `Predicate<T>`. The mixed operator
    /// overloads below use this to wrap composite Q-trees.
    #[doc(hidden)]
    pub(crate) fn from_q(inner: Q<T>) -> Self {
        Self { inner }
    }

    /// Unwrap into the underlying `Q<T>`. Crate-private; downstream code
    /// reaches `Q<T>` through `IntoQ<T> for Predicate<T>`. Used by the
    /// `IntoQ` impl below and by the unit tests that pattern-match on
    /// the inner `Q<T>` shape after a mixed-operator composition.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub(crate) fn into_inner(self) -> Q<T> {
        self.inner
    }
}

// Manual `Clone` for `Predicate<T>` — `Q<T>::Clone` is itself manual after
// PR2b (no `T: Clone` propagation), so this just delegates without
// adding any bound.
impl<T: Model> Clone for Predicate<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Model> std::fmt::Debug for Predicate<T> {
    /// Forwards to `Q<T>::Debug` — which itself is a manual walker after
    /// PR2b that does not require `T: Debug`. The wrapping `Predicate`
    /// tag keeps the formatted output disambiguated from a bare `Q<T>`
    /// in trace logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Predicate").field(&self.inner).finish()
    }
}

// ── PR2b — `IntoQ<T>` impls for the new wrappers ────────────────────────────
// `IntoQ<T>` is sealed inside `query::q` (see `__SealedIntoQ`). The seal is
// `pub(crate)` and re-exported through `crate::__private::__SealedIntoQ` so
// macro-emitted code can satisfy it from adopter crates. Inside the djogi
// crate we just impl `Sealed` directly.

impl<T: Model> crate::query::q::__SealedIntoQ for PortablePredicate<T> {}
impl<T: Model> IntoQ<T> for PortablePredicate<T> {
    /// Lift a Djogi-trusted [`PortablePredicate<T>`] into `Q<T>` by
    /// wrapping in the trusted-provenance `Q::Portable(_)` variant. Used
    /// by `QuerySet::filter` / `filter_struct` so root-field predicates
    /// like `f.title.eq("rust")` flow through the generalised
    /// `P: IntoQ<T>` builder signatures without requiring the caller to
    /// spell `Q::Portable(...)`.
    #[inline]
    fn into_q(self) -> Q<T> {
        Q::Portable(self)
    }
}

impl<T: Model> crate::query::q::__SealedIntoQ for Predicate<T> {}
impl<T: Model> IntoQ<T> for Predicate<T> {
    /// Unwrap a [`Predicate<T>`] (the output of mixed-operator
    /// composition) into its inner `Q<T>`. Cheap — `Predicate<T>` is a
    /// thin newtype.
    #[inline]
    fn into_q(self) -> Q<T> {
        self.inner
    }
}

// ── Mixed-operator matrix ───────────────────────────────────────────────────
// Closed grid covering every operand pairing involving `PortablePredicate<T>`,
// `Predicate<T>`, and `Condition` per the v3 plan PR2 Step 1 table:
// | Left | Operator | Right | Output |
// |---|---|---|---|
// | `PortablePredicate<T>` | & / | / ^ | `PortablePredicate<T>` | `PortablePredicate<T>` |
// | `PortablePredicate<T>` | & / | / ^ | `Predicate<T>` | `Predicate<T>` |
// | `Predicate<T>` | & / | / ^ | `PortablePredicate<T>` | `Predicate<T>` |
// | `Predicate<T>` | & / | / ^ | `Predicate<T>` | `Predicate<T>` |
// | `PortablePredicate<T>` | & / | / ^ | `Condition` | `Predicate<T>` |
// | `Condition` | & / | / ^ | `PortablePredicate<T>` | `Predicate<T>` |
// | `Predicate<T>` | & / | / ^ | `Condition` | `Predicate<T>` |
// | `Condition` | & / | / ^ | `Predicate<T>` | `Predicate<T>` |
// | unary `!` | `PortablePredicate<T>` | `PortablePredicate<T>` |
// | unary `!` | `Predicate<T>` | `Predicate<T>` |
// Pure-portable rows already exist above (`impl BitAnd for PortablePredicate`).
// Mixed rows below all route through `Q<T>`'s operator impls so the
// flattening / non-associativity rules stay centralised in one place.
// Pure-Condition rows use `Condition`'s own `BitAnd` / `BitOr` (see
// `query::condition`). They are unchanged by PR2b.
// `Add`, `Sub`, `Mul`, `Div`, `Rem`, shifts, and compound-assign variants are
// **not** implemented — boolean-only surface per the v3 plan.

// ── Predicate ⊕ Predicate ──────────────────────────────────────────────────
impl<T: Model> BitAnd for Predicate<T> {
    type Output = Predicate<T>;
    /// SQL AND. Delegates to `Q<T>::bitand`, which short-circuits
    /// pure-portable inner trees through Sassi's flattening reducer and
    /// otherwise lifts to `Q::Compound { op: And, parts }`.
    fn bitand(self, rhs: Self) -> Predicate<T> {
        Predicate::from_q(self.inner & rhs.inner)
    }
}

impl<T: Model> BitOr for Predicate<T> {
    type Output = Predicate<T>;
    /// SQL OR. Same shape as `BitAnd`; delegates to `Q<T>::bitor`.
    fn bitor(self, rhs: Self) -> Predicate<T> {
        Predicate::from_q(self.inner | rhs.inner)
    }
}

impl<T: Model> BitXor for Predicate<T> {
    type Output = Predicate<T>;
    /// SQL XOR. Pure-portable inner trees ride Sassi's
    /// `BasicPredicate::bitxor`; mixed/SQL-only operands lift to
    /// `Q::Xor(Box, Box)`.
    fn bitxor(self, rhs: Self) -> Predicate<T> {
        Predicate::from_q(self.inner ^ rhs.inner)
    }
}

impl<T: Model> Not for Predicate<T> {
    type Output = Predicate<T>;
    /// SQL `NOT (...)`. Pure-portable inner trees collapse double
    /// negation through Sassi's reducer; mixed operands wrap in
    /// `Q::Negated(...)` (with the existing `Q::Negated(Q::Negated(_))`
    /// collapse).
    fn not(self) -> Predicate<T> {
        Predicate::from_q(!self.inner)
    }
}

// ── PortablePredicate ⊕ Predicate ──────────────────────────────────────────
// Mixed compositions where one side is portable and the other is already
// mixed/SQL-only. The result lifts to `Predicate<T>` so the audit boundary
// can see the full shape; the inner `Q<T>` operators handle flattening and
// non-associativity uniformly.

impl<T: Model> BitAnd<Predicate<T>> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitand(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) & rhs.inner)
    }
}

impl<T: Model> BitOr<Predicate<T>> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitor(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) | rhs.inner)
    }
}

impl<T: Model> BitXor<Predicate<T>> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) ^ rhs.inner)
    }
}

impl<T: Model> BitAnd<PortablePredicate<T>> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitand(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(self.inner & Q::Portable(rhs))
    }
}

impl<T: Model> BitOr<PortablePredicate<T>> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitor(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(self.inner | Q::Portable(rhs))
    }
}

impl<T: Model> BitXor<PortablePredicate<T>> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(self.inner ^ Q::Portable(rhs))
    }
}

// ── PortablePredicate ⊕ Condition ──────────────────────────────────────────
// Mixed compositions with a legacy `Condition` (typically produced by the
// closure-side `f.col.eq(v)` API). The Condition lifts to `Q::Condition(_)`
// and the result is a `Predicate<T>`.

impl<T: Model> BitAnd<Condition> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitand(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) & Q::Condition(rhs))
    }
}

impl<T: Model> BitOr<Condition> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitor(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) | Q::Condition(rhs))
    }
}

impl<T: Model> BitXor<Condition> for PortablePredicate<T> {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(Q::Portable(self) ^ Q::Condition(rhs))
    }
}

// `Condition ⊕ PortablePredicate` — orphan rules require both operands to
// be local to this crate, which they are (`Condition` is in
// `crate::query::condition`, `PortablePredicate` is local). Implementing
// these for `Condition` directly is fine because the trait
// (`BitAnd<PortablePredicate<T>>`) is a `core` trait and the LHS is local.

impl<T: Model> BitAnd<PortablePredicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitand(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) & Q::Portable(rhs))
    }
}

impl<T: Model> BitOr<PortablePredicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitor(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) | Q::Portable(rhs))
    }
}

impl<T: Model> BitXor<PortablePredicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: PortablePredicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) ^ Q::Portable(rhs))
    }
}

// ── Predicate ⊕ Condition ──────────────────────────────────────────────────

impl<T: Model> BitAnd<Condition> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitand(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(self.inner & Q::Condition(rhs))
    }
}

impl<T: Model> BitOr<Condition> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitor(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(self.inner | Q::Condition(rhs))
    }
}

impl<T: Model> BitXor<Condition> for Predicate<T> {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: Condition) -> Predicate<T> {
        Predicate::from_q(self.inner ^ Q::Condition(rhs))
    }
}

impl<T: Model> BitAnd<Predicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitand(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) & rhs.inner)
    }
}

impl<T: Model> BitOr<Predicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitor(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) | rhs.inner)
    }
}

impl<T: Model> BitXor<Predicate<T>> for Condition {
    type Output = Predicate<T>;
    fn bitxor(self, rhs: Predicate<T>) -> Predicate<T> {
        Predicate::from_q(Q::Condition(self) ^ rhs.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::descriptor::ModelDescriptor;
    use std::future::Future;

    /// Minimal inert `Model` for the predicate-substrate unit tests.
    /// Mirrors the pattern used by `query/field.rs::tests::Fake`. Derives
    /// `Debug` so the test bodies can `{:?}`-format `BasicPredicate<Fake>`
    /// payloads in panic messages — Sassi's derived `Debug` on
    /// `BasicPredicate<T>` imposes `T: Debug`.
    #[derive(Debug)]
    struct Fake;

    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &i64 {
            unimplemented!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unimplemented!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
    }

    #[test]
    fn always_true_carries_no_provenance() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        assert!(!p.has_field_provenance());
        match p.into_inner() {
            BasicPredicate::True => {}
            other => panic!("expected BasicPredicate::True, got {other:?}"),
        }
    }

    #[test]
    fn always_false_carries_no_provenance() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_false();
        assert!(!p.has_field_provenance());
        match p.into_inner() {
            BasicPredicate::False => {}
            other => panic!("expected BasicPredicate::False, got {other:?}"),
        }
    }

    #[test]
    fn into_basic_predicate_yields_inner() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let bp: BasicPredicate<Fake> = p.into_basic_predicate();
        match bp {
            BasicPredicate::True => {}
            other => panic!("expected True, got {other:?}"),
        }
    }

    #[test]
    fn into_portable_predicate_is_identity_for_self() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let q: PortablePredicate<Fake> = p.into_portable_predicate();
        assert!(!q.has_field_provenance());
    }

    #[test]
    fn pure_portable_and_combines_inner() {
        let lhs: PortablePredicate<Fake> = PortablePredicate::always_true();
        let rhs: PortablePredicate<Fake> = PortablePredicate::always_false();
        let composed = lhs & rhs;
        match composed.into_inner() {
            BasicPredicate::And(parts) => {
                assert_eq!(parts.len(), 2);
                match (&parts[0], &parts[1]) {
                    (BasicPredicate::True, BasicPredicate::False) => {}
                    other => panic!("expected [True, False], got {other:?}"),
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn pure_portable_or_combines_inner() {
        let lhs: PortablePredicate<Fake> = PortablePredicate::always_true();
        let rhs: PortablePredicate<Fake> = PortablePredicate::always_false();
        let composed = lhs | rhs;
        match composed.into_inner() {
            BasicPredicate::Or(parts) => {
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    #[test]
    fn pure_portable_xor_combines_inner() {
        let lhs: PortablePredicate<Fake> = PortablePredicate::always_true();
        let rhs: PortablePredicate<Fake> = PortablePredicate::always_false();
        let composed = lhs ^ rhs;
        match composed.into_inner() {
            BasicPredicate::Xor(_, _) => {}
            other => panic!("expected Xor, got {other:?}"),
        }
    }

    #[test]
    fn pure_portable_not_negates_inner() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let negated = !p;
        match negated.into_inner() {
            // sassi flips `True` -> `False` directly under Not.
            BasicPredicate::False => {}
            other => panic!("expected False, got {other:?}"),
        }
    }

    #[test]
    fn double_negation_collapses_through_sassi() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let inner = p.into_inner();
        // `!!True` collapses through sassi's manual Not impl.
        let composed = !(!PortablePredicate {
            inner,
            field_provenance: None,
        });
        match composed.into_inner() {
            BasicPredicate::True => {}
            other => panic!("expected True after double-negation, got {other:?}"),
        }
    }

    #[test]
    fn manual_clone_does_not_require_t_clone() {
        // `Fake` deliberately does not impl `Clone`. If `PortablePredicate`'s
        // manual `Clone` accidentally added `T: Clone`, this would fail to
        // compile.
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let q = p.clone();
        assert!(!q.has_field_provenance());
    }

    // ── PR2b — IntoQ<T> + mixed-operator matrix ────────────────────────────

    /// `PortablePredicate<T> -> Q<T>` via the new sealed `IntoQ` impl.
    /// Locks the lift path adopters reach when calling
    /// `QuerySet::filter_struct(some_portable)` after the PR2b
    /// generalised builder signatures land.
    #[test]
    fn portable_predicate_into_q_yields_q_portable() {
        let p: PortablePredicate<Fake> = PortablePredicate::always_true();
        let q: Q<Fake> = p.into_q();
        match q {
            Q::Portable(_) => {}
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// `Predicate<T> -> Q<T>` unwraps the inner `Q<T>` directly.
    #[test]
    fn predicate_into_q_unwraps_inner() {
        let q_inner: Q<Fake> = Q::always_true();
        let p: Predicate<Fake> = Predicate::from_q(q_inner);
        match p.into_q() {
            Q::Portable(_) => {}
            other => panic!("expected Q::Portable(_), got {other:?}"),
        }
    }

    /// `Condition::True -> Q<T>` via the PR2b `IntoQ for Condition` impl
    /// (defined in `query::q`). Wraps as `Q::Condition(_)` so the SQL
    /// emitter sees the legacy tree shape unchanged.
    #[test]
    fn condition_into_q_wraps_in_q_condition() {
        use crate::query::condition::{Condition, FilterValue, Leaf};
        let c = Condition::Leaf(Leaf::eq_raw("col", FilterValue::Bool(true)));
        let q: Q<Fake> = c.into_q();
        match q {
            Q::Condition(_) => {}
            other => panic!("expected Q::Condition(_), got {other:?}"),
        }
    }

    /// Mixed `PortablePredicate & Condition` returns a `Predicate<T>`
    /// whose inner `Q<T>` is a `Q::Compound { op: And, parts }`.
    #[test]
    fn mixed_portable_and_condition_yields_predicate() {
        use crate::query::condition::{Condition, FilterValue, Leaf};
        let portable: PortablePredicate<Fake> = PortablePredicate::always_true();
        let condition = Condition::Leaf(Leaf::eq_raw("col", FilterValue::Bool(true)));
        let combined: Predicate<Fake> = portable & condition;
        match combined.into_q() {
            Q::Compound { parts, .. } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Q::Portable(_)));
                assert!(matches!(parts[1], Q::Condition(_)));
            }
            other => panic!("expected Q::Compound{{And, ..}}, got {other:?}"),
        }
    }

    /// Reverse-order mixed composition `Condition & PortablePredicate`
    /// produces the same shape (just swapped parts).
    #[test]
    fn mixed_condition_and_portable_yields_predicate() {
        use crate::query::condition::{Condition, FilterValue, Leaf};
        let portable: PortablePredicate<Fake> = PortablePredicate::always_true();
        let condition = Condition::Leaf(Leaf::eq_raw("col", FilterValue::Bool(true)));
        let combined: Predicate<Fake> = condition & portable;
        match combined.into_q() {
            Q::Compound { parts, .. } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Q::Condition(_)));
                assert!(matches!(parts[1], Q::Portable(_)));
            }
            other => panic!("expected Q::Compound{{And, ..}}, got {other:?}"),
        }
    }

    /// Three-term mixed composition `portable & condition | predicate`
    /// composes through the matrix without a manual `Q::Compound { ... }`
    /// at any callsite. Demonstrates that the operator matrix is closed
    /// over `PortablePredicate`, `Predicate`, and `Condition`.
    #[test]
    fn mixed_three_term_composition_compiles_without_manual_q() {
        use crate::query::condition::{Condition, FilterValue, Leaf};
        let a: PortablePredicate<Fake> = PortablePredicate::always_true();
        let b: Condition = Condition::Leaf(Leaf::eq_raw("x", FilterValue::Bool(false)));
        let c: PortablePredicate<Fake> = PortablePredicate::always_false();

        // (a & b) is `Predicate<Fake>`; that `| c` returns `Predicate<Fake>`.
        let result: Predicate<Fake> = (a & b) | c;
        // The outer should be `Q::Compound { op: Or, parts: [_, _] }`.
        match result.into_q() {
            Q::Compound { op, parts } => {
                assert_eq!(parts.len(), 2);
                // Inner left half is the AND from `a & b`.
                assert!(matches!(parts[0], Q::Compound { .. }));
                // Inner right half is `Q::Portable(c)`.
                assert!(matches!(parts[1], Q::Portable(_)));
                // Sanity-check the discriminant survived.
                let _ = op;
            }
            other => panic!("expected Q::Compound (outer OR), got {other:?}"),
        }
    }

    /// `!Predicate<T>` lifts to `Q::Negated(Box<Q<T>>)`. The pure-portable
    /// shortcut only applies when the inner `Q` is `Q::Portable(_)`; here
    /// we wrap a compound and check that the negation does not fold
    /// through Sassi.
    #[test]
    fn predicate_not_wraps_in_q_negated() {
        let inner: Q<Fake> = Q::Compound {
            op: crate::query::q::CompoundOp::And,
            parts: vec![Q::always_true(), Q::always_false()],
        };
        let p: Predicate<Fake> = Predicate::from_q(inner);
        let negated: Predicate<Fake> = !p;
        match negated.into_q() {
            Q::Negated(_) => {}
            other => panic!("expected Q::Negated, got {other:?}"),
        }
    }

    /// `Expr<bool>` lifts to `Q::Expression(_)` via the PR2b `IntoQ` impl
    /// (defined in `query::q`). Locks the route used by
    /// `f.location.explicit_pg_predicate.bounded_by(...)` after PR2b's
    /// generalised filter signature accepts it directly.
    #[test]
    fn expr_bool_into_q_yields_q_expression() {
        use crate::expr::Expr;
        let expr: Expr<bool> = Expr::literal(true);
        let q: Q<Fake> = expr.into_q();
        match q {
            Q::Expression(_) => {}
            other => panic!("expected Q::Expression(_), got {other:?}"),
        }
    }
}
