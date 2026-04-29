//! Online-safety classification engine — Phase 7.5 T5.
//!
//! Walks a [`SchemaOperation`] (or a delta-worth of them) and assigns
//! each one an [`OnlineSafetyClassification`] verdict per the §7
//! classification table in
//! `docs/superpowers/plans/2026-04-23-phase7-5-live-migrations-and-protected-data-v3.md`.
//!
//! # Boundary contract (§6.5)
//!
//! - **PK-flip routing is exclusive.** When a delta carries
//!   [`SchemaOperation::PkTypeFlipGroup`] or
//!   [`SchemaOperation::PkTypeFlipMultiGroup`], that operation is
//!   already routed through Phase 7's
//!   [`crate::migrate::diff::Classification::PkTypeFlip`] cascade
//!   emitter family (`migrate::pk_flip`). The classifier short-circuits
//!   those entries — they appear in [`classify_delta`]'s output as
//!   skipped (filtered out) so live-plan callers never see them.
//! - **Logging-profile short-circuit.** Per §6.5 of the v3 plan, the
//!   classifier inspects [`ClassifyContext::logging_profile`] and the
//!   [`ClassifyContext::target_database`] field to decide whether to
//!   route through Phase 7.5 at all. Event-log databases never live-
//!   plan; crud-log databases under `light` / `balanced` never live-
//!   plan; only `strict_audit` crud-log + the application database
//!   reach the per-operation classifier.
//!
//! # Determinism
//!
//! Classification is pure: same inputs → same output, no `pg_catalog`
//! reads, no host-variable behaviour. Every dispatch arm cites the
//! §7 table row it maps to in a doc comment so the table stays
//! reviewable against this code.
//!
//! # Aggregation
//!
//! [`classify_delta`] performs the cross-operation aggregation §7
//! requires:
//!
//! - Per-table `AddForeignKey` counts — 4+ FK additions to a single
//!   table (configurable via [`ClassifyContext::multi_fk_threshold`])
//!   escalate every entry on that table to `ExpandContract`.
//! - Inbound FK counts on a `DropTable` — when 4+ existing FKs
//!   reference the table being dropped, the drop escalates to
//!   `ExpandContract` (multi-step DROP CONSTRAINT staging). Inbound
//!   FK counts must be supplied via [`ClassifyContext::inbound_fk_counts`]
//!   because the drop op alone does not carry the foreign-key graph;
//!   compose passes the count from the live snapshot.

use crate::descriptor::DefaultVolatility;
use crate::live_migrate::LoggingProfile;
use crate::migrate::diff::{ColumnChange, SchemaOperation};
use crate::migrate::pg_volatility::{Volatility, classify_default_expression};
use crate::migrate::schema::{
    ColumnSchema, IndexSchema, IndexTargetSchema, OnlineSafetyClassification,
};
use std::collections::BTreeMap;

/// Ambient context the classifier consults that is not carried by a
/// single [`SchemaOperation`].
///
/// Constructed by the compose pipeline once per `(database, app)`
/// bucket and threaded into every classification call so the same
/// configuration drives every operation in the delta.
#[derive(Debug, Clone)]
pub struct ClassifyContext<'a> {
    /// Approximate row count of the operation's target table.
    /// `None` when the count is unknown — the classifier
    /// conservatively treats `None` as "above threshold" (slower
    /// path is safer).
    pub estimated_rows: Option<u64>,

    /// Threshold above which CHECK / NOT NULL / FK validation is
    /// staged via `NOT VALID` + separate `VALIDATE`. Default
    /// `100_000`; sourced from `Djogi.toml` `[live]
    /// validation_threshold_rows`.
    pub validation_threshold_rows: u64,

    /// Threshold for multi-FK staging — adding this many or more FKs
    /// to a single table in one delta escalates each addition to
    /// `ExpandContract`. Default `4`; sourced from `Djogi.toml`
    /// `[live] multi_fk_threshold`.
    pub multi_fk_threshold: u32,

    /// Logging profile in scope for the bucket being classified.
    /// Drives the §6.5 three-DB short-circuit:
    ///
    /// - Event-log database — never live-plans regardless of profile;
    ///   the classifier reports every operation as `OnlineSafe` so
    ///   compose routes the delta directly through Phase 7.
    /// - Crud-log database under [`LoggingProfile::Light`] /
    ///   [`LoggingProfile::Balanced`] — same direct route; brief
    ///   `AccessExclusiveLock` windows on crud-log mirror tables are
    ///   acceptable because the audit contract degrades gracefully.
    /// - Crud-log database under [`LoggingProfile::StrictAudit`] —
    ///   fail-closed semantics make audit-table locks block
    ///   application writes, so populated crud-log mirrors classify
    ///   the same way as the application database.
    /// - Application database — full classifier walk regardless of
    ///   profile.
    pub logging_profile: LoggingProfile,

    /// Which of the three databases the delta targets. Drives the
    /// §6.5 short-circuit alongside `logging_profile`.
    pub target_database: TargetDatabase,

    /// Inbound FK counts keyed by table name — used for the
    /// `DropTable` aggregation (4+ inbound FKs escalate the drop to
    /// `ExpandContract`). Populated by compose from the snapshot's
    /// FK graph; empty maps disable the escalation.
    pub inbound_fk_counts: &'a BTreeMap<String, u32>,

    /// Adopter override per `#[field(default_volatility = "stable")]`
    /// for known-safe UDFs that the static `pg_volatility.rs` table
    /// cannot classify. Keyed by `(table_name, column_name)`. When an
    /// entry is present for an `AddColumn` op, the classifier consults
    /// the override before falling through to the static volatility
    /// table — letting adopters fast-path columns whose default
    /// expression Djogi could not classify deterministically.
    ///
    /// Spec: §3 / §820 of the Phase 7.5 v3 plan. Populated by compose
    /// from `FieldDescriptor::default_volatility_override` (T3, PR 1).
    pub default_volatility_overrides: &'a BTreeMap<(String, String), DefaultVolatility>,
}

