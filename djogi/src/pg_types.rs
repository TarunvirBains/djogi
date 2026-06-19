//! Postgres typed-surface newtypes —).
//! This module owns the Rust newtypes that wrap Postgres column types
//! `postgres-types` does not encode natively (or whose native encoding
//! does not fit the typed surface djogi wants to expose). Each newtype
//! ships its own `postgres_types::{ToSql, FromSql}` impl against the raw
//! Postgres wire format — djogi pulls in no third-party crate to bridge
//! these types.
//! # Types
//! | Type | Postgres column | Feature flag | Issue |
//! |-----------------|------------------------------------------------|--------------|----------------------------------|
//! | [`Interval`] | `INTERVAL` | (always on) | |
//! | [`Range<T>`] | `int4range` / `int8range` / `numrange` / | (always on) | |
//! | | `tsrange` / `tstzrange` / `daterange` | | |
//! | `std::net::IpAddr` | `INET` | `network` | |
//! | [`CidrAddr`] | `CIDR` | `network` | |
//! | [`MacAddr`] | `MACADDR` | `network` | |
//! Additional newtypes (a typed `DOMAIN` discriminator and similar
//! coverage gaps) ship in follow-on dispatches. The module is
//! structured so each newtype is
//! independently `#[cfg]`-gateable when it lands; the Interval and
//! Range newtypes are unconditional because the wire codecs depend
//! only on stdlib byte primitives plus the element type's own `ToSql` /
//! `FromSql` impl. The `network` family is feature-gated per the
//! spec, matching the spatial-flag pattern: descriptor
//! variants stay always-on, only the runtime surface is gated.
//! # Why hand-rolled wire codecs?
//! Pulling in third-party crates (`pg_interval`, `cidr`, `eui48`, …)
//! would add transitive surface area for a few dozen lines of byte-level
//! wire encoding. The Postgres wire formats are stable, narrow, and
//! well-documented; reproducing them keeps djogi's dependency graph
//! shallow.
//! # Path routing
//! Adopter code reaches these types through `djogi::Interval` (re-exported
//! from `djogi::types`, also surfaced through `djogi::prelude::*`).
//! Macro-emitted code routes through `::djogi::*` per
//! `feedback_macro_path_routing.md`.

use bytes::{BufMut, BytesMut};
use postgres_types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use std::error::Error;

// ── Interval ────────────────────────────────────────────────────────────────

/// A Postgres `INTERVAL` value — .
/// Represents a calendar duration as three independent fields:
/// - `months` — calendar months (a year is 12 months).
/// - `days` — calendar days (a day is NOT 24 hours; DST shifts move it
///   by an hour, leap seconds shift it by a fractional second).
/// - `microseconds` — sub-day time component.
/// # Why three fields?
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
/// # Why not `time::Duration`?
/// `time::Duration` (from the `time` crate, djogi's pinned datetime
/// library) is a fixed-microsecond duration. There is no way to
/// encode `1 month` losslessly through it. Adopters who need a
/// time-only interval can use the [`Interval::microseconds_only`]
/// constructor.
/// # Wire format
/// Postgres `INTERVAL` is 16 bytes in binary wire format (see
/// `src/backend/utils/adt/timestamp.c::interval_send` in the Postgres
/// source — `pq_sendint64(time)` then `pq_sendint32(day)` then
/// `pq_sendint32(month)`, all big-endian):
/// | Bytes | Field | Encoding |
/// |--------|----------------|----------|
/// | 0..8 | `microseconds` | `i64` big-endian (sub-day time component) |
/// | 8..12 | `days` | `i32` big-endian (whole days) |
/// | 12..16 | `months` | `i32` big-endian (calendar months) |
/// The order in the wire format is `(microseconds, days, months)`, but
/// the Rust struct lists them in the order most adopters reach for them
/// when reading code (`months`, then `days`, then `microseconds`).
/// # Construction
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
/// # Equality and ordering
/// ## Rust structural equality vs Postgres SQL `=`
/// **Rust `PartialEq` / `Eq` / `Hash` on `Interval` are structural.**
/// All three component fields must match byte-for-byte.
/// `Interval::months_only(1) == Interval::days_only(30)` is `false`
/// in Rust — the two values differ in the `months` and `days` fields
/// even if they nominally span "the same time" on most calendars.
/// `Hash` follows the same structural rule (Rust convention: equal
/// values must hash equal, and only structurally identical values are
/// equal here), so Rust-side hashmap keying is structural, never
/// linearized.
/// **Postgres SQL `=` on `INTERVAL` columns linearizes.**
/// Postgres converts each component before comparing: months are
/// treated as 30 days, and days are treated as 24 hours (86,400
/// seconds = 86,400,000,000 microseconds). The comparison is then
/// performed on the resulting total microsecond count. As a result,
/// `INTERVAL '1 month' = INTERVAL '30 days'` is `true` in Postgres
/// SQL.
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
        // bytes 0..8 microseconds : i64 (sub-day time)
        // bytes 8..12 days : i32
        // bytes 12..16 months : i32
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

// ── Range / RangeBound (#215 typed surface) ─────────────────────
// Postgres range types (`int4range`, `int8range`, `numrange`, `tsrange`,
// `tstzrange`, `daterange`) carry a tagged bound shape: each side is
// inclusive, exclusive, or unbounded, with a separate "empty range" sentinel
// that has no bounds at all. The Rust type mirrors that shape exactly so
// adopters never need to invent their own range encoding when declaring
// `pub period: Range<DateTime>` on a `#[model]` struct.
// #215 ships the user-facing typed field and predicate surface. Two sibling
// lanes remain explicitly out of scope here:
// * — `btree_gist` EXCLUDE constraint grammar (`#[model(exclude(
// ...))]`) and the `CREATE EXTENSION btree_gist` migration step.
// * — PostgreSQL 18 temporal-constraint DDL (`WITHOUT OVERLAPS`,
// `PERIOD` foreign keys, `NOT ENFORCED`, named `NOT NULL`).
// Both lanes consume range columns as their input. Centralizing the
// `Range<T>` shape here keeps future lanes from diverging on
// what "a range column" looks like.
// # No third-party crate
// `postgres-types` exports the range *type OIDs* as `Type::INT4_RANGE`,
// `Type::TSTZ_RANGE`, etc. but does not provide a generic Rust `Range<T>`
// codec. The published `postgres-range` crate is a third-party adapter,
// pinned to its own type design, and unmaintained against the latest
// `postgres-types` major. We hand-roll a 30-line wire codec instead
// no transitive dependency surface, no version pin to worry about.

