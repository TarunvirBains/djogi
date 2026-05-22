//! Typed `VALUES` inline-relation join source — djogi issue #103.
//!
//! # What
//!
//! [`InlineValues<Row>`] is a typed, validated builder for a Postgres
//! `VALUES (..), (..)` clause used as a JOIN source.  Adopters pre-compute a
//! small lookup dataset in Rust, wrap it in `InlineValues`, and join it
//! against a model queryset with an explicit, type-checked `ON` predicate:
//!
//! ```ignore
//! use djogi::prelude::*;
//!
//! let weights = InlineValues::new(
//!     vec![(1_i64, 0.91_f64), (2_i64, 0.72_f64)],
//!     "weights",
//!     ("animal_id", "score"),
//! )?;
//!
//! let pairs: Vec<(Animal, (i64, f64))> = Animal::objects()
//!     .join_values(weights, |animal, values| {
//!         animal.id().eq_values(values.col0())
//!     })
//!     .fetch_all(&mut ctx)
//!     .await?;
//! ```
//!
//! # SQL shape
//!
//! For a two-column row `(A, B)` and user alias `weights`:
//!
//! ```sql
//! SELECT
//!     __djogi_m.id   AS id,
//!     __djogi_m.name AS name,
//!     ...,
//!     weights.animal_id AS __djogi_values_0,
//!     weights.score     AS __djogi_values_1
//! FROM animals AS __djogi_m
//! INNER JOIN (VALUES
//!     ($1::BIGINT, $2::DOUBLE PRECISION),
//!     ($3, $4)
//! ) AS weights(animal_id, score)
//!   ON __djogi_m.id = weights.animal_id
//! [WHERE __djogi_m.status = $5]
//! [ORDER BY __djogi_m.created_at DESC]
//! [LIMIT $6] [OFFSET $7]
//! ```
//!
//! # Safety model
//!
//! - All row data flows through [`SqlAccumulator::push_bind`] — never
//!   string-interpolated.
//! - Alias and column names are validated with
//!   [`crate::ident::check_user_supplied_ident`] at [`InlineValues::new`] time,
//!   rejecting the `__djogi_` prefix and Postgres reserved keywords.
//! - SQL type casts (e.g. `::BIGINT`) come from sealed framework constants on
//!   [`ValuesScalar`] — not from user input.
//! - No implicit `ON TRUE`.  The join predicate is always a structured typed
//!   predicate, never a raw SQL string.
//!
//! # Empty behaviour
//!
//! Empty `InlineValues` is valid.  Terminal methods short-circuit after
//! validation:
//!
//! | Method      | Inner join result            | Left join result                               |
//! |:------------|:-----------------------------|:-----------------------------------------------|
//! | `fetch_all` | `Ok(vec![])`                 | executes query — all left rows with `None`     |
//! | `first`     | `Ok(None)`                   | executes query — first left row with `None`    |
//! | `fetch_one` | `Err(NotFound)`              | executes query — first left row or `NotFound`  |
//! | `count`     | `Ok(0)`                      | executes query — count of left rows            |
//! | `exists`    | `Ok(false)`                  | executes query — any left rows?                |
//!
//! # Supported row types
//!
//! Tuple rows of arity 1–6.  Each column type must implement [`ValuesScalar`].
//! Supported scalars: `String`, `i8`, `u8`, `i16`, `u16`, `i32`, `u32`,
//! `i64`, `u64`, `f32`, `f64`, `bool`, `Decimal`, `Uuid`, `HeerId`,
//! `HeerIdDesc`, `RanjId`, `RanjIdDesc`, `DateTime` (`OffsetDateTime`),
//! `PrimitiveDateTime`, `Date`, `Time`, `Interval`, `Vec<u8>`, and
//! `Option<T>` for each of the above.
//!
//! # Non-goals (v0.1)
//!
//! - No implicit cartesian join.  Use an explicit `cross_join_values` API later.
//! - No struct rows — only tuples.
//! - Very large value lists should be loaded through a temp/staging table; Postgres
//!   plans large `VALUES` clauses expensively.  Keep per-query VALUES under ~1 000
//!   rows; chunk larger inputs or use `COPY` + temp table.
#![allow(clippy::manual_async_fn)]

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::ident::check_user_supplied_ident;
use crate::model::Model;
use crate::pg::accumulator::{SqlAccumulator, as_params};
use crate::pg::decode::{
    FromPgRow, decode_at, decode_narrowed, decode_narrowed_opt, decode_opt_u64_from_decimal,
    decode_u64_from_decimal,
};
use crate::query::portable::PortablePredicateError;
use crate::query::portable::SqlEmitContext;
use crate::query::queryset::{DistinctMode, QuerySet};
use crate::query::sql::{emit_q, q_is_vacuously_true};
use std::future::Future;
use std::marker::PhantomData;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Postgres hard-limits prepared statements to 65 535 bind parameters.
const PG_MAX_PARAMS: usize = 65_535;

/// Framework-owned alias for the model (left) side of a VALUES join.
const MODEL_ALIAS: &str = "__djogi_m";

/// Framework-owned presence sentinel column appended to every VALUES row in
/// LEFT JOIN mode.  Detects "no match" vs "matched row with nullable columns".
const SENTINEL_COL: &str = "__djogi_present";

/// The fixed aliases used in the SELECT list for each projected values column.
const VALUES_ALIASES: [&str; 6] = [
    "__djogi_values_0",
    "__djogi_values_1",
    "__djogi_values_2",
    "__djogi_values_3",
    "__djogi_values_4",
    "__djogi_values_5",
];

// ── Sealed module ─────────────────────────────────────────────────────────────

mod sealed {
    pub trait SealedValuesScalar {}
    pub trait SealedValuesRow {}
    pub trait SealedColumns {}
}

// ── ValuesScalar ──────────────────────────────────────────────────────────────

/// A scalar type that can appear in a typed `VALUES` inline relation column.
///
/// This is a sealed trait — all implementations are inside this module.
/// Adopters use the standard Rust types listed in the module documentation;
/// they do not implement this trait directly.
///
/// # Contract
///
/// - `SQL_CAST` is the Postgres type name used to cast the *first-row*
///   placeholder: `$1::BIGINT`, `$1::TEXT`, etc.
/// - `push_bind_owned` pushes exactly one positional bind slot, performing any
///   widening conversion required (e.g. `u32 → i64`).
/// - `push_null` pushes a typed `NULL` bind for the same wire type.
/// - `decode_values_col` decodes the scalar from a positional row column.
pub trait ValuesScalar: sealed::SealedValuesScalar + Clone + Send + Sync + 'static {
    /// Postgres type cast string (e.g. `"BIGINT"`, `"TEXT"`).
    const SQL_CAST: &'static str;
    /// Push this value as one bind slot (widening if required).
    fn push_bind_owned(self, acc: &mut SqlAccumulator);
    /// Push a typed `NULL` for this scalar's wire type.
    fn push_null(acc: &mut SqlAccumulator);
    /// Decode from a positional column in a Postgres row.
    ///
    /// `alias` is the framework-generated SELECT-list alias
    /// (e.g. `"__djogi_values_0"`), used for error messages and the
    /// debug-build column-name assertion in [`decode_at`].
    fn decode_values_col(
        row: &tokio_postgres::Row,
        idx: usize,
        alias: &'static str,
    ) -> Result<Self, DjogiError>;
}

// ── Scalar impls: direct (no widening) ───────────────────────────────────────

macro_rules! impl_scalar_direct {
    ($T:ty, $CAST:literal) => {
        impl sealed::SealedValuesScalar for $T {}
        impl ValuesScalar for $T {
            const SQL_CAST: &'static str = $CAST;
            fn push_bind_owned(self, acc: &mut SqlAccumulator) {
                acc.push_bind(self);
            }
            fn push_null(acc: &mut SqlAccumulator) {
                acc.push_bind(None::<$T>);
            }
            fn decode_values_col(
                row: &tokio_postgres::Row,
                idx: usize,
                alias: &'static str,
            ) -> Result<Self, DjogiError> {
                decode_at::<$T>(row, idx, alias)
            }
        }

        impl sealed::SealedValuesScalar for Option<$T> {}
        impl ValuesScalar for Option<$T> {
            const SQL_CAST: &'static str = $CAST;
            fn push_bind_owned(self, acc: &mut SqlAccumulator) {
                // Option<T>: ToSql when T: ToSql (postgres-types blanket impl).
                acc.push_bind(self);
            }
            fn push_null(acc: &mut SqlAccumulator) {
                acc.push_bind(None::<$T>);
            }
            fn decode_values_col(
                row: &tokio_postgres::Row,
                idx: usize,
                alias: &'static str,
            ) -> Result<Self, DjogiError> {
                decode_at::<Option<$T>>(row, idx, alias)
            }
        }
    };
}

