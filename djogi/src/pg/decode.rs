//! Public row-decode trait for the tokio-postgres runtime.
//!
//! # What
//!
//! [`FromPgRow`] is the canonical row-decode trait emitted by
//! `#[model]` (see `djogi-macros/src/model/from_row.rs`). Every model
//! gets:
//!
//! - [`FromPgRow::COLUMNS`] — a `&'static [&'static str]` listing the
//!   column names the macro baked in, in canonical SELECT order
//!   (framework fields first: `id`, `created_at`, `updated_at`, then
//!   user fields in declaration order).
//! - [`FromPgRow::COLUMN_LIST`] — the same list joined with `", "`,
//!   ready to interpolate into `SELECT {COLUMN_LIST} FROM t` and
//!   `RETURNING {COLUMN_LIST}` SQL text.
//! - [`FromPgRow::from_pg_row`] — positional decode via
//!   `row.try_get(0)`, `row.try_get(1)`, … matching the `COLUMNS`
//!   order.
//!
//! # Why ordinal, not name-based
//!
//! Ordinal decode skips the per-call name-to-index hash table
//! `tokio_postgres::Row::try_get::<_, &str>(col)` walks on every
//! field. For a row with N columns, ordinal decode is one index read
//! per column; name-based is N string comparisons per column (quadratic
//! in N). The CRUD / QuerySet terminals emit `SELECT {COLUMN_LIST}`
//! (baked at macro time) so the wire column order is always the
//! struct-field order, and positional decode is sound.
//!
//! # Drift safeguard — debug-build name guard
//!
//! The macro emits `debug_assert_eq!(row.columns()[i].name(),
//! Self::COLUMNS[i])` per column. Column-order drift (caller sends a
//! SELECT that doesn't match `COLUMN_LIST`; a future refactor
//! reshapes the builder; a test fixture hand-rolls the wrong SELECT)
//! panics loudly under `cargo test`. Release builds drop the assert —
//! ordinal decode stays a single `try_get(i)` call with no per-row
//! overhead.
//!
//! Joined-row decode uses a different trait ([`FromJoinedPgRow`], T4)
//! because `select_related` adds aliased child columns whose
//! ordinal positions depend on the runtime prefetch graph, not the
//! canonical struct shape.
//!
//! [`FromJoinedPgRow`]: crate::pg::decode::FromJoinedPgRow

use crate::DjogiError;
use tokio_postgres::Row;
use tokio_postgres::types::FromSql;

/// Canonical row-decode trait for `#[model]`-annotated structs.
///
/// Do not implement this manually — `#[model]` emits the impl. Users
/// can still bound generic code on `T: FromPgRow` (e.g. to accept any
/// model in a helper function), which is the intended public shape.
///
/// # Contract
///
/// Implementors must guarantee that:
/// 1. [`COLUMNS`](Self::COLUMNS) lists fields in the exact order
///    [`from_pg_row`](Self::from_pg_row) reads them from the row
///    (ordinal position matches slice index).
/// 2. [`COLUMN_LIST`](Self::COLUMN_LIST) equals
///    [`COLUMNS`](Self::COLUMNS)`.join(", ")` — callers interpolate
///    it into SQL text expecting exactly that shape.
/// 3. [`from_pg_row`](Self::from_pg_row) returns
///    [`Err(DjogiError::Decode)`](crate::DjogiError::Decode) on any
///    column-level type-conversion failure, not panic. (The
///    debug_assert on column-name drift is a separate invariant
///    violation and is allowed to panic.)
pub trait FromPgRow: Sized {
    /// Column names in the canonical SELECT order (framework fields
    /// first, then user fields).
    ///
    /// `COLUMNS[i]` is the name of the column decoded by
    /// `from_pg_row` at ordinal position `i`. The macro uses this
    /// slice both to emit the per-column `debug_assert_eq!` name
    /// guard and to build [`COLUMN_LIST`](Self::COLUMN_LIST) at
    /// compile time.
    const COLUMNS: &'static [&'static str];

    /// Canonical column list for SQL emission — the same names as
    /// [`COLUMNS`](Self::COLUMNS), joined with `", "`. Baked at macro
    /// time so callers never need to allocate.
    ///
    /// Interpolate directly into SQL text:
    ///
    /// ```ignore
    /// let sql = format!("SELECT {} FROM {} WHERE id = $1",
    ///                   <User as FromPgRow>::COLUMN_LIST,
    ///                   User::table_name());
    /// ```
    const COLUMN_LIST: &'static str;

    /// Decode `Self` from a `tokio_postgres::Row` positionally.
    ///
    /// Column ordinals are fixed at macro time and match
    /// [`COLUMNS`](Self::COLUMNS) index-for-index. Callers must
    /// supply a row produced by a SELECT whose projection matches
    /// [`COLUMN_LIST`](Self::COLUMN_LIST) — the CRUD and QuerySet
    /// terminals shipped by Djogi guarantee this; hand-rolled SELECTs
    /// must either interpolate `COLUMN_LIST` or supply columns in the
    /// same order.
    ///
    /// Returns [`DjogiError::Decode`](crate::DjogiError::Decode) with
    /// the offending column name on wire-type mismatch.
    ///
    /// In debug builds, a `debug_assert_eq!` on each
    /// `row.columns()[i].name()` panics if the wire shape drifts
    /// from [`COLUMNS`](Self::COLUMNS). Release builds skip the
    /// guard.
    fn from_pg_row(row: &tokio_postgres::Row) -> Result<Self, crate::DjogiError>;
}