/// One end of a [`Range`] — inclusive of the named value, exclusive of
/// the named value, or unbounded on this side.
/// The variant set matches Postgres's range-bound semantics directly:
/// * `Inclusive(t)` — the bound value `t` is part of the range.
///   Renders as `[t,…]` (lower) or `[…,t]` (upper).
/// * `Exclusive(t)` — the bound value `t` is *not* part of the range.
///   Renders as `(t,…]` (lower) or `[…,t)` (upper).
/// * `Unbounded` — no bound on this side. Postgres terminology calls
///   this "infinite"; the wire format sets the `RANGE_LB_INF` /
///   `RANGE_UB_INF` flag and omits the bound bytes entirely.
///   The empty range is *not* a special `RangeBound` value — it is a
///   separate state on the enclosing [`Range`]. Use [`Range::empty`] when
///   you need the empty-range sentinel; an empty range carries no bound
///   values regardless of which `RangeBound` variants you started with.
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
/// Mirrors Postgres's tagged range layout: each side carries an
/// inclusive / exclusive / unbounded bound, with a separate `empty`
/// flag for the empty-range sentinel. A range with `empty = true`
/// carries no bound values regardless of how it was constructed
/// [`Range::empty`] returns the canonical empty range, and the wire
/// codec collapses any in-memory bound values when the empty flag is
/// set.
/// # Construction
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
/// # Equality and ordering
/// `PartialEq` / `Eq` / `Hash` are structural — two `Range<T>` values
/// compare equal only when their `empty` flag and bound shapes match
/// byte-for-byte. Postgres `=` on range columns has different
/// semantics (range canonicalization on discrete subtypes can render
/// `[1,9]` and `[1,10)` equal at the SQL level for `int4range`); the
/// Rust structural comparison does *not* canonicalize. Adopters who
/// need SQL-level equivalence should route through Postgres `=` via
/// `QuerySet::filter`.
/// `Range` does not implement `Ord` / `PartialOrd` — comparing two
/// ranges that overlap at one endpoint but differ in inclusivity is
/// ambiguous, and the framework refuses to pick an interpretation on
/// the adopter's behalf.
/// # Wire format
/// Postgres range binary wire format (see `range_send` in
/// `src/backend/utils/adt/rangetypes.c`):
/// | Bytes | Field | Encoding |
/// |---------------------|--------------|----------|
/// | 0 | flags | `u8` — see [`RangeFlags`] private constants |
/// | (if lower finite) | lower length | `i32` big-endian, byte count of lower bound |
/// | (if lower finite) | lower bytes | Postgres binary repr of `T` for the element type |
/// | (if upper finite) | upper length | `i32` big-endian, byte count of upper bound |
/// | (if upper finite) | upper bytes | Postgres binary repr of `T` for the element type |
/// The empty range writes a single flag byte (`0x01`) and no bounds.
/// Unbounded ends set the `RANGE_LB_INF` / `RANGE_UB_INF` flag and
/// omit the bound bytes. The wire encoding for the element type `T`
/// is whatever `T::to_sql` produces for the element-type `&Type`
/// e.g. `i32` writes 4 big-endian bytes for `INT4`, `OffsetDateTime`
/// writes 8 big-endian microseconds-since-2000 bytes for `TIMESTAMPTZ`.
/// # Future siblings - DB-level no-overlap
/// `Range<T>` is also the substrate for two future DB-level no-overlap
/// surfaces tracked separately:
/// * **** — `btree_gist` EXCLUDE constraint grammar
///   (`#[model(exclude(...))]`) and `CREATE EXTENSION btree_gist`.
///   The general-purpose no-overlap mechanism that works on every
///   supported Postgres version.
/// * **** — PostgreSQL 18 temporal-constraint DDL
///   (`WITHOUT OVERLAPS`, `PERIOD` foreign keys, `NOT ENFORCED`,
///   named `NOT NULL`). The modern SQL-standard no-overlap mechanism
///   on PG18+.
///   Both lanes consume `Range<T>` columns as their inputs. Neither lane is
///   part of #215.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range<T> {
    lower: RangeBound<T>,
    upper: RangeBound<T>,
    empty: bool,
}

impl<T> Range<T> {
    /// Construct the empty range — the Postgres `'empty'::<subtype>range`
    /// value.
    /// The empty range has no bounds; both `lower` and `upper` are set
    /// to [`RangeBound::Unbounded`] for consistency, and the `empty`
    /// flag suppresses any bound serialization in the wire codec.
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
    /// The framework does **not** validate that `lower <= upper`.
    /// Postgres rejects such ranges at write time with
    /// `ERROR: range lower bound must be less than or equal to range upper bound`;
    /// the Rust type defers ordering judgement to Postgres so it
    /// stays valid for adopter-defined element types whose ordering
    /// the framework cannot know.
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
    /// Postgres canonicalises discrete-subtype ranges (`int4range`,
    /// `int8range`, `daterange`) to lower-inclusive / upper-exclusive
    /// at storage time, so a stored `int4range '[1,9]'` is equal to
    /// `int4range '[1,10)'` at the SQL level. The Rust type preserves
    /// what you passed in; the canonicalisation happens server-side.
    /// # Caveat — discrete upper-MAX bounds
    /// For **discrete** subtypes (`int4range`, `int8range`, `daterange`)
    /// Postgres canonicalises `[lo, hi]` to `[lo, hi + 1)` at write
    /// time. When `hi` is at the element type's representable maximum
    /// (`i32::MAX`, `i64::MAX`, or `time::Date::MAX = 9999-12-31`), the
    /// canonical exclusive upper bound is `hi + 1`, which is one step
    /// past what the type can represent — `i32::MAX + 1` overflows i32
    /// (Postgres returns `integer out of range`), and
    /// `daterange '[..., 9999-12-31]'` canonicalises to an upper bound
    /// of `10000-01-01` that exceeds Djogi's `time::Date` upper-bound
    /// CHECK and is rejected at write time.
    /// Prefer the [`Range::inclusive_exclusive`] form when reaching for
    /// the element type's maximum on a discrete range column:
    /// ```rust,ignore
    /// // Avoid on discrete subtypes — Postgres rejects on canonicalisation:
    /// // let r = Range::inclusive(0_i32, i32::MAX);
    ///
    /// // Prefer — already in canonical form, no overflow on storage:
    /// let r = Range::inclusive_exclusive(0_i32, i32::MAX);
    /// ```
    /// Continuous subtypes (`numrange`, `tstzrange`) are NOT
    /// canonicalised and accept `[..., MAX]` unchanged.
    pub fn inclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Inclusive(lo), RangeBound::Inclusive(hi))
    }

    /// `(lo, hi)` — both endpoints exclusive.
    pub fn exclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Exclusive(lo), RangeBound::Exclusive(hi))
    }

    /// `[lo, hi)` — lower inclusive, upper exclusive.
    /// The canonical "discrete" range shape. Postgres canonicalises
    /// every discrete range to this form at storage time, so adopters
    /// who want their in-memory `Range` shape to match what comes back
    /// out of a `SELECT` reach for this constructor.
    /// This is also the safe form when reaching for the element type's
    /// maximum on a discrete range column — see the caveat on
    /// [`Range::inclusive`].
    pub fn inclusive_exclusive(lo: T, hi: T) -> Self {
        Self::new(RangeBound::Inclusive(lo), RangeBound::Exclusive(hi))
    }

    /// `(lo, hi]` — lower exclusive, upper inclusive.
    /// On discrete subtypes (`int4range`, `int8range`, `daterange`) the
    /// upper-MAX caveat from [`Range::inclusive`] applies — Postgres
    /// canonicalises the upper inclusive bound to `hi + 1` exclusive,
    /// which overflows / exceeds the element type's representable
    /// maximum.
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
/// One impl per supported subtype:
/// | Rust element type | Postgres range type | Bound element type |
/// |------------------------------|---------------------|--------------------|
/// | `i32` | `int4range` | `INT4` |
/// | `i64` | `int8range` | `INT8` |
/// | `rust_decimal::Decimal` | `numrange` | `NUMERIC` |
/// | `time::PrimitiveDateTime` | `tsrange` | `TIMESTAMP` |
/// | `time::OffsetDateTime` | `tstzrange` | `TIMESTAMPTZ` |
/// | `time::Date` | `daterange` | `DATE` |
/// `tsrange` uses `time::PrimitiveDateTime` so timestamp-without-timezone
/// semantics stay explicit. Djogi's `DateTime` alias remains
/// timezone-aware and lowers to `tstzrange`.
/// The trait is open for future extensions but the only implementors
/// shipping in #215 are the six rows above. Because `Range<T>` has a
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