impl_scalar_direct!(String, "TEXT");
impl_scalar_direct!(i16, "SMALLINT");
impl_scalar_direct!(i32, "INTEGER");
impl_scalar_direct!(i64, "BIGINT");
impl_scalar_direct!(f32, "REAL");
impl_scalar_direct!(f64, "DOUBLE PRECISION");
impl_scalar_direct!(bool, "BOOLEAN");
impl_scalar_direct!(rust_decimal::Decimal, "NUMERIC");
impl_scalar_direct!(uuid::Uuid, "UUID");
impl_scalar_direct!(time::OffsetDateTime, "TIMESTAMPTZ");
impl_scalar_direct!(time::PrimitiveDateTime, "TIMESTAMP");
impl_scalar_direct!(time::Date, "DATE");
impl_scalar_direct!(time::Time, "TIME");
impl_scalar_direct!(Vec<u8>, "BYTEA");
impl_scalar_direct!(crate::HeerId, "BIGINT");
impl_scalar_direct!(crate::HeerIdDesc, "BIGINT");
impl_scalar_direct!(crate::RanjId, "UUID");
impl_scalar_direct!(crate::RanjIdDesc, "UUID");
impl_scalar_direct!(crate::Interval, "INTERVAL");

// ── Scalar impls: widened ─────────────────────────────────────────────────────
//
// Narrow integer types that `tokio-postgres` does not support directly must
// be widened to a compatible wire type before binding, and narrowed back on
// decode.  This matches the macro-emitted CRUD path in `sql_bind.rs`.

macro_rules! impl_scalar_widened {
    ($N:ty, $W:ty, $CAST:literal, $widen:expr) => {
        impl sealed::SealedValuesScalar for $N {}
        impl ValuesScalar for $N {
            const SQL_CAST: &'static str = $CAST;
            fn push_bind_owned(self, acc: &mut SqlAccumulator) {
                let wide: $W = $widen(self);
                acc.push_bind(wide);
            }
            fn push_null(acc: &mut SqlAccumulator) {
                acc.push_bind(None::<$W>);
            }
            fn decode_values_col(
                row: &tokio_postgres::Row,
                idx: usize,
                alias: &'static str,
            ) -> Result<Self, DjogiError> {
                decode_narrowed::<$W, $N>(row, idx, alias)
            }
        }

        impl sealed::SealedValuesScalar for Option<$N> {}
        impl ValuesScalar for Option<$N> {
            const SQL_CAST: &'static str = $CAST;
            fn push_bind_owned(self, acc: &mut SqlAccumulator) {
                acc.push_bind(self.map(|v| {
                    let w: $W = $widen(v);
                    w
                }));
            }
            fn push_null(acc: &mut SqlAccumulator) {
                acc.push_bind(None::<$W>);
            }
            fn decode_values_col(
                row: &tokio_postgres::Row,
                idx: usize,
                alias: &'static str,
            ) -> Result<Self, DjogiError> {
                decode_narrowed_opt::<$W, $N>(row, idx, alias)
            }
        }
    };
}

impl_scalar_widened!(i8, i16, "SMALLINT", i16::from);
impl_scalar_widened!(u8, i16, "SMALLINT", i16::from);
impl_scalar_widened!(u16, i32, "INTEGER", i32::from);
impl_scalar_widened!(u32, i64, "BIGINT", i64::from);

// u64 → NUMERIC (rust_decimal::Decimal) — special treatment
impl sealed::SealedValuesScalar for u64 {}
impl ValuesScalar for u64 {
    const SQL_CAST: &'static str = "NUMERIC";
    fn push_bind_owned(self, acc: &mut SqlAccumulator) {
        acc.push_bind(rust_decimal::Decimal::from(self));
    }
    fn push_null(acc: &mut SqlAccumulator) {
        acc.push_bind(None::<rust_decimal::Decimal>);
    }
    fn decode_values_col(
        row: &tokio_postgres::Row,
        idx: usize,
        alias: &'static str,
    ) -> Result<Self, DjogiError> {
        decode_u64_from_decimal(row, idx, alias)
    }
}

impl sealed::SealedValuesScalar for Option<u64> {}
impl ValuesScalar for Option<u64> {
    const SQL_CAST: &'static str = "NUMERIC";
    fn push_bind_owned(self, acc: &mut SqlAccumulator) {
        acc.push_bind(self.map(rust_decimal::Decimal::from));
    }
    fn push_null(acc: &mut SqlAccumulator) {
        acc.push_bind(None::<rust_decimal::Decimal>);
    }
    fn decode_values_col(
        row: &tokio_postgres::Row,
        idx: usize,
        alias: &'static str,
    ) -> Result<Self, DjogiError> {
        decode_opt_u64_from_decimal(row, idx, alias)
    }
}

// ── IntoValuesColumns — arity-checked column-name carrier ────────────────────

/// Sealed trait for the column-name tuple supplied to [`InlineValues::new`].
///
/// Implemented for `(&'static str,)` through the 6-tuple.  The arity of the
/// supplied tuple must match the row type; mismatches are caught at compile
/// time because [`InlineValues::new`] constrains `C = Row::Columns`.
pub trait IntoValuesColumns: sealed::SealedColumns {
    fn into_col_vec(self) -> Vec<&'static str>;
}

// arity-1
impl sealed::SealedColumns for (&'static str,) {}
impl IntoValuesColumns for (&'static str,) {
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0]
    }
}
// arity-2
impl sealed::SealedColumns for (&'static str, &'static str) {}
impl IntoValuesColumns for (&'static str, &'static str) {
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0, self.1]
    }
}
// arity-3
impl sealed::SealedColumns for (&'static str, &'static str, &'static str) {}
impl IntoValuesColumns for (&'static str, &'static str, &'static str) {
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0, self.1, self.2]
    }
}
// arity-4
impl sealed::SealedColumns for (&'static str, &'static str, &'static str, &'static str) {}
impl IntoValuesColumns for (&'static str, &'static str, &'static str, &'static str) {
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0, self.1, self.2, self.3]
    }
}
// arity-5
impl sealed::SealedColumns
    for (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )
{
}
impl IntoValuesColumns
    for (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )
{
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0, self.1, self.2, self.3, self.4]
    }
}
// arity-6
impl sealed::SealedColumns
    for (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )
{
}
impl IntoValuesColumns
    for (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )
{
    fn into_col_vec(self) -> Vec<&'static str> {
        vec![self.0, self.1, self.2, self.3, self.4, self.5]
    }
}

// ── ValuesRow — sealed trait for tuple row types ──────────────────────────────

/// Sealed trait for row types stored in an [`InlineValues`].
///
/// Implemented for tuples of arity 1–6 where every element implements
/// [`ValuesScalar`].
pub trait ValuesRow: sealed::SealedValuesRow + Clone + Send + Sync + 'static {
    /// Column-name tuple type (`(&'static str, &'static str)` for arity-2, etc.).
    type Columns: IntoValuesColumns;

    /// Arity (number of scalar columns).
    const ARITY: usize;

    /// Postgres type cast strings for each column, in positional order.
    fn sql_casts() -> Vec<&'static str>;

    /// Push one row with first-row casts: `($n::CAST_A, $m::CAST_B, ...)`.
    fn push_row_binds_first(self, acc: &mut SqlAccumulator);

    /// Push one row without casts: `($n, $m, ...)`.
    fn push_row_binds_rest(self, acc: &mut SqlAccumulator);

    /// Decode this row type from a `tokio_postgres::Row` starting at `start_idx`.
    fn decode_from(row: &tokio_postgres::Row, start_idx: usize) -> Result<Self, DjogiError>;
}

// ── push helpers ──────────────────────────────────────────────────────────────