/// Prefix-aware joined-row decoder for `#[model]` structs.
///
/// # What
///
/// [`FromJoinedPgRow`] is the sibling of [`FromPgRow`] for row shapes
/// where a model's columns are available under a caller-supplied
/// prefix. `select_related` uses this for child aliases such as
/// `"rel_owner_id.name"`, but the trait works for any projection that
/// follows the same `"{prefix}{field_name}"` convention.
///
/// # How
///
/// The `#[model]` macro emits one impl per model. Passing `""`
/// decodes the model from bare column names; passing a non-empty prefix
/// decodes the same model from aliased columns in a joined row.
///
/// `#[doc(hidden)]` — adopters do not implement this trait by hand; the
/// macro emits the impl. The trait stays `pub` because cross-crate
/// macro emission needs `::djogi::pg::decode::FromJoinedPgRow` to
/// resolve.
#[doc(hidden)]
pub trait FromJoinedPgRow: Sized {
    /// Decode `Self` from `row`, reading each field from the
    /// `"{prefix}{field_name}"` column.
    fn from_joined_pg_row(row: &Row, prefix: &str) -> Result<Self, DjogiError>;
}

/// Decode a column at a positional index, with a debug-build name
/// guard and a column-name-tagged error.
///
/// Both the canonical `FromPgRow::from_pg_row` body emitted by
/// `#[model]` and the visage `FromPgRow` body emitted per
/// `#[model(visages = "...")]` route here. Centralises the
/// column-order drift assertion (active only in debug builds) and the
/// `tokio_postgres::Error → DjogiError::Decode` mapping that would
/// otherwise duplicate at every macro-emitted column site.
///
/// `name` is the canonical SELECT-order column name baked in at macro
/// time. In debug builds, a mismatch between `name` and
/// `row.columns()[idx].name()` panics with the offending positions —
/// surfacing column-order drift loudly in `cargo test`. Release builds
/// drop the assert; ordinal decode stays a single `try_get(idx)` call.
///
/// `#[doc(hidden)]` — emitted by `#[model]` and `#[derive(Visage)]`,
/// not user-facing.
#[doc(hidden)]
pub fn decode_at<'a, T>(row: &'a Row, idx: usize, name: &'static str) -> Result<T, DjogiError>
where
    T: FromSql<'a>,
{
    debug_assert_eq!(
        row.columns()[idx].name(),
        name,
        "FromPgRow column-order drift: position {} expected {:?}, got {:?}",
        idx,
        name,
        row.columns()[idx].name(),
    );
    row.try_get::<_, T>(idx)
        .map_err(|e| DjogiError::Decode(format!("column `{}`: {}", name, e)))
}

