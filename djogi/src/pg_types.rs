//! Postgres typed-surface newtypes — Phase 8.5 Cluster 4 (djogi#170 umbrella).
//!
//! This module owns the Rust newtypes that wrap Postgres column types
//! `postgres-types` does not encode natively (or whose native encoding
//! does not fit the typed surface djogi wants to expose). Each newtype
//! ships its own `postgres_types::{ToSql, FromSql}` impl against the raw
//! Postgres wire format — djogi pulls in no third-party crate to bridge
//! these types.
//!
//! # Types
//!
//! | Type            | Postgres column                                | Feature flag | Issue                            |
//! |-----------------|------------------------------------------------|--------------|----------------------------------|
//! | [`Interval`]    | `INTERVAL`                                     | (always on)  | djogi#212                        |
//! | [`Range<T>`]    | `int4range` / `int8range` / `numrange` /       | (always on)  | djogi#148 + #150 (Phase 8.5 G0)  |
//! |                 | `tstzrange` / `daterange`                      |              |                                  |
//!
//! Additional newtypes (`MacAddr` / `CidrAddr` for `MACADDR` / `CIDR`,
//! a typed `DOMAIN` discriminator, and similar coverage gaps) ship in
//! follow-on dispatches against the djogi#170 umbrella. The module is
//! structured so each newtype is independently `#[cfg]`-gateable when
//! it lands; the Interval and Range newtypes are unconditional because
//! the wire codecs depend only on stdlib byte primitives plus the
//! element type's own `ToSql` / `FromSql` impl.
//!
//! # Why hand-rolled wire codecs?
//!
//! Pulling in third-party crates (`pg_interval`, `cidr`, `eui48`, …)
//! would add transitive surface area for a few dozen lines of byte-level
//! wire encoding. The Postgres wire formats are stable, narrow, and
//! well-documented; reproducing them keeps djogi's dependency graph
//! shallow.
//!
//! # Path routing
//!
//! Adopter code reaches these types through `djogi::Interval` (re-exported
//! from `djogi::types`, also surfaced through `djogi::prelude::*`).
//! Macro-emitted code routes through `::djogi::*` per
//! `feedback_macro_path_routing.md`.

use bytes::{BufMut, BytesMut};
use postgres_types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use std::error::Error;

// ── Interval ────────────────────────────────────────────────────────────────

/// A Postgres `INTERVAL` value — djogi#212.
///
/// Represents a calendar duration as three independent fields:
///
/// - `months` — calendar months (a year is 12 months).
/// - `days` — calendar days (a day is NOT 24 hours; DST shifts move it
///   by an hour, leap seconds shift it by a fractional second).
/// - `microseconds` — sub-day time component.
///
/// # Why three fields?
///
/// Postgres `INTERVAL` is intrinsically a tagged three-tuple, not a
/// single duration. `2 days` and `48 hours` are NOT the same — adding
/// `2 days` to a `TIMESTAMPTZ` straddling a DST boundary yields a
/// different result than adding `48 hours`. Likewise `1 month` does
/// not have a fixed microsecond count (28 / 29 / 30 / 31 days
/// depending on the anchor date). Collapsing the three components
/// into a single `time::Duration` would silently corrupt these
/// semantics. The newtype mirrors the Postgres wire format exactly:
/// adopters who need calendar arithmetic preserve the three-component
/// split; adopters who only ever use one component (e.g.
/// `microseconds_only(1_500_000)` for a 1.5 s interval) construct it
/// explicitly.
///
/// # Why not `time::Duration`?
///
/// `time::Duration` (from the `time` crate, djogi's pinned datetime
/// library) is a fixed-microsecond duration. There is no way to
/// encode `1 month` losslessly through it. Adopters who need a
/// time-only interval can use the [`Interval::microseconds_only`]
/// constructor.
///
/// # Wire format
///
/// Postgres `INTERVAL` is 16 bytes in binary wire format (see
/// `src/backend/utils/adt/timestamp.c::interval_send` in the Postgres
/// source — `pq_sendint64(time)` then `pq_sendint32(day)` then
/// `pq_sendint32(month)`, all big-endian):
///
/// | Bytes  | Field          | Encoding |
/// |--------|----------------|----------|
/// | 0..8   | `microseconds` | `i64` big-endian (sub-day time component) |
/// | 8..12  | `days`         | `i32` big-endian (whole days) |
/// | 12..16 | `months`       | `i32` big-endian (calendar months) |
///
/// The order in the wire format is `(microseconds, days, months)`, but
/// the Rust struct lists them in the order most adopters reach for them
/// when reading code (`months`, then `days`, then `microseconds`).
///
/// # Construction
///
/// ```rust
/// use djogi::Interval;
///
/// // 1 month, 2 days, 3.5 seconds
/// let mixed = Interval { months: 1, days: 2, microseconds: 3_500_000 };
///
/// // 90-day window (no month component)
/// let ninety_days = Interval::days_only(90);
///
/// // 1.5 second time-only interval
/// let one_and_a_half = Interval::microseconds_only(1_500_000);
/// ```
///
/// # Equality and ordering
///
/// ## Rust structural equality vs Postgres SQL `=`
///
/// **Rust `PartialEq` / `Eq` / `Hash` on `Interval` are structural.**
/// All three component fields must match byte-for-byte.
/// `Interval::months_only(1) == Interval::days_only(30)` is `false`
/// in Rust — the two values differ in the `months` and `days` fields
/// even if they nominally span "the same time" on most calendars.
/// `Hash` follows the same structural rule (Rust convention: equal
/// values must hash equal, and only structurally identical values are
/// equal here), so Rust-side hashmap keying is structural, never
/// linearized.
///
/// **Postgres SQL `=` on `INTERVAL` columns linearizes.**
/// Postgres converts each component before comparing: months are
/// treated as 30 days, and days are treated as 24 hours (86,400
/// seconds = 86,400,000,000 microseconds). The comparison is then
/// performed on the resulting total microsecond count. As a result,
/// `INTERVAL '1 month' = INTERVAL '30 days'` is `true` in Postgres
/// SQL.
///
/// **The practical implication for `QuerySet::filter`.** Calling
/// `QuerySet::filter(|f| f.duration().eq(Interval::months_only(1)))`
/// forwards to a Postgres `=` predicate, not to Rust `PartialEq`.
/// Rows whose stored duration is `Interval::days_only(30)` (or any
/// other combination that linearizes to 30 days × 86,400 s) will
/// match — even though `Interval::months_only(1) !=
/// Interval::days_only(30)` in Rust. Adopters who need only
/// structurally identical rows to match must add a client-side filter
/// after the fetch, or store the duration in a form that avoids
/// cross-component ambiguity (e.g. always use `microseconds_only`).
///
/// `Interval` does NOT implement `Ord` / `PartialOrd`: comparing
/// `1 month` against `30 days` is intrinsically ambiguous (depends on
/// which month). Adopters who need ordering can derive it on a wrapper
/// that fixes the comparison anchor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Interval {
    /// Calendar months. A year is `months: 12`.
    pub months: i32,
    /// Calendar days. NOT 86_400 seconds — see the type docs.
    pub days: i32,
    /// Sub-day time component in microseconds.
    pub microseconds: i64,
}

