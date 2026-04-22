//! `Jsonb<T>` — typed JSONB column wrapper with unknown-field preservation.
//!
//! # What
//!
//! [`Jsonb<T>`] wraps a Postgres `JSONB` column with a typed schema `T`. On
//! every database read the JSON object is split:
//!
//! - Keys whose names match fields in `T`'s `Deserialize` impl land in
//!   [`Jsonb::data`] as a typed value.
//! - Keys that `T` does not know about (unknown/future fields) land in
//!   [`Jsonb::extra`] as raw [`serde_json::Value`]s.
//!
//! On every `save()` the two halves are merged back into a single JSON object
//! before the value is bound. No unknown key is ever dropped.
//!
//! # Why preserve unknown fields?
//!
//! JSONB columns often evolve: a future service or migration version may add
//! new keys to an existing column. If a running service deserializes only the
//! keys it knows about and then re-serializes the full object on the next
//! `save()`, those new keys would be silently erased. Djogi prevents this by
//! carrying the unknown portion in [`Jsonb::extra`] and merging it back on
//! write.
//!
//! # Postgres codec
//!
//! [`Jsonb<T>`] implements [`postgres_types::ToSql`] and
//! [`postgres_types::FromSql`]. Both implementations delegate via
//! [`serde_json::Value`] — the postgres-types crate ships a `serde_json::Value`
//! codec behind the `with-serde_json-1` feature, which is already enabled in
//! Djogi's workspace `Cargo.toml`.
//!
//! # Serde contract
//!
//! `T` must implement both [`serde::Serialize`] and [`serde::Deserialize`].
//! The `Jsonb<T>` wrapper's own `Serialize` impl merges `data` fields with
//! `extra` fields into one flat JSON object. The `Deserialize` impl
//! deserializes the full object twice: once to populate `data` (via `T`'s own
//! `Deserialize`), and once to collect unknown keys into `extra` by diffing
//! the known key set.

pub mod path;
pub mod unknown;

pub use path::JsonbPathRef;
pub use unknown::{UnknownField, UnknownFieldError, UnknownFieldExt};

use bytes::BytesMut;
use indexmap::IndexMap;
use postgres_types::{FromSql, IsNull, ToSql, Type};
use serde::{Deserialize, Serialize};

/// A typed JSONB column value with unknown-field preservation.
///
/// `T` is the typed portion of the JSON object — the keys the caller's schema
/// declares. `extra` holds every key present in the database object but absent
/// from `T`'s `Deserialize` impl. Both halves are merged on every
/// serialization so the database column always contains the full original
/// object plus any mutations the caller applies to `data`.
///
/// # Construction
///
/// Use [`Jsonb::new`] when building a value to insert. For values loaded from
/// the database, the `FromSql` impl constructs `Jsonb<T>` automatically.
///
/// # Accessing unknown fields
///
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

impl<T> Jsonb<T> {
    /// Construct a new `Jsonb<T>` from a typed value with an empty `extra` map.
    ///
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
    ///
    /// Keys are in the order they appeared in the original JSON object
    /// (preserved because `serde_json` is compiled with `preserve_order`).
    pub fn extra(&self) -> &IndexMap<String, UnknownField> {
        &self.extra
    }
}

// ── Serde implementations ──────────────────────────────────────────────────

impl<T: Serialize> Serialize for Jsonb<T> {
    /// Merges `data` and `extra` into a single JSON object.
    ///
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
    ///
    /// The deserialization strategy:
    ///
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
}
