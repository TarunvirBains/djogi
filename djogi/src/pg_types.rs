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
//! | Type        | Postgres column     | Feature flag | Issue        |
//! |-------------|---------------------|--------------|--------------|
//! | [`Interval`] | `INTERVAL`         | (always on)  | djogi#212    |
//!
//! Additional newtypes (`MacAddr` / `CidrAddr` for `MACADDR` / `CIDR`,
//! a typed `DOMAIN` discriminator, and similar coverage gaps) ship in
//! follow-on dispatches against the djogi#170 umbrella. The module is
//! structured so each newtype is independently `#[cfg]`-gateable when
//! it lands; the Interval newtype is unconditional because the wire
//! codec depends only on stdlib byte primitives.
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
/// `Interval` derives `PartialEq` / `Eq` against the three component
/// fields. Two intervals are equal iff every field matches — `1 month`
/// and `30 days` are NOT equal even though they often render the same
/// way to a human. This matches Postgres's structural-equality
/// semantics for `INTERVAL` columns.
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
}
