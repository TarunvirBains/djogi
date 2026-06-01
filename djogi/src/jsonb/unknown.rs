//! Unknown-field type alias and conversion extension trait.
//! # What
//! [`UnknownField`] is a type alias for [`serde_json::Value`]. Every JSON key
//! present in the database column but absent from the typed `T` schema in
//! [`super::Jsonb<T>`] lands here during deserialization. It is preserved
//! across every [`Model::save`](crate::model::Model::save) call so that
//! unknown columns from future schema versions (or sibling services) are
//! never silently dropped.
//! # Why a type alias, not a newtype?
//! A newtype would require wrapping + unwrapping at every boundary where the
//! user inspects the extra map. The type alias keeps the public surface
//! minimal: call [`UnknownFieldExt::try_as_str`] / [`try_as_i64`] etc.
//! directly on the `&UnknownField` reference from
//! [`Jsonb::extra`](super::Jsonb::extra) without any `.0` unwrap.
//! # Conversion contract
//! All conversions return `Result<_, UnknownFieldError>` — no implicit
//! coercion, no `Default` fallback. The README mandates this to keep unknown
//! field access auditable: every interaction is a deliberate `?` at the call
//! site.

/// A single unknown-field payload from a JSONB column — a raw
/// [`serde_json::Value`] preserved verbatim from the database row.
/// Values in the [`super::Jsonb::extra`] map are never type-checked against
/// the `T` schema and are never modified by `save` (they are round-tripped
/// intact). Accessing their content is always fallible — use the methods on
/// [`UnknownFieldExt`].
pub type UnknownField = serde_json::Value;

/// Errors produced when converting an [`UnknownField`] to a typed Rust value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UnknownFieldError {
    /// The JSON value is present but has the wrong JSON type.
    /// For example, calling [`UnknownFieldExt::try_as_i64`] on a JSON string
    /// produces `TypeMismatch { expected: "i64", actual: "string" }`.
    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        /// The Rust type the caller requested.
        expected: &'static str,
        /// The JSON type discriminant of the actual value (e.g. `"string"`,
        /// `"number"`, `"boolean"`, `"array"`, `"object"`, `"null"`).
        actual: &'static str,
    },
    /// Deserialization via [`serde_json`] failed.
    /// Returned only by [`UnknownFieldExt::try_into_typed`] when the JSON
    /// structure does not match the target type's `Deserialize` impl.
    #[error("deserialize: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// Typed accessor methods on [`UnknownField`] values.
/// All methods are infallible at the Rust type level but return `Result` to
/// force the caller to handle mismatches explicitly. No coercion is performed:
/// a JSON number is not coerced to a string; a JSON `null` is not accepted as
/// a valid `i64`.
pub trait UnknownFieldExt {
    /// Borrow the inner string, or `Err(TypeMismatch)` if the value is not a
    /// JSON string.
    fn try_as_str(&self) -> Result<&str, UnknownFieldError>;

    /// Extract the value as `i64`, or `Err(TypeMismatch)` if the value is not
    /// a JSON number representable as `i64`.
    fn try_as_i64(&self) -> Result<i64, UnknownFieldError>;

    /// Extract the value as `f64`, or `Err(TypeMismatch)` if the value is not
    /// a JSON number.
    fn try_as_f64(&self) -> Result<f64, UnknownFieldError>;

    /// Extract the value as `bool`, or `Err(TypeMismatch)` if the value is not
    /// a JSON boolean.
    fn try_as_bool(&self) -> Result<bool, UnknownFieldError>;

    /// Borrow the inner array slice, or `Err(TypeMismatch)` if the value is
    /// not a JSON array.
    fn try_as_array(&self) -> Result<&[serde_json::Value], UnknownFieldError>;

    /// Borrow the inner object map, or `Err(TypeMismatch)` if the value is not
    /// a JSON object.
    fn try_as_object(
        &self,
    ) -> Result<&serde_json::Map<String, serde_json::Value>, UnknownFieldError>;

    /// Deserialize the value into any `T: serde::de::DeserializeOwned`.
    /// Uses [`serde_json::from_value`] internally. Returns
    /// `Err(UnknownFieldError::Deserialize(_))` if the JSON structure does not
    /// match `T`'s `Deserialize` impl.
    fn try_into_typed<T: serde::de::DeserializeOwned>(&self) -> Result<T, UnknownFieldError>;
}