impl<'a> ClassifyContext<'a> {
    /// Reasonable defaults for testing / inline construction. Production
    /// callers populate every field from `Djogi.toml`.
    pub fn application_default(
        inbound_fk_counts: &'a BTreeMap<String, u32>,
        default_volatility_overrides: &'a BTreeMap<(String, String), DefaultVolatility>,
    ) -> Self {
        Self {
            estimated_rows: None,
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::Application,
            inbound_fk_counts,
            default_volatility_overrides,
        }
    }
}

/// Which of the three Djogi databases the delta is targeting. Phase 7
/// keeps three connection pools (application data, CRUD audit log,
/// event log) and the classifier's §6.5 short-circuit varies per pool.
///
/// `#[non_exhaustive]` so future targets (e.g. a separate vector-store
/// database) can be added without breaking downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TargetDatabase {
    /// Adopter's application data — full classifier walk applies.
    Application,
    /// Per-model `_logs` mirror tables. Behaviour varies by
    /// [`LoggingProfile`] — see [`ClassifyContext::logging_profile`].
    CrudLog,
    /// `tracing`-driven event log. Best-effort under every built-in
    /// profile; never live-plans.
    EventLog,
}

/// Classify a single [`SchemaOperation`] against the §7 table.
///
/// **Pre-condition.** The caller has already filtered out
/// [`SchemaOperation::PkTypeFlipGroup`] / `PkTypeFlipMultiGroup`
/// operations — those are routed through Phase 7's `pk_flip`
/// emitter family and never reach this classifier per the §6.5
/// boundary contract. The function still has match arms for those
/// variants (returning [`OnlineSafetyClassification::OfflineOnly`]
/// so a misuse caller gets a refused-classification verdict — see
/// the dispatch arm), but production callers go through
/// [`classify_delta`] which performs the filtering as part of its
/// walk.
pub fn classify_operation(
    op: &SchemaOperation,
    ctx: &ClassifyContext<'_>,
) -> OnlineSafetyClassification {
    if is_pk_type_flip_operation(op) {
        return OnlineSafetyClassification::OfflineOnly;
    }

    // §6.5 short-circuit. Event-log target never live-plans; crud-log
    // under non-strict profiles never live-plans either. The classifier
    // returns `OnlineSafe` so the runner applies the operation
    // directly via Phase 7's regular path.
    if !classifier_applies(ctx) {
        return OnlineSafetyClassification::OnlineSafe;
    }

    match op {
        // §7: "Add nullable column" → OnlineSafe (no backfill, no
        // lock window beyond the catalog touch). Non-nullable columns
        // dispatch through default-expression analysis.
        SchemaOperation::AddColumn { table, column } => classify_add_column(table, column, ctx),

        // §7: "Drop column" → FastLockDestructiveGuarded (corrected
        // per Codex P1-01 — destroys data + invalidates dependents).
        SchemaOperation::DropColumn { .. } => {
            OnlineSafetyClassification::FastLockDestructiveGuarded
        }

        // §7: "Rename column (with #[field(renamed_from = ...)])" →
        // OnlineSafe (catalog-only).
        SchemaOperation::RenameColumn { .. } => OnlineSafetyClassification::OnlineSafe,

        // §7: column-type changes — heuristic walk.
        SchemaOperation::AlterColumn { change, .. } => classify_column_change(change, ctx),

        // §7: "Add FK" → OnlineSafe when below threshold (NOT VALID +
        // VALIDATE single statement); otherwise ExpandContract.
        // Multi-FK aggregation is handled by classify_delta — this
        // entry-point classifies a single FK without aggregation.
        SchemaOperation::AddForeignKey { .. } => classify_fk_addition(ctx),

        // §7: dropping an FK is a constraint removal — OnlineSafe
        // (catalog-only; no data loss in the FK column itself).
        SchemaOperation::DropForeignKey { .. } => OnlineSafetyClassification::OnlineSafe,

        // §7: "Add index" — concurrently=true → OnlineSafe; otherwise
        // ExpandContract. The `requires_out_of_transaction` flag on
        // IndexSchema mirrors the `concurrently = true` model knob.
        SchemaOperation::AddIndex(index) => classify_index_addition(index),

        // §7: "Drop index" — catalog-only; OnlineSafe regardless of
        // concurrent flag because Postgres' DROP INDEX is fast and
        // does not lock the table heavily. (DROP INDEX CONCURRENTLY
        // exists for replication-lag concerns but is not classified
        // distinctly here.)
        SchemaOperation::DropIndex(_) => OnlineSafetyClassification::OnlineSafe,

        // §7: "Add table" — pure additive; OnlineSafe.
        SchemaOperation::AddTable(_) => OnlineSafetyClassification::OnlineSafe,

        // §7: "Drop table" with 4+ inbound FKs → ExpandContract;
        // otherwise FastLockDestructiveGuarded. The aggregation walks
        // the bucket-level inbound counts, so single-table classification
        // returns the conservative "drop is destructive" verdict.
        SchemaOperation::DropTable(table) => classify_drop_table(table, ctx),

        // §7: "Rename table" → OnlineSafe.
        SchemaOperation::RenameTable { .. } => OnlineSafetyClassification::OnlineSafe,

        // §7: "Enum rewrite (add-value)" → OnlineSafe.
        SchemaOperation::AddEnum(_) | SchemaOperation::AddEnumVariant { .. } => {
            OnlineSafetyClassification::OnlineSafe
        }

        // §7: "Enum rewrite (rename / remove value)" → OfflineOnly.
        // The differ never emits a "DropEnumVariant" op (Postgres
        // has no such DDL); enum drops are handled below.
        SchemaOperation::DropEnum(_) => OnlineSafetyClassification::OfflineOnly,

        // App-level metadata changes — folder rename + ledger UPDATE,
        // no SQL DDL on the application schema.
        SchemaOperation::RenameApp { .. } | SchemaOperation::MoveModelBetweenApps { .. } => {
            OnlineSafetyClassification::OnlineSafe
        }

        // PK-flip ops must be filtered before reaching this dispatch —
        // they belong to Phase 7's `pk_flip` cascade emitter family per
        // the §6.5 boundary contract, not the Phase 7.5 online-safety
        // surface. A misuse caller bypassing `classify_delta` should
        // get a refused-classification verdict so Phase 7's runner
        // refuses to apply rather than silently fast-applying a PK
        // flip. `OfflineOnly` is the safe-by-default verdict.
        SchemaOperation::PkTypeFlip { .. }
        | SchemaOperation::PkTypeFlipGroup(_)
        | SchemaOperation::PkTypeFlipMultiGroup(_) => OnlineSafetyClassification::OfflineOnly,

        // §7: "Opaque type transform" / unsupported variants →
        // OfflineOnly (operator must hand-edit).
        SchemaOperation::Unsupported { .. } => OnlineSafetyClassification::OfflineOnly,
    }
}