/// Push one scalar bind and append `::CAST` to the SQL text (first-row path).
#[inline]
fn push_bind_with_cast<V: ValuesScalar>(v: V, acc: &mut SqlAccumulator) {
    v.push_bind_owned(acc);
    acc.push_sql("::");
    acc.push_sql(V::SQL_CAST);
}

// ── ValuesRow tuple impls ─────────────────────────────────────────────────────

macro_rules! impl_values_row {
    ( $arity:expr ; $col_tuple:ty ; $( $idx:tt $T:ident ),+ ) => {
        impl< $($T: ValuesScalar),+ > sealed::SealedValuesRow for ( $($T,)+ ) {}

        impl< $($T: ValuesScalar),+ > ValuesRow for ( $($T,)+ ) {
            type Columns = $col_tuple;
            const ARITY: usize = $arity;

            fn sql_casts() -> Vec<&'static str> {
                vec![ $($T::SQL_CAST),+ ]
            }

            fn push_row_binds_first(self, acc: &mut SqlAccumulator) {
                acc.push_sql("(");
                let mut _first = true;
                $(
                    if !_first { acc.push_sql(", "); }
                    push_bind_with_cast::<$T>(self.$idx, acc);
                    _first = false;
                )+
                acc.push_sql(")");
            }

            fn push_row_binds_rest(self, acc: &mut SqlAccumulator) {
                acc.push_sql("(");
                let mut _first = true;
                $(
                    if !_first { acc.push_sql(", "); }
                    self.$idx.push_bind_owned(acc);
                    _first = false;
                )+
                acc.push_sql(")");
            }

            fn decode_from(
                row: &tokio_postgres::Row,
                start_idx: usize,
            ) -> Result<Self, DjogiError> {
                Ok(( $( $T::decode_values_col(row, start_idx + $idx, VALUES_ALIASES[$idx])?, )+ ))
            }
        }
    };
}

impl_values_row!(1; (&'static str,); 0 A);
impl_values_row!(2; (&'static str, &'static str); 0 A, 1 B);
impl_values_row!(3; (&'static str, &'static str, &'static str); 0 A, 1 B, 2 C);
impl_values_row!(4; (&'static str, &'static str, &'static str, &'static str); 0 A, 1 B, 2 C, 3 D);
impl_values_row!(5; (&'static str, &'static str, &'static str, &'static str, &'static str); 0 A, 1 B, 2 C, 3 D, 4 E);
impl_values_row!(6; (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str); 0 A, 1 B, 2 C, 3 D, 4 E, 5 F);

// ── ValuesFieldRef ────────────────────────────────────────────────────────────

/// A typed reference to one column in a [`ValuesFields`] bag.
///
/// Created by `ValuesFields::{col0, col1, …}` inside the `ON` closure.  `V`
/// ties the reference to a specific Rust type so [`FieldRef::eq_values`] can
/// enforce a type match between the model column and the values column at
/// compile time.
pub struct ValuesFieldRef<V> {
    pub(crate) col_idx: usize,
    _v: PhantomData<fn() -> V>,
}

impl<V> ValuesFieldRef<V> {
    pub(crate) fn new(col_idx: usize) -> Self {
        Self {
            col_idx,
            _v: PhantomData,
        }
    }
}

impl<V> Copy for ValuesFieldRef<V> {}
impl<V> Clone for ValuesFieldRef<V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> std::fmt::Debug for ValuesFieldRef<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ValuesFieldRef(col_idx={})", self.col_idx)
    }
}

// ── ValuesFields ──────────────────────────────────────────────────────────────

/// Zero-sized bag of typed column handles for a VALUES row.
///
/// Received as the second argument of the `ON` closure in
/// [`QuerySet::join_values`] / [`QuerySet::left_join_values`].  Each method
/// returns a [`ValuesFieldRef<V>`] tied to the column's Rust type.
pub struct ValuesFields<Row>(PhantomData<fn() -> Row>);

impl<Row> Default for ValuesFields<Row> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
impl<Row> Copy for ValuesFields<Row> {}
impl<Row> Clone for ValuesFields<Row> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A: ValuesScalar> ValuesFields<(A,)> {
    /// Typed reference to the first (and only) column.
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
}
impl<A: ValuesScalar, B: ValuesScalar> ValuesFields<(A, B)> {
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
    pub fn col1(self) -> ValuesFieldRef<B> {
        ValuesFieldRef::new(1)
    }
}
impl<A: ValuesScalar, B: ValuesScalar, C: ValuesScalar> ValuesFields<(A, B, C)> {
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
    pub fn col1(self) -> ValuesFieldRef<B> {
        ValuesFieldRef::new(1)
    }
    pub fn col2(self) -> ValuesFieldRef<C> {
        ValuesFieldRef::new(2)
    }
}
impl<A: ValuesScalar, B: ValuesScalar, C: ValuesScalar, D: ValuesScalar>
    ValuesFields<(A, B, C, D)>
{
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
    pub fn col1(self) -> ValuesFieldRef<B> {
        ValuesFieldRef::new(1)
    }
    pub fn col2(self) -> ValuesFieldRef<C> {
        ValuesFieldRef::new(2)
    }
    pub fn col3(self) -> ValuesFieldRef<D> {
        ValuesFieldRef::new(3)
    }
}
impl<A: ValuesScalar, B: ValuesScalar, C: ValuesScalar, D: ValuesScalar, E: ValuesScalar>
    ValuesFields<(A, B, C, D, E)>
{
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
    pub fn col1(self) -> ValuesFieldRef<B> {
        ValuesFieldRef::new(1)
    }
    pub fn col2(self) -> ValuesFieldRef<C> {
        ValuesFieldRef::new(2)
    }
    pub fn col3(self) -> ValuesFieldRef<D> {
        ValuesFieldRef::new(3)
    }
    pub fn col4(self) -> ValuesFieldRef<E> {
        ValuesFieldRef::new(4)
    }
}
impl<
    A: ValuesScalar,
    B: ValuesScalar,
    C: ValuesScalar,
    D: ValuesScalar,
    E: ValuesScalar,
    F: ValuesScalar,
> ValuesFields<(A, B, C, D, E, F)>
{
    pub fn col0(self) -> ValuesFieldRef<A> {
        ValuesFieldRef::new(0)
    }
    pub fn col1(self) -> ValuesFieldRef<B> {
        ValuesFieldRef::new(1)
    }
    pub fn col2(self) -> ValuesFieldRef<C> {
        ValuesFieldRef::new(2)
    }
    pub fn col3(self) -> ValuesFieldRef<D> {
        ValuesFieldRef::new(3)
    }
    pub fn col4(self) -> ValuesFieldRef<E> {
        ValuesFieldRef::new(4)
    }
    pub fn col5(self) -> ValuesFieldRef<F> {
        ValuesFieldRef::new(5)
    }
}

// ── ValuesOn — structured ON predicate ───────────────────────────────────────

/// A type-safe ON predicate for a VALUES join.
///
/// Constructed by [`FieldRef::eq_values`] and composed with `&`.
/// Only equality predicates are supported in v0.1.
///
/// There is intentionally no raw-SQL constructor.  Any future bypass must
/// follow Djogi's explicit bypass culture.
pub enum ValuesOn<T: Model> {
    /// `__djogi_m.<model_col> = <alias>.<values_col_name>`
    Eq {
        /// Model column (from `FieldRef::column()` — a `&'static str`).
        model_col: &'static str,
        /// Zero-based position of the values column in the row tuple.
        values_col_idx: usize,
        #[doc(hidden)]
        _phantom: PhantomData<fn() -> T>,
    },
    /// Conjunction (`lhs AND rhs`).
    And(Box<ValuesOn<T>>, Box<ValuesOn<T>>),
}

impl<T: Model> std::ops::BitAnd for ValuesOn<T> {
    type Output = ValuesOn<T>;
    fn bitand(self, rhs: ValuesOn<T>) -> ValuesOn<T> {
        ValuesOn::And(Box::new(self), Box::new(rhs))
    }
}

impl<T: Model> std::fmt::Debug for ValuesOn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValuesOn::Eq {
                model_col,
                values_col_idx,
                ..
            } => write!(f, "ValuesOn::Eq({model_col} = col{values_col_idx})"),
            ValuesOn::And(l, r) => write!(f, "ValuesOn::And({l:?}, {r:?})"),
        }
    }
}

// ── FieldRef::eq_values ───────────────────────────────────────────────────────

/// Extend `FieldRef<M, V>` with `eq_values` when `V: ValuesScalar`.
///
/// Placed here rather than in `field.rs` so that `ValuesOn` / `ValuesFieldRef`
/// do not need to be visible from that module.  Rust allows inherent impls
/// in any module of the same crate.
impl<M: Model, V: ValuesScalar> crate::query::field::FieldRef<M, V> {
    /// Build an equality predicate between this model column and a VALUES
    /// column.
    ///
    /// Both sides must share the same Rust type `V`; comparing a
    /// `FieldRef<T, i64>` to a `ValuesFieldRef<f64>` is a compile error.
    pub fn eq_values(self, rhs: ValuesFieldRef<V>) -> ValuesOn<M> {
        ValuesOn::Eq {
            model_col: self.column(),
            values_col_idx: rhs.col_idx,
            _phantom: PhantomData,
        }
    }
}

/// Extend `DjogiField<M, V>` with `eq_values` when `V: ValuesScalar`.
///
/// Phase 8eta PR3 changed the macro to emit `DjogiField<M, V>` as the root
/// field accessor type (wrapping `FieldRef`).  Both the legacy `FieldRef`
/// path (unit tests, pre-PR3 code) and the `DjogiField` path (macro-emitted
/// `{Model}Fields` closures) must work so adopters can write:
///
/// ```ignore
/// Animal::objects().join_values(weights, |a, v| {
///     a.id().eq_values(v.col0())  // a.id() returns DjogiField<Animal, HeerIdDesc>
/// })
/// ```
impl<M: Model, V: ValuesScalar> crate::query::field::DjogiField<M, V> {
    /// Build an equality predicate between this model column and a VALUES
    /// column.
    ///
    /// Both sides must share the same Rust type `V`.  Comparing a
    /// `DjogiField<T, i64>` to a `ValuesFieldRef<f64>` is a compile error.
    ///
    /// ```ignore
    /// .join_values(weights, |animal, v| {
    ///     animal.id().eq_values(v.col0())      // HeerIdDesc matches col0 ✓
    ///     // animal.id().eq_values(v.col1())   // different V → compile error
    /// })
    /// ```
    pub fn eq_values(self, rhs: ValuesFieldRef<V>) -> ValuesOn<M> {
        self.__sql_field().eq_values(rhs)
    }
}

// ── InlineValues ──────────────────────────────────────────────────────────────

/// A validated, typed inline VALUES relation.
///
/// Holds a list of row tuples plus validated SQL identifiers (alias and column
/// names).  Constructed via [`InlineValues::new`], which validates all
/// identifiers at construction time.
pub struct InlineValues<Row: ValuesRow> {
    pub(crate) rows: Vec<Row>,
    /// Validated SQL identifier for the relation alias (e.g. `"weights"`).
    pub(crate) alias: String,
    /// Validated column names, one per `Row::ARITY` element.
    pub(crate) columns: Vec<&'static str>,
}

impl<Row: ValuesRow> Clone for InlineValues<Row> {
    fn clone(&self) -> Self {
        InlineValues {
            rows: self.rows.clone(),
            alias: self.alias.clone(),
            columns: self.columns.clone(),
        }
    }
}

impl<Row: ValuesRow> std::fmt::Debug for InlineValues<Row> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineValues")
            .field("alias", &self.alias)
            .field("columns", &self.columns)
            .field("row_count", &self.rows.len())
            .finish()
    }
}

