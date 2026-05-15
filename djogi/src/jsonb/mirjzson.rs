//! `MirJzSON` — Djogi's raw JSONB escape hatch over Sassi `JSahibON`.
//!
//! # What
//!
//! [`MirJzSON`] is the explicit JSONB field type for genuinely *unschemed*
//! JSON columns — payloads whose shape is owned by an external system, or
//! whose schema evolves faster than the Rust model can track. Unlike
//! [`Jsonb<T>`](crate::jsonb::Jsonb), `MirJzSON` carries no typed schema:
//! the entire JSON document is represented as a Sassi [`sassi::JSahibON`]
//! value, which preserves object insertion order, distinguishes JSON
//! `null` from absence, and exposes numeric carriers (`I64` / `U64` /
//! `F64`) with cross-numeric equality softening.
//!
//! # Why a Djogi wrapper around `sassi::JSahibON`?
//!
//! Djogi's database codec, model gating, query construction, and SQL
//! lowering all need to talk about a JSONB column that round-trips through
//! Sassi's portable JSON value model. Importing `sassi::JSahibON` at every
//! adopter site would expose Sassi's internal cache value model on the
//! database storage boundary; a thin Djogi wrapper keeps the surface
//! Djogi-owned while delegating value semantics (numeric carrier rules,
//! object-equality contracts, JSON `null` semantics) entirely to Sassi.
//!
//! # Construction
//!
//! Three named construction routes; **no** `Default` impl. Djogi
//! intentionally requires every `MirJzSON` to come from one of:
//!
//! - [`From<sassi::JSahibON>`](MirJzSON#impl-From<JSahibON>-for-MirJzSON) —
//!   already-portable value, no validation needed.
//! - [`TryFrom<serde_json::Value>`](MirJzSON#impl-TryFrom<Value>-for-MirJzSON)
//!   — fallible bridge through Sassi's `serde-json-bridge`; rejects
//!   non-finite floats and `serde_json::Number` carriers outside Sassi's
//!   supported range.
//! - The Postgres `FromSql` impl — reads JSONB bytes off the wire,
//!   projects to [`sassi::JSahibON`] via `serde_json::Value`.
//!
//! There is intentionally no [`From<MirJzSON> for JSahibON`] — projection
//! is named ([`as_jsahibon`](MirJzSON::as_jsahibon) /
//! [`into_jsahibon`](MirJzSON::into_jsahibon)) so the
//! database-to-portable boundary is visible at call sites.
//!
//! # Trait posture
//!
//! `MirJzSON` is **not** `PartialEq` / `Eq` / `Hash` / `PartialOrd`. Whole-
//! value JSON predicates go through the explicit JSON predicate methods
//! on `DjogiField<M, MirJzSON>::jsahibon()`, not through root
//! `DjogiField::eq`. Removing the equality / ordering trait impls is
//! deliberate: a `MirJzSON == MirJzSON` comparison at the model layer
//! would silently disagree with `jsahibon().eq_json(_)` because Sassi's
//! `JSahibON` equality folds across numeric carriers (`I64(1) ==
//! U64(1) == F64(1.0)`) but Rust `PartialEq` on the wrapper would imply
//! reflexivity over the literal `JSahibON` variant — diverging
//! semantics for the same surface predicate.
//!
//! # Relationship to `Jsonb<T>`
//!
//! `Jsonb<T>` remains the typed-schema JSONB path: structural validation
//! at decode, unknown-field preservation, typed `.path::<V>("a.b")`
//! queries through `JsonbPathRef`. `MirJzSON` is the raw-JSONB sibling:
//! the schema is the database's, not Rust's. Djogi requires a
//! `#[mirjzson(justification = "...")]` attribute on every `MirJzSON`
//! field to make the "this is genuinely unschemed" decision visible in
//! code review.

use bytes::BytesMut;
use postgres_types::{FromSql, IsNull, ToSql, Type};

