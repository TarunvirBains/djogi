//! Raw SQL escape hatch for queries beyond the `QuerySet` surface.
//!
//! All three functions accept any `sqlx::Executor<Database = Postgres>`, so
//! they work equally with `&PgPool` and `&mut *Transaction<'_, Postgres>`
//! (the deref-reborrow pattern that yields `&mut PgConnection`).
//!
//! # API shape — closure-based binding
//!
//! Each function takes the SQL string plus a `bind_fn` closure that receives
//! a `sqlx::query::Query{As,Scalar,}` builder and returns it with `.bind()`
//! calls chained. This keeps the external dependency on sqlx's query types
//! at a single boundary — user code holds an opaque builder between the
//! library entry point and the terminal `fetch_*` / `execute`.
//!
//! # Example
//!
//! ```rust,ignore
//! let published: Vec<Post> = djogi::raw::query_as(
//!     &pool,
//!     "SELECT * FROM posts WHERE published = $1",
//!     |q| q.bind(true),
//! ).await?;
//!
//! let count: i64 = djogi::raw::query_scalar(
//!     &pool,
//!     "SELECT COUNT(*) FROM posts WHERE view_count > $1",
//!     |q| q.bind(100i32),
//! ).await?;
//!
//! djogi::raw::execute(
//!     &pool,
//!     "UPDATE posts SET published = false WHERE view_count < $1",
//!     |q| q.bind(1i32),
//! ).await?;
//! ```
//!
//! # Why `&str` and not `&'static str`
//!
//! Dynamic SQL built at runtime (e.g. from user-supplied condition builders
//! or shell commands) cannot be `'static`. SQLx 0.8 accepts `&str` for its
//! query entry points, so we follow suit. The cost is that the caller must
//! keep the SQL string alive for the duration of the call — typical Rust
//! borrow-checker territory, not a new constraint.

use crate::DjogiError;
use sqlx::postgres::PgArguments;
use sqlx::{Executor, FromRow, Postgres};

/// Execute a raw SQL query and return a `Vec<T>` via `FromRow`.
///
/// Returns an empty `Vec` if the query matches zero rows — this is the
/// natural `fetch_all` behaviour, not an error.
pub async fn query_as<'e, T, E, F>(executor: E, sql: &str, bind_fn: F) -> Result<Vec<T>, DjogiError>
where
    T: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
    E: Executor<'e, Database = Postgres>,
    F: for<'q> FnOnce(
        sqlx::query::QueryAs<'q, Postgres, T, PgArguments>,
    ) -> sqlx::query::QueryAs<'q, Postgres, T, PgArguments>,
{
    let q = sqlx::query_as::<Postgres, T>(sql);
    let q = bind_fn(q);
    Ok(q.fetch_all(executor).await?)
}

/// Execute a raw SQL query and return a single scalar value.
///
/// Returns `DjogiError::NotFound { table: "<raw query>" }` if the query
/// produces zero rows. The raw API has no inherent table context, so the
/// sentinel string `"<raw query>"` is used for the `table` field — callers
/// who need better diagnostics should log the `sql` argument themselves or
/// use `query_as` with a wrapper tuple type and inspect the length.
pub async fn query_scalar<'e, T, E, F>(executor: E, sql: &str, bind_fn: F) -> Result<T, DjogiError>
where
    T: for<'r> sqlx::Decode<'r, Postgres> + sqlx::Type<Postgres> + Send + Unpin,
    E: Executor<'e, Database = Postgres>,
    F: for<'q> FnOnce(
        sqlx::query::QueryScalar<'q, Postgres, T, PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, Postgres, T, PgArguments>,
{
    let q = sqlx::query_scalar::<Postgres, T>(sql);
    let q = bind_fn(q);
    q.fetch_optional(executor)
        .await?
        .ok_or(DjogiError::NotFound {
            table: "<raw query>",
        })
}

/// Execute a raw SQL statement without returning rows (INSERT, UPDATE, DELETE, DDL).
pub async fn execute<'e, E, F>(executor: E, sql: &str, bind_fn: F) -> Result<(), DjogiError>
where
    E: Executor<'e, Database = Postgres>,
    F: for<'q> FnOnce(
        sqlx::query::Query<'q, Postgres, PgArguments>,
    ) -> sqlx::query::Query<'q, Postgres, PgArguments>,
{
    let q = sqlx::query(sql);
    let q = bind_fn(q);
    q.execute(executor).await?;
    Ok(())
}
