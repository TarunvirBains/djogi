//! Raw SQL escape hatch for queries beyond the `QuerySet` surface.
//!
//! All three functions take `&mut DjogiContext`, matching every other CRUD entry
//! point in the framework post-Phase-4-retrofit.
//!
//! # T2 → T5 placeholder
//!
//! Phase 5-Zero T2 removes the sqlx executor model from `DjogiContext`. The `raw`
//! module's implementation requires the full tokio-postgres execution path that T5
//! will build (typed bind arrays via `postgres_types::ToSql`, row decode via
//! `__from_pg_row`, and the cleaned-up `PgConnection` dispatch helpers). Stubbing
//! the bodies here prevents T2's compiler from seeing sqlx executor calls after
//! the pool/transaction types have switched, without forcing T5's scope into T2.
//!
//! T5 will replace every `todo!()` body with the real tokio-postgres implementation.
//!
//! # Future API shape
//!
//! The three entry points remain — same names, same `&mut DjogiContext` receiver,
//! same return types. Only the internals (bind mechanism, row decode) change in T5.

use crate::DjogiError;
use crate::context::DjogiContext;

/// Execute a raw SQL query and return a `Vec<T>` via row decode.
///
/// # T2 placeholder
///
/// This method is a stub. T5 replaces the body with the tokio-postgres
/// implementation.
pub async fn query_as<T, F>(
    _ctx: &mut DjogiContext,
    _sql: &str,
    _bind_fn: F,
) -> Result<Vec<T>, DjogiError>
where
    T: Send + 'static,
    F: Send + 'static,
{
    todo!(
        "djogi::raw::query_as is not yet implemented on the tokio-postgres substrate; \
         T5 (Phase 5-Zero) will replace this body. Use QuerySet terminals for now."
    )
}

/// Execute a raw SQL query and return a single scalar value.
///
/// # T2 placeholder
///
/// This method is a stub. T5 replaces the body with the tokio-postgres
/// implementation.
pub async fn query_scalar<T, F>(
    _ctx: &mut DjogiContext,
    _sql: &str,
    _bind_fn: F,
) -> Result<T, DjogiError>
where
    T: Send + 'static,
    F: Send + 'static,
{
    todo!(
        "djogi::raw::query_scalar is not yet implemented on the tokio-postgres substrate; \
         T5 (Phase 5-Zero) will replace this body. Use QuerySet terminals for now."
    )
}

/// Execute a raw SQL statement without returning rows (INSERT, UPDATE, DELETE, DDL).
///
/// # T2 placeholder
///
/// This method is a stub. T5 replaces the body with the tokio-postgres
/// implementation.
pub async fn execute<F>(_ctx: &mut DjogiContext, _sql: &str, _bind_fn: F) -> Result<(), DjogiError>
where
    F: Send + 'static,
{
    todo!(
        "djogi::raw::execute is not yet implemented on the tokio-postgres substrate; \
         T5 (Phase 5-Zero) will replace this body. Use QuerySet terminals or \
         DjogiContext::execute for now."
    )
}