impl<Row: ValuesRow> InlineValues<Row> {
    /// Create and validate a typed inline VALUES relation.
    ///
    /// # Arguments
    ///
    /// - `rows` — the value data; may be empty (valid zero-row relation).
    /// - `alias` — SQL alias for the VALUES sub-relation (e.g. `"weights"`).
    ///   Must be a plain SQL identifier that does not start with `__djogi_`.
    /// - `columns` — arity-checked tuple of `&'static str` column names.
    ///   Each name must pass the same validation as `alias`.  No duplicates.
    ///
    /// # Errors
    ///
    /// Returns [`DjogiError::Validation`] if:
    /// - `alias` or any column name fails identifier validation.
    /// - Column names contain duplicates.
    /// - `rows.len() × Row::ARITY` exceeds the Postgres parameter ceiling
    ///   (65 535).  Chunk the list or use a staging table instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let weights: InlineValues<(i64, f64)> = InlineValues::new(
    ///     vec![(1_i64, 0.91_f64), (2_i64, 0.72_f64)],
    ///     "weights",
    ///     ("animal_id", "score"),
    /// )?;
    /// ```
    pub fn new(rows: Vec<Row>, alias: &str, columns: Row::Columns) -> Result<Self, DjogiError> {
        check_user_supplied_ident(alias, true).map_err(|e| {
            DjogiError::Validation(format!(
                "InlineValues alias {alias:?} is invalid: {e:?}. \
                 Supply a plain SQL identifier that does not start with `__djogi_`."
            ))
        })?;

        let col_vec = columns.into_col_vec();
        debug_assert_eq!(
            col_vec.len(),
            Row::ARITY,
            "InlineValues column count mismatch (framework bug)"
        );

        for col in &col_vec {
            check_user_supplied_ident(col, true).map_err(|e| {
                DjogiError::Validation(format!(
                    "InlineValues column name {col:?} is invalid: {e:?}. \
                     Supply plain SQL identifiers that do not start with `__djogi_`."
                ))
            })?;
        }

        {
            let mut seen = std::collections::HashSet::with_capacity(col_vec.len());
            for col in &col_vec {
                if !seen.insert(*col) {
                    return Err(DjogiError::Validation(format!(
                        "InlineValues column {col:?} appears more than once; \
                         duplicate column names are not allowed."
                    )));
                }
            }
        }

        let param_count = rows.len().checked_mul(Row::ARITY).ok_or_else(|| {
            DjogiError::Validation(
                "InlineValues parameter count overflowed; \
                     chunk the list or use a staging table."
                    .into(),
            )
        })?;
        if param_count > PG_MAX_PARAMS {
            return Err(DjogiError::Validation(format!(
                "InlineValues would require {param_count} bind parameters \
                 ({} rows × {} columns), exceeding Postgres' limit of {PG_MAX_PARAMS}. \
                 Chunk the list into smaller batches or load it into a \
                 temporary/staging table before joining.",
                rows.len(),
                Row::ARITY,
            )));
        }

        Ok(InlineValues {
            rows,
            alias: alias.to_owned(),
            columns: col_vec,
        })
    }

    /// Returns `true` if this relation contains no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

// ── ValuesJoinedQuerySet (inner join) ─────────────────────────────────────────

/// A lazy INNER JOIN of a model queryset against an inline VALUES relation.
///
/// Constructed by [`QuerySet::join_values`].  Terminals produce `(T, Row)` pairs.
pub struct ValuesJoinedQuerySet<T: Model, Row: ValuesRow> {
    pub(crate) left: QuerySet<T>,
    pub(crate) values: InlineValues<Row>,
    pub(crate) on: ValuesOn<T>,
}

impl<T: Model, Row: ValuesRow> std::fmt::Debug for ValuesJoinedQuerySet<T, Row> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValuesJoinedQuerySet")
            .field("values", &self.values)
            .field("on", &self.on)
            .finish()
    }
}

// ── LeftValuesJoinedQuerySet (left join) ──────────────────────────────────────

/// A lazy LEFT JOIN of a model queryset against an inline VALUES relation.
///
/// Constructed by [`QuerySet::left_join_values`].  Terminals produce
/// `(T, Option<Row>)` pairs — `None` when no values row matched.
pub struct LeftValuesJoinedQuerySet<T: Model, Row: ValuesRow> {
    pub(crate) left: QuerySet<T>,
    pub(crate) values: InlineValues<Row>,
    pub(crate) on: ValuesOn<T>,
}

impl<T: Model, Row: ValuesRow> std::fmt::Debug for LeftValuesJoinedQuerySet<T, Row> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeftValuesJoinedQuerySet")
            .field("values", &self.values)
            .field("on", &self.on)
            .finish()
    }
}

// ── QuerySet builder methods ──────────────────────────────────────────────────

