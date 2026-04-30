//! Researcher — per-organization field staff.
//!
//! ## What this demonstrates
//!
//! - `tenant_key = "org_id"` — Postgres-native row-level security. The
//!   migration differ emits an `ALTER TABLE ... ENABLE ROW LEVEL SECURITY`
//!   statement plus a `CREATE POLICY` filtering on
//!   `current_setting('app.tenant_id')::bigint`. Inside an `atomic()`
//!   scope, `ctx.set_tenant(org_id)` activates the policy for the current
//!   transaction.
//! - Model-level `fts(source = "notes", dictionary = "english")` — Djogi
//!   only supports model-level FTS specs (per `docs/spec/decisions.md`).
//!   The macro emits an `FtsDescriptor`, the `search` GENERATED tsvector
//!   column, a GIN index, and a `ResearcherFields::search()` typed
//!   accessor for `@@` predicates.
//! - `Tracked<String>` on `name` — name changes write a row in the
//!   structural CRUD audit log without explicit calls.
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
    /// Tenant scope. The RLS policy filters every `Researcher` row by
    /// `org_id = current_setting('app.tenant_id')::bigint`.
    pub org_id: i64,

    pub name: Tracked<String>,

    pub email: String,

    /// Long-form field notes. Concatenated into the model-level `search`
    /// tsvector by the FTS configuration above.
    pub notes: String,
}