impl Interval {
    /// Construct an `Interval` from three explicit components.
    pub const fn new(months: i32, days: i32, microseconds: i64) -> Self {
        Self {
            months,
            days,
            microseconds,
        }
    }

    /// Construct an interval with only the `months` component populated.
    pub const fn months_only(months: i32) -> Self {
        Self {
            months,
            days: 0,
            microseconds: 0,
        }
    }

    /// Construct an interval with only the `days` component populated.
    pub const fn days_only(days: i32) -> Self {
        Self {
            months: 0,
            days,
            microseconds: 0,
        }
    }

    /// Construct an interval with only the `microseconds` (sub-day time)
    /// component populated.
    pub const fn microseconds_only(microseconds: i64) -> Self {
        Self {
            months: 0,
            days: 0,
            microseconds,
        }
    }
}

impl ToSql for Interval {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // Postgres INTERVAL binary wire format: 16 bytes, big-endian.
        //
        //   bytes 0..8   microseconds : i64 (sub-day time)
        //   bytes 8..12  days         : i32
        //   bytes 12..16 months       : i32
        //
        // `BytesMut::put_i64` / `put_i32` write big-endian by default,
        // which matches the Postgres binary protocol.
        out.put_i64(self.microseconds);
        out.put_i32(self.days);
        out.put_i32(self.months);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }

    to_sql_checked!();
}

impl<'a> FromSql<'a> for Interval {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        if raw.len() != 16 {
            return Err(format!(
                "Postgres INTERVAL wire format must be exactly 16 bytes, got {}",
                raw.len()
            )
            .into());
        }
        let microseconds = i64::from_be_bytes(raw[0..8].try_into().unwrap());
        let days = i32::from_be_bytes(raw[8..12].try_into().unwrap());
        let months = i32::from_be_bytes(raw[12..16].try_into().unwrap());
        Ok(Interval {
            months,
            days,
            microseconds,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }
}

impl crate::descriptor::DjogiSqlType for Interval {
    const SQL_TYPE: &'static str = "INTERVAL";
}

// ── Range / RangeBound (Phase 8.5 G0 — djogi#148 + #150 substrate) ──────────
//
// Postgres range types (`int4range`, `int8range`, `numrange`, `tstzrange`,
// `daterange`) carry a tagged bound shape: each side of the range is
// inclusive, exclusive, or unbounded, with a separate "empty range" sentinel
// that has no bounds at all. The Rust type mirrors that shape exactly so
// adopters never need to invent their own range encoding when declaring
// `pub period: Range<DateTime>` on a `#[model]` struct.
//
// G0 ships ONLY the substrate: the Rust type, the wire codec, and the
// descriptor lowering hook. The downstream lanes are explicitly future
// work and intentionally *not* included here:
//
// * djogi#148 — `btree_gist` EXCLUDE constraint grammar (`#[model(exclude(
//   ...))]`), the `&&` overlap operator surface, and the `CREATE EXTENSION
//   btree_gist` migration step.
// * djogi#150 — PostgreSQL 18 temporal-constraint DDL (`WITHOUT OVERLAPS`,
//   `PERIOD` foreign keys, `NOT ENFORCED`, named `NOT NULL`).
//
// Both lanes consume range columns as their input. Centralising the
// `Range<T>` shape here keeps the two future lanes from diverging on
// what "a range column" looks like.
//
// # No third-party crate
//
// `postgres-types` exports the range *type OIDs* as `Type::INT4_RANGE`,
// `Type::TSTZ_RANGE`, etc. but does not provide a generic Rust `Range<T>`
// codec. The published `postgres-range` crate is a third-party adapter,
// pinned to its own type design, and unmaintained against the latest
// `postgres-types` major. We hand-roll a 30-line wire codec instead —
// no transitive dependency surface, no version pin to worry about.

/// One end of a [`Range`] — inclusive of the named value, exclusive of
/// the named value, or unbounded on this side.
///
/// The variant set matches Postgres's range-bound semantics directly:
///
/// * `Inclusive(t)` — the bound value `t` is part of the range.
///   Renders as `[t,…]` (lower) or `[…,t]` (upper).
/// * `Exclusive(t)` — the bound value `t` is *not* part of the range.
///   Renders as `(t,…]` (lower) or `[…,t)` (upper).
/// * `Unbounded` — no bound on this side. Postgres terminology calls
///   this "infinite"; the wire format sets the `RANGE_LB_INF` /
///   `RANGE_UB_INF` flag and omits the bound bytes entirely.
///
/// The empty range is *not* a special `RangeBound` value — it is a
/// separate state on the enclosing [`Range`]. Use [`Range::empty`] when
/// you need the empty-range sentinel; an empty range carries no bound
/// values regardless of which `RangeBound` variants you started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RangeBound<T> {
    /// `[t,…]` / `[…,t]` — the bound value is part of the range.
    Inclusive(T),
    /// `(t,…]` / `[…,t)` — the bound value is *not* part of the range.
    Exclusive(T),
    /// No bound on this side. Postgres wire format calls this "infinite".
    Unbounded,
}