impl RangeSubtype for time::PrimitiveDateTime {
    fn pg_range_type() -> Type {
        Type::TS_RANGE
    }
    fn pg_element_type() -> Type {
        Type::TIMESTAMP
    }
    const PG_RANGE_NAME: &'static str = "TSRANGE";
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
// The flag bit layout matches Postgres's `rangetypes.h` definitions
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

// ── Network types — MacAddr / CidrAddr (, `network` feature) ────────
// Postgres ships three network-shaped column types — `INET`, `CIDR`, and
// `MACADDR`. Djogi's coverage of them:
// * **`INET`**: `std::net::IpAddr` reuses the always-on `postgres-types`
// native `ToSql` / `FromSql` impls. The netmask defaults to /32 (IPv4)
// / /128 (IPv6) — host addresses without explicit network prefixes.
// Adopters who need a non-default netmask on an `INET` column reach
// for the same future-work direction that `CIDR` opens: a typed
// newtype carrying both address and prefix. Today, full-prefix INET
// carries the host-only adopter case; future-work follow-up adds
// `InetAddr { addr, prefix }` symmetric with `CidrAddr` once an
// adopter use case surfaces.
// * **`CIDR`**: typed `CidrAddr { addr, prefix }` newtype below. The
// wire format is identical to INET (4-byte header + variable address
// bytes) but the Postgres type OID differs and the validator rejects
// non-network addresses (host bits past the prefix must be zero).
// * **`MACADDR`**: typed `MacAddr([u8; 6])` newtype below. The wire
// format is the bare 6-byte address with no header.
// All three column types are recognised behind the `network` Cargo
// feature flag (see `djogi/Cargo.toml`). The descriptor enum variants
// (`FieldSqlType::Inet` / `Cidr` / `Macaddr`) are always present so
// migration differ / docs / `Display` surfaces stay stable across
// feature states; only the runtime Rust types and the bind/decode
// paths are gated. Adopters with the feature off who declare a field
// of type `MacAddr` or `CidrAddr` get an "unresolved type" compile
// error at the model struct, matching the spatial flag's behaviour.
// # Why no third-party crate?
// The Postgres INET/CIDR/MACADDR wire formats are stable, documented,
// and narrow. `postgres-protocol` ships its own `inet_to_sql` /
// `inet_from_sql` helpers (which `postgres-types` reaches for
// `std::net::IpAddr`'s native impl), but djogi consumes
// `postgres-protocol` only transitively — pulling it in as a direct
// dependency, or pulling in `eui48` / `mac_address` / `cidr`, would
// increase the dependency surface for ~60 lines of byte-level encoding.
// The hand-rolled codec here mirrors the same wire format and pulls in
// no extra crate.

/// PGSQL_AF_INET — Postgres-internal address-family code for IPv4 in
/// the binary INET / CIDR wire format. Not the same as Linux's
/// `AF_INET` (2 on Linux too, but Postgres explicitly redefines it to
/// stay portable across OSes; we never assume an alias).
#[cfg(feature = "network")]
const PGSQL_AF_INET: u8 = 2;

/// PGSQL_AF_INET6 — Postgres-internal address-family code for IPv6.
/// Linux uses 10 / 26 / 28 for `AF_INET6` depending on the platform;
/// Postgres uses a stable 3 in the wire format regardless.
#[cfg(feature = "network")]
const PGSQL_AF_INET6: u8 = 3;

// ── MacAddr ─────────────────────────────────────────────────────────────────

/// A Postgres `MACADDR` value — .
/// A 6-byte EUI-48 MAC address. The wire encoding is the bare 6 bytes
/// in network (big-endian) order with no header.
/// # Construction
/// ```rust,ignore
/// use djogi::MacAddr;
///
/// // From the canonical 6-byte octet tuple.
/// let mac = MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
///
/// // Parse from the canonical colon-separated form.
/// let mac = "aa:bb:cc:dd:ee:ff".parse::<MacAddr>().unwrap();
/// ```
/// # Equality and ordering
/// `MacAddr` derives `PartialEq` / `Eq` / `Hash` / `PartialOrd` /
/// `Ord` over the underlying 6-byte array. Rust structural equality
/// matches Postgres `=` exactly — both compare bytes — so portable
/// `eq` / `neq` predicates can route through `DjogiPortableEq` without
/// an SQL-only escape hatch the way [`Interval`] requires.
/// # Wire format
/// Postgres binary `MACADDR` wire format: 6 bytes, oui first. See
/// `pq_sendbytes(value->a, 6)` in `src/backend/utils/adt/mac.c`.
/// # `MACADDR8` (EUI-64) is intentionally out of scope
/// Postgres also ships `MACADDR8` (8-byte EUI-64). The
/// surface targets the more common 6-byte EUI-48 form. Adopters with
/// `MACADDR8` columns today use `Custom("MACADDR8")` + a hand-rolled
/// newtype; the tracks future-work for full
/// EUI-64 coverage if adopter demand surfaces.
#[cfg(feature = "network")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddr([u8; 6]);

#[cfg(feature = "network")]
impl MacAddr {
    /// Construct a `MacAddr` from the canonical 6-byte octet array.
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    /// Return the underlying 6-byte octet array.
    pub const fn octets(&self) -> [u8; 6] {
        self.0
    }
}

/// Error returned by `MacAddr::from_str` when the input is not a valid
/// canonical-form MAC address.
/// Three failure modes:
/// * `WrongLength { found }` — the input did not parse into exactly six
///   octets (typically because the wrong number of `:` / `-` separators
///   was present, or no separators at all).
/// * `InvalidOctet { index, octet }` — one of the six octet positions
///   carried a token that was not a 2-byte uppercase / lowercase hex
///   pair. The `index` is 0-based.
/// * `MixedSeparators` — the input mixed `:` and `-` separators, which
///   is ambiguous. Pick one separator style.
///   The variant payloads are deliberately concrete (lengths, indexes,
///   the offending octet) so adopters can compose richer error messages
///   without re-parsing.
#[cfg(feature = "network")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacAddrParseError {
    /// Wrong number of octets after splitting.
    WrongLength { found: usize },
    /// One octet failed to parse as a 2-digit hex byte.
    InvalidOctet { index: usize, octet: String },
    /// Input mixed `:` and `-` separators.
    MixedSeparators,
}