/// Errors produced while decoding [`MirJzSON`] from external representations.
///
/// `MirJzSON`'s public construction routes (`TryFrom<serde_json::Value>`,
/// `postgres_types::FromSql`) all funnel non-portable JSON content through
/// this typed error rather than panicking. Callers receive a `MirJzSON` only
/// when every numeric carrier and floating-point value fits Sassi's
/// `JSahibON` value model — non-finite f64, `serde_json::Number` carriers
/// outside the supported range, and structural decode failures all surface
/// here.
///
/// Marked `#[non_exhaustive]` so future Sassi value-model evolutions
/// (e.g. arbitrary-precision numeric carriers) can add variants without
/// breaking downstream `match` blocks.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MirJzSONError {
    /// The source `serde_json::Value` (or the JSONB bytes decoded through
    /// `serde_json::Value`) contained content Sassi's `JSahibON` model
    /// cannot represent — non-finite `f64`, `serde_json::Number` carriers
    /// outside Sassi's supported range, or a future structural extension.
    /// The inner string is Sassi's own error description; Djogi forwards
    /// it verbatim so the error message tracks Sassi's contract rather
    /// than translating across two error surfaces.
    #[error("MirJzSON cannot represent the source JSON value: {0}")]
    UnsupportedJsonValue(String),

    /// Decoding raw JSONB bytes from Postgres failed at the
    /// `serde_json::Value` layer. The inner message is `serde_json`'s
    /// own diagnostic — typically a malformed UTF-8 sequence or an
    /// unsupported version byte (Postgres prefixes binary JSONB with a
    /// version byte that must be `\x01`).
    #[error("MirJzSON failed to decode JSONB bytes: {0}")]
    JsonDecode(String),
}

/// Djogi's raw JSONB escape-hatch field type.
///
/// Wraps a Sassi [`sassi::JSahibON`] portable JSON value. See the
/// module-level docs for the design rationale, construction routes, and
/// the deliberately-absent `PartialEq` / `Hash` / `PartialOrd` posture.
///
/// # Layout
///
/// `MirJzSON` is `#[repr(transparent)]` around its single
/// `portable: sassi::JSahibON` field. This is **load-bearing for the
/// query path**: the `.jsahibon()` accessor lifts a
/// `fn(&M) -> &MirJzSON` extractor into a
/// `fn(&M) -> &sassi::JSahibON` extractor by transmuting the function
/// pointer — sound only because the two reference types share a
/// layout and ABI under `repr(transparent)`. The Rust reference
/// guarantees this for the field's reference *and* for niche
/// optimizations on `Option<MirJzSON>`, which is required for the
/// optional-field builder.
///
/// # Example
///
/// ```ignore
/// use djogi::prelude::*;
/// use djogi::jsonb::MirJzSON;
///
/// #[model(table = "events")]
/// pub struct Event {
///     #[mirjzson(justification = "payload schema is owned by upstream partner SDK")]
///     pub payload: MirJzSON,
/// }
/// ```
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct MirJzSON {
    portable: sassi::JSahibON,
}

impl MirJzSON {
    /// Borrow the inner Sassi portable JSON value.
    ///
    /// Named projection — the database-to-portable boundary is visible at
    /// the call site. Callers that need to walk the JSON tree directly
    /// (debug formatters, custom Punnu evaluators, downstream Sassi
    /// integrations) read through this accessor.
    pub fn as_jsahibon(&self) -> &sassi::JSahibON {
        &self.portable
    }

    /// Consume into the inner Sassi portable JSON value.
    ///
    /// Mirror of [`as_jsahibon`](Self::as_jsahibon) for the by-value path.
    /// Useful when the caller owns the `MirJzSON` and wants to hand the
    /// `JSahibON` to a Sassi-side API (`Punnu::insert`, cache snapshot,
    /// etc.) without cloning.
    pub fn into_jsahibon(self) -> sassi::JSahibON {
        self.portable
    }
}

impl From<sassi::JSahibON> for MirJzSON {
    /// Lift an already-portable `JSahibON` into [`MirJzSON`].
    ///
    /// Infallible — the inner value already lives in Sassi's portable
    /// model, so no further validation is required. The wrapping is a
    /// zero-cost move; callers that want to keep the original Sassi
    /// value can use [`MirJzSON::as_jsahibon`] or
    /// [`MirJzSON::into_jsahibon`] to project back.
    fn from(value: sassi::JSahibON) -> Self {
        Self { portable: value }
    }
}

impl TryFrom<serde_json::Value> for MirJzSON {
    type Error = MirJzSONError;