impl<T: Model> QuerySet<T> {
    /// INNER JOIN this queryset against an inline VALUES relation.
    ///
    /// Returns a [`ValuesJoinedQuerySet<T, Row>`] whose terminals produce
    /// `(T, Row)` pairs.
    ///
    /// # Unsupported left-queryset state
    ///
    /// `.prefetch(…)`, `.select_related(…)`, `.cache(…)`, row locks, and
    /// non-default `distinct` are rejected at terminal time with
    /// [`DjogiError::Validation`].  Filters, ordering, limit, and offset are
    /// supported.
    ///
    /// # Short-circuit
    ///
    /// If the left queryset is `none()`-derived or `InlineValues` has zero
    /// rows, terminals return the empty result without a database round-trip.
    pub fn join_values<Row, F>(
        self,
        values: InlineValues<Row>,
        on_fn: F,
    ) -> ValuesJoinedQuerySet<T, Row>
    where
        Row: ValuesRow,
        F: FnOnce(T::Fields, ValuesFields<Row>) -> ValuesOn<T>,
    {
        let on = on_fn(Default::default(), Default::default());
        ValuesJoinedQuerySet {
            left: self,
            values,
            on,
        }
    }

    /// LEFT JOIN this queryset against an inline VALUES relation.
    ///
    /// Returns a [`LeftValuesJoinedQuerySet<T, Row>`] whose terminals produce
    /// `(T, Option<Row>)` pairs.
    ///
    /// # Empty values
    ///
    /// Unlike [`QuerySet::join_values`], an empty `InlineValues` does **not**
    /// short-circuit to zero results.  All model rows are returned with `None`
    /// for the values column.
    pub fn left_join_values<Row, F>(
        self,
        values: InlineValues<Row>,
        on_fn: F,
    ) -> LeftValuesJoinedQuerySet<T, Row>
    where
        Row: ValuesRow,
        F: FnOnce(T::Fields, ValuesFields<Row>) -> ValuesOn<T>,
    {
        let on = on_fn(Default::default(), Default::default());
        LeftValuesJoinedQuerySet {
            left: self,
            values,
            on,
        }
    }
}

// ── Left-queryset state validation ───────────────────────────────────────────

fn validate_left_qs<T: Model>(qs: &QuerySet<T>, site: &str) -> Result<(), DjogiError> {
    if !qs.prefetch_paths.is_empty() {
        return Err(DjogiError::Validation(format!(
            "{site}: left queryset has prefetch paths, which change the row shape. \
             Drop .prefetch(…) calls before .join_values(…) / .left_join_values(…)."
        )));
    }
    if !qs.select_related_paths.is_empty() {
        return Err(DjogiError::Validation(format!(
            "{site}: left queryset has select_related paths, which expand the \
             SELECT list incompatibly.  Drop .select_related(…) calls."
        )));
    }
    if qs.cache_target.is_some() {
        return Err(DjogiError::Validation(format!(
            "{site}: left queryset is bound to a Punnu via .cache(…). \
             VALUES join terminals return pairs, not bare model rows.  \
             Drop the .cache(…) call."
        )));
    }
    if !matches!(qs.lock, crate::query::lock::LockMode::None) {
        return Err(DjogiError::Validation(format!(
            "{site}: left queryset carries a row-level lock, which is not \
             supported on VALUES joins.  Drop the row-lock call."
        )));
    }
    if !matches!(qs.distinct, DistinctMode::None) {
        return Err(DjogiError::Validation(format!(
            "{site}: left queryset carries a non-default DISTINCT mode, which \
             is not supported on VALUES joins.  Drop .distinct…() calls."
        )));
    }
    Ok(())
}

// ── SQL builders ─────────────────────────────────────────────────────────────

/// Build the full SELECT SQL for an INNER VALUES join.
///
/// # Preconditions
///
/// - `vqs.values.rows` is non-empty (callers short-circuit before calling).
/// - Left queryset state has been validated.
pub(crate) fn build_values_join_select<T, Row>(
    vqs: &ValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let mut acc = SqlAccumulator::new("");
    emit_select_projection::<T, Row>(&vqs.values, &mut acc, false);
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);
    push_inner_join_values(&vqs.values, &vqs.on, &mut acc);
    push_qualified_tail(&mut acc, &vqs.left)?;
    Ok(acc)
}

/// Build the full SELECT SQL for a non-empty LEFT VALUES join.
pub(crate) fn build_left_values_join_select<T, Row>(
    vqs: &LeftValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let mut acc = SqlAccumulator::new("");
    emit_select_projection::<T, Row>(&vqs.values, &mut acc, true);
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);
    push_left_join_values(&vqs.values, &vqs.on, &mut acc);
    push_qualified_tail(&mut acc, &vqs.left)?;
    Ok(acc)
}

/// Build SELECT SQL for a LEFT JOIN where the values side is empty.
///
/// Per the owner decision, uses a **typed zero-row relation** subquery rather
/// than inlining `NULL::TYPE` directly in the SELECT list.  Postgres can
/// fully type-check and plan the query from the subquery column types, and
/// the ON predicate is preserved so the join shape is structurally identical
/// to the non-empty path.
///
/// Emitted shape (arity-2 example):
///
/// ```sql
/// SELECT __djogi_m.id   AS id,
///        __djogi_m.name AS name, ...,
///        weights.animal_id AS __djogi_values_0,
///        weights.score     AS __djogi_values_1,
///        weights.__djogi_present AS __djogi_values_present
/// FROM animals AS __djogi_m
/// LEFT JOIN (
///     SELECT NULL::BIGINT         AS animal_id,
///            NULL::DOUBLE PRECISION AS score,
///            NULL::BOOLEAN          AS __djogi_present
///     WHERE 1=0
/// ) AS weights
/// ON __djogi_m.id = weights.animal_id
/// [WHERE ...] [ORDER BY ...] [LIMIT $n] [OFFSET $n]
/// ```
///
/// The `WHERE 1=0` in the subquery is a constant-false predicate; the Postgres
/// planner folds it away (zero rows, no scan), but the column definitions
/// provide the type context needed to validate the outer query.
pub(crate) fn build_left_values_join_empty_select<T, Row>(
    vqs: &LeftValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let values = &vqs.values;
    let on = &vqs.on;

    let mut acc = SqlAccumulator::new("");

    // SELECT: model columns qualified by MODEL_ALIAS, then values columns
    // from the alias (same projection shape as the non-empty left-join path).
    emit_select_projection::<T, Row>(values, &mut acc, true);

    // FROM <table> AS __djogi_m
    acc.push_sql(" FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);

    // LEFT JOIN (typed zero-row subquery) AS <alias>
    //
    // The subquery selects typed NULLs for each values column plus the
    // sentinel, with WHERE 1=0 to guarantee zero rows.  The column names
    // match what the outer SELECT projection references so Postgres can
    // resolve the aliases.
    acc.push_sql(" LEFT JOIN (SELECT ");
    let casts = Row::sql_casts();
    for (i, user_col) in values.columns.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql("NULL::");
        acc.push_sql(casts[i]);
        acc.push_sql(" AS ");
        acc.push_sql(user_col);
    }
    if !values.columns.is_empty() {
        acc.push_sql(", ");
    }
    acc.push_sql("NULL::BOOLEAN AS ");
    acc.push_sql(SENTINEL_COL);
    acc.push_sql(" WHERE 1=0) AS ");
    acc.push_sql(&values.alias);

    // ON predicate — same structured predicate as the non-empty path.
    acc.push_sql(" ON ");
    push_on_predicate(on, values, &mut acc);

    // WHERE / ORDER BY / LIMIT / OFFSET
    push_qualified_tail(&mut acc, &vqs.left)?;

    Ok(acc)
}

/// Build COUNT(*) for an INNER VALUES join.
pub(crate) fn build_values_join_count<T, Row>(
    vqs: &ValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);
    push_inner_join_values(&vqs.values, &vqs.on, &mut acc);
    push_qualified_where(&mut acc, &vqs.left)?;
    Ok(acc)
}