#[cfg(feature = "network")]
impl std::fmt::Display for MacAddrParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongLength { found } => {
                write!(f, "MAC address must have 6 octets, got {found}")
            }
            Self::InvalidOctet { index, octet } => {
                write!(
                    f,
                    "MAC address octet at index {index} is not 2 hex digits: {octet:?}"
                )
            }
            Self::MixedSeparators => {
                f.write_str("MAC address mixes ':' and '-' separators; use one or the other")
            }
        }
    }
}

#[cfg(feature = "network")]
impl std::error::Error for MacAddrParseError {}

#[cfg(feature = "network")]
impl std::fmt::Display for MacAddr {
    /// Render in the canonical colon-separated lowercase form
    /// (`aa:bb:cc:dd:ee:ff`). Matches the most common adopter
    /// expectation and Postgres's own `MACADDR` text representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

#[cfg(feature = "network")]
impl std::str::FromStr for MacAddr {
    type Err = MacAddrParseError;

    /// Parse a colon- or hyphen-separated MAC address into a `MacAddr`.
    /// Accepts the two most common canonical forms — `aa:bb:cc:dd:ee:ff`
    /// (Unix / Postgres default) and `aa-bb-cc-dd-ee-ff` (Windows
    /// default). Mixed-separator input (`aa:bb-cc:dd-ee:ff`) is
    /// rejected. Hex characters may be upper or lower case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let has_colon = s.contains(':');
        let has_hyphen = s.contains('-');
        if has_colon && has_hyphen {
            return Err(MacAddrParseError::MixedSeparators);
        }
        let sep = if has_hyphen { '-' } else { ':' };
        let parts: Vec<&str> = s.split(sep).collect();
        if parts.len() != 6 {
            return Err(MacAddrParseError::WrongLength { found: parts.len() });
        }
        let mut out = [0u8; 6];
        for (i, part) in parts.iter().enumerate() {
            if part.len() != 2 {
                return Err(MacAddrParseError::InvalidOctet {
                    index: i,
                    octet: (*part).to_string(),
                });
            }
            out[i] = u8::from_str_radix(part, 16).map_err(|_| MacAddrParseError::InvalidOctet {
                index: i,
                octet: (*part).to_string(),
            })?;
        }
        Ok(MacAddr(out))
    }
}

#[cfg(feature = "network")]
impl ToSql for MacAddr {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // Postgres MACADDR binary wire format: 6 bytes, oui first.
        out.put_slice(&self.0);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::MACADDR
    }

    to_sql_checked!();
}

#[cfg(feature = "network")]
impl<'a> FromSql<'a> for MacAddr {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        if raw.len() != 6 {
            return Err(format!(
                "Postgres MACADDR wire format must be exactly 6 bytes, got {}",
                raw.len()
            )
            .into());
        }
        let mut out = [0u8; 6];
        out.copy_from_slice(raw);
        Ok(MacAddr(out))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::MACADDR
    }
}

#[cfg(feature = "network")]
impl crate::descriptor::DjogiSqlType for MacAddr {
    const SQL_TYPE: &'static str = "MACADDR";
}

// ── CidrAddr ────────────────────────────────────────────────────────────────

/// A Postgres `CIDR` value — .
/// Carries both an IP address and an explicit network prefix length.
/// Postgres CIDR rejects values whose host bits past the prefix are
/// non-zero (e.g., `192.168.1.5/24` is rejected because the trailing
/// `.5` falls inside the host portion of a /24 network — the host bits
/// `.0.0.0.5` are non-zero), so `CidrAddr::new` performs the same
/// check at construction time and returns an error rather than
/// deferring to the database round-trip.
/// # Construction
/// ```rust,ignore
/// use djogi::CidrAddr;
/// use std::net::{IpAddr, Ipv4Addr};
///
/// // 192.168.1.0/24 — a /24 network.
/// let net = CidrAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)), 24).unwrap();
///
/// // 192.168.1.5/24 — rejected: host bits past the prefix are non-zero.
/// assert!(CidrAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).is_err());
/// ```
/// # Equality and ordering
/// `CidrAddr` derives `PartialEq` / `Eq` / `Hash` over both the address
/// and the prefix. Rust structural equality matches Postgres `=` on
/// `CIDR` columns: two CIDR values are equal when their addresses and
/// prefixes both match byte-for-byte.
/// # INET vs CIDR distinction
/// `std::net::IpAddr` maps to `INET` (always /32 or /128 — the host-
/// address case). `CidrAddr` maps to `CIDR`. The two Rust types are
/// distinct so the macro routes them to distinct column types without
/// needing a `#[field(ip_type = "cidr")]` attribute; the type-driven
/// dispatch matches djogi's `HeerId` vs `RanjId` pattern.
/// # Wire format
/// Postgres binary CIDR / INET wire format (identical layout; the type
/// OID disambiguates):
/// | Bytes | Field | Encoding |
/// |-------|--------------|----------|
/// | 0 | `family` | `u8` — `PGSQL_AF_INET` (2) for IPv4, `PGSQL_AF_INET6` (3) for IPv6 |
/// | 1 | `prefix` | `u8` — 0..=32 for IPv4, 0..=128 for IPv6 |
/// | 2 | `is_cidr` | `u8` — 0 for INET, 1 for CIDR |
/// | 3 | `len` | `u8` — 4 for IPv4, 16 for IPv6 |
/// | 4.. | `addr_bytes` | 4 or 16 octets, network (big-endian) order |
#[cfg(feature = "network")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CidrAddr {
    addr: std::net::IpAddr,
    prefix: u8,
}

/// Error returned by `CidrAddr::new` when the address / prefix
/// combination is not a valid Postgres `CIDR` value.
#[cfg(feature = "network")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidrAddrError {
    /// Prefix length exceeds the address family maximum (32 for IPv4,
    /// 128 for IPv6).
    PrefixTooLarge { prefix: u8, max: u8 },
    /// One or more bits in the host portion (past the prefix) are
    /// non-zero. Postgres `CIDR` rejects such values; `192.168.1.5/24`
    /// is the canonical rejected shape (`.5` falls in the host portion
    /// of a /24).
    HostBitsSet,
}

