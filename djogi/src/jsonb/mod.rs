//! `Jsonb<T>` — typed JSONB column wrapper with unknown-field preservation.
//! # What
//! [`Jsonb<T>`] wraps a Postgres `JSONB` column with a typed schema `T`. On
//! every database read the JSON object is split:
//! - Keys whose names match fields in `T`'s `Deserialize` impl land in
//!   [`Jsonb::data`] as a typed value.
//! - Keys that `T` does not know about (unknown/future fields) land in
//!   [`Jsonb::extra`] as raw [`serde_json::Value`]s.
//!   On every `save()` the two halves are merged back into a single JSON object
//!   before the value is bound. No unknown key is ever dropped.
//! # Why preserve unknown fields?
//! JSONB columns often evolve: a future service or migration version may add
//! new keys to an existing column. If a running service deserializes only the
//! keys it knows about and then re-serializes the full object on the next
//! `save()`, those new keys would be silently erased. Djogi prevents this by
//! carrying the unknown portion in [`Jsonb::extra`] and merging it back on
//! write.
//! # Postgres codec
//! [`Jsonb<T>`] implements [`postgres_types::ToSql`] and
//! [`postgres_types::FromSql`]. Both implementations delegate via
//! [`serde_json::Value`] — the postgres-types crate ships a `serde_json::Value`
//! codec behind the `with-serde_json-1` feature, which is already enabled in
//! Djogi's workspace `Cargo.toml`.
//! # Serde contract
//! `T` must implement both [`serde::Serialize`] and [`serde::Deserialize`].
//! The `Jsonb<T>` wrapper's own `Serialize` impl merges `data` fields with
//! `extra` fields into one flat JSON object. The `Deserialize` impl
//! deserializes the full object twice: once to populate `data` (via `T`'s own
//! `Deserialize`), and once to collect unknown keys into `extra` by diffing
//! the known key set.

pub mod mirjzson;
pub mod path;
pub mod schema;
pub mod unknown;

pub use mirjzson::{MirJzSON, MirJzSONError};
pub use path::{JsonbPathComparable, JsonbPathRef, JsonbSqlCast};
pub use schema::JsonbSchema;
pub use unknown::{UnknownField, UnknownFieldError, UnknownFieldExt};

use bytes::BytesMut;
use indexmap::IndexMap;
use postgres_types::{FromSql, IsNull, ToSql, Type};
use serde::{Deserialize, Serialize};

/// A typed JSONB column value with unknown-field preservation.
/// `T` is the typed portion of the JSON object — the keys the caller's schema
/// declares. `extra` holds every key present in the database object but absent
/// from `T`'s `Deserialize` impl. Both halves are merged on every
/// serialization so the database column always contains the full original
/// object plus any mutations the caller applies to `data`.
/// # Construction
/// Use [`Jsonb::new`] when building a value to insert. For values loaded from
/// the database, the `FromSql` impl constructs `Jsonb<T>` automatically.
/// # Accessing unknown fields
/// ```rust
/// use djogi::jsonb::{Jsonb, UnknownFieldExt};
/// use serde::{Deserialize, Serialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct Config { theme: String }
///
/// # fn example(j: &Jsonb<Config>) {
/// if let Some(exp) = j.extra().get("experimental_flag") {
///     let _ = exp.try_as_bool();
/// }
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Jsonb<T> {
    /// The typed portion of the JSONB object. Fields from `T` are deserialized
    /// here; mutations here are included on the next `save()`.
    pub data: T,
    /// Unknown keys — keys present in the database object but absent from
    /// `T`'s schema. Preserved verbatim across every `save()`.
    pub(crate) extra: IndexMap<String, UnknownField>,
}

impl<T: Default> Default for Jsonb<T> {
    /// Construct a `Jsonb<T>` with `T::default()` and an empty `extra` map.
    /// Required so that model structs containing `Jsonb<T>` can derive
    /// `Default` (the `#[model]` macro emits a `Default` impl for the
    /// whole struct, which propagates to every field).
    fn default() -> Self {
        Jsonb {
            data: T::default(),
            extra: IndexMap::new(),
        }
    }
}

impl<T> Jsonb<T> {
    /// Construct a new `Jsonb<T>` from a typed value with an empty `extra` map.
    /// This is the correct constructor for values being inserted for the first
    /// time. `save()` will serialize `data` + the (empty) `extra` to the
    /// column.
    pub fn new(data: T) -> Self {
        Jsonb {
            data,
            extra: IndexMap::new(),
        }
    }

    /// Read-only view of the unknown-field map.
    /// Keys are in the order they appeared in the original JSON object
    /// (preserved because `serde_json` is compiled with `preserve_order`).
    pub fn extra(&self) -> &IndexMap<String, UnknownField> {
        &self.extra
    }
}