    /// Project a `serde_json::Value` into [`MirJzSON`] through Sassi's
    /// `serde-json-bridge`.
    ///
    /// Fails when:
    ///
    /// - The input contains a non-finite `f64` (Sassi rejects `NaN` /
    ///   `±Infinity` in its `JFiniteF64` carrier).
    /// - A `serde_json::Number` cannot fit any of Sassi's `i64` / `u64` /
    ///   finite `f64` carriers (arbitrary-precision number outside both
    ///   signed and unsigned 64-bit ranges).
    ///
    /// Both failure modes surface as [`MirJzSONError::UnsupportedJsonValue`]
    /// with Sassi's own diagnostic message forwarded verbatim — the error
    /// tracks Sassi's contract rather than translating across two surfaces.
    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        sassi::JSahibON::try_from(value)
            .map(MirJzSON::from)
            .map_err(|err| MirJzSONError::UnsupportedJsonValue(err.to_string()))
    }
}

impl From<MirJzSON> for serde_json::Value {
    /// Project a [`MirJzSON`] into `serde_json::Value` through Sassi's
    /// `serde-json-bridge`.
    ///
    /// Total: every variant of Sassi's `JSahibON` value model maps onto
    /// a `serde_json::Value` (numeric carriers all round-trip into
    /// `serde_json::Number`; the finite-`f64` invariant guarantees
    /// `Number::from_f64` returns `Some` for the `F64` arm).
    ///
    /// Useful for adopter code that needs to ship `MirJzSON` through a
    /// `serde_json`-native serialization path (HTTP responses, log
    /// payloads, etc.) without round-tripping through the Postgres codec.
    fn from(value: MirJzSON) -> Self {
        value.portable.into()
    }
}

// ── Postgres JSONB codec ───────────────────────────────────────────────────
//
// `ToSql` / `FromSql` route through `serde_json::Value` because:
//
// 1. `tokio-postgres` already ships a vetted `serde_json::Value` codec
//    behind its `with-serde_json-1` feature (already enabled in djogi's
//    workspace `Cargo.toml`). Wiring `MirJzSON` through that codec
//    inherits its battle-tested binary-JSONB framing (the leading
//    `\x01` version byte, the UTF-8 payload validation).
// 2. Sassi's `serde-json-bridge` is the **single owner** of the
//    `serde_json::Value <-> JSahibON` projection. Reimplementing the
//    bridge inside Djogi's codec path would let the two definitions
//    drift — Sassi could tighten numeric-range checks without Djogi
//    learning of the change.

impl ToSql for MirJzSON {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // Clone the inner `JSahibON` so the codec can consume it through
        // `serde_json::Value`. `JSahibON: Clone` is cheap for primitives
        // and `O(n)` for objects/arrays — the same cost as the `Jsonb<T>`
        // codec's `serde_json::to_value(self)` round-trip.
        let value: serde_json::Value = self.portable.clone().into();
        value.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        <serde_json::Value as ToSql>::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl<'a> FromSql<'a> for MirJzSON {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        // Step 1: decode JSONB bytes into a `serde_json::Value`. The
        // codec wraps the inner error in a `Box<dyn Error>` because the
        // postgres-types FromSql contract requires it.
        let value = <serde_json::Value as FromSql>::from_sql(ty, raw).map_err(|err| {
            Box::new(MirJzSONError::JsonDecode(err.to_string()))
                as Box<dyn std::error::Error + Sync + Send>
        })?;
        // Step 2: project into Sassi's `JSahibON` value model. Non-
        // portable JSONB content (non-finite f64, out-of-range numeric
        // carriers) surfaces as a typed `MirJzSONError::UnsupportedJsonValue`
        // — the codec never panics.
        let portable = sassi::JSahibON::try_from(value).map_err(|err| {
            Box::new(MirJzSONError::UnsupportedJsonValue(err.to_string()))
                as Box<dyn std::error::Error + Sync + Send>
        })?;
        Ok(MirJzSON { portable })
    }