/// Walk a delta's operations, applying per-operation classification
/// plus cross-operation aggregation rules from §7:
///
/// - PK-flip groups are filtered out (routed through Phase 7 directly).
/// - 4+ FK additions to a single table escalate every addition on
///   that table to `ExpandContract`.
/// - 4+ inbound FK references on a `DropTable` escalate that drop to
///   `ExpandContract`.
///
/// Returns `(operation, classification)` pairs in input order. The
/// caller decides what to do with each verdict — the live-plan layer
/// keys off `ExpandContract`; the regular Phase 7 runner consumes the
/// other variants.
pub fn classify_delta(
    ops: &[SchemaOperation],
    ctx: &ClassifyContext<'_>,
) -> Vec<(SchemaOperation, OnlineSafetyClassification)> {
    if !classifier_applies(ctx) {
        let mut out: Vec<(SchemaOperation, OnlineSafetyClassification)> =
            Vec::with_capacity(ops.len());
        for op in ops {
            if is_pk_type_flip_operation(op) {
                continue;
            }
            out.push((op.clone(), OnlineSafetyClassification::OnlineSafe));
        }
        return out;
    }

    // Pre-pass — count FK additions per table for the multi-FK rule.
    let mut fk_addition_counts: BTreeMap<&str, u32> = BTreeMap::new();
    for op in ops {
        if let SchemaOperation::AddForeignKey { table, .. } = op {
            *fk_addition_counts.entry(table.as_str()).or_default() += 1;
        }
    }

    let mut out: Vec<(SchemaOperation, OnlineSafetyClassification)> = Vec::with_capacity(ops.len());
    for op in ops {
        // Skip PK-flip groups — Phase 7 territory. The standalone
        // `PkTypeFlip` variant survives only for unit-test fixtures
        // (production deltas always carry `PkTypeFlipGroup` after the
        // bucket-walk finalisation) — also filter it for safety.
        if is_pk_type_flip_operation(op) {
            continue;
        }

        let mut verdict = classify_operation(op, ctx);

        // Multi-FK escalation — when a single table receives `multi_fk_threshold`
        // or more FK additions in this delta, each addition lifts to
        // `ExpandContract` per §7.
        if let SchemaOperation::AddForeignKey { table, .. } = op
            && let Some(count) = fk_addition_counts.get(table.as_str())
            && *count >= ctx.multi_fk_threshold
        {
            verdict = OnlineSafetyClassification::ExpandContract;
        }

        if index_replacement_requires_refusal(op, ops) {
            verdict = OnlineSafetyClassification::OfflineOnly;
        }

        out.push((op.clone(), verdict));
    }
    out
}

fn is_pk_type_flip_operation(op: &SchemaOperation) -> bool {
    matches!(
        op,
        SchemaOperation::PkTypeFlip { .. }
            | SchemaOperation::PkTypeFlipGroup(_)
            | SchemaOperation::PkTypeFlipMultiGroup(_)
    )
}

/// `true` iff the classifier should run on operations targeting this
/// `(target_database, logging_profile)` pair. `false` triggers the
/// §6.5 short-circuit — every operation classifies as `OnlineSafe`
/// and routes through Phase 7 directly.
fn classifier_applies(ctx: &ClassifyContext<'_>) -> bool {
    match ctx.target_database {
        TargetDatabase::Application => true,
        TargetDatabase::CrudLog => matches!(ctx.logging_profile, LoggingProfile::StrictAudit),
        TargetDatabase::EventLog => false,
    }
}

