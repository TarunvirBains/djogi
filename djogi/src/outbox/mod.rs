//! Transactional outbox — (write side) + (worker side).
//! Models annotated with `#[model(events)]` emit one outbox row into
//! `{table}_outbox` on every successful `create`, `save`, `delete`,
//! `update_returning_pair`, or `delete_returning` performed through a
//! [`DjogiContext`]. Because the outbox INSERT shares the same context (and
//! therefore the same active transaction), the outbox row commits or rolls back
//! atomically with the primary write — no separate "event publisher" fan-out is
//! required at the write side. A downstream poller / CDC consumer drains
//! `{table}_outbox` asynchronously.
//! # PG18 OLD/NEW RETURNING and the outbox
//! `Model::update_returning_pair` and `QuerySet::execute_returning_pairs`
//! expose both the pre- and post-update row snapshots to the **caller**, but
//! the outbox **still emits a single `Save` row** with the DB-returned
//! post-image (`pair.new`) as the payload, after `outbox_exclude` filtering.
//! This matches the `save()` outbox contract — no diff-shaped `{ old, new }`
//! payload is emitted in v1.
//! Rationale: the existing outbox schema (`action` + single `payload` column)
//! cannot represent a diff without a breaking schema change. Downstream
//! consumers that currently process `Save` payloads would break if the payload
//! shape changed silently. A diff-outbox schema requires a separate plan
//! (migration, new action type, worker shape change, and publisher review).
//! `Model::delete_returning` similarly emits a single `Delete` row with the
//! DB-returned pre-delete snapshot as the payload — identical to `delete()`
//! but using the authoritative DB snapshot rather than the consumed `self`.
//! # Module layout
//! - `mod.rs` (this file) — write side: `emit_event`, `OutboxAction`, payload shaping.
//!   All exports are unchanged.
//! - `worker` — claim-pending, mark-published/failed, recover-stale. Feature-gated
//!   on the `outbox` cargo feature.
//! - `publisher` — `Publisher` trait + `PublishError` enum. Feature-gated on `outbox`.
//! - `publishers` — concrete publisher implementations. `NotifyPublisher` is always
//!   available under `outbox`; Redis/Kafka/NATS ship behind their own sub-feature flags.
//! # Why this lives here (not inside the macro body)
//! The macro emits a **one-line call** into this module after every
//! successful CRUD:
//! ```ignore
//! if Self::descriptor().has_outbox {
//!     ::djogi::outbox::emit_event(ctx, &row, ::djogi::outbox::OutboxAction::Create).await?;
//! }
//! ```
//! Keeping the payload shaping + dispatch out of the macro has three
//! benefits:
//! 1. **Easier to audit.** Reviewers read plain Rust rather than a
//!    `quote! { ... }` block to understand payload exclusion, bind
//!    layout, and error flow.
//! 2. **One place to evolve.** Later-phase additions (encrypted payload
//!    fields, tenant partitioning, per-action `rationale` columns) land
//!    here without touching the macro.
//! 3. **Unit-testable.** `emit_event` is a plain async function; its
//!    payload-shaping helper is testable without bringing up a proc-macro
//!    harness.
//! # How it composes with `atomic()`
//! The emitter takes `&mut DjogiContext` and dispatches via the same
//! execution helpers the macro CRUD uses. A pool-backed context writes
//! outside any transaction; a transaction-backed context writes inside
//! the outermost atomic scope. Rollback of the outer scope removes both
//! the primary row and its outbox companion — the "transactional" in
//! *transactional outbox* is Postgres' own ACID guarantee, not a
//! framework-level two-phase dance.
//! # Payload filtering
//! `emit_event` serializes the row via `serde_json::to_value` and then
//! walks `T::descriptor().fields` to strip keys whose
//! [`FieldDescriptor::outbox_exclude`] flag is set. The exclusion is
//! key-based (not index-based) so that even if serde's field order
//! drifts, the filter stays correct.
//! # DDL side-channel (handoff — deferred)
//! The eventual plan is for the macro to write a DDL stub to
//! `target/djogi_outbox/{table}_outbox.sql` at build time so the Phase
//! 7 migration differ can consume it. **That emission is not shipped
//! today** — this module ships only the runtime payload/insert path.
//! Integration tests hand-write the matching DDL in
//! `tests/integration/migrations/transactions/*.sql`; a later change will
//! land the macro-side side-channel and retire those hand-written
//! fixtures.