/// Build COUNT(*) for a LEFT VALUES join.
pub(crate) fn build_left_values_join_count<T, Row>(
    vqs: &LeftValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let mut acc = SqlAccumulator::new("SELECT COUNT(*) FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);
    if !vqs.values.is_empty() {
        push_left_join_values(&vqs.values, &vqs.on, &mut acc);
    }
    push_qualified_where(&mut acc, &vqs.left)?;
    Ok(acc)
}

/// Build EXISTS for an INNER VALUES join.
pub(crate) fn build_values_join_exists<T, Row>(
    vqs: &ValuesJoinedQuerySet<T, Row>,
) -> Result<SqlAccumulator, PortablePredicateError>
where
    T: Model + FromPgRow,
    Row: ValuesRow,
{
    let mut acc = SqlAccumulator::new("SELECT EXISTS (SELECT 1 FROM ");
    acc.push_sql(T::table_name());
    acc.push_sql(" AS ");
    acc.push_sql(MODEL_ALIAS);
    push_inner_join_values(&vqs.values, &vqs.on, &mut acc);
    push_qualified_where(&mut acc, &vqs.left)?;
    acc.push_sql(")");
    Ok(acc)
}

// ── SQL sub-helpers ───────────────────────────────────────────────────────────

/// Emit `SELECT <model cols>, <values cols> [, sentinel]`.
fn emit_select_projection<T: Model + FromPgRow, Row: ValuesRow>(
    values: &InlineValues<Row>,
    acc: &mut SqlAccumulator,
    with_sentinel: bool,
) {
    acc.push_sql("SELECT ");
    for (i, col) in <T as FromPgRow>::COLUMNS.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(MODEL_ALIAS);
        acc.push_sql(".");
        acc.push_sql(col);
        acc.push_sql(" AS ");
        acc.push_sql(col);
    }
    for (i, user_col) in values.columns.iter().enumerate() {
        acc.push_sql(", ");
        acc.push_sql(&values.alias);
        acc.push_sql(".");
        acc.push_sql(user_col);
        acc.push_sql(" AS ");
        acc.push_sql(VALUES_ALIASES[i]);
    }
    if with_sentinel {
        acc.push_sql(", ");
        acc.push_sql(&values.alias);
        acc.push_sql(".");
        acc.push_sql(SENTINEL_COL);
        acc.push_sql(" AS __djogi_values_present");
    }
}

/// Emit `INNER JOIN (VALUES ...) AS alias(cols) ON ...`.
fn push_inner_join_values<Row: ValuesRow, T: Model>(
    values: &InlineValues<Row>,
    on: &ValuesOn<T>,
    acc: &mut SqlAccumulator,
) {
    acc.push_sql(" INNER JOIN (VALUES ");
    push_values_rows(values, acc, false);
    acc.push_sql(") AS ");
    acc.push_sql(&values.alias);
    push_col_list(values, acc, false);
    acc.push_sql(" ON ");
    push_on_predicate(on, values, acc);
}

/// Emit `LEFT JOIN (VALUES ...) AS alias(cols, __djogi_present) ON ...`.
fn push_left_join_values<Row: ValuesRow, T: Model>(
    values: &InlineValues<Row>,
    on: &ValuesOn<T>,
    acc: &mut SqlAccumulator,
) {
    acc.push_sql(" LEFT JOIN (VALUES ");
    push_values_rows(values, acc, true);
    acc.push_sql(") AS ");
    acc.push_sql(&values.alias);
    push_col_list(values, acc, true);
    acc.push_sql(" ON ");
    push_on_predicate(on, values, acc);
}

/// Emit `VALUES (...), (...)`.  If `with_sentinel`, appends `, TRUE` to each row.
fn push_values_rows<Row: ValuesRow>(
    values: &InlineValues<Row>,
    acc: &mut SqlAccumulator,
    with_sentinel: bool,
) {
    let mut rows = values.rows.iter().cloned();
    if let Some(first) = rows.next() {
        first.push_row_binds_first(acc);
        if with_sentinel {
            let popped = acc.pop_sql_suffix(")");
            debug_assert!(popped, "push_row_binds_first must end with ')'");
            acc.push_sql(", TRUE)");
        }
    }
    for row in rows {
        acc.push_sql(", ");
        row.push_row_binds_rest(acc);
        if with_sentinel {
            let popped = acc.pop_sql_suffix(")");
            debug_assert!(popped, "push_row_binds_rest must end with ')'");
            acc.push_sql(", TRUE)");
        }
    }
}

/// Emit `(col0, col1, ...)` for the VALUES AS clause.
fn push_col_list<Row: ValuesRow>(
    values: &InlineValues<Row>,
    acc: &mut SqlAccumulator,
    with_sentinel: bool,
) {
    acc.push_sql("(");
    for (i, col) in values.columns.iter().enumerate() {
        if i > 0 {
            acc.push_sql(", ");
        }
        acc.push_sql(col);
    }
    if with_sentinel {
        if !values.columns.is_empty() {
            acc.push_sql(", ");
        }
        acc.push_sql(SENTINEL_COL);
    }
    acc.push_sql(")");
}

/// Emit the ON predicate recursively.
fn push_on_predicate<T: Model, Row: ValuesRow>(
    on: &ValuesOn<T>,
    values: &InlineValues<Row>,
    acc: &mut SqlAccumulator,
) {
    match on {
        ValuesOn::Eq {
            model_col,
            values_col_idx,
            ..
        } => {
            acc.push_sql(MODEL_ALIAS);
            acc.push_sql(".");
            acc.push_sql(model_col);
            acc.push_sql(" = ");
            acc.push_sql(&values.alias);
            acc.push_sql(".");
            acc.push_sql(values.columns[*values_col_idx]);
        }
        ValuesOn::And(l, r) => {
            acc.push_sql("(");
            push_on_predicate(l, values, acc);
            acc.push_sql(" AND ");
            push_on_predicate(r, values, acc);
            acc.push_sql(")");
        }
    }
}

/// Emit WHERE (qualified) + ORDER BY + LIMIT + OFFSET.
fn push_qualified_tail<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
) -> Result<(), PortablePredicateError> {
    push_qualified_where(acc, qs)?;
    if !qs.ordering.is_empty() {
        acc.push_sql(" ORDER BY ");
        for (i, o) in qs.ordering.iter().enumerate() {
            if i > 0 {
                acc.push_sql(", ");
            }
            o.emit(acc, Some(MODEL_ALIAS));
        }
    }
    if let Some(n) = qs.limit {
        acc.push_sql(" LIMIT ");
        acc.push_bind(n);
    }
    if let Some(n) = qs.offset {
        acc.push_sql(" OFFSET ");
        acc.push_bind(n);
    }
    Ok(())
}

/// Emit WHERE clause with column references qualified by MODEL_ALIAS.
fn push_qualified_where<T: Model>(
    acc: &mut SqlAccumulator,
    qs: &QuerySet<T>,
) -> Result<(), PortablePredicateError> {
    if q_is_vacuously_true(&qs.condition) {
        return Ok(());
    }
    acc.push_sql(" WHERE ");
    emit_q::<T>(acc, &qs.condition, SqlEmitContext::joined(MODEL_ALIAS))
}

// ── Shared decode helpers ─────────────────────────────────────────────────────

fn decode_inner_pair<T: Model + FromPgRow, Row: ValuesRow>(
    pg_row: &tokio_postgres::Row,
) -> Result<(T, Row), DjogiError> {
    let model = T::from_pg_row(pg_row)?;
    let row = Row::decode_from(pg_row, <T as FromPgRow>::COLUMNS.len())?;
    Ok((model, row))
}

fn decode_left_pair<T: Model + FromPgRow, Row: ValuesRow>(
    pg_row: &tokio_postgres::Row,
    model_col_count: usize,
) -> Result<(T, Option<Row>), DjogiError> {
    let model = T::from_pg_row(pg_row)?;
    let sentinel_idx = model_col_count + Row::ARITY;
    let present: Option<bool> = pg_row
        .try_get::<_, Option<bool>>(sentinel_idx)
        .map_err(|e| {
            DjogiError::Decode(format!(
                "column `__djogi_values_present` at position {sentinel_idx}: {e}"
            ))
        })?;
    let values_row = if present.unwrap_or(false) {
        Some(Row::decode_from(pg_row, model_col_count)?)
    } else {
        None
    };
    Ok((model, values_row))
}

// ── Terminals: ValuesJoinedQuerySet ───────────────────────────────────────────

