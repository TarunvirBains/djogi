//! Researcher — per-organization field staff.
//!
//! ## What this demonstrates
//!
//! - `tenant_key = "org_id"` — declares Postgres-native row-level security
//! intent at the macro layer. The descriptor carries `rls_enabled = true`
//! and `tenant_key = "org_id"` through the projection; `ALTER TABLE …
//! ENABLE ROW LEVEL SECURITY` and `CREATE POLICY` DDL emission is tracked
//! for. Once DDL lands, `ctx.set_tenant(org_id)` inside
//! an `atomic()` scope will activate the policy for the current
//! transaction.
//! - Model-level `fts(source = "notes", dictionary = "english")` — Djogi
//! only supports model-level FTS specs (per `docs/spec/decisions.md`).
//! The macro emits an `FtsDescriptor`, the `search` GENERATED tsvector
//! column, a GIN index, and a `ResearcherFields::search()` typed
//! accessor for `@@` predicates.
//! - `Tracked<String>` on `name` — declares field-change tracking. Audit
//! `_logs` mirror-table wiring and `audit_pool` configuration are out of
//! scope for this example; the `Tracked` annotation is present to show
//! the macro surface.
//!
//! `org_id` is a plain `i64` rather than a foreign key — the example
//! does not ship an `Organization` model. Real apps would point at one.

use djogi::prelude::*;

#[model(
 table = "researchers",
 pk = HeerId,
 tenant_key = "org_id",
 fts(source = "notes", dictionary = "english"),
)]
#[derive(Debug, Clone)]
pub struct Researcher {
    /// Tenant scope. The `tenant_key = "org_id"` annotation declares RLS
    /// intent at the macro layer; DDL emission is a future target.
    pub org_id: i64,

    pub name: Tracked<String>,

    pub email: String,

    /// Long-form field notes. Concatenated into the model-level `search`
    /// tsvector by the FTS configuration above.
    pub notes: String,
}