/// §7: "Add nullable column" → OnlineSafe; non-nullable depends on the
/// default expression's volatility.
///
/// Volatility resolution order (§820): adopter override per
/// [`ClassifyContext::default_volatility_overrides`] takes precedence
/// over the static `pg_volatility.rs` lookup, so known-safe UDFs
/// asserted via `#[field(default_volatility = "stable")]` reach the
/// Pg18 fast-path even when the static table would conservatively
/// classify the expression as VOLATILE.
fn classify_add_column(
    table: &str,
    column: &ColumnSchema,
    ctx: &ClassifyContext<'_>,
) -> OnlineSafetyClassification {
    if column.nullable {
        // Nullable column — no backfill, catalog-only.
        return OnlineSafetyClassification::OnlineSafe;
    }
    // Non-nullable. Pg18 catalog-fast-paths a non-nullable add only
    // when the default is non-volatile. No default at all → backfill
    // required → ExpandContract.
    let Some(default) = column.default_sql.as_deref() else {
        return OnlineSafetyClassification::ExpandContract;
    };
    // Adopter override wins over the static table — the override is
    // a deliberate assertion that a Djogi-unclassifiable expression is
    // safe to fast-path. T3 enforces that overrides only attach to
    // fields with a default expression, so the lookup is always
    // meaningful when present.
    if let Some(override_volatility) = ctx
        .default_volatility_overrides
        .get(&(table.to_string(), column.name.clone()))
    {
        return match override_volatility {
            DefaultVolatility::Immutable | DefaultVolatility::Stable => {
                OnlineSafetyClassification::OnlineSafe
            }
            DefaultVolatility::Volatile => OnlineSafetyClassification::ExpandContract,
        };
    }
    match classify_default_expression(default) {
        Volatility::Immutable | Volatility::Stable => OnlineSafetyClassification::OnlineSafe,
        Volatility::Volatile => OnlineSafetyClassification::ExpandContract,
    }
}

/// §7: column-type / nullability / default / check / unique / indexed
/// changes.
fn classify_column_change(
    change: &ColumnChange,
    ctx: &ClassifyContext<'_>,
) -> OnlineSafetyClassification {
    match change {
        // §7: "Add NOT NULL constraint to populated table" →
        // ExpandContract when above `validation_threshold_rows`;
        // single-statement OnlineSafe below threshold (Pg18
        // `CHECK (col IS NOT NULL) NOT VALID` + `VALIDATE` + `SET NOT
        // NULL` reduces to a direct `SET NOT NULL` on small tables).
        // The reverse direction (NOT NULL → NULL) is catalog-only.
        ColumnChange::SetNullable(now_nullable) => {
            if *now_nullable {
                OnlineSafetyClassification::OnlineSafe
            } else {
                classify_validation_against_threshold(ctx)
            }
        }

        // SET DEFAULT / DROP DEFAULT — catalog-only.
        ColumnChange::SetDefault(_) => OnlineSafetyClassification::OnlineSafe,

        // §7: "Change column type" — multiple sub-cases.
        ColumnChange::ChangeType { from, to } => classify_type_change(from, to),

        // §7: "Add CHECK constraint to populated table" → ExpandContract
        // when above `validation_threshold_rows`; below threshold the
        // ADD CHECK validates inline as a single statement and stays
        // OnlineSafe. SET CHECK to None (drop) is always catalog-only.
        ColumnChange::SetCheck(new_check) => {
            if new_check.is_some() {
                classify_validation_against_threshold(ctx)
            } else {
                OnlineSafetyClassification::OnlineSafe
            }
        }

        // §7: "Add unique constraint to populated table" → ExpandContract
        // (CREATE UNIQUE INDEX CONCURRENTLY + ADD CONSTRAINT USING
        // INDEX). Dropping a unique constraint is catalog-only.
        ColumnChange::SetUnique(new_unique) => {
            if *new_unique {
                OnlineSafetyClassification::ExpandContract
            } else {
                OnlineSafetyClassification::OnlineSafe
            }
        }

        // Implicit per-column index flag. Adding routes through index
        // classification (assume non-concurrent for the per-column
        // shortcut — operators reach for the explicit `IndexSpec` when
        // they need concurrent builds); dropping is catalog-only.
        ColumnChange::SetIndexed(now_indexed) => {
            if *now_indexed {
                OnlineSafetyClassification::ExpandContract
            } else {
                OnlineSafetyClassification::OnlineSafe
            }
        }
    }
}

/// §7: "Change column type" routing.
///
/// - Identical types → OnlineSafe (no-op alter).
/// - Pg18 binary-coercible same storage (`varchar(n)` → `varchar(m)`
///   with m >= n; `text` ↔ `varchar(n)` for n large enough) →
///   OnlineSafe.
/// - Widening without rewrite (`int4` → `int8`, `int2` → `int4`) →
///   OnlineSafe.
/// - Otherwise → ExpandContract via shadow-column pattern. The
///   "narrowing with truncation" → OfflineOnly case requires explicit
///   adopter signal (`#[field(version, transform = ...)]`) the
///   classifier cannot infer from the type pair alone, so the
///   conservative ExpandContract verdict applies until that signal
///   surfaces in a later phase.
fn classify_type_change(from: &str, to: &str) -> OnlineSafetyClassification {
    if from == to {
        return OnlineSafetyClassification::OnlineSafe;
    }
    if is_binary_coercible_widening(from, to) {
        return OnlineSafetyClassification::OnlineSafe;
    }
    OnlineSafetyClassification::ExpandContract
}