/// Decode one scalar value from a row by ordinal position.
///
/// Centralises the `tokio_postgres::Error -> DjogiError` conversion for
/// scalar terminals and raw-row helpers.
///
/// `#[doc(hidden)]` — emitted by `djogi::primary_key!` for newtype-PK
/// decode; not user-facing.
#[doc(hidden)]
pub fn try_get_scalar<'a, T>(row: &'a Row, idx: usize) -> Result<T, DjogiError>
where
    T: FromSql<'a>,
{
    row.try_get(idx).map_err(DjogiError::from)
}

/// Decode a column stored as a wider type `W` and narrow it to target type `N`.
///
/// Used by `#[model]` for fields whose Rust source type is narrower than the
/// SQL carrier: `i8 / u8 → SMALLINT (i16)`, `u16 → INTEGER (i32)`,
/// `u32 → BIGINT (i64)`.  The debug-build column-name guard from
/// [`decode_at`] runs on the wide read; the subsequent narrowing applies
/// `N::try_from(wide_value)`.
///
/// Returns `DjogiError::Decode` if the stored value is out of the narrow
/// type's representable range. This is the second line of defence behind
/// the type-derived CHECK that `#[model]` emits via the projection layer —
/// it fires when a row lands without the CHECK in place (e.g. a schema
/// snapshot reapplied before the migration ran, or a direct `COPY … FROM`
/// that bypasses check constraints).
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_narrowed<'a, W, N>(
    row: &'a Row,
    idx: usize,
    name: &'static str,
) -> Result<N, DjogiError>
where
    W: FromSql<'a> + std::fmt::Display + Copy,
    N: TryFrom<W>,
    <N as TryFrom<W>>::Error: std::fmt::Display,
{
    let wide: W = decode_at(row, idx, name)?;
    N::try_from(wide).map_err(|e| {
        DjogiError::Decode(format!("column `{name}`: value {wide} out of range: {e}",))
    })
}

/// Decode a nullable column stored as a wider type `W` and narrow to `N`.
///
/// The `Option`-flavoured companion to [`decode_narrowed`] for nullable
/// fields. Decodes `Option<W>` from the wire and maps the inner value
/// through `N::try_from`, propagating `DjogiError::Decode` on
/// out-of-range values.
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_narrowed_opt<'a, W, N>(
    row: &'a Row,
    idx: usize,
    name: &'static str,
) -> Result<Option<N>, DjogiError>
where
    W: FromSql<'a> + std::fmt::Display + Copy,
    N: TryFrom<W>,
    <N as TryFrom<W>>::Error: std::fmt::Display,
{
    let wide: Option<W> = decode_at(row, idx, name)?;
    wide.map(|w| {
        N::try_from(w).map_err(|e| {
            DjogiError::Decode(format!("column `{name}`: value {w} out of range: {e}",))
        })
    })
    .transpose()
}

/// Decode a `u64` field stored as `NUMERIC(20, 0)`.
///
/// `NUMERIC(20, 0)` is the SQL carrier for `u64` columns: it is the
/// smallest Postgres numeric type whose positive range covers all
/// `u64::MAX` (18_446_744_073_709_551_615). The decode path reads a
/// `rust_decimal::Decimal` and converts via `Decimal::to_u64()`.
///
/// Returns `DjogiError::Decode` if the stored value does not fit in
/// `u64` (negative values or values exceeding `u64::MAX` stored by an
/// external writer that bypassed the type-derived CHECK).
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_u64_from_decimal(
    row: &Row,
    idx: usize,
    name: &'static str,
) -> Result<u64, DjogiError> {
    use rust_decimal::prelude::ToPrimitive as _;
    let dec: rust_decimal::Decimal = decode_at(row, idx, name)?;
    dec.to_u64().ok_or_else(|| {
        DjogiError::Decode(format!(
            "column `{name}`: Decimal value {dec} out of u64 range",
        ))
    })
}