/// Returns the JSON type name as a `&'static str` for [`UnknownFieldError::TypeMismatch`].
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

impl UnknownFieldExt for UnknownField {
    fn try_as_str(&self) -> Result<&str, UnknownFieldError> {
        self.as_str()
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "str",
                actual: json_type_name(self),
            })
    }

    fn try_as_i64(&self) -> Result<i64, UnknownFieldError> {
        self.as_i64()
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "i64",
                actual: json_type_name(self),
            })
    }

    fn try_as_f64(&self) -> Result<f64, UnknownFieldError> {
        self.as_f64()
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "f64",
                actual: json_type_name(self),
            })
    }

    fn try_as_bool(&self) -> Result<bool, UnknownFieldError> {
        self.as_bool()
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "bool",
                actual: json_type_name(self),
            })
    }

    fn try_as_array(&self) -> Result<&[serde_json::Value], UnknownFieldError> {
        self.as_array()
            .map(|v| v.as_slice())
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "array",
                actual: json_type_name(self),
            })
    }

    fn try_as_object(
        &self,
    ) -> Result<&serde_json::Map<String, serde_json::Value>, UnknownFieldError> {
        self.as_object()
            .ok_or_else(|| UnknownFieldError::TypeMismatch {
                expected: "object",
                actual: json_type_name(self),
            })
    }

    fn try_into_typed<T: serde::de::DeserializeOwned>(&self) -> Result<T, UnknownFieldError> {
        Ok(serde_json::from_value(self.clone())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn try_as_str_on_string_succeeds() {
        let v: UnknownField = json!("hello");
        assert_eq!(v.try_as_str().unwrap(), "hello");
    }

    #[test]
    fn try_as_str_on_number_fails_with_type_mismatch() {
        let v: UnknownField = json!(42);
        let err = v.try_as_str().unwrap_err();
        assert!(matches!(
            err,
            UnknownFieldError::TypeMismatch {
                expected: "str",
                actual: "number"
            }
        ));
    }

    #[test]
    fn try_as_i64_on_number_succeeds() {
        let v: UnknownField = json!(42);
        assert_eq!(v.try_as_i64().unwrap(), 42);
    }

    #[test]
    fn try_as_i64_on_bool_fails() {
        let v: UnknownField = json!(true);
        let err = v.try_as_i64().unwrap_err();
        assert!(matches!(
            err,
            UnknownFieldError::TypeMismatch {
                expected: "i64",
                actual: "boolean"
            }
        ));
    }

    #[test]
    fn try_as_f64_on_number_succeeds() {
        let v: UnknownField = json!(1.5);
        let result = v.try_as_f64().unwrap();
        assert!((result - 1.5).abs() < 1e-10);
    }

    #[test]
    fn try_as_bool_on_true_succeeds() {
        let v: UnknownField = json!(true);
        assert!(v.try_as_bool().unwrap());
    }

    #[test]
    fn try_as_bool_on_null_fails() {
        let v: UnknownField = json!(null);
        let err = v.try_as_bool().unwrap_err();
        assert!(matches!(
            err,
            UnknownFieldError::TypeMismatch {
                expected: "bool",
                actual: "null"
            }
        ));
    }

    #[test]
    fn try_as_array_on_array_succeeds() {
        let v: UnknownField = json!([1, 2, 3]);
        let arr = v.try_as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn try_as_object_on_object_succeeds() {
        let v: UnknownField = json!({"key": "val"});
        let obj = v.try_as_object().unwrap();
        assert_eq!(obj.get("key").unwrap().as_str().unwrap(), "val");
    }

    #[test]
    fn try_into_typed_deserializes_struct() {
        #[derive(serde::Deserialize, PartialEq, Debug)]
        struct Foo {
            #[allow(dead_code)]
            x: i32,
        }
        let v: UnknownField = json!({"x": 5});
        let foo: Foo = v.try_into_typed().unwrap();
        assert_eq!(foo, Foo { x: 5 });
    }

    #[test]
    fn try_into_typed_fails_on_mismatch() {
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct Foo {
            x: i32,
        }
        let v: UnknownField = json!({"x": "not_a_number"});
        assert!(v.try_into_typed::<Foo>().is_err());
    }
}