/// A Postgres range value over an element type `T`.
///
/// Mirrors Postgres's tagged range layout: each side carries an
/// inclusive / exclusive / unbounded bound, with a separate `empty`
/// flag for the empty-range sentinel. A range with `empty = true`
/// carries no bound values regardless of how it was constructed —
/// [`Range::empty`] returns the canonical empty range, and the wire
/// codec collapses any in-memory bound values when the empty flag is
/// set.
///
/// # Construction
///
/// ```rust
/// use djogi::{Range, RangeBound};
///
/// // [1, 10) — the canonical "discrete" range shape (lower-inclusive,
/// // upper-exclusive). The most common adopter shape for integer ranges.
/// let r: Range<i32> = Range::inclusive_exclusive(1, 10);
///
/// // [t, ∞) — bounded below, unbounded above.
/// let from5: Range<i32> = Range::new(RangeBound::Inclusive(5), RangeBound::Unbounded);
///
/// // Empty range.
/// let nothing: Range<i32> = Range::empty();
/// ```
///
/// # Equality and ordering
///
/// `PartialEq` / `Eq` / `Hash` are structural — two `Range<T>` values
/// compare equal only when their `empty` flag and bound shapes match
/// byte-for-byte. Postgres `=` on range columns has different
/// semantics (range canonicalization on discrete subtypes can render
/// `[1,9]` and `[1,10)` equal at the SQL level for `int4range`); the
/// Rust structural comparison does *not* canonicalize. Adopters who
/// need SQL-level equivalence should route through Postgres `=` via
/// `QuerySet::filter`.
///
/// `Range` does not implement `Ord` / `PartialOrd` — comparing two
/// ranges that overlap at one endpoint but differ in inclusivity is
/// ambiguous, and the framework refuses to pick an interpretation on
/// the adopter's behalf.
///
/// # Wire format
///
/// Postgres range binary wire format (see `range_send` in
/// `src/backend/utils/adt/rangetypes.c`):
///
/// | Bytes               | Field        | Encoding |
/// |---------------------|--------------|----------|
/// | 0                   | flags        | `u8` — see [`RangeFlags`] private constants |
/// | (if lower finite)   | lower length | `i32` big-endian, byte count of lower bound |
/// | (if lower finite)   | lower bytes  | Postgres binary repr of `T` for the element type |
/// | (if upper finite)   | upper length | `i32` big-endian, byte count of upper bound |
/// | (if upper finite)   | upper bytes  | Postgres binary repr of `T` for the element type |
///
/// The empty range writes a single flag byte (`0x01`) and no bounds.
/// Unbounded ends set the `RANGE_LB_INF` / `RANGE_UB_INF` flag and
/// omit the bound bytes. The wire encoding for the element type `T`
/// is whatever `T::to_sql` produces for the element-type `&Type` —
/// e.g. `i32` writes 4 big-endian bytes for `INT4`, `OffsetDateTime`
/// writes 8 big-endian microseconds-since-2000 bytes for `TIMESTAMPTZ`.
///
/// # Future siblings — DB-level no-overlap
///
/// `Range<T>` is the substrate shared by two future DB-level
/// no-overlap surfaces (Phase 8.5 G0 establishes the substrate; the
/// downstream lanes are tracked separately):
///
/// * **djogi#148** — `btree_gist` EXCLUDE constraint grammar
///   (`#[model(exclude(...))]`), the `&&` overlap operator, and
///   `CREATE EXTENSION btree_gist`. The general-purpose no-overlap
///   mechanism that works on every supported Postgres version.
/// * **djogi#150** — PostgreSQL 18 temporal-constraint DDL
///   (`WITHOUT OVERLAPS`, `PERIOD` foreign keys, `NOT ENFORCED`,
///   named `NOT NULL`). The modern SQL-standard no-overlap mechanism
///   on PG18+.
///
/// Both lanes consume `Range<T>` columns as their inputs. Neither
/// lane is shipped by G0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range<T> {
    lower: RangeBound<T>,
    upper: RangeBound<T>,
    empty: bool,
}

impl<T> Range<T> {
    /// Construct the empty range — the Postgres `'empty'::<subtype>range`
    /// value.
    ///
    /// The empty range has no bounds; both `lower` and `upper` are set
    /// to [`RangeBound::Unbounded`] for consistency, and the `empty`
    /// flag suppresses any bound serialization in the wire codec.
    ///
    /// `Range::empty()` is the default value for `Range<T>` regardless
    /// of `T` — see the [`Default`] impl below.
    pub const fn empty() -> Self {
        Self {
            lower: RangeBound::Unbounded,
            upper: RangeBound::Unbounded,
            empty: true,
        }
    }

    /// Construct a non-empty range from explicit bounds.
    ///
    /// The framework does **not** validate that `lower <= upper`.
    /// Postgres rejects such ranges at write time with
    /// `ERROR: range lower bound must be less than or equal to range upper bound`;
    /// the Rust type defers ordering judgement to Postgres so it
    /// stays valid for adopter-defined element types whose ordering
    /// the framework cannot know.
    ///
    /// Use [`Range::empty`] for the empty range — passing
    /// [`RangeBound::Unbounded`] for both sides produces `(-∞, +∞)`,
    /// **not** the empty range.
    pub const fn new(lower: RangeBound<T>, upper: RangeBound<T>) -> Self {
        Self {
            lower,
            upper,
            empty: false,
        }
    }

    /// `true` if this range is the empty-range sentinel.
    ///
    /// Empty ranges never carry bound values regardless of how they
    /// were constructed; an in-memory empty range with non-`Unbounded`
    /// bound variants is normalised to no-bounds when serialised.
    pub const fn is_empty(&self) -> bool {
        self.empty
    }

    /// The lower bound of the range. Always [`RangeBound::Unbounded`]
    /// when [`Range::is_empty`] returns `true`.
    pub const fn lower(&self) -> &RangeBound<T> {
        &self.lower
    }