/// `true` when `from → to` is a Pg18 binary-coercible widening — no
/// table rewrite required.
///
/// Recognises the common cases the spec calls out: integer widening
/// (`int2` → `int4` / `int8`; `int4` → `int8`) and varchar-length
/// widening / text broadening. Other type pairs return `false` so the
/// caller takes the conservative ExpandContract path.
fn is_binary_coercible_widening(from: &str, to: &str) -> bool {
    let f = from.trim().to_ascii_lowercase();
    let t = to.trim().to_ascii_lowercase();

    // Integer widening — Pg18 stores `int2` / `int4` / `int8` with
    // increasing storage but the catalog rewrite is fast-path because
    // the tuple header carries the width.
    let is_widening_int = matches!(
        (f.as_str(), t.as_str()),
        ("smallint", "integer")
            | ("smallint", "bigint")
            | ("integer", "bigint")
            | ("int2", "int4")
            | ("int2", "int8")
            | ("int4", "int8")
    );
    if is_widening_int {
        return true;
    }

    // Varchar-length widening — `varchar(n)` → `varchar(m)` with m >=
    // n, and `varchar(_)` → `text`. We extract the parenthesised length
    // by manual byte scan.
    if let Some((from_kind, from_len)) = parse_varchar(&f)
        && let Some((to_kind, to_len)) = parse_varchar(&t)
        && from_kind == to_kind
        && let (Some(fl), Some(tl)) = (from_len, to_len)
        && tl >= fl
    {
        return true;
    }
    if parse_varchar(&f).is_some() && t == "text" {
        return true;
    }

    false
}

/// Parse a `varchar` / `character varying` / `char` / `character` type
/// string into `(kind, optional length)`. Returns `None` for non-
/// varchar-family types.
///
/// `kind` is the canonical name (`"varchar"` for `varchar` or
/// `character varying`; `"char"` for `char` or `character`). The
/// length is `None` when no parenthesised length is present.
fn parse_varchar(t: &str) -> Option<(&'static str, Option<u32>)> {
    let normalized = t.trim();
    let (kind, rest) = if let Some(rest) = normalized.strip_prefix("character varying") {
        ("varchar", rest.trim_start())
    } else if let Some(rest) = normalized.strip_prefix("varchar") {
        ("varchar", rest.trim_start())
    } else if let Some(rest) = normalized.strip_prefix("character") {
        ("char", rest.trim_start())
    } else if let Some(rest) = normalized.strip_prefix("char") {
        ("char", rest.trim_start())
    } else {
        return None;
    };
    if rest.is_empty() {
        return Some((kind, None));
    }
    // Expect `(<digits>)` — anything else is a different type that
    // happens to share a prefix.
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
        return None;
    }
    let inner = &rest[1..rest.len() - 1].trim();
    let len: u32 = inner.parse().ok()?;
    Some((kind, Some(len)))
}

/// §7: "Add FK on tables ≤ threshold rows" → OnlineSafe; above
/// threshold (or unknown row count) → ExpandContract.
///
/// Multi-FK aggregation (4+ FKs on one table) is layered on top by
/// [`classify_delta`].
fn classify_fk_addition(ctx: &ClassifyContext<'_>) -> OnlineSafetyClassification {
    classify_validation_against_threshold(ctx)
}

/// Shared decision for the §7 family of "add validating constraint to
/// populated table" rows — CHECK additions (line 814), NOT NULL
/// additions (line 815), and FK validation (line 816). Each routes
/// through the same `validation_threshold_rows` knob so adopters get a
/// single tunable on `Djogi.toml`.
///
/// Returns [`OnlineSafetyClassification::OnlineSafe`] iff
/// `estimated_rows` is known and at-or-below the threshold; otherwise
/// [`OnlineSafetyClassification::ExpandContract`]. Unknown row count
/// (`None`) takes the conservative above-threshold path — the slower
/// staged-validation plan is always safe; the catalog-only fast path is
/// only safe when the row count is provably small.
fn classify_validation_against_threshold(ctx: &ClassifyContext<'_>) -> OnlineSafetyClassification {
    match ctx.estimated_rows {
        Some(rows) if rows <= ctx.validation_threshold_rows => {
            OnlineSafetyClassification::OnlineSafe
        }
        _ => OnlineSafetyClassification::ExpandContract,
    }
}

/// §7: "Add index" → OnlineSafe iff `concurrently = true`; otherwise
/// ExpandContract with the Phase 7 advisory warning. Hash indexes
/// without concurrent are refused at compose time (a separate
/// validation entry point handles the refusal — out of T5 scope).
fn classify_index_addition(index: &IndexSchema) -> OnlineSafetyClassification {
    if index.requires_out_of_transaction {
        // CREATE INDEX CONCURRENTLY — runs outside a transaction,
        // does not block writes.
        return OnlineSafetyClassification::OnlineSafe;
    }
    // Non-concurrent — catalog-only on an empty table, but on a
    // populated table it holds an AccessExclusiveLock for the duration
    // of the build. The classifier conservatively reports
    // ExpandContract regardless of `index.kind`; the alternative
    // (OnlineSafe with an unbounded lock window) would silently skip
    // the live-plan handoff for populated tables.
    OnlineSafetyClassification::ExpandContract
}

/// §7: replacing an index is only online when both DROP and CREATE use
/// the out-of-transaction path. Otherwise the replacement is refused
/// because a live-plan handoff cannot make the blocking index build
/// safe after the drop/create pair has already been chosen.
fn index_replacement_requires_refusal(op: &SchemaOperation, ops: &[SchemaOperation]) -> bool {
    match op {
        SchemaOperation::AddIndex(add) => ops.iter().any(|other| {
            if let SchemaOperation::DropIndex(drop) = other {
                indexes_replace_each_other(add, drop)
                    && (!add.requires_out_of_transaction || !drop.requires_out_of_transaction)
            } else {
                false
            }
        }),
        SchemaOperation::DropIndex(drop) => ops.iter().any(|other| {
            if let SchemaOperation::AddIndex(add) = other {
                indexes_replace_each_other(add, drop)
                    && (!add.requires_out_of_transaction || !drop.requires_out_of_transaction)
            } else {
                false
            }
        }),
        _ => false,
    }
}