impl<T, Row> ValuesJoinedQuerySet<T, Row>
where
    T: Model + FromPgRow + Send + Unpin,
    Row: ValuesRow + Send + Unpin,
{
    /// Execute and collect all `(T, Row)` pairs.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<(T, Row)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "join_values::fetch_all")?;
            if self.left.is_empty() || self.values.is_empty() {
                return Ok(vec![]);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = build_values_join_select(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            ctx.query_all(&sql, &params)
                .await?
                .iter()
                .map(|r| decode_inner_pair::<T, Row>(r))
                .collect()
        }
    }

    /// Return the first `(T, Row)` pair, or `None`.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<(T, Row)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "join_values::first")?;
            if self.left.is_empty() || self.values.is_empty() {
                return Ok(None);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let mut limited = self;
            limited.left.limit = Some(1);
            let (sql, binds) = build_values_join_select(&limited)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            ctx.query_opt(&sql, &params)
                .await?
                .as_ref()
                .map(|r| decode_inner_pair::<T, Row>(r))
                .transpose()
        }
    }

    /// Expect exactly one pair; error on zero or multiple.
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<(T, Row), DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "join_values::fetch_one")?;
            if self.left.is_empty() || self.values.is_empty() {
                return Err(DjogiError::not_found(T::table_name()));
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let mut probe = self;
            probe.left.limit = Some(2);
            let (sql, binds) = build_values_join_select(&probe)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            match rows.len() {
                0 => Err(DjogiError::not_found(T::table_name())),
                1 => decode_inner_pair::<T, Row>(&rows[0]),
                n => Err(DjogiError::multiple_objects(T::table_name(), n)),
            }
        }
    }

    /// Count matching pairs.
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "join_values::count")?;
            if self.left.is_empty() || self.values.is_empty() {
                return Ok(0);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = build_values_join_count(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            row.try_get::<_, i64>(0)
                .map_err(|e| DjogiError::Decode(format!("join_values count: {e}")))
        }
    }

    /// Return `true` if at least one pair matches.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "join_values::exists")?;
            if self.left.is_empty() || self.values.is_empty() {
                return Ok(false);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = build_values_join_exists(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            row.try_get::<_, bool>(0)
                .map_err(|e| DjogiError::Decode(format!("join_values exists: {e}")))
        }
    }
}

// ── Terminals: LeftValuesJoinedQuerySet ───────────────────────────────────────

