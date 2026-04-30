//! Researcher — per-organization field staff.
//!
//! Demonstrates:
//! - `tenant_key = "org_id"` — Postgres-native row-level security.
//!   Researchers are scoped per organization; setting `app.tenant_id`
//!   in the session pins all `Researcher` queries to one org.
//! - FTS index on `notes` (long-form column) — `notes_search` virtual
//!   field exposes `tsvector` search via `Researcher::objects().search(...)`.
//! - `Tracked` on the `name` field — every change writes a row in the
//!   structural-CRUD audit log without explicit calls.
//!
//! `org_id` is a plain `i64` rather than an FK — the example doesn't
//! ship an `Organization` model. Real apps would point at one.

use djogi::prelude::*;

#[model(table = "researchers", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct Researcher {
    /// Tenant scope. RLS uses `current_setting('app.tenant_id')::bigint`.
    pub org_id: i64,

    pub name: Tracked<String>,

    pub email: String,

    /// Long-form field notes. FTS-indexed; the macro emits a generated
    /// `tsvector` column and a GIN index.
    #[field(fts = "english")]
    pub notes: String,
}