    /// The upper bound of the range. Always [`RangeBound::Unbounded`]
    /// when [`Range::is_empty`] returns `true`.
    pub const fn upper(&self) -> &RangeBound<T> {
        &self.upper
    }
}

impl<T> Range<T> {
    /// `[lo, hi]` — both endpoints inclusive.
    ///
    /// Postgres canonicalises discrete-subtype ranges (`int4range`,
    /// `int8range`, `daterange`) to lower-inclusive / upper-exclusive
    /// at storage time, so a stored `int4range '[1,9]'` is equal to
    /// `int4range '[1,10)'` at the SQL level. The Rust type preserves
    /// what you passed in; the canonicalisation happens server-side.
    pub fn inclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Inclusive(lo), RangeBound::Inclusive(hi))
    }

    /// `(lo, hi)` — both endpoints exclusive.
    pub fn exclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Exclusive(lo), RangeBound::Exclusive(hi))
    }

    /// `[lo, hi)` — lower inclusive, upper exclusive.
    ///
    /// The canonical "discrete" range shape. Postgres canonicalises
    /// every discrete range to this form at storage time, so adopters
    /// who want their in-memory `Range` shape to match what comes back
    /// out of a `SELECT` reach for this constructor.
    pub fn inclusive_exclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Inclusive(lo), RangeBound::Exclusive(hi))
    }

    /// `(lo, hi]` — lower exclusive, upper inclusive.
    pub fn exclusive_inclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Exclusive(lo), RangeBound::Inclusive(hi))
    }
}

impl<T> Default for Range<T> {
    /// `Default` returns the empty range — the only `Range<T>` shape
    /// that does not depend on `T`. Adopters who want `[0, 0)` or
    /// similar must construct it explicitly.
    fn default() -> Self {
        Self::empty()
    }
}

/// Maps a Rust element type to the Postgres range type and bound
/// element type used to serialise a [`Range`] of that element type.
///
/// One impl per supported subtype:
///
/// | Rust element type            | Postgres range type | Bound element type |
/// |------------------------------|---------------------|--------------------|
/// | `i32`                        | `int4range`         | `INT4`             |
/// | `i64`                        | `int8range`         | `INT8`             |
/// | `rust_decimal::Decimal`      | `numrange`          | `NUMERIC`          |
/// | `time::OffsetDateTime`       | `tstzrange`         | `TIMESTAMPTZ`      |
/// | `time::Date`                 | `daterange`         | `DATE`             |
///
/// `tsrange` (timestamp-without-timezone) is intentionally not
/// supported — Djogi pins to `TIMESTAMPTZ` exclusively for temporal
/// columns (see `docs/spec/decisions.md` for the rationale), so
/// adopters reach for `Range<DateTime>` (which lowers to `tstzrange`)
/// rather than juggling the timezone-stripped variant.
///
/// The trait is open for future extensions but the only implementors
/// shipping in G0 are the five rows above. Because `Range<T>` has a
/// blanket `DjogiSqlType` impl for every `T: RangeSubtype`, adopters
/// who add a custom range subtype only need to implement this trait
/// for the element type; the `Range<T>` descriptor SQL type then
/// follows from `T::PG_RANGE_NAME`.
pub trait RangeSubtype: Sized {
    /// The Postgres range type the wire codec accepts via
    /// `ToSql::accepts` / `FromSql::accepts`.
    fn pg_range_type() -> Type;

    /// The Postgres element type used when the wire codec calls
    /// `T::to_sql` / `T::from_sql` on each bound.
    fn pg_element_type() -> Type;

    /// Canonical uppercase Postgres range type name — used by the
    /// [`crate::descriptor::DjogiSqlType::SQL_TYPE`] impls to render
    /// the column-type string emitted in `CREATE TABLE` and the
    /// migration snapshot. Examples: `"INT4RANGE"`, `"TSTZRANGE"`.
    const PG_RANGE_NAME: &'static str;
}

impl RangeSubtype for i32 {
    fn pg_range_type() -> Type {
        Type::INT4_RANGE
    }
    fn pg_element_type() -> Type {
        Type::INT4
    }
    const PG_RANGE_NAME: &'static str = "INT4RANGE";
}

impl RangeSubtype for i64 {
    fn pg_range_type() -> Type {
        Type::INT8_RANGE
    }
    fn pg_element_type() -> Type {
        Type::INT8
    }
    const PG_RANGE_NAME: &'static str = "INT8RANGE";
}

impl RangeSubtype for rust_decimal::Decimal {
    fn pg_range_type() -> Type {
        Type::NUM_RANGE
    }
    fn pg_element_type() -> Type {
        Type::NUMERIC
    }
    const PG_RANGE_NAME: &'static str = "NUMRANGE";
}

impl RangeSubtype for time::OffsetDateTime {
    fn pg_range_type() -> Type {
        Type::TSTZ_RANGE
    }
    fn pg_element_type() -> Type {
        Type::TIMESTAMPTZ
    }
    const PG_RANGE_NAME: &'static str = "TSTZRANGE";
}

impl RangeSubtype for time::Date {
    fn pg_range_type() -> Type {
        Type::DATE_RANGE
    }
    fn pg_element_type() -> Type {
        Type::DATE
    }
    const PG_RANGE_NAME: &'static str = "DATERANGE";
}

// ── DjogiSqlType — descriptor lowering for each Range<T> instantiation ─────

impl<T: RangeSubtype> crate::descriptor::DjogiSqlType for Range<T> {
    const SQL_TYPE: &'static str = T::PG_RANGE_NAME;
}

// ── Wire codec helpers ──────────────────────────────────────────────────────
//
// The flag bit layout matches Postgres's `rangetypes.h` definitions —
// see `RANGE_EMPTY`, `RANGE_LB_INC`, `RANGE_UB_INC`, `RANGE_LB_INF`,
// `RANGE_UB_INF`. The `RANGE_LB_NULL` / `RANGE_UB_NULL` flags are
// historical (Postgres deprecated NULL bounds long ago) and never set
// on output — we treat them as a decode-time error so unexpected
// upstream behaviour surfaces loudly rather than silently corrupting
// bound values.