/// Decode a nullable `u64` field stored as `NUMERIC(20, 0)`.
///
/// The `Option`-flavoured companion to [`decode_u64_from_decimal`].
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_opt_u64_from_decimal(
    row: &Row,
    idx: usize,
    name: &'static str,
) -> Result<Option<u64>, DjogiError> {
    use rust_decimal::prelude::ToPrimitive as _;
    let dec: Option<rust_decimal::Decimal> = decode_at(row, idx, name)?;
    dec.map(|d| {
        d.to_u64().ok_or_else(|| {
            DjogiError::Decode(format!(
                "column `{name}`: Decimal value {d} out of u64 range",
            ))
        })
    })
    .transpose()
}

/// Name-based variant of [`decode_narrowed`] for the `FromJoinedPgRow` path.
///
/// `FromJoinedPgRow` decodes columns by prefixed name rather than ordinal
/// index. This helper decodes `W` by the given column name and narrows to `N`,
/// matching the semantics of [`decode_narrowed`] but accepting a dynamically
/// computed `&str` name instead of a `&'static str` ordinal-path name.
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_narrowed_by_name<'a, W, N>(row: &'a Row, col_name: &str) -> Result<N, DjogiError>
where
    W: FromSql<'a> + std::fmt::Display + Copy,
    N: TryFrom<W>,
    <N as TryFrom<W>>::Error: std::fmt::Display,
{
    let wide: W = row
        .try_get::<_, W>(col_name)
        .map_err(|e| DjogiError::Decode(format!("column `{col_name}`: {e}")))?;
    N::try_from(wide).map_err(|e| {
        DjogiError::Decode(format!(
            "column `{col_name}`: value {wide} out of range: {e}",
        ))
    })
}

/// Nullable name-based variant of [`decode_narrowed_by_name`].
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_narrowed_opt_by_name<'a, W, N>(
    row: &'a Row,
    col_name: &str,
) -> Result<Option<N>, DjogiError>
where
    W: FromSql<'a> + std::fmt::Display + Copy,
    N: TryFrom<W>,
    <N as TryFrom<W>>::Error: std::fmt::Display,
{
    let wide: Option<W> = row
        .try_get::<_, Option<W>>(col_name)
        .map_err(|e| DjogiError::Decode(format!("column `{col_name}`: {e}")))?;
    wide.map(|w| {
        N::try_from(w).map_err(|e| {
            DjogiError::Decode(format!("column `{col_name}`: value {w} out of range: {e}",))
        })
    })
    .transpose()
}

/// Name-based variant of [`decode_u64_from_decimal`].
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_u64_from_decimal_by_name(row: &Row, col_name: &str) -> Result<u64, DjogiError> {
    use rust_decimal::prelude::ToPrimitive as _;
    let dec: rust_decimal::Decimal = row
        .try_get::<_, rust_decimal::Decimal>(col_name)
        .map_err(|e| DjogiError::Decode(format!("column `{col_name}`: {e}")))?;
    dec.to_u64().ok_or_else(|| {
        DjogiError::Decode(format!(
            "column `{col_name}`: Decimal value {dec} out of u64 range",
        ))
    })
}

/// Nullable name-based variant of [`decode_u64_from_decimal`].
///
/// `#[doc(hidden)]` — emitted by `#[model]`; not user-facing.
#[doc(hidden)]
pub fn decode_opt_u64_from_decimal_by_name(
    row: &Row,
    col_name: &str,
) -> Result<Option<u64>, DjogiError> {
    use rust_decimal::prelude::ToPrimitive as _;
    let dec: Option<rust_decimal::Decimal> = row
        .try_get::<_, Option<rust_decimal::Decimal>>(col_name)
        .map_err(|e| DjogiError::Decode(format!("column `{col_name}`: {e}")))?;
    dec.map(|d| {
        d.to_u64().ok_or_else(|| {
            DjogiError::Decode(format!(
                "column `{col_name}`: Decimal value {d} out of u64 range",
            ))
        })
    })
    .transpose()
}
