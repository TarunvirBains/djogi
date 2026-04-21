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
//! Joined-row decode uses a different trait ([`FromJoinedRow`], T4)
//! because `select_related` adds aliased child columns whose
//! ordinal positions depend on the runtime prefetch graph, not the
//! canonical struct shape.
//!
//! [`FromJoinedRow`]: crate::relation::joined_row::FromJoinedRow

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