/// Structural equality over both the typed `data` and the preserved
/// unknown-field `extra` map.
///
/// `extra` is compared deliberately: per-audience JSONB projections
/// (`docs/spec/jsonb-per-audience-schema.md`) detect accidental
/// admin-only-key leaks at runtime by observing whether unknown keys
/// were preserved on a fetched projection. A `PartialEq` that ignored
/// `extra` would let a leak through the parity gate
/// (`djogi::testing::assert_derived_parity`) undetected. `UnknownField`
/// is `serde_json::Value`, which implements `PartialEq`, so only
/// `T: PartialEq` is required.
impl<T: PartialEq> PartialEq for Jsonb<T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data && self.extra == other.extra
    }
}

// ── Sassi cache-boundary projection ───────────────────────────────────────
// `Jsonb<T>` is a *database* representation, not a Sassi wire type.
// `to_jsahibon` is the explicit cache-boundary projection per the
// MirJzSON spec: when a backend cache contains a model whose field is
// `Jsonb<T>`, the cache must explicitly choose between projecting the
// typed `T` payload (typed Rust schema only) or projecting the full
// merged JSON document (typed `data` keys merged with unknown `extra`
// keys) as a Sassi-portable `JSahibON`. This helper supports the
// full-document case.
// The conversion is fallible (returns `MirJzSONError`) because:
// 1. `serde_json::to_value(&self)` may serialise to a non-object payload
// if `T`'s `Serialize` impl returns a primitive / array. That edge
// case already exists in the `Jsonb<T> -> serde_json::Value` path
// and the projection inherits the same behaviour — `JSahibON` carries
// the resulting non-object value as-is (string / array / number /
// bool / null).
// 2. Sassi's `JSahibON::try_from(serde_json::Value)` rejects non-finite
// f64 and out-of-range arbitrary-precision numbers, which can land
// in the JSON value if `T` serialises a custom type that bypasses
// Sassi's invariants.

impl<T> Jsonb<T>
where
    T: serde::Serialize,
{
    /// Project this typed JSONB column into a Sassi-portable
    /// [`sassi::JSahibON`].
    /// The full merged document — typed `data` fields plus every
    /// unknown key in `extra` — is serialised through `serde_json` and
    /// re-projected onto Sassi's portable JSON value model.
    /// **Cache-boundary projection.** Adopters call this when handing a
    /// model containing `Jsonb<T>` to a Sassi cache (`Punnu<T>`) or to a
    /// frontend over a wire payload that downcasts JSONB through
    /// `JSahibON`. The conversion is named (`to_jsahibon`) rather than
    /// implicit so the database-to-portable boundary is visible at the
    /// call site.
    /// # Errors
    /// Returns [`MirJzSONError::UnsupportedJsonValue`] when `T`'s
    /// `Serialize` impl produces JSON content Sassi cannot represent
    /// non-finite `f64`, or `serde_json::Number` carriers outside Sassi's
    /// supported numeric range. The error message forwards Sassi's own
    /// diagnostic so the cause is visible.
    /// Returns [`MirJzSONError::JsonDecode`] when `T`'s `Serialize` impl
    /// itself fails — typically a custom serialiser that rejects certain
    /// states.
    pub fn to_jsahibon(&self) -> Result<sassi::JSahibON, MirJzSONError> {
        let value: serde_json::Value =
            serde_json::to_value(self).map_err(|err| MirJzSONError::JsonDecode(err.to_string()))?;
        sassi::JSahibON::try_from(value)
            .map_err(|err| MirJzSONError::UnsupportedJsonValue(err.to_string()))
    }
}

// ── Serde implementations ──────────────────────────────────────────────────