// ---------------------------------------------------------------------------
// Worker / publisher sub-modules — feature-gated on `outbox`.
// ---------------------------------------------------------------------------

#[cfg(feature = "outbox")]
pub mod publisher;
#[cfg(feature = "outbox")]
pub mod publishers;
#[cfg(feature = "outbox")]
pub mod worker;

/// Re-exported for `djogi::outbox::PublishError` and `djogi::outbox::Publisher`
/// convenience paths.
#[cfg(feature = "outbox")]
pub use publisher::{PublishError, Publisher};

/// Re-exported for the `djogi::outbox::OutboxRow` convenience path.
#[cfg(feature = "outbox")]
pub use worker::OutboxRow;

// ---------------------------------------------------------------------------
// Write-side implementation — always compiled.
// ---------------------------------------------------------------------------

use crate::DjogiError;
use crate::context::DjogiContext;
use crate::descriptor::ModelDescriptor;
use crate::model::Model;
use postgres_types::ToSql;
use serde::Serialize;
use std::fmt::Write as _;

// Postgres parameter indices are `u16`-bounded (`$1..$65535`). Bulk save
// outbox inserts use 3 binds per row (`row_id`, `action`, `payload`), so
// cap each statement well below the ceiling.
const BULK_SAVE_INSERT_MAX_BINDS: usize = 30_000;
const BULK_SAVE_INSERT_ROWS_PER_CHUNK: usize = BULK_SAVE_INSERT_MAX_BINDS / 3;

/// The three CRUD operations a model can emit an outbox row for.
/// Stored as a `TEXT` column so downstream consumers (Python, TS, SQL
/// dashboards) do not need Postgres enum-to-enum bindings. Lowercase
/// ASCII to match the informal convention used by CDC tools like
/// Debezium (`c` / `u` / `d` → `create` / `update` / `delete`; we use
/// `save` rather than `update` because the framework's write verb is
/// `save`, not "update").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxAction {
    /// `Model::create` — new row inserted into the primary table.
    Create,
    /// `Model::save` — existing row updated via `UPDATE ... RETURNING *`.
    /// Payload carries the DB-refreshed values (post-trigger, post-
    /// default) because `save` rehydrates the receiver before the
    /// outbox call runs.
    Save,
    /// `Model::delete` — row removed from the primary table. Payload
    /// carries the pre-delete snapshot so downstream consumers can
    /// reconstruct what was removed.
    Delete,
}

impl OutboxAction {
    /// String literal used in the outbox `action` column. Stable — any
    /// rename would be a breaking change for downstream consumers, so
    /// future additions land as new variants rather than renames.
    pub const fn as_sql_str(&self) -> &'static str {
        match self {
            OutboxAction::Create => "create",
            OutboxAction::Save => "save",
            OutboxAction::Delete => "delete",
        }
    }
}