impl<T, Row> LeftValuesJoinedQuerySet<T, Row>
where
    T: Model + FromPgRow + Send + Unpin,
    Row: ValuesRow + Send + Unpin,
{
    /// Execute and collect all `(T, Option<Row>)` pairs.
    pub fn fetch_all<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Vec<(T, Option<Row>)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "left_join_values::fetch_all")?;
            if self.left.is_empty() {
                return Ok(vec![]);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let col_count = <T as FromPgRow>::COLUMNS.len();
            let (sql, binds) = (if self.values.is_empty() {
                build_left_values_join_empty_select(&self)
            } else {
                build_left_values_join_select(&self)
            })
            .map_err(DjogiError::from)?
            .into_parts();
            let params = as_params(&binds);
            ctx.query_all(&sql, &params)
                .await?
                .iter()
                .map(|r| decode_left_pair::<T, Row>(r, col_count))
                .collect()
        }
    }

    /// Return the first `(T, Option<Row>)` pair, or `None` if no model rows.
    pub fn first<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<Option<(T, Option<Row>)>, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "left_join_values::first")?;
            if self.left.is_empty() {
                return Ok(None);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let col_count = <T as FromPgRow>::COLUMNS.len();
            let mut limited = self;
            limited.left.limit = Some(1);
            let (sql, binds) = (if limited.values.is_empty() {
                build_left_values_join_empty_select(&limited)
            } else {
                build_left_values_join_select(&limited)
            })
            .map_err(DjogiError::from)?
            .into_parts();
            let params = as_params(&binds);
            ctx.query_opt(&sql, &params)
                .await?
                .as_ref()
                .map(|r| decode_left_pair::<T, Row>(r, col_count))
                .transpose()
        }
    }

    /// Expect exactly one model row.
    pub fn fetch_one<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<(T, Option<Row>), DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "left_join_values::fetch_one")?;
            if self.left.is_empty() {
                return Err(DjogiError::not_found(T::table_name()));
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let col_count = <T as FromPgRow>::COLUMNS.len();
            let mut probe = self;
            probe.left.limit = Some(2);
            let (sql, binds) = (if probe.values.is_empty() {
                build_left_values_join_empty_select(&probe)
            } else {
                build_left_values_join_select(&probe)
            })
            .map_err(DjogiError::from)?
            .into_parts();
            let params = as_params(&binds);
            let rows = ctx.query_all(&sql, &params).await?;
            match rows.len() {
                0 => Err(DjogiError::not_found(T::table_name())),
                1 => decode_left_pair::<T, Row>(&rows[0], col_count),
                n => Err(DjogiError::multiple_objects(T::table_name(), n)),
            }
        }
    }

    /// Count model rows (left join with empty values = count of all model rows).
    pub fn count<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<i64, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "left_join_values::count")?;
            if self.left.is_empty() {
                return Ok(0);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = build_left_values_join_count(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            row.try_get::<_, i64>(0)
                .map_err(|e| DjogiError::Decode(format!("left_join_values count: {e}")))
        }
    }

    /// Return `true` if any model rows exist.
    pub fn exists<'ctx>(
        self,
        ctx: &'ctx mut DjogiContext,
    ) -> impl Future<Output = Result<bool, DjogiError>> + Send + 'ctx
    where
        T: 'ctx,
        Row: 'ctx,
    {
        async move {
            validate_left_qs(&self.left, "left_join_values::exists")?;
            if self.left.is_empty() {
                return Ok(false);
            }
            crate::query::terminal::auto_set_tenant::<T>(ctx).await?;
            let (sql, binds) = build_left_values_join_count(&self)
                .map_err(DjogiError::from)?
                .into_parts();
            let params = as_params(&binds);
            let row = ctx.query_one(&sql, &params).await?;
            let n: i64 = row
                .try_get::<_, i64>(0)
                .map_err(|e| DjogiError::Decode(format!("left_join_values exists: {e}")))?;
            Ok(n > 0)
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::ModelDescriptor;

    // Minimal in-crate stub model — mirrors the `Fake` model pattern used by
    // `query::queryset` unit tests.  SQL-shape tests only inspect the emitted
    // SQL string; no actual database round-trip occurs.
    struct Stub;
    impl crate::model::__sealed::Sealed for Stub {}
    #[allow(clippy::manual_async_fn)]
    impl crate::model::Model for Stub {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "stub_table"
        }
        fn pk_value(&self) -> &i64 {
            unreachable!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unreachable!()
        }
        fn get(
            _ctx: &mut DjogiContext,
            _id: i64,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn create(
            _ctx: &mut DjogiContext,
            _v: Self,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send {
            async { unreachable!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
        fn delete(
            self,
            _ctx: &mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<(), DjogiError>> + Send {
            async { unreachable!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut DjogiContext,
        ) -> impl std::future::Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unreachable!() }
        }
    }

    impl FromPgRow for Stub {
        const COLUMNS: &'static [&'static str] = &["id"];
        const COLUMN_LIST: &'static str = "id";
        fn from_pg_row(_row: &tokio_postgres::Row) -> Result<Self, DjogiError> {
            unreachable!()
        }
    }

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn inline_values_accepts_tuple_rows_and_static_columns() {
        let iv: InlineValues<(i64, f64)> = InlineValues::new(
            vec![(1_i64, 0.91_f64), (2_i64, 0.72_f64)],
            "weights",
            ("animal_id", "score"),
        )
        .expect("should succeed");
        assert_eq!(iv.rows.len(), 2);
        assert_eq!(iv.alias, "weights");
        assert_eq!(iv.columns, vec!["animal_id", "score"]);
        assert!(!iv.is_empty());
    }

    #[test]
    fn inline_values_accepts_empty_rows() {
        let iv: InlineValues<(i64, f64)> =
            InlineValues::new(vec![], "scores", ("id", "score")).expect("empty rows are valid");
        assert!(iv.is_empty());
        assert_eq!(iv.len(), 0);
    }

    #[test]
    fn inline_values_rejects_bad_alias() {
        let err = InlineValues::<(i64,)>::new(vec![(1_i64,)], "bad alias", ("col",)).unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation");
        };
        assert!(msg.contains("bad alias"), "got: {msg}");
    }

    #[test]
    fn inline_values_rejects_bad_column() {
        let err = InlineValues::<(i64,)>::new(vec![(1_i64,)], "alias", ("bad col",)).unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation")
        };
        assert!(msg.contains("bad col"), "got: {msg}");
    }

    #[test]
    fn inline_values_rejects_reserved_djogi_prefix() {
        let err =
            InlineValues::<(i64,)>::new(vec![(1_i64,)], "__djogi_scores", ("col",)).unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation")
        };
        assert!(msg.contains("__djogi_scores"), "got: {msg}");
    }

    #[test]
    fn inline_values_rejects_reserved_djogi_prefix_column() {
        let err = InlineValues::<(i64,)>::new(vec![(1_i64,)], "weights", ("__djogi_internal",))
            .unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation")
        };
        assert!(msg.contains("__djogi_internal"), "got: {msg}");
    }

    #[test]
    fn inline_values_rejects_duplicate_columns() {
        let err =
            InlineValues::<(i64, f64)>::new(vec![(1_i64, 0.5_f64)], "weights", ("score", "score"))
                .unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation")
        };
        assert!(msg.contains("score"), "got: {msg}");
    }

    #[test]
    fn inline_values_rejects_parameter_count_overflow() {
        let rows: Vec<(i64,)> = vec![(0_i64,); PG_MAX_PARAMS + 1];
        let err = InlineValues::new(rows, "t", ("col",)).unwrap_err();
        let DjogiError::Validation(msg) = err else {
            panic!("expected Validation")
        };
        assert!(
            msg.contains("65535") || msg.contains("65536") || msg.contains("param"),
            "got: {msg}"
        );
    }

    // ── SQL shape ─────────────────────────────────────────────────────────────

    fn stub_qs() -> QuerySet<Stub> {
        QuerySet::new()
    }

    fn stub_vqs(alias: &str) -> ValuesJoinedQuerySet<Stub, (i64, f64)> {
        let values = InlineValues::new(vec![(1_i64, 0.5_f64)], alias, ("aid", "sc")).unwrap();
        ValuesJoinedQuerySet {
            left: stub_qs(),
            values,
            on: ValuesOn::Eq {
                model_col: "id",
                values_col_idx: 0,
                _phantom: PhantomData,
            },
        }
    }

    #[test]
    fn values_join_sql_projects_model_then_values_columns() {
        let sql = build_values_join_select(&stub_vqs("w")).unwrap();
        let s = sql.sql().to_owned();
        assert!(
            s.starts_with("SELECT __djogi_m.id AS id"),
            "model columns first; sql = {s}"
        );
        assert!(
            s.contains("w.aid AS __djogi_values_0"),
            "values col 0; sql = {s}"
        );
        assert!(
            s.contains("w.sc AS __djogi_values_1"),
            "values col 1; sql = {s}"
        );
    }

    #[test]
    fn values_join_sql_casts_first_row_placeholders() {
        let values =
            InlineValues::new(vec![(1_i64, 0.5_f64), (2_i64, 0.8_f64)], "w", ("aid", "sc"))
                .unwrap();
        let vqs = ValuesJoinedQuerySet {
            left: stub_qs(),
            values,
            on: ValuesOn::Eq {
                model_col: "id",
                values_col_idx: 0,
                _phantom: PhantomData,
            },
        };
        let acc = build_values_join_select(&vqs).unwrap();
        let s = acc.sql().to_owned();
        assert!(
            s.contains("$1::BIGINT") && s.contains("$2::DOUBLE PRECISION"),
            "first row has casts; sql = {s}"
        );
        assert!(
            s.contains(", ($3, $4)"),
            "second row bare params; sql = {s}"
        );
    }

    #[test]
    fn values_join_sql_uses_lexical_bind_order() {
        // VALUES binds appear before WHERE binds because the JOIN clause
        // precedes the WHERE clause in the emitted SQL.
        use crate::query::condition::{Condition, FilterValue, Leaf};
        let values = InlineValues::new(vec![(99_i64, 1.0_f64)], "w", ("aid", "sc")).unwrap();
        let qs = stub_qs().filter(|_| Condition::Leaf(Leaf::eq_raw("id", FilterValue::I64(42))));
        let vqs = ValuesJoinedQuerySet {
            left: qs,
            values,
            on: ValuesOn::Eq {
                model_col: "id",
                values_col_idx: 0,
                _phantom: PhantomData,
            },
        };
        let acc = build_values_join_select(&vqs).unwrap();
        let s = acc.sql().to_owned();
        let pos_values = s.find("$1::BIGINT").expect("VALUES bind first");
        let pos_where = s.rfind("$3").expect("WHERE bind present");
        assert!(pos_values < pos_where, "VALUES before WHERE; sql = {s}");
        assert_eq!(acc.bind_count(), 3, "2 VALUES + 1 WHERE; sql = {s}");
    }

    #[test]
    fn values_join_sql_emits_structured_on_not_on_true() {
        let acc = build_values_join_select(&stub_vqs("w")).unwrap();
        let s = acc.sql().to_owned();
        assert!(
            s.contains("ON __djogi_m.id = w.aid"),
            "structured ON; sql = {s}"
        );
        assert!(!s.contains("ON TRUE"), "no ON TRUE; sql = {s}");
    }

    #[test]
    fn inline_values_empty_is_valid_construction() {
        let iv: InlineValues<(i64,)> =
            InlineValues::new(vec![], "w", ("id",)).expect("empty is valid");
        assert!(iv.is_empty());
    }

    #[test]
    fn left_join_empty_values_sql_uses_typed_zero_row_relation_join() {
        // Owner decision: empty InlineValues on a left join must use a typed
        // zero-row relation shape — a LEFT JOIN against a subquery that returns
        // no rows but has correctly-typed columns — NOT an alternate SELECT path
        // that inlines NULL::TYPE directly in the top-level projection.
        let values: InlineValues<(i64, f64)> =
            InlineValues::new(vec![], "w", ("aid", "sc")).unwrap();
        let vqs = LeftValuesJoinedQuerySet {
            left: stub_qs(),
            values,
            on: ValuesOn::Eq {
                model_col: "id",
                values_col_idx: 0,
                _phantom: PhantomData,
            },
        };
        let acc = build_left_values_join_empty_select(&vqs).unwrap();
        let s = acc.sql().to_owned();

        // The zero-row typed subquery must be present inside a LEFT JOIN.
        assert!(
            s.contains("LEFT JOIN (SELECT"),
            "must emit a LEFT JOIN with typed subquery; sql = {s}"
        );
        // Typed NULL columns inside the subquery.
        assert!(
            s.contains("NULL::BIGINT AS aid"),
            "typed null col0 inside subquery; sql = {s}"
        );
        assert!(
            s.contains("NULL::DOUBLE PRECISION AS sc"),
            "typed null col1 inside subquery; sql = {s}"
        );
        assert!(
            s.contains("NULL::BOOLEAN AS __djogi_present"),
            "typed null sentinel inside subquery; sql = {s}"
        );
        // Constant-false predicate collapses the subquery to zero rows.
        assert!(s.contains("WHERE 1=0"), "zero-row guard; sql = {s}");
        // The subquery is aliased to the user alias so ON and the outer
        // projection can reference it.
        assert!(
            s.contains(") AS w"),
            "subquery aliased to user alias; sql = {s}"
        );
        // The structured ON predicate is still emitted (same shape as non-empty).
        assert!(
            s.contains("ON __djogi_m.id = w.aid"),
            "ON predicate present; sql = {s}"
        );
        // Outer projection references the alias, not raw NULLs.
        assert!(
            s.contains("w.aid AS __djogi_values_0"),
            "outer projection references alias; sql = {s}"
        );
        assert!(
            s.contains("w.__djogi_present AS __djogi_values_present"),
            "sentinel from alias; sql = {s}"
        );
        // No binds — the subquery uses SQL literals only.
        assert_eq!(
            acc.bind_count(),
            0,
            "no binds for empty left join; sql = {s}"
        );
    }
}
