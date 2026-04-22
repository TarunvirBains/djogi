//! Runtime glue for `#[derive(DjogiEnum)]`.
//!
//! Most of the codec logic (ToSql / FromSql impls) is generated per-enum by the proc macro.
//! This module holds shared error types and re-exports that complete the runtime surface.
//!
//! # Design
//!
//! A Postgres enum column round-trips as a string on the wire. `#[derive(DjogiEnum)]`
//! generates:
//!
//! 1. `ToSql` — encodes `self` as the mapped string label.
//! 2. `FromSql` — decodes a wire string, matches against known variants, returns
//!    `Err(EnumDecodeError::UnknownVariant { ... })` for unrecognised labels.
//! 3. `inventory::submit!(EnumDescriptor { ... })` — registers the enum's metadata so
//!    the Phase 7 migration differ can emit `CREATE TYPE ... AS ENUM (...)`.
//! 4. A `variants()` convenience fn returning the mapped string slice.

/// Decode failed: the Postgres wire string did not match any known variant.
///
/// Returned (boxed) from `FromSql::from_sql` when the wire bytes decode to a string that
/// is not in the enum's variant map. The `postgres_type` field names the Postgres enum
/// type (e.g. `"vehicle_status"`) so error messages identify the column clearly.
#[derive(Debug)]
pub struct EnumDecodeError {
    /// Postgres type name — matches `EnumDescriptor::postgres_type`.
    pub postgres_type: &'static str,
    /// The wire string that did not match any variant.
    pub received: String,
    /// The variants the decoder expected — for human-readable error messages.
    pub expected: &'static [&'static str],
}

impl std::fmt::Display for EnumDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown variant `{}` for Postgres enum `{}`; expected one of: {}",
            self.received,
            self.postgres_type,
            self.expected.join(", ")
        )
    }
}

impl std::error::Error for EnumDecodeError {}

#[cfg(test)]
mod tests {
    use super::EnumDecodeError;

    #[test]
    fn decode_error_display() {
        let err = EnumDecodeError {
            postgres_type: "vehicle_status",
            received: "unknown_val".to_owned(),
            expected: &["active", "in_maintenance", "decommissioned"],
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unknown_val"),
            "error message must include the received value"
        );
        assert!(
            msg.contains("vehicle_status"),
            "error message must include the postgres type name"
        );
        assert!(
            msg.contains("active"),
            "error message must include expected variants"
        );
    }
}