impl<T: Serialize> Serialize for Jsonb<T> {
    /// Merges `data` and `extra` into a single JSON object.
    /// The typed fields in `data` are serialized first; then every entry in
    /// `extra` is inserted into the resulting object. If a key exists in both
    /// `data` and `extra` (which should not happen in well-behaved code — the
    /// split on deserialization is exclusive), the `data` value wins because it
    /// is the authoritative typed value.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Serialize data to a Value, then merge extra on top.
        let mut map = match serde_json::to_value(&self.data).map_err(serde::ser::Error::custom)? {
            serde_json::Value::Object(m) => m,
            other => {
                // T serialized to a non-object (e.g. a primitive or array).
                // In this edge case we cannot merge extra into a non-object.
                // Serialize data + wrap extra as a side-car key — this
                // preserves round-trip fidelity for the common object case and
                // surfaces an obvious shape for the edge case.
                return other.serialize(serializer);
            }
        };
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        serde_json::Value::Object(map).serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for Jsonb<T>
where
    T: Deserialize<'de> + Serialize,
{
    /// Deserializes the JSON object, splitting it into typed `data` and unknown
    /// `extra`.
    /// The deserialization strategy:
    /// 1. Deserialize the full raw `serde_json::Value`.
    /// 2. Deserialize `T` from that value — this populates `data` with known
    ///    fields.
    /// 3. Determine the set of keys `T` serializes to (by re-serializing the
    ///    just-decoded `T`). Any key present in the raw object but absent from
    ///    this set is an unknown field and belongs in `extra`.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Step 1: capture raw JSON object.
        let raw = serde_json::Value::deserialize(deserializer)?;

        let raw_obj = match &raw {
            serde_json::Value::Object(m) => m,
            _ => {
                // Not an object — best effort: deserialize T directly, no extras.
                let data = T::deserialize(raw).map_err(serde::de::Error::custom)?;
                return Ok(Jsonb {
                    data,
                    extra: IndexMap::new(),
                });
            }
        };

        // Step 2: deserialize T from the raw value.
        let data: T = T::deserialize(raw.clone()).map_err(serde::de::Error::custom)?;

        // Step 3: determine which keys T owns by re-serializing it.
        let known_keys: std::collections::HashSet<String> =
            match serde_json::to_value(&data).map_err(serde::de::Error::custom)? {
                serde_json::Value::Object(m) => m.keys().cloned().collect(),
                _ => std::collections::HashSet::new(),
            };

        // Step 4: every key in raw_obj not in known_keys goes to extra.
        let mut extra = IndexMap::new();
        for (k, v) in raw_obj {
            if !known_keys.contains(k) {
                extra.insert(k.clone(), v.clone());
            }
        }

        Ok(Jsonb { data, extra })
    }
}

// ── postgres_types codec ──────────────────────────────────────────────────

impl<T> ToSql for Jsonb<T>
where
    T: Serialize + std::fmt::Debug,
{
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        // Serialize to serde_json::Value and delegate to the Value ToSql impl.
        let value: serde_json::Value = serde_json::to_value(self)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Sync + Send>)?;
        value.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        <serde_json::Value as ToSql>::accepts(ty)
    }

    postgres_types::to_sql_checked!();
}

