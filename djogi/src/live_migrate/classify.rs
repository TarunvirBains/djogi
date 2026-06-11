//! Online-safety classification engine.
//! Walks a [`SchemaOperation`] (or a delta-worth of them) and assigns
//! each one an [`OnlineSafetyClassification`] verdict per the
//! classification table in `docs/spec/live-migrations.md`.
//! # Boundary contract (§6.5)
//! - **PK-flip routing is exclusive.** When a delta carries
//!   [`SchemaOperation::PkTypeFlipGroup`] or
//!   [`SchemaOperation::PkTypeFlipMultiGroup`], that operation is
//!   already routed through the
//!   [`crate::migrate::diff::Classification::PkTypeFlip`] cascade
//!   emitter family (`migrate::pk_flip`). The classifier short-circuits
//!   those entries — they appear in [`classify_delta`]'s output as
//!   skipped (filtered out) so live-plan callers never see them.
//! - **Logging-profile short-circuit.** Per §6.5 of the v3 plan, the
//!   classifier inspects [`ClassifyContext::logging_profile`] and the
//!   [`ClassifyContext::target_database`] field to decide whether to
//!   route through at all. Event-log databases never live-
//!   plan; crud-log databases under `light` / `balanced` never live-
//!   plan; only `strict_audit` crud-log + the application database
//!   reach the per-operation classifier.
//! # Determinism
//! Classification is pure: same inputs → same output, no `pg_catalog`
//! reads, no host-variable behaviour. Every dispatch arm cites the
//! §7 table row it maps to in a doc comment so the table stays
//! reviewable against this code.
//! # Aggregation
//! [`classify_delta`] performs the cross-operation aggregation §7
//! requires:
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
    ColumnSchema, IndexKindSchema, IndexSchema, IndexTargetSchema, OnlineSafetyClassification,
};
use std::collections::BTreeMap;

/// Ambient context the classifier consults that is not carried by a
/// single [`SchemaOperation`].
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
    /// - Event-log database — never live-plans regardless of profile;
    ///   the classifier reports every operation as `OnlineSafe` so
    ///   compose routes the delta directly through .
    /// - Crud-log database under [`LoggingProfile::Light`] /
    ///   [`LoggingProfile::Balanced`] — same direct route; brief
    ///   `AccessExclusiveLock` windows on crud-log mirror tables are
    ///   acceptable because the audit contract degrades gracefully.
    /// - Crud-log database under [`LoggingProfile::StrictAudit`]
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
    /// Spec: §3 / §820 of the v3 plan. Populated by compose
    /// from `FieldDescriptor::default_volatility_override`.
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