#[cfg(feature = "network")]
impl std::fmt::Display for CidrAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrefixTooLarge { prefix, max } => {
                write!(
                    f,
                    "CIDR prefix {prefix} exceeds maximum {max} for this address family"
                )
            }
            Self::HostBitsSet => f.write_str(
                "CIDR host bits past the prefix must be zero; \
                 mask the host bits before constructing CidrAddr",
            ),
        }
    }
}

#[cfg(feature = "network")]
impl std::error::Error for CidrAddrError {}

#[cfg(feature = "network")]
impl CidrAddr {
    /// Construct a `CidrAddr` from an address and a prefix length.
    /// Validates that the prefix is in range (0..=32 for IPv4,
    /// 0..=128 for IPv6) and that all bits past the prefix in the
    /// address are zero. The latter check matches Postgres's CIDR
    /// admission rule.
    pub fn new(addr: std::net::IpAddr, prefix: u8) -> Result<Self, CidrAddrError> {
        let max_prefix: u8 = match addr {
            std::net::IpAddr::V4(_) => 32,
            std::net::IpAddr::V6(_) => 128,
        };
        if prefix > max_prefix {
            return Err(CidrAddrError::PrefixTooLarge {
                prefix,
                max: max_prefix,
            });
        }
        if !host_bits_zero(addr, prefix) {
            return Err(CidrAddrError::HostBitsSet);
        }
        Ok(Self { addr, prefix })
    }

    /// Return the network base address.
    pub const fn addr(&self) -> std::net::IpAddr {
        self.addr
    }

    /// Return the network prefix length (0..=32 for IPv4, 0..=128 for IPv6).
    pub const fn prefix(&self) -> u8 {
        self.prefix
    }
}

/// `true` if every bit in `addr` strictly past the `prefix`-th most
/// significant bit is zero — Postgres's CIDR admission rule. A `/0`
/// prefix accepts only the all-zero address; a `/32` (IPv4) or `/128`
/// (IPv6) prefix accepts any address. The implementation walks the
/// big-endian octet stream; for each octet entirely inside the host
/// portion it requires the octet to be zero, and for the boundary
/// octet (if any) it masks off the bits inside the network portion
/// and checks the remainder.
#[cfg(feature = "network")]
fn host_bits_zero(addr: std::net::IpAddr, prefix: u8) -> bool {
    fn check(octets: &[u8], prefix: u8) -> bool {
        let prefix = prefix as usize;
        for (i, byte) in octets.iter().enumerate() {
            let start_bit = i * 8;
            if start_bit >= prefix {
                // Entire octet is in the host portion.
                if *byte != 0 {
                    return false;
                }
            } else if start_bit + 8 > prefix {
                // Boundary octet — keep the top `(prefix - start_bit)`
                // bits, zero-check the remainder.
                let net_bits = prefix - start_bit;
                let mask: u8 = if net_bits == 0 {
                    0xff
                } else {
                    0xffu8.checked_shr(net_bits as u32).unwrap_or(0)
                };
                if byte & mask != 0 {
                    return false;
                }
            }
        }
        true
    }
    match addr {
        std::net::IpAddr::V4(a) => check(&a.octets(), prefix),
        std::net::IpAddr::V6(a) => check(&a.octets(), prefix),
    }
}

#[cfg(feature = "network")]
impl ToSql for CidrAddr {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        write_inet_payload(self.addr, self.prefix, /*is_cidr=*/ 1, out);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::CIDR
    }

    to_sql_checked!();
}

#[cfg(feature = "network")]
impl<'a> FromSql<'a> for CidrAddr {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let (addr, prefix) = read_inet_payload(raw)?;
        // Postgres should already enforce host-bit-zero on stored CIDR
        // values, but we re-check to catch wire corruption / replication
        // glitches loudly rather than silently surfacing invalid CIDR.
        if !host_bits_zero(addr, prefix) {
            return Err(format!(
                "decoded CIDR has non-zero host bits: addr={addr}, prefix={prefix}"
            )
            .into());
        }
        Ok(CidrAddr { addr, prefix })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::CIDR
    }
}

#[cfg(feature = "network")]
impl crate::descriptor::DjogiSqlType for CidrAddr {
    const SQL_TYPE: &'static str = "CIDR";
}

#[cfg(feature = "network")]
impl std::fmt::Display for CidrAddr {
    /// Render in the canonical `<addr>/<prefix>` form matching Postgres's
    /// `CIDR` text representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

// ── InetAddr ────────────────────────────────────────────────────────────────

/// A Postgres `INET` value carrying both a host address and an explicit
/// network prefix length.
///
/// # What
/// `INET` stores an IP address that *may* carry host bits past the
/// prefix (e.g. `192.168.1.5/24` — the `.5` host part is preserved).
/// This is the defining difference from [`CidrAddr`]: `CIDR` rejects
/// non-zero host bits, `INET` keeps them.
///
/// # Why InetAddr when `std::net::IpAddr` already maps to INET?
/// A field typed `std::net::IpAddr` maps to an `INET` column, but the
/// `postgres-types` native codec presents the value to Rust with the
/// netmask collapsed to /32 (IPv4) or /128 (IPv6) — the stored prefix
/// is discarded on decode. Reach for `InetAddr` when the prefix length
/// is meaningful data your application reads back (interface
/// configuration, address-with-netmask records). Reach for plain
/// `IpAddr` when you only ever store and compare bare host addresses.
///
/// # InetAddr vs CidrAddr — which to use
/// | | `InetAddr` (INET) | [`CidrAddr`] (CIDR) |
/// |----------------------|-----------------------------------|-------------------------------|
/// | Host bits past prefix | Permitted (`192.168.1.5/24` OK) | Rejected (must be zero) |
/// | Models | A host address *with* a netmask | A network / subnet |
/// | Postgres column | `INET` | `CIDR` |
/// | Example | `10.0.0.7/8` (host .7 on a /8 net) | `10.0.0.0/8` (the /8 network) |
/// Use `InetAddr` for "this host, on this size of network". Use
/// `CidrAddr` for "this network range". When you only have a bare
/// address and never need the prefix, use `std::net::IpAddr`.
///
/// # Construction
/// ```rust,ignore
/// use djogi::InetAddr;
/// use std::net::{IpAddr, Ipv4Addr};
///
/// // 192.168.1.5/24 — a host address that retains its /24 netmask.
/// let host = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).unwrap();
/// assert_eq!(host.prefix(), 24);
///
/// // An impossible prefix is rejected at construction, not at the DB.
/// assert!(InetAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 33).is_err());
/// ```
///
/// # Equality and ordering
/// `InetAddr` derives `PartialEq` / `Eq` / `Hash` over both the address
/// and the prefix. Rust structural equality matches Postgres `=` on
/// `INET` columns: two INET values are equal when their addresses and
/// prefixes both match.
///
/// # Wire format
/// Identical layout to [`CidrAddr`] (4-byte header + 4/16 address
/// bytes); the `is_cidr` header byte is `0` for INET. See the table on
/// [`CidrAddr`] for the byte-level breakdown.
#[cfg(feature = "network")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InetAddr {
    addr: std::net::IpAddr,
    prefix: u8,
}

