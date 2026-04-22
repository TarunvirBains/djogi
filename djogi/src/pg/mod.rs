//! Internal Postgres substrate — `SqlAccumulator`, `DjogiPool`, and `PgConnection`.
//!
//! This module is `pub(crate)` — it is an implementation detail of the framework,
//! not part of the public API. Macro-emitted code routes through `::djogi::__private::pg`
//! (re-exported from `lib.rs`), not through `::djogi::pg` directly.
//!
//! # Module layout
//!
//! | Submodule         | Contents                                                         |
//! |-------------------|------------------------------------------------------------------|
//! | `accumulator`     | `SqlAccumulator` — typed SQL builder with positional `$n` binds |
//! | `pool`            | `DjogiPool` wrapping `deadpool_postgres::Pool`                  |
//! | `connection`      | `PgConnection` wrapping `deadpool_postgres::Object` + stmt cache |

pub mod accumulator;
pub mod connection;
pub mod cursor;
pub mod decode;
pub mod pool;