fn indexes_replace_each_other(add: &IndexSchema, drop: &IndexSchema) -> bool {
    add.table == drop.table && index_targets_overlap(&add.target, &drop.target)
}

fn index_targets_overlap(left: &IndexTargetSchema, right: &IndexTargetSchema) -> bool {
    match (left, right) {
        (IndexTargetSchema::Columns(left_cols), IndexTargetSchema::Columns(right_cols)) => {
            left_cols.iter().any(|left_col| {
                right_cols
                    .iter()
                    .any(|right_col| left_col.name == right_col.name)
            })
        }
        _ => false,
    }
}

/// §7: "Drop table with 4+ inbound FKs" → ExpandContract; otherwise
/// FastLockDestructiveGuarded.
fn classify_drop_table(table: &str, ctx: &ClassifyContext<'_>) -> OnlineSafetyClassification {
    let inbound = ctx.inbound_fk_counts.get(table).copied().unwrap_or(0);
    if inbound >= ctx.multi_fk_threshold {
        return OnlineSafetyClassification::ExpandContract;
    }
    OnlineSafetyClassification::FastLockDestructiveGuarded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::diff::{ColumnChange, SchemaOperation};
    use crate::migrate::schema::{
        ColumnSchema, ForeignKeySchema, IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema,
        IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema, OnDeleteSchema,
        PkKindSchema,
    };

    fn nullable_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: None,
            default_sql: None,
            foreign_key: None,
            index_type: None,
            indexed: false,
            max_length: None,
            name: name.to_string(),
            nullable: true,
            on_delete: None,
            outbox_exclude: false,
            rationale: None,
            relation_kind: None,
            renamed_from: None,
            sequence_within: None,
            sql_type: "TEXT".to_string(),
            unique: false,
        }
    }

    fn non_null_column(name: &str, default: Option<&str>) -> ColumnSchema {
        ColumnSchema {
            nullable: false,
            default_sql: default.map(|s| s.to_string()),
            ..nullable_column(name)
        }
    }

    fn index(name: &str, table: &str, concurrently: bool) -> IndexSchema {
        IndexSchema {
            extension_dependency: None,
            include: Vec::new(),
            index_type: IndexTypeSchema::BTree,
            kind: IndexKindSchema::NonUnique,
            name: name.to_string(),
            nulls_not_distinct: false,
            predicate: None,
            requires_out_of_transaction: concurrently,
            table: table.to_string(),
            target: IndexTargetSchema::Columns(Vec::new()),
        }
    }

    fn index_on_column(name: &str, table: &str, column: &str, concurrently: bool) -> IndexSchema {
        IndexSchema {
            target: IndexTargetSchema::Columns(vec![IndexColumnSchema {
                name: column.to_string(),
                nulls: IndexNullsOrderSchema::Default,
                opclass: None,
                order: IndexOrderSchema::Asc,
            }]),
            ..index(name, table, concurrently)
        }
    }

    fn fk_for(table: &str) -> ForeignKeySchema {
        ForeignKeySchema {
            deferrable: false,
            initially_deferred: false,
            on_delete: OnDeleteSchema::Restrict,
            ref_column: "id".to_string(),
            ref_table: table.to_string(),
        }
    }

    fn ctx_app(estimated: Option<u64>) -> (BTreeMap<String, u32>, ClassifyContext<'static>) {
        // SAFETY: leak the inbound + override maps for tests so the
        // lifetimes fit ClassifyContext<'static>. Tests only —
        // production code constructs a freshly borrowed context per
        // call.
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        (
            BTreeMap::new(),
            ClassifyContext {
                estimated_rows: estimated,
                validation_threshold_rows: 100_000,
                multi_fk_threshold: 4,
                logging_profile: LoggingProfile::Balanced,
                target_database: TargetDatabase::Application,
                inbound_fk_counts: inbound,
                default_volatility_overrides: overrides,
            },
        )
    }

    #[test]
    fn add_nullable_column_classifies_as_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: nullable_column("nickname"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_non_null_column_with_constant_default_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("score", Some("0")),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_non_null_column_with_now_default_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("created_at", Some("now()")),
        };
        // `now()` is STABLE — Pg18 catalog-only fast-path applies.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_non_null_column_with_volatile_default_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("token", Some("gen_random_uuid()")),
        };
        // `gen_random_uuid()` is VOLATILE — 3-step pattern required.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_non_null_column_without_default_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("required", None),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn drop_column_is_fast_lock_destructive_guarded() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::DropColumn {
            table: "users".to_string(),
            column: "old".to_string(),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::FastLockDestructiveGuarded
        );
    }

    #[test]
    fn rename_column_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::RenameColumn {
            table: "users".to_string(),
            from: "name".to_string(),
            to: "full_name".to_string(),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn tighten_nullability_above_threshold_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(500_000));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        // Populated table above threshold — staged
        // `CHECK (col IS NOT NULL) NOT VALID` + `VALIDATE` + `SET NOT
        // NULL` per §815.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn tighten_nullability_below_threshold_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(50_000));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        // Small table — direct `SET NOT NULL` validates inline as a
        // single statement.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn tighten_nullability_unknown_rows_is_expand_contract() {
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email".to_string(),
            change: ColumnChange::SetNullable(false),
        };
        // Unknown row count → conservative staged path.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn relax_nullability_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email".to_string(),
            change: ColumnChange::SetNullable(true),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn integer_widening_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "id".to_string(),
            change: ColumnChange::ChangeType {
                from: "integer".to_string(),
                to: "bigint".to_string(),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn varchar_widening_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "name".to_string(),
            change: ColumnChange::ChangeType {
                from: "varchar(64)".to_string(),
                to: "varchar(128)".to_string(),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn varchar_to_text_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "bio".to_string(),
            change: ColumnChange::ChangeType {
                from: "varchar(255)".to_string(),
                to: "text".to_string(),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn text_to_varchar_is_expand_contract() {
        // Narrowing direction — conservative ExpandContract.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "bio".to_string(),
            change: ColumnChange::ChangeType {
                from: "text".to_string(),
                to: "varchar(255)".to_string(),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_fk_below_threshold_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(50_000));
        let op = SchemaOperation::AddForeignKey {
            table: "posts".to_string(),
            column: "author_id".to_string(),
            fk: fk_for("authors"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_fk_above_threshold_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(500_000));
        let op = SchemaOperation::AddForeignKey {
            table: "posts".to_string(),
            column: "author_id".to_string(),
            fk: fk_for("authors"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_fk_unknown_rows_is_expand_contract() {
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AddForeignKey {
            table: "posts".to_string(),
            column: "author_id".to_string(),
            fk: fk_for("authors"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_concurrent_index_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(index("ix_a", "users", true));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_non_concurrent_index_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(index("ix_a", "users", false));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn drop_table_few_inbound_fks_is_destructive_guarded() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::DropTable("legacy".to_string());
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::FastLockDestructiveGuarded
        );
    }

    #[test]
    fn drop_table_many_inbound_fks_is_expand_contract() {
        let mut inbound: BTreeMap<String, u32> = BTreeMap::new();
        inbound.insert("legacy".to_string(), 5);
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(inbound));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::Application,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::DropTable("legacy".to_string());
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_enum_variant_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddEnumVariant {
            enum_name: "status".to_string(),
            variant: "ARCHIVED".to_string(),
            anchor: None,
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn drop_enum_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::DropEnum("status".to_string());
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn unsupported_op_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::Unsupported {
            reason: "partition method change".to_string(),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn classifier_is_deterministic() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("token", Some("gen_random_uuid()")),
        };
        let first = classify_operation(&op, &ctx);
        let second = classify_operation(&op, &ctx);
        let third = classify_operation(&op, &ctx);
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first, OnlineSafetyClassification::ExpandContract);
    }

    #[test]
    fn event_log_target_short_circuits_to_online_safe() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::StrictAudit,
            target_database: TargetDatabase::EventLog,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        // Without short-circuit this would be ExpandContract; the
        // §6.5 rule routes event-log targets directly to Phase 7.
        let op = SchemaOperation::AddColumn {
            table: "events".to_string(),
            column: non_null_column("required", None),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn event_log_delta_short_circuit_is_not_overridden_by_multi_fk_aggregation() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(50),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::StrictAudit,
            target_database: TargetDatabase::EventLog,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let ops: Vec<SchemaOperation> = (0..4)
            .map(|i| SchemaOperation::AddForeignKey {
                table: "events".to_string(),
                column: format!("ref_{i}"),
                fk: fk_for("authors"),
            })
            .collect();
        let out = classify_delta(&ops, &ctx);
        for (_op, verdict) in &out {
            assert_eq!(*verdict, OnlineSafetyClassification::OnlineSafe);
        }
    }

    #[test]
    fn crud_log_under_balanced_short_circuits() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::CrudLog,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::DropColumn {
            table: "users_log".to_string(),
            column: "old".to_string(),
        };
        // Non-strict crud-log: short-circuit reports OnlineSafe so
        // Phase 7 applies the drop directly (no live plan, no
        // FastLockDestructiveGuarded gate from this layer).
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn crud_log_under_strict_audit_runs_full_classifier() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::StrictAudit,
            target_database: TargetDatabase::CrudLog,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::DropColumn {
            table: "users_log".to_string(),
            column: "old".to_string(),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::FastLockDestructiveGuarded
        );
    }

    #[test]
    fn classify_delta_skips_pk_flip_groups() {
        use crate::migrate::diff::{PkFlipDirection, PkFlipJoinTableOption, PkTypeFlipGroup};
        use crate::migrate::schema::PkKindSchema;
        let (_unused, ctx) = ctx_app(Some(0));
        let group = PkTypeFlipGroup {
            parent_table: "users".to_string(),
            parent_from: PkKindSchema::HeerId,
            parent_to: PkKindSchema::HeerIdRecencyBiased,
            direction: PkFlipDirection::AscToDesc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: None,
            co_destructive: false,
            co_lossy: false,
            join_table_option: PkFlipJoinTableOption::OptionA,
        };
        let ops = vec![
            SchemaOperation::PkTypeFlipGroup(group),
            SchemaOperation::AddColumn {
                table: "users".to_string(),
                column: nullable_column("nickname"),
            },
        ];
        let out = classify_delta(&ops, &ctx);
        assert_eq!(out.len(), 1, "PkTypeFlipGroup must be filtered out");
        assert!(matches!(out[0].0, SchemaOperation::AddColumn { .. }));
        assert_eq!(out[0].1, OnlineSafetyClassification::OnlineSafe);
    }

    #[test]
    fn classify_delta_escalates_multi_fk_additions() {
        let (_unused, ctx) = ctx_app(Some(50)); // tiny table — single FK below threshold otherwise
        let ops: Vec<SchemaOperation> = (0..4)
            .map(|i| SchemaOperation::AddForeignKey {
                table: "posts".to_string(),
                column: format!("ref_{i}"),
                fk: fk_for("authors"),
            })
            .collect();
        let out = classify_delta(&ops, &ctx);
        assert_eq!(out.len(), 4);
        for (_op, verdict) in &out {
            // Each individual FK below threshold would be OnlineSafe;
            // the aggregate multi-FK rule lifts every entry to
            // ExpandContract.
            assert_eq!(*verdict, OnlineSafetyClassification::ExpandContract);
        }
    }

    #[test]
    fn set_check_below_threshold_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(50_000));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "age".to_string(),
            change: ColumnChange::SetCheck(Some("age >= 0".to_string())),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn set_check_above_threshold_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(200_000));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "age".to_string(),
            change: ColumnChange::SetCheck(Some("age >= 0".to_string())),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn set_check_unknown_rows_is_expand_contract() {
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "age".to_string(),
            change: ColumnChange::SetCheck(Some("age >= 0".to_string())),
        };
        // Unknown row count → conservative staged-validation path.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn default_volatility_override_stable_routes_to_online_safe() {
        // Build a context whose override map asserts the default is
        // STABLE. Without the override the static table classifies
        // `gen_random_uuid()` as VOLATILE → ExpandContract.
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let mut overrides_map: BTreeMap<(String, String), DefaultVolatility> = BTreeMap::new();
        overrides_map.insert(
            ("users".to_string(), "token".to_string()),
            DefaultVolatility::Stable,
        );
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(overrides_map));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::Application,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("token", Some("gen_random_uuid()")),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn default_volatility_override_immutable_routes_to_online_safe() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let mut overrides_map: BTreeMap<(String, String), DefaultVolatility> = BTreeMap::new();
        overrides_map.insert(
            ("users".to_string(), "token".to_string()),
            DefaultVolatility::Immutable,
        );
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(overrides_map));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::Application,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("token", Some("my_pure_udf()")),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn default_volatility_override_absent_falls_through_to_static_table() {
        // No entry in the override map → static `pg_volatility.rs`
        // table classifies `gen_random_uuid()` as VOLATILE →
        // ExpandContract.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: non_null_column("token", Some("gen_random_uuid()")),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn pk_type_flip_dispatch_returns_offline_only_to_refuse_misuse() {
        // A misuse caller bypassing classify_delta and dispatching a
        // PK-flip op directly through classify_operation must get a
        // refused-classification verdict — not OnlineSafe — so the
        // Phase 7 runner refuses to apply rather than silently
        // fast-applying. PK-flip routing is Phase 7's exclusive
        // territory.
        use crate::migrate::diff::{PkFlipDirection, PkFlipJoinTableOption, PkTypeFlipGroup};
        use crate::migrate::schema::PkKindSchema;
        let (_unused, ctx) = ctx_app(Some(0));
        let group = PkTypeFlipGroup {
            parent_table: "users".to_string(),
            parent_from: PkKindSchema::HeerId,
            parent_to: PkKindSchema::HeerIdRecencyBiased,
            direction: PkFlipDirection::AscToDesc,
            children: Vec::new(),
            self_fk: None,
            join_tables: Vec::new(),
            cycles: Vec::new(),
            partitioned_parent: None,
            co_destructive: false,
            co_lossy: false,
            join_table_option: PkFlipJoinTableOption::OptionA,
        };
        let op = SchemaOperation::PkTypeFlipGroup(group);
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn pk_type_flip_dispatch_refuses_even_when_target_short_circuits() {
        let inbound: &'static BTreeMap<String, u32> = Box::leak(Box::new(BTreeMap::new()));
        let overrides: &'static BTreeMap<(String, String), DefaultVolatility> =
            Box::leak(Box::new(BTreeMap::new()));
        let ctx = ClassifyContext {
            estimated_rows: Some(0),
            validation_threshold_rows: 100_000,
            multi_fk_threshold: 4,
            logging_profile: LoggingProfile::Balanced,
            target_database: TargetDatabase::EventLog,
            inbound_fk_counts: inbound,
            default_volatility_overrides: overrides,
        };
        let op = SchemaOperation::PkTypeFlip {
            table: "users".to_string(),
            from: PkKindSchema::HeerId,
            to: PkKindSchema::HeerIdRecencyBiased,
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn replacement_index_with_blocking_side_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(50));
        let ops = vec![
            SchemaOperation::DropIndex(index_on_column("ix_old", "users", "email", false)),
            SchemaOperation::AddIndex(index_on_column("ix_new", "users", "email", true)),
        ];
        let out = classify_delta(&ops, &ctx);
        assert_eq!(out.len(), 2);
        for (_op, verdict) in &out {
            assert_eq!(*verdict, OnlineSafetyClassification::OfflineOnly);
        }
    }

    #[test]
    fn classify_delta_does_not_escalate_below_multi_fk_threshold() {
        let (_unused, ctx) = ctx_app(Some(50));
        let ops: Vec<SchemaOperation> = (0..3)
            .map(|i| SchemaOperation::AddForeignKey {
                table: "posts".to_string(),
                column: format!("ref_{i}"),
                fk: fk_for("authors"),
            })
            .collect();
        let out = classify_delta(&ops, &ctx);
        for (_op, verdict) in &out {
            // 3 FKs on one table, threshold is 4 — each stays at the
            // per-op verdict (OnlineSafe for low-row tables).
            assert_eq!(*verdict, OnlineSafetyClassification::OnlineSafe);
        }
    }
}