/// Error returned by [`InetAddr::new`] when the address / prefix
/// combination is not a valid Postgres `INET` value.
///
/// Unlike [`CidrAddrError`], there is no `HostBitsSet` variant — `INET`
/// permits host bits past the prefix by design.
#[cfg(feature = "network")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InetAddrError {
    /// Prefix length exceeds the address family maximum (32 for IPv4,
    /// 128 for IPv6).
    PrefixTooLarge {
        /// The rejected prefix length.
        prefix: u8,
        /// The maximum valid prefix for the address family (32 or 128).
        max: u8,
    },
}

#[cfg(feature = "network")]
impl std::fmt::Display for InetAddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrefixTooLarge { prefix, max } => {
                write!(
                    f,
                    "INET prefix {prefix} exceeds maximum {max} for this address family"
                )
            }
        }
    }
}

#[cfg(feature = "network")]
impl std::error::Error for InetAddrError {}

#[cfg(feature = "network")]
impl InetAddr {
    /// Construct an `InetAddr` from an address and a prefix length.
    ///
    /// Validates that the prefix is in range (0..=32 for IPv4,
    /// 0..=128 for IPv6). Unlike [`CidrAddr::new`], host bits past the
    /// prefix are **permitted** — that is the INET-vs-CIDR difference.
    ///
    /// # Errors
    /// Returns [`InetAddrError::PrefixTooLarge`] when `prefix` exceeds
    /// the address family maximum.
    pub fn new(addr: std::net::IpAddr, prefix: u8) -> Result<Self, InetAddrError> {
        let max_prefix: u8 = match addr {
            std::net::IpAddr::V4(_) => 32,
            std::net::IpAddr::V6(_) => 128,
        };
        if prefix > max_prefix {
            return Err(InetAddrError::PrefixTooLarge {
                prefix,
                max: max_prefix,
            });
        }
        Ok(Self { addr, prefix })
    }

    /// Return the host address.
    pub const fn addr(&self) -> std::net::IpAddr {
        self.addr
    }

    /// Return the network prefix length (0..=32 for IPv4, 0..=128 for IPv6).
    pub const fn prefix(&self) -> u8 {
        self.prefix
    }
}

#[cfg(feature = "network")]
impl std::fmt::Display for InetAddr {
    /// Render in the canonical `<addr>/<prefix>` form matching Postgres's
    /// `INET` text representation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[cfg(feature = "network")]
impl ToSql for InetAddr {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        // INET shares CIDR's wire layout; the is_cidr byte is 0 for INET.
        write_inet_payload(self.addr, self.prefix, /*is_cidr=*/ 0, out);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INET
    }

    to_sql_checked!();
}

#[cfg(feature = "network")]
impl<'a> FromSql<'a> for InetAddr {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + Sync + Send>> {
        // INET permits host bits past the prefix, so — unlike CidrAddr —
        // there is no host-bits-zero re-check here. read_inet_payload
        // already validates the header (family, prefix bound, length).
        let (addr, prefix) = read_inet_payload(raw)?;
        Ok(InetAddr { addr, prefix })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INET
    }
}

#[cfg(feature = "network")]
impl crate::descriptor::DjogiSqlType for InetAddr {
    const SQL_TYPE: &'static str = "INET";
}

/// Shared INET/CIDR wire encoder. The on-wire format is identical for
/// INET and CIDR; only the Postgres type OID and the `is_cidr` byte
/// differ. `CidrAddr::to_sql` sets `is_cidr = 1`; future INET typed
/// newtypes (if added) would set `is_cidr = 0`. Today `INET` columns
/// go through `postgres-types`' native `IpAddr` impl, so this helper
/// is used only by `CidrAddr`.
#[cfg(feature = "network")]
fn write_inet_payload(addr: std::net::IpAddr, prefix: u8, is_cidr: u8, out: &mut BytesMut) {
    let family = match addr {
        std::net::IpAddr::V4(_) => PGSQL_AF_INET,
        std::net::IpAddr::V6(_) => PGSQL_AF_INET6,
    };
    out.put_u8(family);
    out.put_u8(prefix);
    out.put_u8(is_cidr);
    match addr {
        std::net::IpAddr::V4(a) => {
            out.put_u8(4);
            out.put_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            out.put_u8(16);
            out.put_slice(&a.octets());
        }
    }
}