const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;
const RANGE_LB_NULL: u8 = 0x20;
const RANGE_UB_NULL: u8 = 0x40;
// `RANGE_CONTAIN_EMPTY` (0x80) is a GiST-internal flag that never
// appears on output; we don't reference it because Postgres strips it
// before the wire encoding.

fn encode_range<T>(
    range: &Range<T>,
    element_ty: &Type,
    out: &mut BytesMut,
) -> Result<IsNull, Box<dyn Error + Sync + Send>>
where
    T: ToSql,
{
    if range.empty {
        out.put_u8(RANGE_EMPTY);
        return Ok(IsNull::No);
    }

    let mut flags = 0u8;
    match &range.lower {
        RangeBound::Inclusive(_) => flags |= RANGE_LB_INC,
        RangeBound::Exclusive(_) => {}
        RangeBound::Unbounded => flags |= RANGE_LB_INF,
    }
    match &range.upper {
        RangeBound::Inclusive(_) => flags |= RANGE_UB_INC,
        RangeBound::Exclusive(_) => {}
        RangeBound::Unbounded => flags |= RANGE_UB_INF,
    }
    out.put_u8(flags);

    // Lower bound bytes (only if finite).
    if let RangeBound::Inclusive(v) | RangeBound::Exclusive(v) = &range.lower {
        encode_bound(v, element_ty, out)?;
    }
    // Upper bound bytes (only if finite).
    if let RangeBound::Inclusive(v) | RangeBound::Exclusive(v) = &range.upper {
        encode_bound(v, element_ty, out)?;
    }
    Ok(IsNull::No)
}

fn encode_bound<T: ToSql>(
    value: &T,
    element_ty: &Type,
    out: &mut BytesMut,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    // Postgres wire format: i32 BE length prefix, then the bound
    // bytes. The length is the byte count of the bound, not including
    // the length prefix itself. We reserve four bytes for the length,
    // call into `T::to_sql` to fill in the bound bytes, then backpatch
    // the length once we know how many bytes the bound consumed.
    let len_pos = out.len();
    out.put_i32(0);
    let value_start = out.len();
    match value.to_sql(element_ty, out)? {
        IsNull::No => {}
        IsNull::Yes => {
            // Postgres range bounds cannot be NULL — the deprecated
            // `RANGE_LB_NULL` / `RANGE_UB_NULL` flags are never set
            // on output. If `T::to_sql` returns `IsNull::Yes` for a
            // bound, that's a programming error (an `Option<T>` slipped
            // in where a `T` was expected). Reject loudly rather than
            // silently producing a malformed range.
            return Err(
                "Postgres range bounds cannot be NULL; use RangeBound::Unbounded for an open end"
                    .into(),
            );
        }
    }
    let value_len = i32::try_from(out.len() - value_start)
        .map_err(|_| "range bound length exceeds i32::MAX bytes")?;
    out[len_pos..len_pos + 4].copy_from_slice(&value_len.to_be_bytes());
    Ok(())
}

fn decode_range<'a, T>(
    mut raw: &'a [u8],
    element_ty: &Type,
) -> Result<Range<T>, Box<dyn Error + Sync + Send>>
where
    T: FromSql<'a>,
{
    if raw.is_empty() {
        return Err("Postgres range wire format must contain at least one flags byte".into());
    }
    let flags = raw[0];
    raw = &raw[1..];

    if flags & RANGE_EMPTY != 0 {
        // Empty ranges carry no bound bytes; reject any trailing
        // payload so a malformed wire response surfaces loudly.
        if !raw.is_empty() {
            return Err(format!(
                "Postgres range wire format: empty-range flag set but {} trailing bytes present",
                raw.len()
            )
            .into());
        }
        return Ok(Range::empty());
    }

    if flags & RANGE_LB_NULL != 0 || flags & RANGE_UB_NULL != 0 {
        // NULL-bound flags are historical and never set on output by
        // any supported Postgres version. If they appear, the upstream
        // is misbehaving; surface that loudly rather than guessing.
        return Err(format!(
            "Postgres range wire format: NULL-bound flag set (flags = 0x{flags:02x}); not supported"
        )
        .into());
    }

    let lower = if flags & RANGE_LB_INF != 0 {
        RangeBound::Unbounded
    } else {
        let (value, rest) = decode_bound::<T>(raw, element_ty)?;
        raw = rest;
        if flags & RANGE_LB_INC != 0 {
            RangeBound::Inclusive(value)
        } else {
            RangeBound::Exclusive(value)
        }
    };

    let upper = if flags & RANGE_UB_INF != 0 {
        RangeBound::Unbounded
    } else {
        let (value, rest) = decode_bound::<T>(raw, element_ty)?;
        raw = rest;
        if flags & RANGE_UB_INC != 0 {
            RangeBound::Inclusive(value)
        } else {
            RangeBound::Exclusive(value)
        }
    };

    if !raw.is_empty() {
        return Err(format!(
            "Postgres range wire format: {} trailing bytes after upper bound",
            raw.len()
        )
        .into());
    }
    Ok(Range::new(lower, upper))
}

fn decode_bound<'a, T: FromSql<'a>>(
    buf: &'a [u8],
    element_ty: &Type,
) -> Result<(T, &'a [u8]), Box<dyn Error + Sync + Send>> {
    if buf.len() < 4 {
        return Err("Postgres range wire format: bound length prefix truncated".into());
    }
    let len = i32::from_be_bytes(buf[..4].try_into().unwrap());
    if len < 0 {
        return Err(format!("Postgres range wire format: negative bound length {len}").into());
    }
    let len = len as usize;
    let buf = &buf[4..];
    if buf.len() < len {
        return Err(format!(
            "Postgres range wire format: bound length says {len} bytes but only {} available",
            buf.len()
        )
        .into());
    }
    let body = &buf[..len];
    let rest = &buf[len..];
    let value = T::from_sql(element_ty, body)?;
    Ok((value, rest))
}

// ── ToSql / FromSql for Range<T> ────────────────────────────────────────────

