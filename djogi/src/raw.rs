//! Raw SQL escape hatch for queries beyond the `QuerySet` surface.
//!
//! All three functions take `&mut DjogiContext`, matching every other CRUD entry
//! point in the framework post-Phase-4-retrofit. Internally each function
//! pattern-matches on the context's inner variant and dispatches to the sqlx
//! `Executor` for that variant. Callers who need raw sqlx access (e.g. to build
//! their own `QueryBuilder`) can still reach through to `ctx.pool()` or `ctx.tx()`
//! for direct handles.
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
//! let mut ctx = DjogiContext::from_pool(pool.clone());
//!
//! let published: Vec<Post> = djogi::raw::query_as(
//!     &mut ctx,
//!     "SELECT * FROM posts WHERE published = $1",
//!     |q| q.bind(true),
//! ).await?;
//!
//! let count: i64 = djogi::raw::query_scalar(
//!     &mut ctx,
//!     "SELECT COUNT(*) FROM posts WHERE view_count > $1",
//!     |q| q.bind(100i32),
//! ).await?;
//!
//! djogi::raw::execute(
//!     &mut ctx,
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
use crate::context::{ContextInner, DjogiContext};
use sqlx::postgres::PgArguments;
use sqlx::{FromRow, Postgres};

/// Execute a raw SQL query and return a `Vec<T>` via `FromRow`.
///
/// Returns an empty `Vec` if the query matches zero rows — this is the
/// natural `fetch_all` behaviour, not an error.
pub async fn query_as<T, F>(
    ctx: &mut DjogiContext,
    sql: &str,
    bind_fn: F,
) -> Result<Vec<T>, DjogiError>
where
    T: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
    F: for<'q> FnOnce(
        sqlx::query::QueryAs<'q, Postgres, T, PgArguments>,
    ) -> sqlx::query::QueryAs<'q, Postgres, T, PgArguments>,
{
    let q = sqlx::query_as::<Postgres, T>(sql);
    let q = bind_fn(q);
    let rows = match ctx.inner_mut() {
        ContextInner::Pool(pool) => q.fetch_all(&*pool).await?,
        ContextInner::Transaction(tx) => q.fetch_all(&mut **tx).await?,
    };
    Ok(rows)
}

/// Execute a raw SQL query and return a single scalar value.
///
/// Returns `DjogiError::NotFound { table: "<raw query>" }` if the query
/// produces zero rows. The raw API has no inherent table context, so the
/// sentinel string `"<raw query>"` is used for the `table` field — callers
/// who need better diagnostics should log the `sql` argument themselves or
/// use `query_as` with a wrapper tuple type and inspect the length.
pub async fn query_scalar<T, F>(
    ctx: &mut DjogiContext,
    sql: &str,
    bind_fn: F,
) -> Result<T, DjogiError>
where
    T: for<'r> sqlx::Decode<'r, Postgres> + sqlx::Type<Postgres> + Send + Unpin,
    F: for<'q> FnOnce(
        sqlx::query::QueryScalar<'q, Postgres, T, PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, Postgres, T, PgArguments>,
{
    let q = sqlx::query_scalar::<Postgres, T>(sql);
    let q = bind_fn(q);
    let opt = match ctx.inner_mut() {
        ContextInner::Pool(pool) => q.fetch_optional(&*pool).await?,
        ContextInner::Transaction(tx) => q.fetch_optional(&mut **tx).await?,
    };
    opt.ok_or_else(|| DjogiError::not_found("<raw query>"))
}

/// Execute a raw SQL statement without returning rows (INSERT, UPDATE, DELETE, DDL).
pub async fn execute<F>(ctx: &mut DjogiContext, sql: &str, bind_fn: F) -> Result<(), DjogiError>
where
    F: for<'q> FnOnce(
        sqlx::query::Query<'q, Postgres, PgArguments>,
    ) -> sqlx::query::Query<'q, Postgres, PgArguments>,
{
    let q = sqlx::query(sql);
    let q = bind_fn(q);
    match ctx.inner_mut() {
        ContextInner::Pool(pool) => {
            q.execute(&*pool).await?;
        }
        ContextInner::Transaction(tx) => {
            q.execute(&mut **tx).await?;
        }
    }
    Ok(())
}