/// Emit a transactional outbox row for `row` through `ctx`.
/// Called from macro-generated CRUD bodies exactly when
/// `T::descriptor().has_outbox` is `true` — i.e. the model opted into
/// `#[model(events)]`. The caller holds the responsibility of gating
/// the call: `emit_event` itself always writes, so it must not be
/// invoked for non-events models.
/// # Error flow
/// - `serde_json::to_value` failure → `DjogiError::Serde`.
/// - Database execute failure → `DjogiError::Pg`.
///   Both are propagated verbatim so the calling CRUD method's `?`
///   rolls the transaction back on any failure.
/// # Payload shape
/// `serde_json::to_value(row)` produces a `Value::Object` keyed by
/// the struct's field names (serde's default `rename_all`). The
/// filter walks `T::descriptor().fields` and removes any key whose
/// `outbox_exclude` flag is set, so the emitted JSONB carries only
/// the columns the model author deemed safe to share. Non-object
/// outputs (e.g. a user who derived `Serialize` for a tuple struct)
/// pass through unfiltered — `outbox_exclude` keys off column names,
/// which only exist on record-shaped payloads.
pub async fn emit_event<T: Model + Serialize>(
    ctx: &mut DjogiContext,
    row: &T,
    action: OutboxAction,
) -> Result<(), DjogiError>
where
    T::Pk: std::fmt::Display,
{
    let payload = build_payload(row, T::descriptor())?;
    // Validate the **derived** outbox table identifier (not the source
    // table alone). The 7-byte `_outbox` suffix can push an otherwise-
    // valid 57-63 byte table name past Postgres's 63-byte identifier
    // limit. Matches the defense-in-depth check in
    // `query::refresh::fetch_delta` and `outbox::worker::validate_table_ident`.
    let outbox_table = crate::migrate::naming::outbox_table_name(T::table_name());
    crate::ident::check_plain_ident(&outbox_table, false).map_err(|e| {
        DjogiError::Db(crate::error::DbError::other(format!(
            "outbox emit_event: invalid outbox table name {outbox_table:?}: {e:?}"
        )))
    })?;
    let sql = insert_sql(&outbox_table);
    let action_str = action.as_sql_str();

    let params: &[&(dyn ToSql + Sync)] = &[row.pk_value(), &action_str, &payload];
    ctx.execute(&sql, params).await?;

    // Synchronous in-process publisher hook for `crate::notify`
    // subscribers — fires `pg_notify('djogi_<table>', '{kind,id}')`
    // in the same transaction. Distinct from the
    // `NotifyPublisher`, which carries full JSONB payloads on
    // user-named channels for downstream relay. The `check_plain_ident`
    // pass guards against a malicious macro fork smuggling an invalid
    // table name through the SQL embedding.
    #[cfg(feature = "notify")]
    {
        let table = T::table_name();
        let channel = format!("djogi_{table}");
        // Validate the **derived channel** (not just the table). The
        // 6-byte `djogi_` prefix can push an otherwise-valid 58-63 byte
        // table name past Postgres's 63-byte identifier limit, which
        // matches the subscriber-side validation in `crate::notify`.
        crate::ident::check_plain_ident(&channel, false).map_err(|e| {
            DjogiError::Db(crate::error::DbError::other(format!(
                "notify hook: invalid channel name {channel:?}: {e:?}"
            )))
        })?;
        let id_str = format!("{}", row.pk_value());
        let notify_payload = build_notify_payload(action, &id_str);
        ctx.execute(
            "SELECT pg_notify($1, $2)",
            &[&channel.as_str(), &notify_payload.as_str()],
        )
        .await?;
    }

    Ok(())
}

/// Emit one `Save` outbox row per entry in `rows` using a single batched
/// `INSERT ... VALUES (...), (...), ...` statement.
/// This is used by bulk `execute_returning_pairs` to preserve per-row outbox
/// semantics without issuing one INSERT round-trip per updated row.
pub async fn emit_save_events_batch<T: Model + Serialize>(
    ctx: &mut DjogiContext,
    rows: &[&T],
) -> Result<(), DjogiError>
where
    T::Pk: std::fmt::Display,
{
    if rows.is_empty() {
        return Ok(());
    }

    let outbox_table = crate::migrate::naming::outbox_table_name(T::table_name());
    crate::ident::check_plain_ident(&outbox_table, false).map_err(|e| {
        DjogiError::Db(crate::error::DbError::other(format!(
            "outbox emit_save_events_batch: invalid outbox table name {outbox_table:?}: {e:?}"
        )))
    })?;

    let mut payloads = Vec::with_capacity(rows.len());
    for row in rows {
        payloads.push(build_payload(row, T::descriptor())?);
    }

    let action = OutboxAction::Save.as_sql_str();
    for (chunk_idx, chunk) in rows
        .chunks(BULK_SAVE_INSERT_ROWS_PER_CHUNK.max(1))
        .enumerate()
    {
        let mut sql = format!("INSERT INTO {outbox_table} (row_id, action, payload) VALUES ");
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(chunk.len() * 3);
        let payload_base = chunk_idx * BULK_SAVE_INSERT_ROWS_PER_CHUNK.max(1);

        for (i, row) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let base = i * 3;
            let _ = write!(sql, "(${}, ${}, ${})", base + 1, base + 2, base + 3);
            params.push(row.pk_value());
            params.push(&action);
            params.push(&payloads[payload_base + i]);
        }

        ctx.execute(&sql, &params).await?;
    }

    #[cfg(feature = "notify")]
    {
        let table = T::table_name();
        let channel = format!("djogi_{table}");
        crate::ident::check_plain_ident(&channel, false).map_err(|e| {
            DjogiError::Db(crate::error::DbError::other(format!(
                "notify hook: invalid channel name {channel:?}: {e:?}"
            )))
        })?;
        for row in rows {
            let id_str = format!("{}", row.pk_value());
            let notify_payload = build_notify_payload(OutboxAction::Save, &id_str);
            ctx.execute(
                "SELECT pg_notify($1, $2)",
                &[&channel.as_str(), &notify_payload.as_str()],
            )
            .await?;
        }
    }

    Ok(())
}