    fn accepts(ty: &Type) -> bool {
        <serde_json::Value as FromSql>::accepts(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `From<JSahibON> for MirJzSON` is the identity-style wrap — the
    /// inner value survives projection round-trips exactly.
    #[test]
    fn from_jsahibon_roundtrips_through_projection() {
        let original = sassi::JSahibON::I64(42);
        let mir: MirJzSON = original.clone().into();
        let back = mir.into_jsahibon();
        assert_eq!(back, original);
    }

    /// `as_jsahibon` borrows without consuming; the wrapper survives.
    #[test]
    fn as_jsahibon_borrows_without_consuming() {
        let mir: MirJzSON = sassi::JSahibON::Bool(true).into();
        let borrowed = mir.as_jsahibon();
        match borrowed {
            sassi::JSahibON::Bool(true) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
        // mir is still owned — drop the borrow and take ownership.
        let _owned = mir.into_jsahibon();
    }

    /// `TryFrom<serde_json::Value>` accepts a plain JSON object and
    /// projects every key into the `JObject` carrier in insertion order.
    #[test]
    fn try_from_json_value_accepts_object() {
        let raw = json!({"a": 1, "b": "two"});
        let mir = MirJzSON::try_from(raw).expect("plain object must convert");
        let portable = mir.into_jsahibon();
        match portable {
            sassi::JSahibON::Object(obj) => {
                assert_eq!(obj.len(), 2);
                // Sassi's JObject preserves insertion order.
                let keys: Vec<&str> = obj.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(keys, ["a", "b"]);
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    /// `TryFrom<serde_json::Value>` rejects numeric content that cannot
    /// be projected onto any of Sassi's `i64` / `u64` / finite-`f64`
    /// carriers. With `arbitrary_precision` enabled on `serde_json`,
    /// numbers parse as strings and only fail conversion when their
    /// magnitude exceeds `f64::MAX` — at which point `as_f64()` returns
    /// `None` and the bridge surfaces a typed `UnsupportedJsonValue`.
    ///
    /// Numbers that fit `f64` (even very large ones like `1e35`) are
    /// accepted as `JSahibON::F64`; that is documented Sassi semantics
    /// — the rejection only fires when *every* numeric carrier is out of
    /// range. The literal below has roughly `10^500`, well past `f64`'s
    /// 1.797e308 ceiling.
    #[test]
    fn try_from_json_value_rejects_out_of_range_numbers() {
        // Build a number string with ~500 leading digits — clearly past
        // every numeric carrier Sassi supports. With `arbitrary_precision`
        // serde_json keeps it as a string-backed `Number`, then as_i64 /
        // as_u64 / as_f64 all return None, surfacing the typed error.
        let huge_str = format!("1{}", "0".repeat(500));
        let huge: serde_json::Value =
            serde_json::from_str(&huge_str).expect("arbitrary-precision number parses");
        let err = MirJzSON::try_from(huge).expect_err("oversized number must fail");
        match err {
            MirJzSONError::UnsupportedJsonValue(_) => {}
            other => panic!("expected UnsupportedJsonValue, got {other:?}"),
        }
    }

    /// `From<MirJzSON> for serde_json::Value` is total — every Sassi
    /// `JSahibON` variant projects without panicking.
    #[test]
    fn into_json_value_is_total_for_all_carriers() {
        let cases = vec![
            sassi::JSahibON::Null,
            sassi::JSahibON::Bool(false),
            sassi::JSahibON::I64(-7),
            sassi::JSahibON::U64(u64::MAX),
            // 2.5 is a finite non-PI-approximation literal; clippy's
            // `approx_constant` lint flags `3.14` because the test's intent
            // is "any finite value" and the lint expects authors mean
            // `std::f64::consts::PI`. The value is irrelevant — only the
            // finite-`f64` round-trip matters.
            sassi::JSahibON::F64(sassi::JFiniteF64::try_new(2.5).unwrap()),
            sassi::JSahibON::String("hello".to_string()),
            sassi::JSahibON::Array(vec![sassi::JSahibON::I64(1), sassi::JSahibON::I64(2)]),
        ];
        for portable in cases {
            let mir: MirJzSON = portable.clone().into();
            let json: serde_json::Value = mir.into();
            // Round-trip back to JSahibON should reproduce the original.
            let mir2 = MirJzSON::try_from(json).expect("projected value must round-trip");
            assert_eq!(mir2.into_jsahibon(), portable);
        }
    }

    /// JSON `null` and "missing" stay distinct: `JSahibON::Null`
    /// survives projection as the JSON literal `null`, not as an absent
    /// key. This is the property `IsJsonNull` / `Missing` predicates
    /// rely on at the SQL boundary.
    #[test]
    fn json_null_and_missing_stay_distinct() {
        let mir: MirJzSON = sassi::JSahibON::Null.into();
        let json: serde_json::Value = mir.into();
        assert_eq!(json, serde_json::Value::Null);
        // Re-project: still JSahibON::Null.
        let back = MirJzSON::try_from(json).unwrap();
        assert_eq!(back.into_jsahibon(), sassi::JSahibON::Null);
    }
}