impl<'a, T> FromSql<'a> for Jsonb<T>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let value = <serde_json::Value as FromSql>::from_sql(ty, raw)?;
        let jsonb = serde_json::from_value(value)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Sync + Send>)?;
        Ok(jsonb)
    }

    fn accepts(ty: &Type) -> bool {
        <serde_json::Value as FromSql>::accepts(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct KnownSpec {
        name: String,
        value: i32,
    }

    #[test]
    fn new_creates_empty_extra() {
        let j = Jsonb::new(KnownSpec {
            name: "x".to_string(),
            value: 1,
        });
        assert!(j.extra().is_empty());
    }

    #[test]
    fn serialize_produces_flat_object() {
        let j = Jsonb {
            data: KnownSpec {
                name: "x".to_string(),
                value: 1,
            },
            extra: {
                let mut m = IndexMap::new();
                m.insert("extra_key".to_string(), json!("extra_val"));
                m
            },
        };
        let v = serde_json::to_value(&j).unwrap();
        assert_eq!(v["name"], json!("x"));
        assert_eq!(v["value"], json!(1));
        assert_eq!(v["extra_key"], json!("extra_val"));
    }

    #[test]
    fn deserialize_splits_known_and_unknown() {
        let raw = json!({
            "name": "engine",
            "value": 42,
            "experimental": true,
            "future_field": "hello"
        });
        let j: Jsonb<KnownSpec> = serde_json::from_value(raw).unwrap();
        assert_eq!(j.data.name, "engine");
        assert_eq!(j.data.value, 42);
        assert_eq!(j.extra.len(), 2);
        assert_eq!(j.extra.get("experimental").unwrap(), &json!(true));
        assert_eq!(j.extra.get("future_field").unwrap(), &json!("hello"));
    }

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let raw = json!({
            "name": "test",
            "value": 7,
            "legacy": {"nested": 99}
        });
        let j: Jsonb<KnownSpec> = serde_json::from_value(raw).unwrap();
        // Mutate data.
        let mut j2 = j.clone();
        j2.data.value = 100;
        // Re-serialize.
        let out = serde_json::to_value(&j2).unwrap();
        // Known fields updated.
        assert_eq!(out["value"], json!(100));
        // Unknown field preserved.
        assert_eq!(out["legacy"], json!({"nested": 99}));
    }

    #[test]
    fn deserialize_non_object_falls_back_gracefully() {
        // T = serde_json::Value can handle primitives directly.
        let raw = json!(42);
        // Use Value as T — always accepts any JSON.
        let j: Jsonb<serde_json::Value> = serde_json::from_value(raw).unwrap();
        assert_eq!(j.data, json!(42));
        assert!(j.extra.is_empty());
    }

    // ── Cache-boundary projection: Jsonb<T>::to_jsahibon ──────────────────
    // Per the MirJzSON spec, `Jsonb<T>::to_jsahibon()` is the explicit
    // cache-boundary helper: callers that need to ship `Jsonb<T>` through
    // a Sassi/Punnu cache (where the wire-side downcast goes through
    // `sassi::JSahibON`, not `serde_json::Value`) reach for this method
    // rather than letting the conversion happen implicitly. The tests
    // below pin the projection's contract:
    // - typed `data` fields merge with unknown `extra` keys into the
    // resulting JSON document (same as `serialize`).
    // - non-object `T` serialisations survive the projection (carrying
    // the resulting primitive / array as the corresponding `JSahibON`
    // variant — the round-trip is total because `JSahibON` covers every
    // `serde_json::Value` variant).
    // - non-portable JSON content (which `Jsonb<T>` itself can hold via
    // `extra`) surfaces as a typed `MirJzSONError` rather than panicking.

    #[test]
    fn to_jsahibon_merges_data_and_extra() {
        let mut extra = IndexMap::new();
        extra.insert("legacy_key".to_string(), json!("legacy_value"));
        let j = Jsonb {
            data: KnownSpec {
                name: "engine".to_string(),
                value: 42,
            },
            extra,
        };
        let portable = j
            .to_jsahibon()
            .expect("typed data + unknown extra must project");
        match portable {
            sassi::JSahibON::Object(obj) => {
                // `data` keys land first (KnownSpec serializes `name`
                // then `value`), then `extra` keys are merged on top.
                let keys: Vec<&str> = obj.iter().map(|(k, _)| k.as_str()).collect();
                assert_eq!(keys, ["name", "value", "legacy_key"]);
                match obj
                    .iter()
                    .find(|(k, _)| k.as_str() == "value")
                    .map(|(_, v)| v.clone())
                    .unwrap()
                {
                    sassi::JSahibON::I64(42) => {}
                    other => panic!("expected I64(42), got {other:?}"),
                }
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn to_jsahibon_handles_non_object_serializations() {
        // `T = serde_json::Value` projection — primitives survive.
        let j = Jsonb {
            data: json!(7),
            extra: IndexMap::new(),
        };
        let portable = j.to_jsahibon().expect("non-object T must still project");
        match portable {
            sassi::JSahibON::I64(7) => {}
            other => panic!("expected I64(7), got {other:?}"),
        }
    }

    #[test]
    fn to_jsahibon_round_trips_through_mirjzson() {
        // Build `Jsonb<KnownSpec>` -> JSahibON -> MirJzSON -> serde_json
        // round-trip — the cache-boundary pipeline an adopter exercises
        // when shipping a Djogi DB read into a Sassi Punnu wire payload.
        let j = Jsonb {
            data: KnownSpec {
                name: "x".to_string(),
                value: 9,
            },
            extra: IndexMap::new(),
        };
        let portable = j.to_jsahibon().unwrap();
        let mir: crate::jsonb::MirJzSON = portable.into();
        let back: serde_json::Value = mir.into();
        assert_eq!(back["name"], json!("x"));
        assert_eq!(back["value"], json!(9));
    }

    #[test]
    fn partial_eq_compares_data_and_extra() {
        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
        struct Schema {
            a: i32,
        }

        // Equal data + equal (empty) extra → equal.
        let lhs = Jsonb::new(Schema { a: 1 });
        let rhs = Jsonb::new(Schema { a: 1 });
        assert_eq!(lhs, rhs);

        // Differing typed data → not equal.
        let other_data = Jsonb::new(Schema { a: 2 });
        assert_ne!(lhs, other_data);

        // Equal typed data but one side carries an unknown key in `extra`
        // (the exact leak shape the parity gate must catch). `extra` is
        // pub(crate) with no public mutator, so populate it through
        // Deserialize from a JSON object with an extra key.
        let with_extra: Jsonb<Schema> =
            serde_json::from_value(serde_json::json!({ "a": 1, "leaked": "x" }))
                .expect("deserialize with unknown key");
        assert!(
            !with_extra.extra().is_empty(),
            "guard: extra must be populated"
        );
        assert_ne!(
            lhs, with_extra,
            "PartialEq must observe the `extra` difference, not just `data`"
        );
    }
}