/// Which of the three Djogi databases the delta is targeting.
/// keeps three connection pools (application data, CRUD audit log,
/// event log) and the classifier's §6.5 short-circuit varies per pool.
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
/// **Pre-condition.** The caller has already filtered out
/// [`SchemaOperation::PkTypeFlipGroup`] / `PkTypeFlipMultiGroup`
/// operations — those are routed through the `pk_flip`
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
    // directly via the regular path.
    if !classifier_applies(ctx) {
        return OnlineSafetyClassification::OnlineSafe;
    }

    match op {
        // §7: "Add nullable column" → OnlineSafe (no backfill, no
        // lock window beyond the catalog touch). Non-nullable columns
        // dispatch through default-expression analysis.
        SchemaOperation::AddColumn { table, column } => classify_add_column(table, column, ctx),

        // §7: "Drop column" → FastLockDestructiveGuarded (destroys data +
        // invalidates dependents).
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

        // §7 (PR 7): "Add EXCLUDE constraint" — empty existing table
        // (estimated_rows == Some(0)) routes through OnlineSafe; the
        // ALTER TABLE inline form applies in a single transactional
        // segment. Populated tables (Some(n) where n > 0) AND unknown
        // row counts (None) classify OfflineOnly: Pg18 has no
        // `NOT VALID` for `EXCLUDE`, so two-phase staging is
        // structurally impossible — the constraint check runs under
        // AccessExclusiveLock against every existing row. The
        // unknown-row-count case takes the conservative offline path
        // because the classifier cannot prove the table is empty.
        SchemaOperation::AddExclusionConstraint { .. } => match ctx.estimated_rows {
            Some(0) => OnlineSafetyClassification::OnlineSafe,
            _ => OnlineSafetyClassification::OfflineOnly,
        },

        // Dropping an exclusion constraint is catalog-only — Postgres
        // releases the underlying GiST/B-tree index without scanning
        // rows. OnlineSafe.
        SchemaOperation::DropExclusionConstraint { .. } => OnlineSafetyClassification::OnlineSafe,

        // §7: "Add index" — concurrently=true → OnlineSafe; otherwise
        // ExpandContract. The `requires_out_of_transaction` flag on
        // IndexSchema mirrors the `concurrently = true` model knob.
        // Empty-table fast-path: estimated_rows == Some(0) routes
        // non-concurrent non-unique adds to OnlineSafe (the
        // AccessExclusiveLock is instant on a zero-row table).
        SchemaOperation::AddIndex(index) => classify_index_addition(index, ctx),

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

        // Djogi#217 — `COMMENT ON TABLE <t> IS '<text>'` /
        // `IS NULL` is a catalog-only write against `pg_description`.
        // No row touch, no lock window beyond the brief catalog update.
        // OnlineSafe regardless of from/to direction.
        SchemaOperation::SetTableComment { .. } => OnlineSafetyClassification::OnlineSafe,

        // Djogi#218 — table storage-parameter metadata
        // changes are catalog reloption updates; they do not rewrite
        // existing rows.
        SchemaOperation::SetStorageParams { .. } => OnlineSafetyClassification::OnlineSafe,

        // Djogi#219 — `ALTER TABLE ... SET TABLESPACE`
        // rewrites the table's physical file and takes an ACCESS
        // EXCLUSIVE lock, so live planning must treat it as offline.
        SchemaOperation::SetTablespace { .. } => OnlineSafetyClassification::OfflineOnly,

        // PK-flip ops belong to the core-migration `pk_flip` cascade
        // emitter family — they must be filtered out by `classify_delta`
        // before reaching this dispatch per the §6.5 boundary contract.
        // A misuse caller bypassing the delta walk should get a
        // refused-classification verdict so the runner refuses to apply
        // rather than silently fast-applying a PK flip. `OfflineOnly`
        // is the safe-by-default verdict.
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
/// - PK-flip groups are filtered out (routed through directly).
/// - 4+ FK additions to a single table escalate every addition on
///   that table to `ExpandContract`.
/// - 4+ inbound FK references on a `DropTable` escalate that drop to
///   `ExpandContract`.
///   Returns `(operation, classification)` pairs in input order. The
///   caller decides what to do with each verdict — the live-plan layer
///   keys off `ExpandContract`; the regular runner consumes the
///   other variants.
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
        // Skip PK-flip groups — territory. The standalone
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
/// and routes through directly.
fn classifier_applies(ctx: &ClassifyContext<'_>) -> bool {
    match ctx.target_database {
        TargetDatabase::Application => true,
        TargetDatabase::CrudLog => matches!(ctx.logging_profile, LoggingProfile::StrictAudit),
        TargetDatabase::EventLog => false,
    }
}

/// §7: "Add nullable column" → OnlineSafe iff there is no default;
/// nullability is orthogonal to the volatility classification.
/// A nullable add with a volatile default (`gen_random_uuid()` /
/// `random()` / `clock_timestamp()`) still requires the 3-step
/// ExpandContract pattern: Pg18's catalog-only fast-path is gated on
/// the default being non-volatile, and Postgres evaluates the default
/// once-per-row at backfill time regardless of the column's NULL
/// permission. Both the nullable and non-nullable cases therefore route
/// through the same volatility/override pipeline.
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
    // §7 (PR 7): "Add stored generated column" — empty existing table
    // (estimated_rows == Some(0)) routes through OnlineSafe; the
    // ALTER TABLE ADD COLUMN form applies in a single transactional
    // segment. Populated tables (Some(n) where n > 0) and unknown
    // row counts (None) classify OfflineOnly: Postgres rewrites every
    // row under AccessExclusiveLock to materialise the stored
    // expression. No `Pattern` exists for the populated case — see
    // [`crate::live_migrate::patterns::generated_column_refusal`].
    if column.generated.is_some() {
        return match ctx.estimated_rows {
            Some(0) => OnlineSafetyClassification::OnlineSafe,
            _ => OnlineSafetyClassification::OfflineOnly,
        };
    }
    let Some(default) = column.default_sql.as_deref() else {
        // No default at all. Nullable → catalog-only fast-path.
        // Non-nullable without a default → backfill required because
        // Postgres has nothing to populate the column with.
        if column.nullable {
            return OnlineSafetyClassification::OnlineSafe;
        }
        return OnlineSafetyClassification::ExpandContract;
    };
    // Default is present — same volatility/override pipeline applies
    // regardless of nullability. Adopter override wins over the static
    // table; the macro enforces that overrides only attach to fields with a
    // default expression, so the lookup is always meaningful when
    // present.
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
        // `using.is_some` signals "this is a non-default
        // cast"; the live-plan shadow-column pattern can only emit a
        // plain SQL cast (`<col>::<to>`) and cannot replicate an
        // adopter-supplied expression in the backfill UPDATE. Route
        // such changes to `OfflineOnly` so the dispatcher never
        // receives an op whose adopter expression it would silently
        // drop. The dispatcher / pattern emitters retain a
        // belt-and-braces refusal as a defense-in-depth check (see
        // `dispatch_pattern` and the `replacement_column` /
        // `codec_transition` emitters).
        // When `using.is_none()` the lock window is governed by the
        // cast pair alone and the existing pair-based dispatch
        // applies.
        ColumnChange::ChangeType { using: Some(_), .. } => OnlineSafetyClassification::OfflineOnly,
        ColumnChange::ChangeType {
            from,
            to,
            using: None,
        } => classify_type_change(from, to),

        // §7: "Add CHECK constraint to populated table" → ExpandContract
        // when above `validation_threshold_rows`; below threshold the
        // ADD CHECK validates inline as a single statement and stays
        // OnlineSafe. Pure DROP (`to = None`) is always catalog-only.
        // `from` carries the prior expression for non-lossy rollback
        // but does not change the online-safety classification — the
        // forward step's lock window is governed entirely by `to`.
        ColumnChange::SetCheck { to, .. } => {
            if to.is_some() {
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

        // §7 (PR 7): "Change stored generated column expression on
        // populated table" → OfflineOnly. Postgres re-evaluates the
        // generation expression for every row under
        // AccessExclusiveLock, which is structurally the same lock
        // window that ExpandContract is meant to avoid — a shadow-
        // column pattern offers no relief because the row rewrite
        // still happens. See
        // [`crate::live_migrate::patterns::generated_column_refusal`]
        // for the no-`Pattern` rationale. Dropping the generation
        // (to = None) routes the same way; the catalog-only path is
        // not reachable for a generated column on a populated table.
        ColumnChange::SetGenerated { .. } => OnlineSafetyClassification::OfflineOnly,

        // Identity-column transitions: `ALTER COLUMN ADD GENERATED ... AS IDENTITY`
        // is catalog-only
        // Postgres allocates the sequence, no row rewrite. The
        // sequence's start value is set after MAX(c) for existing rows
        // automatically. Same for DROP IDENTITY (catalog-only) and
        // SET GENERATED kind change (catalog-only). All three route to
        // OnlineSafe.
        ColumnChange::SetIdentity { .. } => OnlineSafetyClassification::OnlineSafe,

        // Djogi#217 — `COMMENT ON COLUMN <t>.<c> IS '<text>'`
        // / `IS NULL` is a catalog-only write against `pg_description`.
        // No row touch, no lock window beyond the brief catalog update.
        // OnlineSafe regardless of from/to direction.
        // Postgres docs: §"COMMENT" (no lock-window guidance because
        // `pg_description` updates are catalog-only).
        ColumnChange::SetComment { .. } => OnlineSafetyClassification::OnlineSafe,

        // A codec add / swap / drop is never a plain SQL cast — every row
        // must be re-encoded under the new codec, which an in-place
        // `<col>::BYTEA` backfill cannot do (it would write plaintext /
        // old-format bytes into a column whose schema claims the new codec).
        // The framework refuses to auto-generate an online migration; the
        // operator re-encrypts manually. Classification mirrors the codec-
        // transition table (issue #371) via `classify_codec_transition`.
        ColumnChange::CodecChange {
            from_codec,
            to_codec,
        } => classify_codec_transition(from_codec.as_deref(), to_codec.as_deref()),
    }
}

/// Classify an at-rest codec transition for online-safety (issue #371).
/// Every codec-involving transition is a full row re-encode, so none is a
/// catalog-only `OnlineSafe` change except the degenerate same-codec no-op
/// (which the differ filters upstream — it only emits `CodecChange` when
/// `before.codec != after.codec`).
///
/// - plaintext → codec (`None → Some`): full offline re-encode → `OfflineOnly`.
/// - codec → plaintext (`Some → None`): full decode-in-place rewrite →
///   `OfflineOnly`.
/// - codec → different codec (`Some(a) → Some(b)`, `a != b`): dual-write /
///   backfill (`ExpandContract`), though v1's online emitter is fenced (the
///   offline compose path is the v1 story).
/// - same codec (`Some(a) → Some(a)`): degenerate no-op (`OnlineSafe`,
///   defensive — the differ never emits this in practice).
fn classify_codec_transition(from: Option<&str>, to: Option<&str>) -> OnlineSafetyClassification {
    match (from, to) {
        // No-op (identical codec) — defensive; differ filters identical pairs.
        (Some(a), Some(b)) if a == b => OnlineSafetyClassification::OnlineSafe,
        // codec → different codec: dual-write / backfill (expand-contract),
        // but the online emitter is fenced (issue #371); the offline compose
        // path is the v1 story.
        (Some(_), Some(_)) => OnlineSafetyClassification::ExpandContract,
        // plaintext → codec, or codec → plaintext: full offline re-encode.
        (None, Some(_)) | (Some(_), None) => OnlineSafetyClassification::OfflineOnly,
        // No codec on either side — `emit_alter_column` never pushes
        // `CodecChange` for this case (it only pushes when
        // `before.codec != after.codec`), so this arm is unreachable; keep it
        // total and route to the safe default.
        (None, None) => OnlineSafetyClassification::OnlineSafe,
    }
}

/// §7: "Change column type" routing.
/// - Identical types → OnlineSafe (no-op alter).
/// - Pg18 binary-coercible same storage (`varchar(n)` → `varchar(m)`
///   with m >= n; `text` ↔ `varchar(n)` for n large enough) →
///   OnlineSafe.
/// - Widening without rewrite (`int4` → `int8`, `int2` → `int4`) →
///   OnlineSafe.
/// - Known narrowing pairs (BIGINT → INT4, varchar(N) → varchar(M)
///   with M < N, TEXT → varchar(N), NUMERIC precision/scale loss) →
///   OfflineOnly. Narrowing risks truncation / overflow at the row
///   level; without an explicit `#[field(version, transform = ...)]`
///   signal the classifier cannot prove the conversion is lossless,
///   so it refuses the live path. When the transform field lands in a
///   later phase, this routing refines to ExpandContract when the
///   transform is present.
/// - Other / unknown type changes (ENUM rename, JSONB shape change,
///   foreign-type swaps) → ExpandContract via shadow-column pattern.
///   These cases require backfill and operator gates regardless of
///   direction.
fn classify_type_change(from: &str, to: &str) -> OnlineSafetyClassification {
    if from == to {
        return OnlineSafetyClassification::OnlineSafe;
    }
    if is_binary_coercible_widening(from, to) {
        return OnlineSafetyClassification::OnlineSafe;
    }
    if is_narrowing_or_truncating(from, to) {
        return OnlineSafetyClassification::OfflineOnly;
    }
    OnlineSafetyClassification::ExpandContract
}

/// `true` when `from → to` is a known narrowing / truncating pair.
/// Recognised cases per §7:
/// - Integer narrowing: BIGINT → INT4, BIGINT → SMALLINT, INT4 →
///   SMALLINT (overflow risk).
/// - varchar-length narrowing: `varchar(N)` → `varchar(M)` with
///   `M < N` (truncation risk).
/// - text → `varchar(N)` (truncation risk, regardless of N).
/// - NUMERIC narrowing: `numeric(p1, s1)` → `numeric(p2, s2)` with
///   `p2 < p1` or `s2 < s1` (precision / scale loss).
///   Pairs the classifier cannot recognise as narrowing fall through
///   the caller then routes them via the regular ExpandContract path.
fn is_narrowing_or_truncating(from: &str, to: &str) -> bool {
    let f = from.trim().to_ascii_lowercase();
    let t = to.trim().to_ascii_lowercase();

    // Integer narrowing — canonicalise aliases first so cross-alias
    // pairs (e.g., `bigint -> int4`, `int8 -> integer`) are matched.
    if let (Some(fw), Some(tw)) = (canonical_int_width(&f), canonical_int_width(&t))
        && tw < fw
    {
        return true;
    }

    // varchar / char length narrowing — same kind, smaller length.
    if let Some((from_kind, from_len)) = parse_varchar(&f)
        && let Some((to_kind, to_len)) = parse_varchar(&t)
        && from_kind == to_kind
        && let (Some(fl), Some(tl)) = (from_len, to_len)
        && tl < fl
    {
        return true;
    }

    // text → varchar(N) — any length is potentially narrower than
    // unbounded text.
    if f == "text" && parse_varchar(&t).is_some() {
        return true;
    }

    // NUMERIC narrowing — precision, scale, or integer-digit room
    // loss. Postgres normalises `numeric(p)` to `numeric(p, 0)` so the
    // parser returns `(Some(p), Some(0))` for that form; bare
    // `numeric` returns `(None, None)`. An unbounded source classed
    // against a bounded destination is narrowing: the destination
    // imposes a ceiling that may reject existing rows. Bounded → bounded
    // narrowing compares precision, scale, and integer-digit room
    // (precision - scale) independently — `numeric(10) -> numeric(12,2)`
    // preserves all 10 integer digits and is widening, but
    // `numeric(10) -> numeric(10,2)` shrinks integer room to 8 and is
    // narrowing.
    if let Some(from) = parse_numeric_params(&f)
        && let Some(to) = parse_numeric_params(&t)
    {
        match (from, to) {
            (
                NumericTypmod::Bounded {
                    precision: fp,
                    scale: fs,
                },
                NumericTypmod::Bounded {
                    precision: tp,
                    scale: ts,
                },
            ) => {
                let from_int_digits = fp.saturating_sub(fs);
                let to_int_digits = tp.saturating_sub(ts);
                if tp < fp || ts < fs || to_int_digits < from_int_digits {
                    return true;
                }
            }
            (NumericTypmod::Unbounded, NumericTypmod::Bounded { .. }) => return true,
            _ => {}
        }
    }

    false
}

/// Canonical integer width in bits, with Postgres alias normalisation:
/// `bigint`/`int8` → 64, `integer`/`int`/`int4` → 32, `smallint`/`int2`
/// → 16. Returns `None` for non-integer SQL types.
fn canonical_int_width(name: &str) -> Option<u8> {
    match name {
        "bigint" | "int8" => Some(64),
        "integer" | "int" | "int4" => Some(32),
        "smallint" | "int2" => Some(16),
        _ => None,
    }
}

/// Postgres NUMERIC type modifier.
/// `numeric(p)` is normalised to `Bounded { precision: p, scale: 0 }`
/// per Postgres semantics — an omitted scale means scale-zero, not
/// "scale unknown". Bare `numeric` is `Unbounded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericTypmod {
    Unbounded,
    Bounded { precision: u32, scale: u32 },
}