/// Build the slim `{kind, id}` JSON payload for the notify hook.
/// `serde_json::json!` (not `format!`) so a future PK type whose
/// `Display` produces double-quotes or backslashes still emits valid
/// JSON.
#[cfg(feature = "notify")]
pub(crate) fn build_notify_payload(action: OutboxAction, id_str: &str) -> String {
    serde_json::json!({
        "kind": action.as_sql_str(),
        "id": id_str,
    })
    .to_string()
}

/// Build the outbox `INSERT` SQL for a given outbox table. Caller is
/// responsible for validating the identifier via `check_plain_ident`
/// before passing it in (the SQL embeds the name unquoted).
fn insert_sql(outbox_table: &str) -> String {
    format!("INSERT INTO {outbox_table} (row_id, action, payload) VALUES ($1, $2, $3)")
}

/// Serialize `row` into a JSON object, then strip any keys the
/// descriptor marks `outbox_exclude`.
/// Separated from `emit_event` so the filter semantics are unit-
/// testable without a database. Accepts `&ModelDescriptor` rather
/// than re-fetching via `T::descriptor()` so the test callers can
/// pass a minimal fixture descriptor.
fn build_payload<T: Serialize>(
    row: &T,
    descriptor: &ModelDescriptor,
) -> Result<serde_json::Value, DjogiError> {
    let mut value = serde_json::to_value(row)?;
    if let serde_json::Value::Object(ref mut map) = value {
        for field in descriptor.fields {
            if field.outbox_exclude {
                map.remove(field.name);
            }
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    //! Payload-shaping tests — `emit_event` itself needs a live DB and
    //! is covered by the integration suite. The filter logic
    //! is pure and lives here.

    use super::*;
    use crate::descriptor::{
        FieldDescriptor, FieldSqlType, ModelDescriptor, PkType, field_descriptor, model_descriptor,
    };

    fn minimal_descriptor_with_exclusions(exclusions: &'static [&'static str]) -> ModelDescriptor {
        // Build a descriptor whose fields match the serde output of the
        // test payload. Columns listed in `exclusions` are flagged
        // `outbox_exclude = true`; all others are `false`. A `&[]`
        // `exclusions` slice yields a descriptor with no exclusions
        // the identity case.
        // We use static `FieldDescriptor` slices so the returned
        // `ModelDescriptor` can borrow them with `'static` lifetimes.
        static FIELDS: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("keep", FieldSqlType::Text, false)
            },
            FieldDescriptor {
                outbox_exclude: true,
                ..field_descriptor("secret", FieldSqlType::Text, false)
            },
        ];

        // The test helper caller hands in `&["secret"]` to match the
        // static above, or `&[]` to bypass exclusions. Rather than
        // building the descriptor dynamically (which would require
        // leaking memory or fighting the 'static bound), we return
        // the pre-baked slice when the caller asked for the "secret"
        // exclusion and a separate non-excluding slice otherwise.
        static FIELDS_NO_EXCLUDE: &[FieldDescriptor] = &[
            FieldDescriptor {
                ..field_descriptor("keep", FieldSqlType::Text, false)
            },
            FieldDescriptor {
                ..field_descriptor("secret", FieldSqlType::Text, false)
            },
        ];

        let fields = if exclusions.contains(&"secret") {
            FIELDS
        } else {
            FIELDS_NO_EXCLUDE
        };

        ModelDescriptor {
            has_outbox: true,
            ..model_descriptor("TestRow", "test_rows", PkType::HeerId, fields)
        }
    }

    #[derive(Serialize)]
    struct TestRow {
        keep: &'static str,
        secret: &'static str,
    }

    #[test]
    fn build_payload_strips_excluded_fields() {
        let row = TestRow {
            keep: "public",
            secret: "confidential",
        };
        let descriptor = minimal_descriptor_with_exclusions(&["secret"]);
        let payload = build_payload(&row, &descriptor).unwrap();

        let obj = payload.as_object().expect("payload must be an object");
        assert!(obj.contains_key("keep"));
        assert!(
            !obj.contains_key("secret"),
            "outbox_exclude field must be stripped, got: {obj:?}"
        );
        assert_eq!(obj["keep"], serde_json::Value::String("public".into()));
    }

    #[test]
    fn build_payload_preserves_all_fields_when_no_exclusions() {
        let row = TestRow {
            keep: "a",
            secret: "b",
        };
        let descriptor = minimal_descriptor_with_exclusions(&[]);
        let payload = build_payload(&row, &descriptor).unwrap();

        let obj = payload.as_object().expect("payload must be an object");
        assert!(obj.contains_key("keep"));
        assert!(obj.contains_key("secret"));
    }

    #[test]
    fn outbox_action_as_sql_str_matches_spec() {
        assert_eq!(OutboxAction::Create.as_sql_str(), "create");
        assert_eq!(OutboxAction::Save.as_sql_str(), "save");
        assert_eq!(OutboxAction::Delete.as_sql_str(), "delete");
    }

    #[test]
    fn insert_sql_uses_caller_supplied_table() {
        // Caller is now responsible for `_outbox` suffix construction +
        // ident validation; `insert_sql` is a pure formatter.
        let sql = insert_sql("accounts_outbox");
        assert!(sql.contains("accounts_outbox"));
        assert!(sql.contains("row_id"));
        assert!(sql.contains("action"));
        assert!(sql.contains("payload"));
        assert!(sql.contains("$1"));
        assert!(sql.contains("$2"));
        assert!(sql.contains("$3"));
    }

    // build_notify_payload byte-shape tests. The receiver's decoder
    // depends on `kind` being a known enum variant and `id` being a
    // string, so a regression that widened the payload or reordered
    // keys would fail here.
    #[cfg(feature = "notify")]
    #[test]
    fn notify_payload_shape_create_byte_exact() {
        let payload = build_notify_payload(OutboxAction::Create, "12345");
        assert_eq!(payload, r#"{"kind":"create","id":"12345"}"#);
    }

    #[cfg(feature = "notify")]
    #[test]
    fn notify_payload_shape_save_byte_exact() {
        let payload = build_notify_payload(OutboxAction::Save, "67890");
        assert_eq!(payload, r#"{"kind":"save","id":"67890"}"#);
    }

    #[cfg(feature = "notify")]
    #[test]
    fn notify_payload_shape_delete_byte_exact() {
        let payload = build_notify_payload(OutboxAction::Delete, "abcdef-ghi");
        assert_eq!(payload, r#"{"kind":"delete","id":"abcdef-ghi"}"#);
    }

    #[cfg(feature = "notify")]
    #[test]
    fn notify_payload_round_trips_via_serde() {
        let payload = build_notify_payload(OutboxAction::Save, "99");
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("payload must be valid JSON");
        assert_eq!(parsed["kind"], "save");
        assert_eq!(parsed["id"], "99");
    }

    #[cfg(feature = "notify")]
    #[test]
    fn notify_payload_escapes_special_chars_in_id() {
        // Defensive: PK types whose Display output contains double
        // quotes or backslashes must still produce valid JSON.
        // serde_json::json! handles this; raw format! would not.
        let payload = build_notify_payload(OutboxAction::Save, r#"id"with\quotes"#);
        let parsed: serde_json::Value =
            serde_json::from_str(&payload).expect("escaped payload must round-trip");
        assert_eq!(parsed["id"], r#"id"with\quotes"#);
    }
}