/// Shared INET/CIDR wire decoder. Returns `(addr, prefix)` and validates
/// the four-byte header. The `is_cidr` byte is read but not consulted
/// Postgres tags the value with the column type, and the type OID
/// already disambiguates INET from CIDR at the `accepts` boundary above.
/// We accept either value of the byte to stay robust against future
/// upstream changes.
#[cfg(feature = "network")]
fn read_inet_payload(
    mut raw: &[u8],
) -> Result<(std::net::IpAddr, u8), Box<dyn Error + Sync + Send>> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    if raw.len() < 4 {
        return Err(
            "Postgres INET/CIDR wire format: header truncated (need 4 header bytes)".into(),
        );
    }
    let family = raw[0];
    let prefix = raw[1];
    let _is_cidr = raw[2];
    let len = raw[3];
    raw = &raw[4..];

    let addr = match family {
        PGSQL_AF_INET => {
            if prefix > 32 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv4 prefix {prefix} exceeds maximum 32"
                )
                .into());
            }
            if len != 4 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv4 length byte must be 4, got {len}"
                )
                .into());
            }
            if raw.len() != 4 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv4 needs 4 address bytes, got {}",
                    raw.len()
                )
                .into());
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(raw);
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        PGSQL_AF_INET6 => {
            if prefix > 128 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv6 prefix {prefix} exceeds maximum 128"
                )
                .into());
            }
            if len != 16 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv6 length byte must be 16, got {len}"
                )
                .into());
            }
            if raw.len() != 16 {
                return Err(format!(
                    "Postgres INET/CIDR wire format: IPv6 needs 16 address bytes, got {}",
                    raw.len()
                )
                .into());
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(raw);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        other => {
            return Err(format!(
                "Postgres INET/CIDR wire format: unknown address family {other} \
                 (expected {PGSQL_AF_INET} for IPv4 or {PGSQL_AF_INET6} for IPv6)"
            )
            .into());
        }
    };
    Ok((addr, prefix))
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

    // ── Range / RangeBound ──────────────────────────────────────

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
        assert!(<Range<time::PrimitiveDateTime> as Default>::default().is_empty());
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
            <time::PrimitiveDateTime as RangeSubtype>::pg_range_type(),
            Type::TS_RANGE
        );
        assert_eq!(
            <time::PrimitiveDateTime as RangeSubtype>::pg_element_type(),
            Type::TIMESTAMP
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
        assert_eq!(
            <Range<time::PrimitiveDateTime> as DjogiSqlType>::SQL_TYPE,
            "TSRANGE"
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
    fn range_round_trip_through_wire_codec_preserves_ts_bounds() {
        let lo = time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(2026, time::Month::January, 1).unwrap(),
            time::Time::MIDNIGHT,
        );
        let hi = time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(2026, time::Month::January, 2).unwrap(),
            time::Time::MIDNIGHT,
        );
        let original: Range<time::PrimitiveDateTime> = Range::inclusive_exclusive(lo, hi);
        let mut buf = BytesMut::new();
        original.to_sql(&Type::TS_RANGE, &mut buf).unwrap();
        let decoded =
            <Range<time::PrimitiveDateTime> as FromSql>::from_sql(&Type::TS_RANGE, &buf).unwrap();
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
        // PrimitiveDateTime range accepts TS_RANGE only.
        assert!(<Range<time::PrimitiveDateTime> as ToSql>::accepts(
            &Type::TS_RANGE
        ));
        assert!(!<Range<time::PrimitiveDateTime> as ToSql>::accepts(
            &Type::TSTZ_RANGE
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

    // ── MacAddr ───────────────────────────

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_constructor_round_trips_octets() {
        let m = MacAddr::new([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
        assert_eq!(m.octets(), [0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_display_renders_canonical_colon_form_lowercase_hex() {
        let m = MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(format!("{m}"), "aa:bb:cc:dd:ee:ff");
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_str_parses_canonical_colon_form() {
        let m: MacAddr = "aa:bb:cc:dd:ee:ff".parse().unwrap();
        assert_eq!(m, MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_str_parses_hyphen_form() {
        let m: MacAddr = "AA-BB-CC-DD-EE-FF".parse().unwrap();
        assert_eq!(m, MacAddr::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_str_rejects_mixed_separators() {
        let err = "aa:bb-cc:dd-ee:ff".parse::<MacAddr>().unwrap_err();
        assert_eq!(err, MacAddrParseError::MixedSeparators);
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_str_rejects_wrong_length() {
        let err = "aa:bb:cc:dd:ee".parse::<MacAddr>().unwrap_err();
        assert_eq!(err, MacAddrParseError::WrongLength { found: 5 });
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_str_rejects_non_hex_octet() {
        let err = "aa:zz:cc:dd:ee:ff".parse::<MacAddr>().unwrap_err();
        assert_eq!(
            err,
            MacAddrParseError::InvalidOctet {
                index: 1,
                octet: "zz".into(),
            }
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_to_sql_writes_six_bytes_in_order() {
        let m = MacAddr::new([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
        let mut buf = BytesMut::new();
        m.to_sql(&Type::MACADDR, &mut buf).unwrap();
        assert_eq!(buf.as_ref(), &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab]);
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_round_trip_through_wire_codec() {
        let m = MacAddr::new([0xde, 0xad, 0xbe, 0xef, 0x00, 0x42]);
        let mut buf = BytesMut::new();
        m.to_sql(&Type::MACADDR, &mut buf).unwrap();
        let decoded = MacAddr::from_sql(&Type::MACADDR, &buf).unwrap();
        assert_eq!(decoded, m);
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_from_sql_rejects_wrong_length() {
        let err = MacAddr::from_sql(&Type::MACADDR, &[0u8; 5]).unwrap_err();
        assert!(
            err.to_string().contains("6 bytes"),
            "error should name the expected length; got: {err}"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn macaddr_accepts_only_macaddr_type() {
        assert!(<MacAddr as ToSql>::accepts(&Type::MACADDR));
        assert!(!<MacAddr as ToSql>::accepts(&Type::BYTEA));
        assert!(<MacAddr as FromSql>::accepts(&Type::MACADDR));
        assert!(!<MacAddr as FromSql>::accepts(&Type::BYTEA));
    }

    // ── CidrAddr ──────────────────────────

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_accepts_valid_ipv4_network() {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 0));
        let cidr = CidrAddr::new(addr, 24).unwrap();
        assert_eq!(cidr.addr(), addr);
        assert_eq!(cidr.prefix(), 24);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_rejects_ipv4_with_host_bits_set() {
        // 192.168.1.5/24 — the .5 falls inside the host portion.
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 5));
        let err = CidrAddr::new(addr, 24).unwrap_err();
        assert_eq!(err, CidrAddrError::HostBitsSet);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_rejects_prefix_too_large_for_ipv4() {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0));
        let err = CidrAddr::new(addr, 33).unwrap_err();
        assert_eq!(
            err,
            CidrAddrError::PrefixTooLarge {
                prefix: 33,
                max: 32
            }
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_rejects_prefix_too_large_for_ipv6() {
        let addr = std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
        let err = CidrAddr::new(addr, 129).unwrap_err();
        assert_eq!(
            err,
            CidrAddrError::PrefixTooLarge {
                prefix: 129,
                max: 128
            }
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_accepts_all_bits_zero_with_zero_prefix() {
        let v4 = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let v6 = std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
        assert!(CidrAddr::new(v4, 0).is_ok());
        assert!(CidrAddr::new(v6, 0).is_ok());
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_accepts_full_prefix_with_any_address() {
        // /32 accepts any IPv4 address (host portion is empty).
        let v4 = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 20, 30, 40));
        assert!(CidrAddr::new(v4, 32).is_ok());
        // /128 accepts any IPv6 address.
        let v6: std::net::IpAddr = "2001:db8::1".parse().unwrap();
        assert!(CidrAddr::new(v6, 128).is_ok());
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_ipv6_network_validation_walks_boundary_octet() {
        // 2001:db8::/32 — valid; high 32 bits set, rest zero.
        let net: std::net::IpAddr = "2001:db8::".parse().unwrap();
        assert!(CidrAddr::new(net, 32).is_ok());
        // 2001:db8:1::/32 — invalid; the 17th byte (index 4) is non-zero
        // but falls in the host portion of a /32.
        let bad: std::net::IpAddr = "2001:db8:1::".parse().unwrap();
        assert_eq!(
            CidrAddr::new(bad, 32).unwrap_err(),
            CidrAddrError::HostBitsSet
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_to_sql_writes_ipv4_header_then_address() {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 0));
        let cidr = CidrAddr::new(addr, 24).unwrap();
        let mut buf = BytesMut::new();
        cidr.to_sql(&Type::CIDR, &mut buf).unwrap();
        assert_eq!(buf.len(), 8);
        assert_eq!(buf[0], PGSQL_AF_INET); // family
        assert_eq!(buf[1], 24); // prefix
        assert_eq!(buf[2], 1); // is_cidr
        assert_eq!(buf[3], 4); // length
        assert_eq!(&buf[4..8], &[192, 168, 1, 0]);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_to_sql_writes_ipv6_header_then_address() {
        let addr: std::net::IpAddr = "2001:db8::".parse().unwrap();
        let cidr = CidrAddr::new(addr, 32).unwrap();
        let mut buf = BytesMut::new();
        cidr.to_sql(&Type::CIDR, &mut buf).unwrap();
        assert_eq!(buf.len(), 20);
        assert_eq!(buf[0], PGSQL_AF_INET6);
        assert_eq!(buf[1], 32);
        assert_eq!(buf[2], 1);
        assert_eq!(buf[3], 16);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_round_trip_through_wire_codec_ipv4() {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 0));
        let cidr = CidrAddr::new(addr, 8).unwrap();
        let mut buf = BytesMut::new();
        cidr.to_sql(&Type::CIDR, &mut buf).unwrap();
        let decoded = CidrAddr::from_sql(&Type::CIDR, &buf).unwrap();
        assert_eq!(decoded, cidr);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_round_trip_through_wire_codec_ipv6() {
        let addr: std::net::IpAddr = "2001:db8:cafe::".parse().unwrap();
        let cidr = CidrAddr::new(addr, 48).unwrap();
        let mut buf = BytesMut::new();
        cidr.to_sql(&Type::CIDR, &mut buf).unwrap();
        let decoded = CidrAddr::from_sql(&Type::CIDR, &buf).unwrap();
        assert_eq!(decoded, cidr);
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_from_sql_rejects_truncated_header() {
        let err = CidrAddr::from_sql(&Type::CIDR, &[2, 24, 1]).unwrap_err();
        assert!(
            err.to_string().contains("truncated"),
            "error should mention truncated header; got: {err}"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_from_sql_rejects_unknown_address_family() {
        // family = 99 (neither PGSQL_AF_INET nor PGSQL_AF_INET6).
        let err = CidrAddr::from_sql(&Type::CIDR, &[99, 24, 1, 4, 0, 0, 0, 0]).unwrap_err();
        assert!(
            err.to_string().contains("address family"),
            "error should mention address family; got: {err}"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_from_sql_rejects_oversized_ipv4_prefix() {
        let err = CidrAddr::from_sql(&Type::CIDR, &[PGSQL_AF_INET, 33, 1, 4, 192, 168, 1, 0])
            .unwrap_err();
        assert!(
            err.to_string().contains("prefix"),
            "error should mention prefix; got: {err}"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_from_sql_rejects_address_with_host_bits_set() {
        // family=PGSQL_AF_INET (2), prefix=24, is_cidr=1, len=4, 192.168.1.5
        let err = CidrAddr::from_sql(&Type::CIDR, &[PGSQL_AF_INET, 24, 1, 4, 192, 168, 1, 5])
            .unwrap_err();
        assert!(
            err.to_string().contains("host bits"),
            "error should mention host bits; got: {err}"
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_accepts_only_cidr_type() {
        assert!(<CidrAddr as ToSql>::accepts(&Type::CIDR));
        assert!(!<CidrAddr as ToSql>::accepts(&Type::INET));
        assert!(<CidrAddr as FromSql>::accepts(&Type::CIDR));
        assert!(!<CidrAddr as FromSql>::accepts(&Type::INET));
    }

    #[cfg(feature = "network")]
    #[test]
    fn cidraddr_display_renders_addr_slash_prefix() {
        let addr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 0));
        let cidr = CidrAddr::new(addr, 8).unwrap();
        assert_eq!(format!("{cidr}"), "10.0.0.0/8");
    }

    // ── InetAddr tests ────────────────────────────────────────

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_new_accepts_host_bits_set() {
        use std::net::{IpAddr, Ipv4Addr};
        // The defining INET-vs-CIDR difference: 192.168.1.5/24 has host
        // bits past the /24 prefix, which CIDR rejects but INET permits.
        let addr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        let inet = InetAddr::new(addr, 24).expect("INET permits host bits");
        assert_eq!(inet.addr(), addr);
        assert_eq!(inet.prefix(), 24);
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_new_rejects_oversized_prefix() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert_eq!(
            InetAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 33),
            Err(InetAddrError::PrefixTooLarge { prefix: 33, max: 32 })
        );
        assert_eq!(
            InetAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 129),
            Err(InetAddrError::PrefixTooLarge { prefix: 129, max: 128 })
        );
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_display_renders_addr_slash_prefix() {
        use std::net::{IpAddr, Ipv4Addr};
        let inet = InetAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 8).unwrap();
        assert_eq!(inet.to_string(), "10.0.0.1/8");
    }

    // ── InetAddr ToSql tests ────────────────────────────────────

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_to_sql_writes_is_cidr_zero_header() {
        use std::net::{IpAddr, Ipv4Addr};
        let inet = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).unwrap();
        let mut buf = BytesMut::new();
        inet.to_sql(&Type::INET, &mut buf).unwrap();
        // family(2) prefix(24) is_cidr(0) len(4) + 4 addr bytes
        assert_eq!(buf.len(), 8);
        assert_eq!(buf[0], PGSQL_AF_INET);
        assert_eq!(buf[1], 24);
        assert_eq!(buf[2], 0, "is_cidr byte MUST be 0 for INET");
        assert_eq!(buf[3], 4);
        assert_eq!(&buf[4..], &[192, 168, 1, 5]);
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_to_sql_accepts_only_inet_type() {
        assert!(<InetAddr as ToSql>::accepts(&Type::INET));
        assert!(!<InetAddr as ToSql>::accepts(&Type::CIDR));
        assert!(!<InetAddr as ToSql>::accepts(&Type::INT8));
    }

    // ── InetAddr FromSql + DjogiSqlType tests ────────────────────

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_round_trip_preserves_host_bits_and_prefix() {
        use std::net::{IpAddr, Ipv4Addr};
        // Host bits set: 192.168.1.5/24. CIDR would reject this on decode;
        // INET must preserve it byte-for-byte.
        let inet = InetAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 24).unwrap();
        let mut buf = BytesMut::new();
        inet.to_sql(&Type::INET, &mut buf).unwrap();
        let decoded = InetAddr::from_sql(&Type::INET, &buf).unwrap();
        assert_eq!(decoded, inet);
        assert_eq!(decoded.prefix(), 24);
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_round_trip_ipv6() {
        use std::net::{IpAddr, Ipv6Addr};
        let inet = InetAddr::new(
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
            64,
        )
        .unwrap();
        let mut buf = BytesMut::new();
        inet.to_sql(&Type::INET, &mut buf).unwrap();
        let decoded = InetAddr::from_sql(&Type::INET, &buf).unwrap();
        assert_eq!(decoded, inet);
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_from_sql_accepts_only_inet_type() {
        assert!(<InetAddr as FromSql>::accepts(&Type::INET));
        assert!(!<InetAddr as FromSql>::accepts(&Type::CIDR));
    }

    #[cfg(feature = "network")]
    #[test]
    fn inet_addr_djogi_sql_type_is_inet() {
        use crate::descriptor::DjogiSqlType;
        assert_eq!(<InetAddr as DjogiSqlType>::SQL_TYPE, "INET");
    }
}