/// Parse `numeric(p)` / `numeric(p, s)` / bare `numeric` (and the
/// `decimal` alias). Returns `None` for non-numeric types.
fn parse_numeric_params(t: &str) -> Option<NumericTypmod> {
    let normalized = t.trim();
    let rest = normalized
        .strip_prefix("numeric")
        .or_else(|| normalized.strip_prefix("decimal"))?
        .trim_start();
    if rest.is_empty() {
        return Some(NumericTypmod::Unbounded);
    }
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
        return None;
    }
    let inner = rest[1..rest.len() - 1].trim();
    if inner.is_empty() {
        return Some(NumericTypmod::Unbounded);
    }
    let mut parts = inner.split(',');
    let p_str = parts.next()?.trim();
    let precision: u32 = p_str.parse().ok()?;
    // Postgres semantics: `numeric(p)` ≡ `numeric(p, 0)`.
    let scale: u32 = match parts.next() {
        Some(s_str) => s_str.trim().parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(NumericTypmod::Bounded { precision, scale })
}

/// `true` when `from → to` is a Pg18 binary-coercible widening — no
/// table rewrite required.
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

/// §7: "Add index" routing.
/// Unique indexes (`UniqueConstraint` / `UniqueIndex`) ALWAYS route
/// through `ExpandContract` regardless of the `concurrently` flag.
/// The §7 rollout for "add unique constraint to populated table" is a
/// 2-step pattern — `CREATE UNIQUE INDEX CONCURRENTLY` builds the
/// index online, then `ALTER TABLE ... ADD CONSTRAINT ... USING INDEX`
/// promotes it. Concurrency is required for the build but not
/// sufficient on its own — the operator-driven 2-step gate is what
/// `ExpandContract` represents.
/// Non-unique indexes:
/// - `concurrently = true` → `OnlineSafe` (`CREATE INDEX CONCURRENTLY`
///   runs outside a transaction and does not block writes).
/// - `concurrently = false`, `estimated_rows == Some(0)` → `OnlineSafe`
///   (PR 7 empty-table fast-path: zero-row tables hold the
///   AccessExclusiveLock for an instant; the build is structurally
///   trivial).
/// - `concurrently = false`, populated or unknown → `ExpandContract`.
///   On a populated table the lock holds for the duration of the
///   build; unknown-row-count takes the conservative path.
///   Hash indexes without concurrent are refused at compose time (a
///   separate validation entry point handles the refusal).
fn classify_index_addition(
    index: &IndexSchema,
    ctx: &ClassifyContext<'_>,
) -> OnlineSafetyClassification {
    match index.kind {
        IndexKindSchema::UniqueConstraint | IndexKindSchema::UniqueIndex => {
            // Concurrency is required but not sufficient — the operator
            // still drives the 2-step build-then-promote rollout.
            OnlineSafetyClassification::ExpandContract
        }
        IndexKindSchema::NonUnique => {
            if index.requires_out_of_transaction {
                OnlineSafetyClassification::OnlineSafe
            } else if ctx.estimated_rows == Some(0) {
                // Empty-table fast-path: zero-row tables hold the
                // AccessExclusiveLock for an instant; the build is
                // structurally trivial. Mirrors the PR 7 routing for
                // EXCLUSION + stored-generated empty-table cases.
                OnlineSafetyClassification::OnlineSafe
            } else {
                OnlineSafetyClassification::ExpandContract
            }
        }
    }
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
        ColumnSchema, ExclusionConstraintSchema, ExclusionElementSchema, ForeignKeySchema,
        GeneratedColumnSchema, IndexColumnSchema, IndexKindSchema, IndexNullsOrderSchema,
        IndexOrderSchema, IndexSchema, IndexTargetSchema, IndexTypeSchema, OnDeleteSchema,
        PkKindSchema,
    };

    fn nullable_column(name: &str) -> ColumnSchema {
        ColumnSchema {
            check: None,
            codec: None,
            comment: None,
            default_sql: None,
            foreign_key: None,
            generated: None,
            identity: None,
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
            type_change_using: None,
        }
    }

    fn non_null_column(name: &str, default: Option<&str>) -> ColumnSchema {
        ColumnSchema {
            nullable: false,
            default_sql: default.map(|s| s.to_string()),
            ..nullable_column(name)
        }
    }

    fn nullable_column_with_default(name: &str, default: &str) -> ColumnSchema {
        ColumnSchema {
            default_sql: Some(default.to_string()),
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

    fn unique_index(
        name: &str,
        table: &str,
        kind: IndexKindSchema,
        concurrently: bool,
    ) -> IndexSchema {
        IndexSchema {
            kind,
            ..index(name, table, concurrently)
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
        // lifetimes fit ClassifyContext<'static>. Tests only
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
    fn ddl_metadata_catalog_writes_classify_online_safe() {
        let (_unused, ctx) = ctx_app(Some(10));
        let table_comment = SchemaOperation::SetTableComment {
            table: "users".to_string(),
            from: None,
            to: Some("Users table".to_string()),
        };
        let storage_params = SchemaOperation::SetStorageParams {
            table: "users".to_string(),
            from: None,
            to: Some("fillfactor=70".to_string()),
        };

        assert_eq!(
            classify_operation(&table_comment, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
        assert_eq!(
            classify_operation(&storage_params, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn set_tablespace_classifies_offline_only() {
        let (_unused, ctx) = ctx_app(Some(10));
        let op = SchemaOperation::SetTablespace {
            table: "users".to_string(),
            from: None,
            to: Some("fastspace".to_string()),
        };

        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
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
    fn add_nullable_column_with_random_default_is_expand_contract() {
        // Spec-correctness: `ADD COLUMN <nullable> DEFAULT random()`
        // STILL requires the 3-step ExpandContract pattern. Pg18's
        // catalog-only fast-path is gated on the default being
        // non-volatile; the column's NULL permission does not change
        // the underlying volatility check. Pre-fix the classifier
        // returned OnlineSafe for any nullable add regardless of
        // default volatility.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: nullable_column_with_default("seed", "random()"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_nullable_column_with_gen_random_uuid_default_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: nullable_column_with_default("token", "gen_random_uuid()"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_nullable_column_with_clock_timestamp_default_is_expand_contract() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "events".to_string(),
            column: nullable_column_with_default("logged_at", "clock_timestamp()"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_nullable_column_with_stable_default_is_online_safe() {
        // Confirms the pipeline handles the non-volatile case for
        // nullable columns too — `now()` is STABLE, catalog-only
        // fast-path still applies.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "events".to_string(),
            column: nullable_column_with_default("logged_at", "now()"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
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
                using: None,
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
                using: None,
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
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn text_to_varchar_is_offline_only() {
        // Narrowing direction — text is unbounded, varchar(N) imposes
        // a maximum length, so the conversion risks truncation.
        // Without an explicit transform signal the classifier refuses
        // the live path.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "bio".to_string(),
            change: ColumnChange::ChangeType {
                from: "text".to_string(),
                to: "varchar(255)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn bigint_to_int4_is_offline_only() {
        // Narrowing integer pair — overflow risk on rows with values
        // outside INT4 range. Routes to OfflineOnly until an explicit
        // transform signal lets the classifier prove the conversion
        // is lossless.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "metrics".to_string(),
            column: "count".to_string(),
            change: ColumnChange::ChangeType {
                from: "bigint".to_string(),
                to: "integer".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn int4_to_smallint_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "metrics".to_string(),
            column: "count".to_string(),
            change: ColumnChange::ChangeType {
                from: "integer".to_string(),
                to: "smallint".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn type_change_with_adopter_using_is_offline_only() {
        // adopter-supplied `using` signals "this is a
        // non-default cast"; the live-plan shadow-column pattern can
        // only emit a plain SQL cast in its backfill and cannot
        // replicate an adopter expression. Route to OfflineOnly
        // regardless of the cast pair.
        // INTEGER → BIGINT without `using` would classify OnlineSafe
        // (benign widening), so the `using.is_some()` arm is the only
        // thing producing OfflineOnly here.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "metrics".to_string(),
            column: "count".to_string(),
            change: ColumnChange::ChangeType {
                from: "integer".to_string(),
                to: "bigint".to_string(),
                using: Some("count::BIGINT".to_string()),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
            "adopter `using` must force OfflineOnly regardless of cast pair",
        );
    }

    #[test]
    fn varchar_narrowing_is_offline_only() {
        // varchar(20) → varchar(10) — truncation risk for rows with
        // values longer than 10 bytes.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "tag".to_string(),
            change: ColumnChange::ChangeType {
                from: "varchar(20)".to_string(),
                to: "varchar(10)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn numeric_precision_loss_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "ledger".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "numeric(20, 4)".to_string(),
                to: "numeric(10, 4)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn numeric_scale_loss_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "ledger".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "numeric(20, 4)".to_string(),
                to: "numeric(20, 2)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn numeric_scale_addition_preserving_integer_digits_is_widening() {
        // Postgres: `numeric(10)` ≡ `numeric(10, 0)` (10 integer
        // digits, 0 fractional). `numeric(10, 0) -> numeric(12, 2)`
        // keeps the same 10 integer digits and adds 2 fractional
        // strictly widening, must NOT classify as narrowing.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "ledger".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "numeric(10)".to_string(),
                to: "numeric(12, 2)".to_string(),
                using: None,
            },
        };
        // The unknown-type-change fallback returns ExpandContract; what
        // matters is that we do NOT misclassify as OfflineOnly.
        assert_ne!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn numeric_integer_digit_room_loss_is_offline_only() {
        // `numeric(10, 0) -> numeric(10, 2)` keeps the same 10 total
        // digits but redirects 2 to fractional, shrinking integer room
        // from 10 to 8. Existing rows with 10-digit integers no longer
        // fit — narrowing.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "ledger".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "numeric(10)".to_string(),
                to: "numeric(10, 2)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn bigint_to_int4_alias_is_offline_only() {
        // Cross-alias narrowing: source uses Postgres canonical name
        // (`bigint`), destination uses int8/int4-style alias. The
        // canonicalising helper must collapse both sides to the same
        // width-keyed lattice before comparing.
        let (_unused, ctx) = ctx_app(Some(0));
        for (from, to) in [
            ("bigint", "int4"),
            ("bigint", "int2"),
            ("int8", "integer"),
            ("int8", "smallint"),
            ("int4", "smallint"),
            ("integer", "int2"),
        ] {
            let op = SchemaOperation::AlterColumn {
                table: "metrics".to_string(),
                column: "count".to_string(),
                change: ColumnChange::ChangeType {
                    from: from.to_string(),
                    to: to.to_string(),
                    using: None,
                },
            };
            assert_eq!(
                classify_operation(&op, &ctx),
                OnlineSafetyClassification::OfflineOnly,
                "{from} -> {to}"
            );
        }
    }

    #[test]
    fn unbounded_numeric_to_bounded_is_offline_only() {
        // `numeric` (unbounded) → `numeric(10, 2)` introduces a
        // precision/scale ceiling that may reject existing rows whose
        // magnitude or scale exceeds the new bound.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "ledger".to_string(),
            column: "amount".to_string(),
            change: ColumnChange::ChangeType {
                from: "numeric".to_string(),
                to: "numeric(10, 2)".to_string(),
                using: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly
        );
    }

    #[test]
    fn unknown_type_change_remains_expand_contract() {
        // ENUM rename / JSONB shape change / other unknown type pair
        // not a known narrowing or widening, so the conservative
        // ExpandContract path applies.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "status".to_string(),
            change: ColumnChange::ChangeType {
                from: "user_status_v1".to_string(),
                to: "user_status_v2".to_string(),
                using: None,
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
    fn add_non_concurrent_index_on_populated_table_is_expand_contract() {
        // Populated table holds AccessExclusiveLock for the duration
        // of the build — escalate to ExpandContract.
        let (_unused, ctx) = ctx_app(Some(50_000));
        let op = SchemaOperation::AddIndex(index("ix_a", "users", false));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_non_concurrent_index_on_empty_table_is_online_safe() {
        // Empty-table fast-path: zero-row tables hold the
        // AccessExclusiveLock for an instant; the build is structurally trivial.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(index("ix_a", "users", false));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
        );
    }

    #[test]
    fn add_non_concurrent_index_with_unknown_row_count_is_expand_contract() {
        // None takes the conservative ExpandContract path — the
        // classifier cannot prove the table is empty.
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AddIndex(index("ix_a", "users", false));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_concurrent_unique_constraint_is_expand_contract() {
        // Per §7: adding a unique constraint to a populated table is a
        // 2-step rollout — CREATE UNIQUE INDEX CONCURRENTLY then
        // ALTER TABLE ... ADD CONSTRAINT ... USING INDEX. Concurrency
        // alone does NOT make the constraint addition OnlineSafe; the
        // operator-driven gate is what ExpandContract represents.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(unique_index(
            "ux_email",
            "users",
            IndexKindSchema::UniqueConstraint,
            true,
        ));
        let verdict = classify_operation(&op, &ctx);
        assert_ne!(
            verdict,
            OnlineSafetyClassification::OnlineSafe,
            "unique constraint must not short-circuit to OnlineSafe even when built concurrently"
        );
        assert_eq!(verdict, OnlineSafetyClassification::ExpandContract);
    }

    #[test]
    fn add_concurrent_unique_index_is_expand_contract() {
        // Same rule applies to UniqueIndex (partial unique / NULLS NOT
        // DISTINCT) — the build is online, the promotion is the gate.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(unique_index(
            "ux_email_partial",
            "users",
            IndexKindSchema::UniqueIndex,
            true,
        ));
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn add_non_concurrent_unique_constraint_is_expand_contract() {
        // Non-concurrent unique build holds AccessExclusiveLock for the
        // duration plus needs the constraint promotion — both reasons
        // route through ExpandContract.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddIndex(unique_index(
            "ux_email",
            "users",
            IndexKindSchema::UniqueConstraint,
            false,
        ));
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
        // §6.5 rule routes event-log targets directly to .
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
        // Applies the drop directly (no live plan, no
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
            change: ColumnChange::SetCheck {
                from: None,
                to: Some("age >= 0".to_string()),
            },
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
            change: ColumnChange::SetCheck {
                from: None,
                to: Some("age >= 0".to_string()),
            },
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
            change: ColumnChange::SetCheck {
                from: None,
                to: Some("age >= 0".to_string()),
            },
        };
        // Unknown row count → conservative staged-validation path.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::ExpandContract
        );
    }

    #[test]
    fn set_check_drop_is_online_safe_regardless_of_prior() {
        // rollback restores the prior CHECK. The classifier still
        // routes purely on `to` — a pure DROP (to = None) is
        // catalog-only regardless of whether `from` is Some or None.
        let (_unused, ctx) = ctx_app(Some(200_000));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "age".to_string(),
            change: ColumnChange::SetCheck {
                from: Some("age >= 0".to_string()),
                to: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe
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
        // Runner refuses to apply rather than silently
        // fast-applying. PK-flip routing is the exclusive
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

    // ── PR 7: EXCLUSION + stored-generated classification ──

    fn exclusion(name: &str) -> ExclusionConstraintSchema {
        ExclusionConstraintSchema {
            deferrable: false,
            elements: vec![ExclusionElementSchema {
                expr: "room_id".to_string(),
                with_operator: "=".to_string(),
            }],
            extension_dependency: None,
            initially_deferred: false,
            name: name.to_string(),
            using: "gist".to_string(),
            where_clause: None,
        }
    }

    fn generated_column(name: &str, expression: &str) -> ColumnSchema {
        ColumnSchema {
            generated: Some(GeneratedColumnSchema {
                expression: expression.to_string(),
                stored: true,
            }),
            ..nullable_column(name)
        }
    }

    #[test]
    fn add_exclusion_constraint_on_empty_table_is_online_safe() {
        // Empty existing table (estimated_rows == Some(0)) →
        // OnlineSafe. The ALTER TABLE inline form applies in a single
        // transactional segment.
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddExclusionConstraint {
            table: "bookings".to_string(),
            exclusion: exclusion("no_overlap"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe,
        );
    }

    #[test]
    fn add_exclusion_constraint_on_populated_table_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(1_000_000));
        let op = SchemaOperation::AddExclusionConstraint {
            table: "bookings".to_string(),
            exclusion: exclusion("no_overlap"),
        };
        // Populated tables: Pg18 has no `NOT VALID` for `EXCLUDE`, so
        // the live-plan layer refuses; operator must hand-edit under
        // a maintenance window per the v3 plan.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    #[test]
    fn add_exclusion_constraint_with_unknown_row_count_is_offline_only() {
        // Unknown row count (None) takes the conservative OfflineOnly
        // path — the classifier cannot prove the table is empty.
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AddExclusionConstraint {
            table: "bookings".to_string(),
            exclusion: exclusion("no_overlap"),
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    #[test]
    fn drop_exclusion_constraint_classifies_as_online_safe() {
        let (_unused, ctx) = ctx_app(Some(1_000_000));
        let op = SchemaOperation::DropExclusionConstraint {
            table: "bookings".to_string(),
            name: "no_overlap".to_string(),
            exclusion: exclusion("no_overlap"),
        };
        // DROP CONSTRAINT releases the underlying GiST index; catalog-
        // only operation regardless of row count.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe,
        );
    }

    #[test]
    fn add_stored_generated_column_on_empty_table_is_online_safe() {
        let (_unused, ctx) = ctx_app(Some(0));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: generated_column("email_lower", "LOWER(email)"),
        };
        // Empty existing table: the rewrite is a no-op data-wise; the
        // ALTER TABLE ADD COLUMN form applies in a single
        // transactional segment.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OnlineSafe,
        );
    }

    #[test]
    fn add_stored_generated_column_on_populated_table_is_offline_only() {
        let (_unused, ctx) = ctx_app(Some(50_000));
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: generated_column("email_lower", "LOWER(email)"),
        };
        // Populated tables: Postgres rewrites every row under
        // AccessExclusiveLock to materialise the stored expression.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    #[test]
    fn add_stored_generated_column_with_unknown_row_count_is_offline_only() {
        let (_unused, ctx) = ctx_app(None);
        let op = SchemaOperation::AddColumn {
            table: "users".to_string(),
            column: generated_column("email_lower", "LOWER(email)"),
        };
        // Unknown row count → conservative offline path.
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    #[test]
    fn alter_column_set_generated_classifies_as_offline_only() {
        let (_unused, ctx) = ctx_app(Some(50));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email_lower".to_string(),
            change: ColumnChange::SetGenerated {
                from: None,
                to: Some(GeneratedColumnSchema {
                    expression: "LOWER(email)".to_string(),
                    stored: true,
                }),
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    #[test]
    fn alter_column_drop_generated_also_classifies_as_offline_only() {
        // Even removing the generated expression is OfflineOnly: Pg
        // re-evaluates the column's storage state under
        // AccessExclusiveLock. There is no online path; the operator
        // hand-edits a DROP+ADD COLUMN sequence.
        let (_unused, ctx) = ctx_app(Some(50));
        let op = SchemaOperation::AlterColumn {
            table: "users".to_string(),
            column: "email_lower".to_string(),
            change: ColumnChange::SetGenerated {
                from: Some(GeneratedColumnSchema {
                    expression: "LOWER(email)".to_string(),
                    stored: true,
                }),
                to: None,
            },
        };
        assert_eq!(
            classify_operation(&op, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }

    // ── Codec-transition classification (issue #371) ────────────────────────

    #[test]
    fn classify_codec_transition_routes_per_table() {
        use OnlineSafetyClassification::*;
        // plaintext -> codec: full offline re-encode.
        assert_eq!(
            classify_codec_transition(None, Some("aes256_gcm_v1")),
            OfflineOnly
        );
        // codec -> plaintext: also a full re-encode.
        assert_eq!(
            classify_codec_transition(Some("aes256_gcm_v1"), None),
            OfflineOnly
        );
        // codec -> different codec: expand-contract (online emitter fenced).
        assert_eq!(
            classify_codec_transition(Some("aes256_gcm_v1"), Some("aes256_gcm_v2")),
            ExpandContract
        );
        // identical codec: degenerate no-op (defensive).
        assert_eq!(
            classify_codec_transition(Some("aes256_gcm_v1"), Some("aes256_gcm_v1")),
            OnlineSafe
        );
    }

    #[test]
    fn classify_column_change_routes_codec_change_offline() {
        let (_unused, ctx) = ctx_app(Some(1_000));
        let change = ColumnChange::CodecChange {
            from_codec: None,
            to_codec: Some("aes256_gcm_v1".to_string()),
        };
        // plaintext -> codec is OfflineOnly regardless of estimated rows.
        assert_eq!(
            classify_column_change(&change, &ctx),
            OnlineSafetyClassification::OfflineOnly,
        );
    }
}
