//! Phase 7-Zero-2 T10 — visage queryset entry (`{Visage}::filter`).
//!
//! Probes the closure-based filter entry point on a scalar-only visage:
//! `XPublic::filter(|f| f.name().eq(...))` typechecks and produces a
//! `VisageQuerySet<XPublic>` whose `.fetch_all(...)` returns
//! `Vec<XPublic>`.
//!
//! The fixture pins the method signature via function coercion so the
//! compile-pass test does not need a live Postgres pool — only the
//! types must align.

use djogi::prelude::*;
use djogi::query::internal::Condition;

#[model(table = "phase7_zero2_t10_xs")]
#[derive(Debug, Clone)]
pub struct X {
    #[field(expose(public))]
    pub name: String,
    // Non-exposed column — must NOT appear in `XPublic`'s narrowed
    // surface. This is the structural enforcement T10 ships.
    pub secret: String,
}

// Closure-based filter entry must coerce into a
// `VisageQuerySet<XPublic>` builder.
#[allow(dead_code)]
fn _filter_entry_returns_visage_queryset() -> djogi::query::VisageQuerySet<XPublic> {
    XPublic::filter(|f| f.name().eq("Ada".to_string()))
}

// Vacuous-true filter is the simplest valid entry — pins the closure
// signature accepts a `_` placeholder when no predicate is needed.
#[allow(dead_code)]
fn _vacuous_filter_compiles() -> djogi::query::VisageQuerySet<XPublic> {
    XPublic::filter(|_| Condition::True)
}

// Builder methods (`order_by`, `limit`, `offset`) are top-level entries
// on the visage too — they fan out into a fresh `VisageQuerySet`.
#[allow(dead_code)]
fn _order_by_entry() -> djogi::query::VisageQuerySet<XPublic> {
    XPublic::order_by(|f| f.name().asc())
}

#[allow(dead_code)]
fn _limit_offset_entries() -> djogi::query::VisageQuerySet<XPublic> {
    XPublic::limit(10).offset(5)
}

// Terminal `fetch_all` resolves to `Vec<XPublic>`. The function-coerced
// signature is the load-bearing typecheck — it proves both that
// `fetch_all` exists on `VisageQuerySet<XPublic>` and that the
// resulting future yields the visage type, not the source model.
#[allow(dead_code)]
fn _fetch_all_returns_vec_visage<'a>(
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<Vec<XPublic>, DjogiError>> + Send + 'a {
    XPublic::filter(|_| Condition::True).fetch_all(ctx)
}

// Aggregate terminals do not require `FromPgRow`; pin their signatures.
#[allow(dead_code)]
fn _count_returns_i64<'a>(
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<i64, DjogiError>> + Send + 'a {
    XPublic::filter(|_| Condition::True).count(ctx)
}

#[allow(dead_code)]
fn _exists_returns_bool<'a>(
    ctx: &'a mut DjogiContext,
) -> impl std::future::Future<Output = Result<bool, DjogiError>> + Send + 'a {
    XPublic::filter(|_| Condition::True).exists(ctx)
}

fn main() {}