impl<T> ToSql for Range<T>
where
    T: ToSql + RangeSubtype + std::fmt::Debug,
{
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        encode_range(self, &T::pg_element_type(), out)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == T::pg_range_type()
    }

    to_sql_checked!();
}

impl<'a, T> FromSql<'a> for Range<T>
where
    T: FromSql<'a> + RangeSubtype + std::fmt::Debug,
{
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        decode_range::<T>(raw, &T::pg_element_type())
    }

    fn accepts(ty: &Type) -> bool {
        *ty == T::pg_range_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_constructors_set_only_the_named_field() {
        assert_eq!(
            Interval::months_only(5),
            Interval {
                months: 5,
                days: 0,
                microseconds: 0
            }
        );
        assert_eq!(
            Interval::days_only(10),
            Interval {
                months: 0,
                days: 10,
                microseconds: 0
            }
        );
        assert_eq!(
            Interval::microseconds_only(1_500_000),
            Interval {
                months: 0,
                days: 0,
                microseconds: 1_500_000
            }
        );
    }

    #[test]
    fn interval_to_sql_writes_16_bytes_big_endian_us_days_months() {
        let iv = Interval::new(13, 7, 1_500_000);
        let mut buf = BytesMut::new();
        iv.to_sql(&Type::INTERVAL, &mut buf).unwrap();
        assert_eq!(buf.len(), 16);
        // microseconds: i64 BE
        assert_eq!(&buf[..8], &1_500_000i64.to_be_bytes());
        // days: i32 BE
        assert_eq!(&buf[8..12], &7i32.to_be_bytes());
        // months: i32 BE
        assert_eq!(&buf[12..16], &13i32.to_be_bytes());
    }

    #[test]
    fn interval_round_trip_through_wire_codec() {
        let iv = Interval::new(-3, 42, -999_999);
        let mut buf = BytesMut::new();
        iv.to_sql(&Type::INTERVAL, &mut buf).unwrap();
        let decoded = Interval::from_sql(&Type::INTERVAL, &buf).unwrap();
        assert_eq!(decoded, iv);
    }

    #[test]
    fn interval_from_sql_rejects_wrong_length() {
        let err = Interval::from_sql(&Type::INTERVAL, &[0u8; 15]).unwrap_err();
        assert!(
            err.to_string().contains("16 bytes"),
            "error must name the expected length, got: {err}"
        );
    }

    #[test]
    fn interval_accepts_only_interval_type() {
        // ToSql::accepts gate
        assert!(<Interval as ToSql>::accepts(&Type::INTERVAL));
        assert!(!<Interval as ToSql>::accepts(&Type::INT8));
        assert!(!<Interval as ToSql>::accepts(&Type::TIMESTAMPTZ));
        // FromSql::accepts gate
        assert!(<Interval as FromSql>::accepts(&Type::INTERVAL));
        assert!(!<Interval as FromSql>::accepts(&Type::INT8));
    }

    // ── Range / RangeBound (Phase 8.5 G0 — djogi#148 + #150 substrate) ──────

    #[test]
    fn range_empty_constructor_carries_unbounded_sides_and_empty_flag() {
        let r: Range<i32> = Range::empty();
        assert!(r.is_empty());
        assert_eq!(*r.lower(), RangeBound::Unbounded);
        assert_eq!(*r.upper(), RangeBound::Unbounded);
    }

    #[test]
    fn range_default_is_empty_for_any_element_type() {
        // `Default` does not constrain `T`; the empty range carries
        // no bound values.
        assert!(<Range<i32> as Default>::default().is_empty());
        assert!(<Range<i64> as Default>::default().is_empty());
        assert!(<Range<rust_decimal::Decimal> as Default>::default().is_empty());
        assert!(<Range<time::Date> as Default>::default().is_empty());
        assert!(<Range<time::OffsetDateTime> as Default>::default().is_empty());
    }

    #[test]
    fn range_inclusive_exclusive_constructor_is_lower_inclusive_upper_exclusive() {
        let r: Range<i32> = Range::inclusive_exclusive(1, 10);
        assert!(!r.is_empty());
        assert_eq!(*r.lower(), RangeBound::Inclusive(1));
        assert_eq!(*r.upper(), RangeBound::Exclusive(10));
    }

    #[test]
    fn range_inclusive_inclusive_constructor_carries_both_inclusive_bounds() {
        let r: Range<i32> = Range::inclusive(0, 100);
        assert_eq!(*r.lower(), RangeBound::Inclusive(0));
        assert_eq!(*r.upper(), RangeBound::Inclusive(100));
    }

    #[test]
    fn range_exclusive_exclusive_constructor_carries_both_exclusive_bounds() {
        let r: Range<i32> = Range::exclusive(0, 100);
        assert_eq!(*r.lower(), RangeBound::Exclusive(0));
        assert_eq!(*r.upper(), RangeBound::Exclusive(100));
    }

    #[test]
    fn range_exclusive_inclusive_constructor_is_lower_exclusive_upper_inclusive() {
        let r: Range<i32> = Range::exclusive_inclusive(0, 100);
        assert_eq!(*r.lower(), RangeBound::Exclusive(0));
        assert_eq!(*r.upper(), RangeBound::Inclusive(100));
    }

    #[test]
    fn range_new_accepts_unbounded_on_both_sides_and_is_not_empty() {
        // (-∞, +∞) is NOT the empty range; it is the universal range.
        let universal: Range<i32> = Range::new(RangeBound::Unbounded, RangeBound::Unbounded);
        assert!(!universal.is_empty());
        assert_eq!(*universal.lower(), RangeBound::Unbounded);
        assert_eq!(*universal.upper(), RangeBound::Unbounded);
    }

    #[test]
    fn range_subtype_mapping_matches_postgres_type_constants() {
        // Pin every shipped subtype's range-type / element-type wiring
        // so a future swap (e.g. accidental TS_RANGE → TSTZ_RANGE)
        // surfaces here rather than silently corrupting wire codecs.
        assert_eq!(<i32 as RangeSubtype>::pg_range_type(), Type::INT4_RANGE);
        assert_eq!(<i32 as RangeSubtype>::pg_element_type(), Type::INT4);
        assert_eq!(<i64 as RangeSubtype>::pg_range_type(), Type::INT8_RANGE);
        assert_eq!(<i64 as RangeSubtype>::pg_element_type(), Type::INT8);
        assert_eq!(
            <rust_decimal::Decimal as RangeSubtype>::pg_range_type(),
            Type::NUM_RANGE
        );
        assert_eq!(
            <rust_decimal::Decimal as RangeSubtype>::pg_element_type(),
            Type::NUMERIC
        );
        assert_eq!(
            <time::OffsetDateTime as RangeSubtype>::pg_range_type(),
            Type::TSTZ_RANGE
        );
        assert_eq!(
            <time::OffsetDateTime as RangeSubtype>::pg_element_type(),
            Type::TIMESTAMPTZ
        );
        assert_eq!(
            <time::Date as RangeSubtype>::pg_range_type(),
            Type::DATE_RANGE
        );
        assert_eq!(<time::Date as RangeSubtype>::pg_element_type(), Type::DATE);
    }

    #[test]
    fn range_djogi_sql_type_renders_canonical_uppercase_name() {
        use crate::descriptor::DjogiSqlType;
        assert_eq!(<Range<i32> as DjogiSqlType>::SQL_TYPE, "INT4RANGE");
        assert_eq!(<Range<i64> as DjogiSqlType>::SQL_TYPE, "INT8RANGE");
        assert_eq!(
            <Range<rust_decimal::Decimal> as DjogiSqlType>::SQL_TYPE,
            "NUMRANGE"
        );
        assert_eq!(
            <Range<time::OffsetDateTime> as DjogiSqlType>::SQL_TYPE,
            "TSTZRANGE"
        );
        assert_eq!(<Range<time::Date> as DjogiSqlType>::SQL_TYPE, "DATERANGE");
    }

    #[test]
    fn range_to_sql_empty_writes_single_flag_byte() {
        let r: Range<i32> = Range::empty();
        let mut buf = BytesMut::new();
        r.to_sql(&Type::INT4_RANGE, &mut buf).unwrap();
        assert_eq!(buf.as_ref(), &[RANGE_EMPTY]);
    }

    #[test]
    fn range_to_sql_inclusive_exclusive_writes_flags_then_two_int4_bounds() {
        let r: Range<i32> = Range::inclusive_exclusive(1, 10);
        let mut buf = BytesMut::new();
        r.to_sql(&Type::INT4_RANGE, &mut buf).unwrap();
        // flag byte (LB_INC, UB_INC bit is 0 because upper is exclusive)
        assert_eq!(buf[0], RANGE_LB_INC);
        // i32 length prefix = 4 bytes, then i32 BE bound (1)
        assert_eq!(&buf[1..5], &4i32.to_be_bytes());
        assert_eq!(&buf[5..9], &1i32.to_be_bytes());
        // upper bound length + value (10)
        assert_eq!(&buf[9..13], &4i32.to_be_bytes());
        assert_eq!(&buf[13..17], &10i32.to_be_bytes());
        assert_eq!(buf.len(), 17);
    }

    #[test]
    fn range_to_sql_unbounded_lower_omits_lower_bytes_and_sets_lb_inf() {
        let r: Range<i32> = Range::new(RangeBound::Unbounded, RangeBound::Inclusive(5));
        let mut buf = BytesMut::new();
        r.to_sql(&Type::INT4_RANGE, &mut buf).unwrap();
        // LB_INF (no lower bytes) + UB_INC (upper inclusive, value follows)
        assert_eq!(buf[0], RANGE_LB_INF | RANGE_UB_INC);
        assert_eq!(&buf[1..5], &4i32.to_be_bytes());
        assert_eq!(&buf[5..9], &5i32.to_be_bytes());
        assert_eq!(buf.len(), 9);
    }

    #[test]
    fn range_to_sql_unbounded_upper_omits_upper_bytes_and_sets_ub_inf() {
        let r: Range<i32> = Range::new(RangeBound::Inclusive(5), RangeBound::Unbounded);
        let mut buf = BytesMut::new();
        r.to_sql(&Type::INT4_RANGE, &mut buf).unwrap();
        // LB_INC (lower inclusive, value follows) + UB_INF (no upper bytes)
        assert_eq!(buf[0], RANGE_LB_INC | RANGE_UB_INF);
        assert_eq!(&buf[1..5], &4i32.to_be_bytes());
        assert_eq!(&buf[5..9], &5i32.to_be_bytes());
        assert_eq!(buf.len(), 9);
    }

    #[test]
    fn range_to_sql_fully_unbounded_writes_only_flag_byte() {
        let r: Range<i32> = Range::new(RangeBound::Unbounded, RangeBound::Unbounded);
        let mut buf = BytesMut::new();
        r.to_sql(&Type::INT4_RANGE, &mut buf).unwrap();
        assert_eq!(buf.as_ref(), &[RANGE_LB_INF | RANGE_UB_INF]);
    }

    #[test]
    fn range_round_trip_through_wire_codec_preserves_bounds() {
        let cases: &[Range<i32>] = &[
            Range::empty(),
            Range::inclusive_exclusive(1, 10),
            Range::inclusive(-5, 5),
            Range::exclusive(0, 100),
            Range::exclusive_inclusive(0, 100),
            Range::new(RangeBound::Unbounded, RangeBound::Inclusive(5)),
            Range::new(RangeBound::Inclusive(5), RangeBound::Unbounded),
            Range::new(RangeBound::Unbounded, RangeBound::Unbounded),
        ];
        for original in cases {
            let mut buf = BytesMut::new();
            original
                .to_sql(&Type::INT4_RANGE, &mut buf)
                .expect("encode");
            let decoded =
                <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &buf).expect("decode");
            assert_eq!(decoded, *original, "round-trip mismatch for {original:?}");
        }
    }

    #[test]
    fn range_round_trip_through_wire_codec_preserves_int8_bounds() {
        let big = i64::MAX - 7;
        let original: Range<i64> = Range::inclusive_exclusive(-big, big);
        let mut buf = BytesMut::new();
        original.to_sql(&Type::INT8_RANGE, &mut buf).unwrap();
        let decoded = <Range<i64> as FromSql>::from_sql(&Type::INT8_RANGE, &buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn range_round_trip_through_wire_codec_preserves_numrange_bounds() {
        let lo = rust_decimal::Decimal::new(-99_999_999, 4);
        let hi = rust_decimal::Decimal::new(12_345, 2);
        let original: Range<rust_decimal::Decimal> = Range::inclusive_exclusive(lo, hi);
        let mut buf = BytesMut::new();
        original.to_sql(&Type::NUM_RANGE, &mut buf).unwrap();
        let decoded =
            <Range<rust_decimal::Decimal> as FromSql>::from_sql(&Type::NUM_RANGE, &buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn range_round_trip_through_wire_codec_preserves_date_bounds() {
        let lo = time::Date::from_calendar_date(2020, time::Month::January, 1).unwrap();
        let hi = time::Date::from_calendar_date(2030, time::Month::December, 31).unwrap();
        let original: Range<time::Date> = Range::inclusive_exclusive(lo, hi);
        let mut buf = BytesMut::new();
        original.to_sql(&Type::DATE_RANGE, &mut buf).unwrap();
        let decoded = <Range<time::Date> as FromSql>::from_sql(&Type::DATE_RANGE, &buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn range_round_trip_through_wire_codec_preserves_tstz_bounds() {
        let lo = time::OffsetDateTime::UNIX_EPOCH;
        let hi = lo + time::Duration::days(365);
        let original: Range<time::OffsetDateTime> = Range::inclusive_exclusive(lo, hi);
        let mut buf = BytesMut::new();
        original.to_sql(&Type::TSTZ_RANGE, &mut buf).unwrap();
        let decoded =
            <Range<time::OffsetDateTime> as FromSql>::from_sql(&Type::TSTZ_RANGE, &buf).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn range_accepts_only_the_matching_range_type() {
        // i32 range accepts INT4_RANGE only.
        assert!(<Range<i32> as ToSql>::accepts(&Type::INT4_RANGE));
        assert!(!<Range<i32> as ToSql>::accepts(&Type::INT8_RANGE));
        assert!(!<Range<i32> as ToSql>::accepts(&Type::TSTZ_RANGE));
        assert!(!<Range<i32> as ToSql>::accepts(&Type::INT4));
        assert!(<Range<i32> as FromSql>::accepts(&Type::INT4_RANGE));
        assert!(!<Range<i32> as FromSql>::accepts(&Type::INT8_RANGE));
        // i64 range accepts INT8_RANGE only.
        assert!(<Range<i64> as ToSql>::accepts(&Type::INT8_RANGE));
        assert!(!<Range<i64> as ToSql>::accepts(&Type::INT4_RANGE));
        // DateTime range accepts TSTZ_RANGE only.
        assert!(<Range<time::OffsetDateTime> as ToSql>::accepts(
            &Type::TSTZ_RANGE
        ));
        assert!(!<Range<time::OffsetDateTime> as ToSql>::accepts(
            &Type::DATE_RANGE
        ));
        // Date range accepts DATE_RANGE only.
        assert!(<Range<time::Date> as ToSql>::accepts(&Type::DATE_RANGE));
        assert!(!<Range<time::Date> as ToSql>::accepts(&Type::TSTZ_RANGE));
    }

    #[test]
    fn range_from_sql_rejects_empty_payload() {
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &[]).unwrap_err();
        assert!(
            err.to_string().contains("flags byte"),
            "error should mention the missing flags byte; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_empty_flag_with_trailing_bytes() {
        // 0x01 = RANGE_EMPTY; any trailing bytes after the flag are
        // malformed and must be rejected loudly.
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &[RANGE_EMPTY, 0, 0, 0, 0])
            .unwrap_err();
        assert!(
            err.to_string().contains("trailing"),
            "error should mention trailing bytes; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_null_bound_flags() {
        // RANGE_LB_NULL alone (no actual bound bytes for the deprecated
        // NULL-bound path) must be rejected — those flags are never
        // set on output by any supported Postgres version, and silently
        // treating them as `Unbounded` would mask upstream misbehaviour.
        let err =
            <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &[RANGE_LB_NULL]).unwrap_err();
        assert!(
            err.to_string().contains("NULL-bound flag"),
            "error should reject NULL-bound flag; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_truncated_bound_length_prefix() {
        // LB_INC set, then only 2 bytes (not the required 4) for the
        // length prefix.
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &[RANGE_LB_INC, 0, 0])
            .unwrap_err();
        assert!(
            err.to_string().contains("length prefix"),
            "error should mention length prefix; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_negative_bound_length() {
        // LB_INC set, length prefix is -1 (i32 BE 0xff_ff_ff_ff).
        let payload = [RANGE_LB_INC, 0xff, 0xff, 0xff, 0xff];
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &payload).unwrap_err();
        assert!(
            err.to_string().contains("negative bound length"),
            "error should mention negative length; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_bound_body_shorter_than_length_prefix() {
        // LB_INC set, length prefix claims 4 bytes, but only 2 bytes follow.
        let payload = [RANGE_LB_INC, 0, 0, 0, 4, 0, 0];
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &payload).unwrap_err();
        assert!(
            err.to_string().contains("only"),
            "error should mention truncated body; got: {err}"
        );
    }

    #[test]
    fn range_from_sql_rejects_trailing_bytes_after_upper_bound() {
        // A valid `[1, 10)` int4range encoding followed by one stray byte.
        let mut payload = vec![RANGE_LB_INC];
        payload.extend_from_slice(&4i32.to_be_bytes());
        payload.extend_from_slice(&1i32.to_be_bytes());
        payload.extend_from_slice(&4i32.to_be_bytes());
        payload.extend_from_slice(&10i32.to_be_bytes());
        payload.push(0xff); // stray byte
        let err = <Range<i32> as FromSql>::from_sql(&Type::INT4_RANGE, &payload).unwrap_err();
        assert!(
            err.to_string().contains("trailing"),
            "error should mention trailing bytes; got: {err}"
        );
    }
}
