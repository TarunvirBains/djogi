//! Djogi CLI library — entry points for the `djogi` binary and for
//! adopter-linked binaries that inject their own [`DescriptorProvider`].
//! The published standalone `djogi` binary links no adopter model crates,
//! so reading the global `inventory` registry directly yields zero adopter
//! models. Injecting a [`DescriptorProvider`] lets an adopter-linked binary
//! supply its own models. See [`run_with_provider`].

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod analyze;
mod db;
mod identity;
mod live;
mod migrations;
mod schema;
mod verify;
mod wrapper;

// Re-export CLI types so the thin `main.rs` shim and downstream crates
// can reference them without duplicating definitions.
#[allow(ambiguous_glob_reexports)]
pub use crate::analyze::*;
pub use crate::db::*;
pub use crate::live::*;
pub use crate::migrations::*;
pub use crate::schema::*;
pub use crate::verify::*;
pub use crate::wrapper::run_cargo_wrapper_from_env;

// Re-export proc macros so adopters write `djogi_cli::djogi_main!(…)` and
// `djogi_cli::link_anchor!()` instead of depending on `djogi-macros` directly.
// `link_anchor!` takes no arguments — it is a per-crate marker placed once in
// each model crate's `lib.rs`.
pub use djogi_macros::{djogi_main, link_anchor};

// Re-export the boundary types so adopters/tests can name them without a
// direct `djogi` dependency line.
pub use djogi::migrate::{DescriptorProvider, InventoryDescriptorProvider};

/// Print a support-boundary preflight error to stderr.
/// Used by every CLI entry point that runs `check_postgres_version`.
/// The "support boundary" prefix distinguishes infrastructure refusals
/// (wrong PG version, missing extension) from policy refusals (localhost
/// gate, production profile) and runtime failures (SQL error, network).
pub fn print_support_boundary_error(subcommand: &str, err: &dyn std::fmt::Display) {
    eprintln!("djogi {subcommand}: support boundary: {err}");
}

#[derive(Parser)]
#[command(name = "djogi", about = "Djogi framework CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: TopCommand,
}

#[derive(Subcommand)]
pub enum TopCommand {
    /// Launch interactive Rhai shell.
    Shell,
    /// Database management.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Schema migration tooling .
    Migrations {
        #[command(subcommand)]
        command: MigrationsCommand,
    },
    /// Compatibility alias for `djogi migrations`. See
    /// `djogi migrations --help` for the full command tree.
    /// Currently only `apply` is supported as an alias:
    /// `djogi migrate apply` delegates to `djogi migrations apply`.
    Migrate {
        #[command(subcommand)]
        command: MigrateCommand,
    },
    /// Live-migration operator surface — drives expand →
    /// backfill → flip → contract sequences for `ExpandContract`-
    /// classified deltas.
    /// Requires PostgreSQL 18 or later.
    Live {
        #[command(subcommand)]
        command: live::LiveCmd,
    },
    /// Render Markdown documentation from the descriptor inventory.
    /// One file per registered model under `<output>/<app>/`, plus a
    /// top-level `README.md` index. Output is byte-deterministic
    /// against the same descriptor set.
    Docs {
        /// Output directory. Defaults to
        /// `<workspace>/target/djogi-docs/`.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 6 — read-only HMAC cross-check of every
    /// `migrations/<target>/<app>/schema_snapshot.json` against the
    /// audit DB's `djogi_ddl_audit` ledger.
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// Exit codes: `0` when every snapshot reports `OK` or `Skipped`
    /// (audit table absent or no audit row yet), `1` on any mismatch
    /// or runtime error (config / connect / I/O / key decode).
    /// **Read-only.** Verify never issues `INSERT`, `UPDATE`,
    /// `DELETE`, or DDL — the only SQL leaving the CLI is a
    /// positional-bind `SELECT` against `djogi_ddl_audit`.
    Verify {
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// 2 — JSON descriptor dump.
    /// Emits a deterministic JSON document covering every model
    /// registered via `inventory::submit!`. Use for agent
    /// integration, CI assertions on schema drift, and
    /// machine-readable handoffs to downstream codegen.
    /// **Read-only.** Schema never opens a Postgres connection;
    /// the inventory walk is fully in-process.
    Schema {
        /// Output format. `json` is the only value in v0.1.0;
        /// `openapi` and `markdown` are reserved for .
        #[arg(long, value_enum, default_value_t = SchemaFormat::Json)]
        format: SchemaFormat,
        /// Optional output file. Absent means stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Partition / vacuum analysis for adopter
    /// Postgres tables. Queries `pg_stat_user_tables` (and, when
    /// installed, `pg_partman`) and recommends vacuum / partition
    /// actions per the precedence laid out in [`analyze::Recommendation`].
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// **Read-only.** Analyze issues only `SELECT` against system
    /// catalogues; it never writes.
    Analyze {
        /// Output format. `human` (default) prints one line per table;
        /// `json` emits a deterministic, sorted array of
        /// `{table, recommendation}` objects suitable for CI
        /// dashboards.
        #[arg(long, value_enum, default_value_t = AnalyzeFormat::Human)]
        format: AnalyzeFormat,
        /// Dead-tuple ratio strictly above which `VacuumNeeded` fires.
        /// Default `0.2` (20% bloat) — typical OLTP workloads tighten
        /// this; warehouse workloads tend to leave it as-is. Validated
        /// at parse time via [`parse_threshold_vacuum`]: rejects NaN /
        /// infinity / values outside `[0.0, 1.0]` so silent
        /// "never-fires" misconfigurations are impossible.
        #[arg(long, default_value_t = 0.2, value_parser = parse_threshold_vacuum)]
        threshold_vacuum: f64,
        /// Live row count strictly above which an unpartitioned table
        /// triggers `PartitionRecommended`. Default `10_000_000`. The
        /// same threshold drives the per-partition row average that
        /// fires `PartitionCountIncrease`.
        #[arg(long, default_value_t = 10_000_000)]
        threshold_partition_rows: i64,
        /// Workspace root override. Defaults to the current working
        /// directory. Mirrors `djogi verify --workspace`.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

/// Output format for `djogi schema`. Mirrors
/// [`schema::SchemaFormat`] so `clap::ValueEnum` lives at the CLI
/// boundary and the `schema` module stays clap-free.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SchemaFormat {
    Json,
}

impl SchemaFormat {
    fn into_schema(self) -> schema::SchemaFormat {
        match self {
            SchemaFormat::Json => schema::SchemaFormat::Json,
        }
    }
}

/// Output format for `djogi analyze` — clap-side mirror of
/// [`analyze::AnalyzeFormat`].
/// This enum exists only so `clap::ValueEnum` can derive the
/// `--format human|json` parser without dragging the clap-derive
/// dependency into the `analyze` module's pure-substrate header.
/// Conversion to the canonical [`analyze::AnalyzeFormat`] happens at
/// the dispatch site via [`Self::into_analyze`].
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AnalyzeFormat {
    Human,
    Json,
}

impl AnalyzeFormat {
    /// Project the clap-side enum onto the canonical
    /// [`analyze::AnalyzeFormat`] consumed by [`analyze::run`].
    fn into_analyze(self) -> analyze::AnalyzeFormat {
        match self {
            AnalyzeFormat::Human => analyze::AnalyzeFormat::Human,
            AnalyzeFormat::Json => analyze::AnalyzeFormat::Json,
        }
    }
}

/// Parse + validate `--threshold-vacuum` at the CLI boundary.
/// Rejects three classes of nonsense input that plain `f64::parse`
/// otherwise lets through:
/// 1. **Non-finite values** (`NaN`, `inf`, `-inf`). Without this guard,
///    `ratio > NaN` evaluates to `false` for every ratio, so
///    `VacuumNeeded` would silently never fire — the worst kind of
///    silent failure for a recommendation engine.
/// 2. **Negative values.** A dead-tuple ratio is bounded in `[0.0, 1.0]`
///    by definition (it's `dead / (live + dead)`), so a negative
///    threshold is operator error, not a tuning choice.
/// 3. **Values above `1.0`.** Same reasoning — no real
///    `pg_stat_user_tables` row can produce a ratio above `1.0`, so a
///    threshold above `1.0` would mean "VacuumNeeded never fires," which
///    is again silent failure rather than legitimate configuration.
///    Wired via clap's `value_parser` attribute so the rejection happens at
///    argument-parsing time — operators see a clear error message and a
///    non-zero exit, never a silently-misbehaving analyze run.
fn parse_threshold_vacuum(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|e: std::num::ParseFloatError| e.to_string())?;
    if !v.is_finite() {
        return Err(format!("threshold_vacuum must be finite (got {s})"));
    }
    if !(0.0..=1.0).contains(&v) {
        return Err(format!("threshold_vacuum must be in [0.0, 1.0] (got {v})"));
    }
    Ok(v)
}

#[derive(Subcommand)]
pub enum DbCommand {
    /// Drop, recreate, and replay every committed migration against
    /// the application database. **Triple-gated** — refuses unless
    /// (a) `DATABASE_URL` resolves to localhost, (b)
    /// `Djogi.toml::profile != "production"`, and (c) explicit
    /// confirmation is supplied via `--yes` or the interactive
    /// prompt. Logging databases (`crud_log`, `event_log`) are NOT
    /// touched.
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// Exit codes: 0 on success, 1 on error (config / network / SQL
    /// / replay), 2 on gate refusal (not localhost, production
    /// profile, missing `--yes`, below PG 18).
    Reset {
        /// Skip the interactive y/N prompt and proceed. Required for
        /// non-interactive invocations (e.g. CI integration suites
        /// that call `db reset` between tests).
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Permit `db reset` to continue even when the live ledger's
        /// checksums no longer match the current on-disk migration
        /// files. Without this flag, checksum drift refuses before
        /// the destructive drop / recreate step.
        #[arg(long, default_value_t = false)]
        allow_checksum_drift_reset: bool,
        /// Maintenance database to connect to for the `DROP DATABASE`
        /// then `CREATE DATABASE` round-trip. Defaults to `postgres`,
        /// the conventional administrative DB present on every
        /// cluster. Override only if the cluster has a different
        /// administrative DB (e.g. AWS RDS uses `rdsadmin`).
        #[arg(long, default_value = "postgres")]
        maintenance_database: String,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Explicit cluster node identity (0..=511). Mutually exclusive
        /// with `--single-node-dev`. Selected-node reset is refused —
        /// use `--single-node-dev` for destructive local reset.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,
        /// Single-node development mode — the only permitted node
        /// identity for destructive reset. Refused in production
        /// profile or `DJOGI_ENV=production`.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },
    /// Run operator-authored SQL seed files in `seeds/<database>/`.
    /// Idempotent — re-runs skip seeds whose `V1:<sha256>` checksum
    /// matches the `djogi_seed_runs` ledger; refuses on checksum
    /// drift. Localhost-gated by default.
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// `--database <name>` selects BOTH the seed directory and the
    /// connection target. The CLI splices `<name>` into
    /// `database.url`'s path component so seeds always land on the
    /// matching DB; a malformed application URL refuses with exit
    /// code 1.
    /// Exit codes: 0 on success, 1 on error (config / network / SQL
    /// / checksum drift / malformed URL), 2 on gate refusal
    /// (non-localhost without `--allow-non-localhost`, below PG 18).
    Seed {
        /// Database name whose seeds directory should be run. The
        /// runner walks `seeds/<database>/*.sql` in alphabetical
        /// order.
        #[arg(long, default_value = "main")]
        database: String,
        /// Allow seeds to run against a non-localhost database. The
        /// gate is lighter than `db reset`'s — useful for CI
        /// integration suites seeding a remote test database.
        #[arg(long, default_value_t = false)]
        allow_non_localhost: bool,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Drop orphaned `djogi_test_<uuid>` databases left over from
    /// crashed `#[djogi_test]` runs (SIGKILL / OOM / panic-after-spawn
    /// before teardown could fire). Triple-gated identical to
    /// `db reset` — localhost (override via `--allow-non-localhost`),
    /// non-production profile, explicit `--yes` (waived under
    /// `--dry-run`).
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// Exit codes: 0 on success, 1 on error (config / connect / SQL),
    /// 2 on gate refusal (non-localhost, production profile, missing
    /// `--yes` without `--dry-run`, below PG 18).
    CleanupTestDbs {
        /// List candidates without dropping. Skips the `--yes`
        /// confirmation gate because no destructive side effect
        /// occurs.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Skip the `--yes` confirmation gate. Required for
        /// non-interactive invocations unless `--dry-run` is also set.
        #[arg(long, default_value_t = false)]
        yes: bool,
        /// Maintenance database to connect to. Defaults to `postgres`,
        /// the conventional administrative DB on every cluster.
        /// Override only when the cluster uses a different admin DB
        /// (e.g. AWS RDS uses `rdsadmin`).
        #[arg(long, default_value = "postgres")]
        maintenance_database: String,
        /// Allow cleanup against a non-localhost cluster. Off by
        /// default — the gate matches `db reset`'s localhost
        /// requirement so destructive ops stay local unless the
        /// operator explicitly opts out.
        #[arg(long, default_value_t = false)]
        allow_non_localhost: bool,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum MigrateCommand {
    /// Alias for `djogi migrations apply`. See
    /// `djogi migrations apply --help` for full documentation.
    /// Record pending migrations as applied in the ledger, optionally
    /// without executing their SQL (`--fake`).
    /// See `djogi migrations apply --help` for crash-recovery behavior,
    /// including already-faked reruns and snapshot rebuilds.
    Apply {
        #[arg(long)]
        workspace: Option<PathBuf>,

        #[arg(long, default_value_t = false)]
        fake: bool,

        #[arg(long)]
        reason: Option<String>,

        /// Explicit cluster node identity (0..=511). See
        /// `djogi migrations apply --help` for details.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,

        /// Single-node development mode. See
        /// `djogi migrations apply --help` for details.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },
}

#[derive(Subcommand)]
pub enum MigrationsCommand {
    /// Compose a new migration from descriptor inventory + last
    /// snapshot.
    Compose {
        /// Operator-facing migration name. Sanitised down to a strict
        /// identifier; defaults to `migration` when empty.
        #[arg(long, default_value = "")]
        name: String,
        /// Allow destructive (drop) operations or tombstoned-app
        /// migrations. Without this flag the compose path refuses
        /// destructive deltas with a structural error.
        #[arg(long, default_value_t = false)]
        allow_destructive: bool,
        /// Discard hand-edits to existing migration files. Without
        /// this flag compose refuses to overwrite any up or down
        /// migration file whose current bytes do NOT match what the
        /// deterministic emitter would freshly produce — the
        /// byte-equality check stands in for a checksum compare
        /// because the emitter is deterministic (same inputs always
        /// produce the same bytes). The check is purely byte-level;
        /// it does not read the pending JSON's `checksum_up` field.
        #[arg(long, default_value_t = false)]
        force_overwrite: bool,
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Print the current state of the migration ledger, grouped by
    /// app. Read-only — does not acquire the workspace lock.
    /// Requires PostgreSQL 18 or later.
    Status {
        /// Workspace root override (only used when reading
        /// `Djogi.toml`).
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Compare live database catalog against the schema snapshot.
    /// Read-only — does not acquire the workspace lock or execute DDL.
    /// Exits 0 if no error-level diagnostics are found. Exits 1 on
    /// runtime errors (config / pool / SQL). Exits 2 if the Postgres
    /// server is below version 18.
    /// Use `--strict` to upgrade out-of-order migration warnings (D622)
    /// to errors, causing verify to exit non-zero when the ledger
    /// contains out-of-order applied rows.
    Verify {
        /// Workspace root override (only used when reading `Djogi.toml`).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Upgrade D622 out-of-order diagnostics from Warning to Error.
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
    /// Reconcile local migration history with the ledger. Default
    /// mode is a read-only diff between the on-disk SQL files and
    /// the ledger. Attune is read-only by default — pass `--apply`
    /// to commit ledger inserts / squash / parent-pointer writes.
    /// `--record` updates the parent repo's recorded submodule
    /// pointer to the resolved Git target after successful
    /// attunement. `--squash --from <ver>` collapses local history
    /// into a single migration (localhost + dev_mode + dev profile +
    /// DJOGI_ENV gates).
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// Exit codes: 0 on success, 1 on runtime error (config / network
    /// / SQL / git), 2 on refusal (gate failure, arg validation,
    /// below PG 18).
    Attune {
        /// Optional Git target to attune the local migration history
        /// to — a local or remote commit / tag / branch. When
        /// omitted, attune reconciles against the current on-disk
        /// state. Resolution: tries local first, then `git fetch
        /// --all` + retries on failure.
        target: Option<String>,
        /// Mutate the database / parent index. Without `--apply`,
        /// attune is a dry-run — it scans, prints the diff, and
        /// exits without inserting / deleting ledger rows or updating
        /// the parent submodule pointer (per
        /// `docs/spec/configuration.md` §14: "does not mutate the
        /// database unless `--apply` is explicitly passed").
        #[arg(long, default_value_t = false)]
        apply: bool,
        /// In Record mode (`--record-ledger`), insert ledger rows for
        /// SQL files present on disk but absent from the ledger. With
        /// a resolved `<target>` argument AND `--apply`, also update
        /// the parent repo's recorded submodule pointer to the target
        /// SHA.
        #[arg(long, default_value_t = false)]
        record: bool,
        /// Activate Record mode — insert ledger rows for SQL files
        /// present on disk but absent from the ledger. Distinct from
        /// `--record` (which controls the parent submodule pointer).
        /// Records the operator-supplied reason in `partial_apply_note`.
        /// Does NOT execute SQL.
        #[arg(
            long = "record-ledger",
            default_value_t = false,
            conflicts_with = "squash"
        )]
        record_ledger: bool,
        /// When `--record-ledger` is set, the rationale recorded on
        /// every inserted ledger row's `partial_apply_note`.
        #[arg(long, default_value = "operator asserted out-of-band apply")]
        record_reason: String,
        /// Coalesce every committed migration from `--from` to HEAD
        /// into a single squashed migration. HISTORY REWRITE — gated
        /// on localhost + dev profile + dev_mode + DJOGI_ENV.
        #[arg(long, default_value_t = false)]
        squash: bool,
        /// Inclusive starting version for `--squash` (e.g.
        /// `V20260101000000__init`).
        #[arg(long)]
        from: Option<String>,
        /// After a successful squash, push the rewritten
        /// `migrations/` submodule to its remote. Without this flag
        /// the rewrite stays local. Squash NEVER auto-publishes.
        #[arg(long, default_value_t = false)]
        publish: bool,
        /// Optional explicit app label to scope `--squash` to a
        /// single bucket. Required when `--from` matches a version in
        /// multiple buckets; auto-detected when the version is unique
        /// to one bucket.
        #[arg(long)]
        app: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Apply all pending migrations in ledger order. This is the canonical spelling;
    /// `djogi migrate apply` is a compatibility alias.
    /// **Transaction semantics** are per-segment: transactional
    /// segments roll back on error; non-transactional segments
    /// autocommit and may leave partial progress.
    /// **On crash** or unexpected termination, re-run
    /// `djogi migrations apply`. For partial non-transactional
    /// progress, use `djogi migrations repair resume-partial`.
    /// **Existing-database adoption:** use `--fake` to mark pending
    /// migrations as applied without executing their SQL. This is for
    /// databases whose schema already exists (from a prior tool, manual
    /// DDL, or restored backup). Use `djogi migrations verify` or
    /// manual inspection to confirm the schema matches the target state
    /// before faking. The `--fake` flag respects the same out-of-order
    /// policy as real apply; if CI/prod policy is `Reject`, fake-apply
    /// on an out-of-order version is also rejected.
    /// **Drift pre-flight:** real apply verifies the live catalog
    /// against the recorded `schema_snapshot.json` before executing
    /// SQL and refuses with exit `2` when error-severity drift is
    /// found. The check self-skips only when the bucket has never
    /// been applied; a missing snapshot on a previously-applied
    /// bucket is itself a refusal (exit `2`) and should be repaired
    /// with `djogi migrations repair snapshot-rebuild` or restored
    /// from version control. `--fake` neither runs the pre-flight nor
    /// reads the snapshot file.
    /// **Node identity:** for operations that execute SQL, supply
    /// `--node-id <id>` (explicit cluster node) or
    /// `--single-node-dev` (dev mode, binds node 1). Mutually exclusive.
    /// Falls back to `HEER_NODE_ID` env var when neither flag is set.
    /// Refuses without identity for non-dev operations (exit 2).
    /// For previewing pending work without executing it, use
    /// `djogi migrations status`.
    /// If the command is interrupted after recording a ledger row with
    /// a terminal status (`applied`, `faked`, `baseline`), re-running
    /// reports `VersionAlreadyApplied` (exit 2). For non-terminal
    /// statuses (`failed`, `rolled_back`), the stale row is removed and
    /// re-apply proceeds automatically. If the snapshot is missing or
    /// stale, reconcile it with `djogi migrations attune` or
    /// `repair snapshot-rebuild`.
    Apply {
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,

        /// Record pending migrations as applied without executing
        /// their SQL. For existing-database adoption only. Requires
        /// `--reason`. Subject to the same out-of-order policy as real
        /// apply; if CI/prod policy is `Reject`, fake-apply on an
        /// out-of-order version is also rejected.
        #[arg(long, default_value_t = false)]
        fake: bool,

        /// Reason for faking these migrations. Required when `--fake`
        /// is set. Persisted to the ledger's audit trail so future
        /// inspections can understand why this version was recorded
        /// without SQL execution. Has no effect on normal (non-fake)
        /// apply.
        #[arg(long)]
        reason: Option<String>,

        /// Explicit cluster node identity (0..=511). Wins over
        /// `HEER_NODE_ID` env var. Mutually exclusive with
        /// `--single-node-dev`. Required for identity-bearing operations
        /// unless `--single-node-dev` is supplied or `HEER_NODE_ID` is set.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,

        /// Single-node development mode — binds node 1 for the duration
        /// of this operation. Mutually exclusive with `--node-id`.
        /// Refused in production profile or `DJOGI_ENV=production`.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },
    /// Roll back applied migrations in reverse ledger insertion order.
    /// Use `--to <version>` to stop once `<version>` remains applied, and
    /// `--dry-run` to preview the selected target set without executing SQL.
    /// Lossy rollback stays fail-closed unless `--allow-data-loss` and
    /// `--reason` are both supplied.
    Rollback {
        /// Stop once this version remains applied. Versions newer than
        /// `--to` are selected for rollback; `--to` itself is kept.
        #[arg(long)]
        to: Option<String>,
        /// Preview the selected rollback set without executing SQL or
        /// mutating the ledger/snapshot.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Permit lossy rollback when the committed down SQL is marked
        /// as data-losing. Requires `--reason`.
        #[arg(long, default_value_t = false, requires = "reason")]
        allow_data_loss: bool,
        /// Audit-trail reason recorded when `--allow-data-loss` is used.
        /// Only meaningful alongside `--allow-data-loss`; supplying it
        /// alone is a parse error.
        #[arg(long, requires = "allow_data_loss")]
        reason: Option<String>,
        /// App label for the migration bucket. Defaults to the global
        /// bucket when not specified.
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// Workspace root override. Defaults to the current working
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Explicit cluster node identity (0..=511). Required for
        /// SQL-executing rollback unless `--single-node-dev` is supplied.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,
        /// Single-node development mode — binds node 1 for rollback.
        /// Refused in production profile or `DJOGI_ENV=production`.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },
    /// Operator-confirmed repair flows for ledger drift, partial
    /// applies, and missing snapshots. Every subcommand requires
    /// explicit confirmation — invoking the CLI subcommand IS the
    /// operator acknowledgment.
    Repair {
        /// The specific repair operation to perform.
        #[command(subcommand)]
        command: RepairSubcommand,
    },
    /// Project the live database schema into a baseline ledger row and
    /// snapshot. Use for existing databases being adopted under Djogi's
    /// migration ledger, where the schema already exists and compose +
    /// apply cannot run on a populated database without a starting point.
    /// Projects the live catalog into a single `baseline` ledger row
    /// (no SQL runs against user tables) and writes the projected
    /// snapshot so future migrations diff against the real DB state.
    /// Invoking the subcommand IS the operator acknowledgment.
    /// Requires PostgreSQL 18 or later — exits with code 2 if the
    /// server is below the minimum.
    /// Exit codes: 0 on success, 1 on runtime error (config / network /
    /// SQL / projection failure), 2 on refusal (empty `--reason`, duplicate
    /// version, unresolvable database URL, snapshot-persist failure after
    /// ledger insert, session-pinning correctness failure, or below PG 18).
    Baseline {
        /// Version label for the baseline ledger row (e.g.
        /// `V00000000000000__baseline`). Must be unique in the ledger.
        version: String,
        /// One-line description stored in the ledger row.
        #[arg(long, default_value = "existing database schema baseline")]
        description: String,
        /// Required non-empty reason recorded in the baseline note
        /// (audit trail entry).
        #[arg(long)]
        reason: String,
        /// App label for the migration bucket. Defaults to the global
        /// bucket (empty string) when not specified.
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Explicit cluster node identity (0..=511). Required for
        /// baseline unless `--single-node-dev` is supplied.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,
        /// Single-node development mode — binds node 1 for baseline.
        /// Refused in production profile or `DJOGI_ENV=production`.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },
}

/// `djogi migrations repair <subcommand>` — the four operator-confirmed
/// repair flows.
/// Each variant maps 1:1 onto a `djogi::migrate::repair::*` library
/// function. Invoking the subcommand IS the operator acknowledgment;
/// there is no separate `--confirm` flag. Every flow pins one Postgres
/// session, takes the per-bucket advisory lock, and holds the workspace
/// file lock for its duration.
/// Exit codes (shared across all four): `0` success, `1`
/// runtime/I/O error (retryable), `2` refusal or structural mismatch
/// (operator must intervene).
#[derive(Clone, Subcommand)]
pub enum RepairSubcommand {
    /// Update ledger checksum when migration file content changed
    /// but the row was already applied.
    ChecksumDrift {
        /// Migration version (e.g. `V20260101000000__add_users`).
        version: String,
        /// App label for the migration bucket. Defaults to the global
        /// bucket (empty string) when not specified.
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// New `checksum_up` value (SHA-256 hex). If omitted, computed
        /// from the committed up SQL file.
        #[arg(long)]
        checksum_up: Option<String>,
        /// New `checksum_down` value (SHA-256 hex). If omitted and
        /// down file exists, computed from committed down SQL file.
        /// Missing down file is a no-op; other read errors abort.
        #[arg(long)]
        checksum_down: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Resolve a partial-apply row by rewriting its status to one of
    /// `rolled_back`, `faked`, or `applied`. Does NOT execute SQL.
    PartialApply {
        /// Migration version to repair.
        version: String,
        /// Resolution: `rolled-back`, `faked`, or `applied`.
        #[arg(value_enum)]
        resolution: PartialApplyResolutionCli,
        /// Operator note persisted in the ledger row's
        /// `partial_apply_note` column.
        #[arg(long, default_value = "operator resolved partial apply via CLI")]
        note: String,
        /// App label (empty string for global bucket).
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },

    /// Resume an interrupted non-transactional apply by re-loading
    /// the committed replay plan and executing remaining steps.
    ResumePartial {
        /// Migration version to resume.
        version: String,
        /// App label (empty string for global bucket).
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Explicit cluster node identity (0..=511). Required for
        /// SQL-executing resume unless `--single-node-dev` is supplied.
        #[arg(long, conflicts_with = "single_node_dev")]
        node_id: Option<u32>,
        /// Single-node development mode — binds node 1 for resume.
        /// Refused in production profile or `DJOGI_ENV=production`.
        #[arg(long, default_value_t = false)]
        single_node_dev: bool,
    },

    /// Rebuild the schema snapshot for a bucket by walking the
    /// ledger and re-projecting from live database state.
    SnapshotRebuild {
        /// App label (empty string for global bucket).
        #[arg(long)]
        app: Option<String>,
        /// Database name. Defaults to `main` if not specified.
        #[arg(long)]
        database: Option<String>,
        /// Explicit snapshot path override. If omitted, derived from
        /// `migrations/<database>/<app>/schema_snapshot.json`.
        #[arg(long)]
        snapshot_path: Option<PathBuf>,
        /// Workspace root override.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

/// CLI-side mirror of [`djogi::migrate::PartialApplyResolution`] for the
/// `repair partial-apply` resolution argument.
/// This enum exists only so `clap::ValueEnum` can parse
/// `rolled-back | faked | applied` at the CLI boundary without the
/// library enum carrying a clap-derive dependency. Conversion to the
/// canonical [`djogi::migrate::PartialApplyResolution`] happens via the
/// `From` impl in the `migrations` module.
#[derive(clap::ValueEnum, Clone, Debug)]
pub enum PartialApplyResolutionCli {
    RolledBack,
    Faked,
    Applied,
}

// ── Entrypoints ───────────────────────────────────────────────────────────

/// Run the CLI by parsing arguments from `std::env::args_os()`.
/// This is the entry point used by the published standalone `djogi`
/// binary. It reads the global link-time [`inventory`] registry via
/// [`djogi::migrate::InventoryDescriptorProvider`].
pub fn run_from_env() -> ExitCode {
    let cli = match Cli::try_parse_from(std::env::args_os()) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };
    dispatch_command(
        &cli.command,
        &djogi::migrate::InventoryDescriptorProvider::new(),
    )
}

/// Run the CLI with an explicit argument iterable. Useful for testing and
/// embedding.
/// Accepts any `IntoIterator<Item = T>` where `T: Into<OsString> + Clone`,
/// matching the bound of [`clap::Parser::try_parse_from`]. In practice,
/// arrays of `&str` (e.g. `["djogi", "migrations", "compose"]`) and
/// `Vec<String>` both satisfy this bound.
/// Falls back to [`djogi::migrate::InventoryDescriptorProvider`] for
/// descriptors.
pub fn run_with_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // Print the clap error / `--help` / `--version` text before
            // returning, matching `run_from_env`. Without this, parse
            // errors and `--help` would be silent.
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };
    dispatch_command(
        &cli.command,
        &djogi::migrate::InventoryDescriptorProvider::new(),
    )
}

/// Run the CLI with an explicit argument iterable and a [`DescriptorProvider`].
/// Accepts any `IntoIterator<Item = T>` where `T: Into<OsString> + Clone`,
/// matching the bound of [`clap::Parser::try_parse_from`].
/// Adopter-linked binaries pass their own provider so descriptor-dependent
/// commands (`compose`, `verify`, `schema`, `docs`) see the adopter's
/// models instead of an empty inventory.
pub fn run_with_provider<I, T>(
    args: I,
    provider: &dyn djogi::migrate::DescriptorProvider,
) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // Print the clap error / `--help` / `--version` text before
            // returning, matching `run_from_env`.
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { 2 } else { 0 });
        }
    };
    dispatch_command(&cli.command, provider)
}

// ── Dispatch ──────────────────────────────────────────────────────────────

fn dispatch_command(
    command: &TopCommand,
    provider: &dyn djogi::migrate::DescriptorProvider,
) -> ExitCode {
    match command {
        TopCommand::Shell => {
            eprintln!("djogi shell: not yet implemented");
            ExitCode::from(0)
        }
        TopCommand::Db { command } => match command {
            DbCommand::Reset {
                yes,
                allow_checksum_drift_reset,
                maintenance_database,
                workspace,
                node_id,
                single_node_dev,
            } => db::reset_cmd(
                *yes,
                *allow_checksum_drift_reset,
                maintenance_database.clone(),
                workspace.clone(),
                *node_id,
                *single_node_dev,
            ),
            DbCommand::Seed {
                database,
                allow_non_localhost,
                workspace,
            } => db::seed_cmd(database.clone(), *allow_non_localhost, workspace.clone()),
            DbCommand::CleanupTestDbs {
                dry_run,
                yes,
                maintenance_database,
                allow_non_localhost,
                workspace,
            } => db::cleanup_test_dbs_cmd(
                *dry_run,
                *yes,
                maintenance_database.clone(),
                *allow_non_localhost,
                workspace.clone(),
            ),
        },
        TopCommand::Docs { output, workspace } => {
            if provider.models().is_empty() {
                print_zero_descriptor_diagnostic("docs");
                return ExitCode::from(2);
            }
            db::docs_cmd(provider, output.clone(), workspace.clone())
        }
        TopCommand::Live { command } => live::dispatch(command.clone()),
        TopCommand::Verify { workspace } => {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("djogi verify: tokio runtime: {e}");
                    return ExitCode::from(1);
                }
            };
            match runtime.block_on(verify::run(workspace.clone())) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("djogi verify: {e}");
                    ExitCode::from(1)
                }
            }
        }
        TopCommand::Schema { format, output } => {
            let models: Vec<&'static djogi::descriptor::ModelDescriptor> = provider.models();
            if models.is_empty() {
                print_zero_descriptor_diagnostic("schema");
                return ExitCode::from(2);
            }
            match schema::run(format.into_schema(), &models, output.clone()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("djogi schema: {e}");
                    ExitCode::from(1)
                }
            }
        }
        TopCommand::Analyze {
            format,
            threshold_vacuum,
            threshold_partition_rows,
            workspace,
        } => {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("djogi analyze: tokio runtime: {e}");
                    return ExitCode::from(1);
                }
            };
            match runtime.block_on(analyze::run(
                workspace.clone(),
                format.into_analyze(),
                *threshold_vacuum,
                *threshold_partition_rows,
            )) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("djogi analyze: {e}");
                    ExitCode::from(1)
                }
            }
        }
        TopCommand::Migrations { command } => match command {
            MigrationsCommand::Compose {
                name,
                allow_destructive,
                force_overwrite,
                workspace,
            } => {
                if provider.models().is_empty() {
                    print_zero_descriptor_diagnostic("migrations compose");
                    return ExitCode::from(2);
                }
                migrations::compose_cmd(
                    provider,
                    name.as_str(),
                    *allow_destructive,
                    *force_overwrite,
                    workspace.clone(),
                )
            }
            MigrationsCommand::Status { workspace } => migrations::status_cmd(workspace.clone()),
            MigrationsCommand::Verify { workspace, strict } => {
                migrations::verify_cmd(provider, workspace.clone(), *strict)
            }
            MigrationsCommand::Attune {
                target,
                apply,
                record,
                record_ledger,
                record_reason,
                squash,
                from,
                publish,
                app,
                workspace,
            } => migrations::attune_cmd(
                target.as_deref(),
                *apply,
                *record,
                *record_ledger,
                record_reason.as_str(),
                *squash,
                from.as_deref(),
                *publish,
                app.as_deref(),
                workspace.clone(),
            ),
            MigrationsCommand::Apply {
                workspace,
                fake,
                reason,
                node_id,
                single_node_dev,
            } => migrations::apply_cmd(
                workspace.clone(),
                *fake,
                reason.clone(),
                *node_id,
                *single_node_dev,
            ),
            MigrationsCommand::Rollback {
                to,
                dry_run,
                allow_data_loss,
                reason,
                app,
                database,
                workspace,
                node_id,
                single_node_dev,
            } => migrations::rollback_cmd(
                to.clone(),
                *dry_run,
                *allow_data_loss,
                reason.clone(),
                app.as_deref(),
                database.as_deref(),
                workspace.clone(),
                *node_id,
                *single_node_dev,
            ),
            MigrationsCommand::Repair { command } => migrations::repair_cmd(command.clone()),
            MigrationsCommand::Baseline {
                version,
                description,
                reason,
                app,
                database,
                workspace,
                node_id,
                single_node_dev,
            } => migrations::baseline_cmd(
                version,
                description,
                reason,
                app.as_deref(),
                database.as_deref(),
                workspace.clone(),
                *node_id,
                *single_node_dev,
            ),
        },
        TopCommand::Migrate { command } => match command {
            MigrateCommand::Apply {
                workspace,
                fake,
                reason,
                node_id,
                single_node_dev,
            } => migrations::apply_cmd(
                workspace.clone(),
                *fake,
                reason.clone(),
                *node_id,
                *single_node_dev,
            ),
        },
    }
}

/// Print the §5.6 dual-cause diagnostic when a descriptor-dependent
/// command (`compose` / `verify` / `schema` / `docs`) resolves zero model
/// descriptors, and exits the command with code `2` (refusal — the
/// command refuses because it cannot see the schema it needs).
/// The message is dual-cause because zero descriptors has two distinct
/// causes the operator must be able to tell apart:
/// 1. they ran the *standalone published* `djogi`, which links no
///    application models (build an adopter-linked `djogi` and run from it;
///    the standalone binary can still `migrations apply`); or
/// 2. this *is* their adopter-linked `djogi` but the linker dropped an
///    unreferenced model crate (ensure every `#[derive(Model)]` crate is
///    referenced via `link_models` / `djogi_main!`).
///    The first line is kept verbatim in sync with the troubleshooting
///    anchor in `docs/guide/adopter-cli.md` ("no djogi models are registered
///    in this binary") so an operator who searches the message lands on the
///    guide section that explains it.
///    `command` is the failing command name (e.g. `"migrations compose"`),
///    echoed so the operator knows which invocation refused. The single
///    emitter feeds `compose`, `verify`, `schema`, and `docs`, so one message
///    covers all four.
pub(crate) fn print_zero_descriptor_diagnostic(command: &str) {
    eprintln!("error: no djogi models are registered in this binary (djogi {command}).");
    eprintln!();
    eprintln!("Descriptor-dependent commands (compose, verify, schema, docs) require a");
    eprintln!("djogi binary linked with your model crates.");
    eprintln!();
    eprintln!("  • If you ran the standalone published `djogi`: that binary links no");
    eprintln!("    application models. Build an adopter-linked `djogi` (see the adopter");
    eprintln!("    CLI guide: docs/guide/adopter-cli.md) and run the command from it.");
    eprintln!("    The standalone binary can still run `djogi migrations apply` against");
    eprintln!("    already-composed pending artifacts.");
    eprintln!();
    eprintln!("  • If this IS your adopter-linked `djogi`: ensure your bin references");
    eprintln!("    every crate that defines `#[derive(Model)]` (link_models / djogi_main!),");
    eprintln!("    or the linker may have dropped an unreferenced model crate.");
}

#[cfg(test)]
/// Single process-wide lock for tests that mutate process env vars.
/// `std::sync::Mutex` is non-reentrant: do not hold two env guards on
/// the same thread or the second lock attempt will deadlock.
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    ENV_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap()
}

#[cfg(test)]
mod tests {
    //! CLI-level argument-parsing tests. These exercise the `value_parser`
    //! attached to `--threshold-vacuum` directly; the goal is to pin the
    //! contract that nonsense input fails at parse time rather than
    //! silently producing a recommendation engine that "never fires."

    use clap::Parser as _;

    use std::path::PathBuf;

    use super::{
        Cli, DbCommand, MigrateCommand, MigrationsCommand, PartialApplyResolutionCli,
        RepairSubcommand, TopCommand, parse_threshold_vacuum,
    };

    #[test]
    fn parse_threshold_vacuum_accepts_valid_values() {
        assert_eq!(parse_threshold_vacuum("0.0").unwrap(), 0.0);
        assert_eq!(parse_threshold_vacuum("0.2").unwrap(), 0.2);
        assert_eq!(parse_threshold_vacuum("1.0").unwrap(), 1.0);
        // Boundary check: strictly inside the closed interval.
        assert_eq!(parse_threshold_vacuum("0.5").unwrap(), 0.5);
    }

    #[test]
    fn parse_threshold_vacuum_rejects_nan_inf_and_out_of_range() {
        // NaN — the entire reason this validator exists. `ratio > NaN`
        // is always false, so silent acceptance would mean VacuumNeeded
        // never fires, ever.
        let err = parse_threshold_vacuum("NaN").unwrap_err();
        assert!(err.contains("finite"), "err: {err}");

        // Positive infinity — same silent-failure mode.
        let err = parse_threshold_vacuum("inf").unwrap_err();
        assert!(err.contains("finite"), "err: {err}");

        // Negative infinity.
        let err = parse_threshold_vacuum("-inf").unwrap_err();
        assert!(err.contains("finite"), "err: {err}");

        // Negative finite — outside `[0.0, 1.0]`.
        let err = parse_threshold_vacuum("-0.1").unwrap_err();
        assert!(err.contains("[0.0, 1.0]"), "err: {err}");

        // Above 1.0 — outside `[0.0, 1.0]`.
        let err = parse_threshold_vacuum("1.5").unwrap_err();
        assert!(err.contains("[0.0, 1.0]"), "err: {err}");

        // Garbage — propagates the underlying ParseFloatError message.
        assert!(parse_threshold_vacuum("not-a-number").is_err());
    }

    #[test]
    fn db_reset_parses_allow_checksum_drift_reset_flag() {
        let cli = Cli::try_parse_from([
            "djogi",
            "db",
            "reset",
            "--yes",
            "--allow-checksum-drift-reset",
        ])
        .expect("flag should parse");

        match cli.command {
            TopCommand::Db {
                command:
                    DbCommand::Reset {
                        yes,
                        allow_checksum_drift_reset,
                        ..
                    },
            } => {
                assert!(yes, "--yes should parse through");
                assert!(
                    allow_checksum_drift_reset,
                    "checksum-drift override flag should parse through"
                );
            }
            _ => panic!("expected db reset command"),
        }
    }

    #[test]
    fn migrate_apply_alias_parses() {
        let cli = Cli::try_parse_from(["djogi", "migrate", "apply"])
            .expect("migrate apply should parse as alias");

        match cli.command {
            TopCommand::Migrate {
                command: MigrateCommand::Apply { .. },
            } => {}
            _ => panic!("expected migrate apply command"),
        }
    }

    #[test]
    fn canonical_migrations_apply_parses() {
        let cli = Cli::try_parse_from(["djogi", "migrations", "apply"])
            .expect("canonical migrations apply should parse");

        match cli.command {
            TopCommand::Migrations {
                command: MigrationsCommand::Apply { .. },
            } => {}
            _ => panic!("expected migrations apply command"),
        }
    }

    #[test]
    fn canonical_migrations_status_still_parses() {
        let cli = Cli::try_parse_from(["djogi", "migrations", "status"])
            .expect("canonical migrations status should parse");

        match cli.command {
            TopCommand::Migrations {
                command: MigrationsCommand::Status { .. },
            } => {}
            _ => panic!("expected migrations status command"),
        }
    }

    #[test]
    fn migrations_verify_parses_with_defaults() {
        let cli = Cli::try_parse_from(["djogi", "migrations", "verify"])
            .expect("migrations verify should parse with no flags");

        match cli.command {
            TopCommand::Migrations {
                command: MigrationsCommand::Verify { workspace, strict },
            } => {
                assert!(workspace.is_none());
                assert!(!strict);
            }
            _ => panic!("expected migrations verify command"),
        }
    }

    #[test]
    fn migrations_verify_parses_with_strict() {
        let cli = Cli::try_parse_from(["djogi", "migrations", "verify", "--strict"])
            .expect("migrations verify --strict should parse");

        match cli.command {
            TopCommand::Migrations {
                command: MigrationsCommand::Verify { strict, .. },
            } => {
                assert!(strict);
            }
            _ => panic!("expected migrations verify command"),
        }
    }

    #[test]
    fn migrations_verify_parses_with_workspace() {
        let cli = Cli::try_parse_from([
            "djogi",
            "migrations",
            "verify",
            "--workspace",
            "/custom/path",
        ])
        .expect("migrations verify --workspace should parse");

        match cli.command {
            TopCommand::Migrations {
                command: MigrationsCommand::Verify { workspace, .. },
            } => {
                assert_eq!(workspace, Some(PathBuf::from("/custom/path")));
            }
            _ => panic!("expected migrations verify command"),
        }
    }

    // ── repair subcommand argument parsing ─────────────────────────────────

    #[test]
    fn parse_repair_checksum_drift_accepts_required_args() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "checksum-drift",
            "V20260101000000__test",
            "--checksum-up",
            "V1:aaaa",
        ]);
        assert!(matches!(cli.command, TopCommand::Migrations { .. }));
    }

    #[test]
    fn parse_repair_checksum_drift_rejects_missing_version() {
        let result = Cli::try_parse_from(["djogi", "migrations", "repair", "checksum-drift"]);
        assert!(result.is_err(), "must require version argument");
    }

    #[test]
    fn parse_repair_partial_apply_accepts_resolution_values() {
        for resolution in ["rolled-back", "faked", "applied"] {
            let cli = Cli::parse_from([
                "djogi",
                "migrations",
                "repair",
                "partial-apply",
                "V20260101000000__test",
                resolution,
            ]);
            assert!(
                matches!(cli.command, TopCommand::Migrations { .. }),
                "resolution={resolution}"
            );
        }
    }

    #[test]
    fn parse_repair_partial_apply_rejects_invalid_resolution() {
        let result = Cli::try_parse_from([
            "djogi",
            "migrations",
            "repair",
            "partial-apply",
            "V20260101000000__test",
            "invalid-resolution",
        ]);
        assert!(result.is_err(), "must reject unknown resolution");
    }

    #[test]
    fn parse_repair_resume_partial_accepts_version() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "resume-partial",
            "V20260101000000__test",
        ]);
        assert!(matches!(cli.command, TopCommand::Migrations { .. }));
    }

    #[test]
    fn parse_repair_snapshot_rebuild_accepts_flags() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "snapshot-rebuild",
            "--app",
            "myapp",
        ]);
        assert!(matches!(cli.command, TopCommand::Migrations { .. }));
    }

    // Field-binding destructuring tests — one per subcommand that carries
    // arguments. The outer-shape `matches!(..)` tests above prove the
    // variant is reached; these prove the named clap fields actually bind
    // to the supplied values (catching a `#[arg(long)]` typo or a
    // positional/flag mix-up that an outer-shape assertion would miss).

    #[test]
    fn parse_repair_checksum_drift_binds_version_and_checksum_up() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "checksum-drift",
            "V20260101000000__add_users",
            "--checksum-up",
            "V1:aaaa",
        ]);
        if let TopCommand::Migrations {
            command: MigrationsCommand::Repair { command },
        } = cli.command
        {
            if let RepairSubcommand::ChecksumDrift {
                version,
                checksum_up,
                ..
            } = command
            {
                assert_eq!(version, "V20260101000000__add_users");
                assert_eq!(checksum_up.as_deref(), Some("V1:aaaa"));
            } else {
                panic!("wrong variant");
            }
        } else {
            panic!("wrong command");
        }
    }

    #[test]
    fn parse_repair_partial_apply_binds_resolution_and_note() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "partial-apply",
            "V20260101000000__add_users",
            "rolled-back",
            "--note",
            "reverted by hot-fix",
        ]);
        if let TopCommand::Migrations {
            command: MigrationsCommand::Repair { command },
        } = cli.command
        {
            if let RepairSubcommand::PartialApply {
                version,
                resolution,
                note,
                ..
            } = command
            {
                assert_eq!(version, "V20260101000000__add_users");
                assert!(matches!(resolution, PartialApplyResolutionCli::RolledBack));
                assert_eq!(note, "reverted by hot-fix");
            } else {
                panic!("wrong variant");
            }
        } else {
            panic!("wrong command");
        }
    }

    #[test]
    fn parse_repair_snapshot_rebuild_binds_app_and_database() {
        let cli = Cli::parse_from([
            "djogi",
            "migrations",
            "repair",
            "snapshot-rebuild",
            "--app",
            "billing",
            "--database",
            "analytics",
        ]);
        if let TopCommand::Migrations {
            command: MigrationsCommand::Repair { command },
        } = cli.command
        {
            if let RepairSubcommand::SnapshotRebuild { app, database, .. } = command {
                assert_eq!(app.as_deref(), Some("billing"));
                assert_eq!(database.as_deref(), Some("analytics"));
            } else {
                panic!("wrong variant");
            }
        } else {
            panic!("wrong command");
        }
    }

    // ── baseline subcommand argument parsing ───────────────────────────────

    /// Extract the `MigrationsCommand::Baseline` variant from a parsed
    /// `Cli`, panicking on any other shape. Used by the baseline
    /// field-binding tests below so each test reads as a flat sequence
    /// of field assertions rather than nested `if let`s.
    fn baseline_command(cli: Cli) -> MigrationsCommand {
        match cli.command {
            TopCommand::Migrations {
                command: command @ MigrationsCommand::Baseline { .. },
            } => command,
            _ => panic!("expected migrations baseline command"),
        }
    }

    #[test]
    fn parse_baseline_accepts_required_args() {
        let cli = Cli::try_parse_from([
            "djogi",
            "migrations",
            "baseline",
            "V00000000000000__baseline",
            "--reason",
            "schema pre-exists from prior tooling",
        ])
        .unwrap();
        let MigrationsCommand::Baseline {
            version,
            reason,
            description,
            app,
            database,
            ..
        } = baseline_command(cli)
        else {
            panic!("expected Baseline");
        };
        assert_eq!(version, "V00000000000000__baseline");
        assert_eq!(reason, "schema pre-exists from prior tooling");
        assert_eq!(description, "existing database schema baseline");
        assert!(app.is_none());
        assert!(database.is_none());
    }

    #[test]
    fn parse_baseline_rejects_missing_version() {
        let result = Cli::try_parse_from(["djogi", "migrations", "baseline", "--reason", "test"]);
        assert!(
            result.is_err(),
            "baseline without version positional should fail"
        );
    }

    #[test]
    fn parse_baseline_rejects_missing_reason() {
        let result = Cli::try_parse_from([
            "djogi",
            "migrations",
            "baseline",
            "V00000000000000__baseline",
        ]);
        assert!(result.is_err(), "baseline without --reason should fail");
    }

    #[test]
    fn parse_baseline_accepts_optional_flags() {
        let cli = Cli::try_parse_from([
            "djogi",
            "migrations",
            "baseline",
            "V00000000000000__baseline",
            "--reason",
            "existing schema",
            "--description",
            "custom description",
            "--app",
            "billing",
            "--database",
            "crud_log",
        ])
        .unwrap();
        let MigrationsCommand::Baseline {
            version,
            reason,
            description,
            app,
            database,
            ..
        } = baseline_command(cli)
        else {
            panic!("expected Baseline");
        };
        assert_eq!(version, "V00000000000000__baseline");
        assert_eq!(reason, "existing schema");
        assert_eq!(description, "custom description");
        assert_eq!(app.as_deref(), Some("billing"));
        assert_eq!(database.as_deref(), Some("crud_log"));
    }

    // ── rollback subcommand argument parsing ───────────────────────────────

    /// Extract the `MigrationsCommand::Rollback` variant from a parsed
    /// `Cli`, panicking on any other shape.
    fn rollback_command(cli: Cli) -> MigrationsCommand {
        match cli.command {
            TopCommand::Migrations {
                command: command @ MigrationsCommand::Rollback { .. },
            } => command,
            _ => panic!("expected migrations rollback command"),
        }
    }

    #[test]
    fn parse_rollback_accepts_required_reason_for_lossy_opt_in() {
        let cli = Cli::try_parse_from([
            "djogi",
            "migrations",
            "rollback",
            "--allow-data-loss",
            "--reason",
            "operator confirmed rollback",
        ])
        .unwrap();
        let MigrationsCommand::Rollback {
            to,
            dry_run,
            allow_data_loss,
            reason,
            app,
            database,
            node_id,
            single_node_dev,
            ..
        } = rollback_command(cli)
        else {
            panic!("expected Rollback");
        };
        assert!(to.is_none());
        assert!(!dry_run);
        assert!(allow_data_loss);
        assert_eq!(reason.as_deref(), Some("operator confirmed rollback"));
        assert!(app.is_none());
        assert!(database.is_none());
        assert!(node_id.is_none());
        assert!(!single_node_dev);
    }

    #[test]
    fn parse_rollback_accepts_to_dry_run_and_bucket_flags() {
        let cli = Cli::try_parse_from([
            "djogi",
            "migrations",
            "rollback",
            "--to",
            "V20260101000000__baseline",
            "--dry-run",
            "--app",
            "billing",
            "--database",
            "analytics",
        ])
        .unwrap();
        let MigrationsCommand::Rollback {
            to,
            dry_run,
            allow_data_loss,
            reason,
            app,
            database,
            node_id,
            single_node_dev,
            ..
        } = rollback_command(cli)
        else {
            panic!("expected Rollback");
        };
        assert_eq!(to.as_deref(), Some("V20260101000000__baseline"));
        assert!(dry_run);
        assert!(!allow_data_loss);
        assert!(reason.is_none());
        assert_eq!(app.as_deref(), Some("billing"));
        assert_eq!(database.as_deref(), Some("analytics"));
        assert!(node_id.is_none());
        assert!(!single_node_dev);
    }

    #[test]
    fn parse_rollback_rejects_lossy_opt_in_without_reason() {
        let result = Cli::try_parse_from(["djogi", "migrations", "rollback", "--allow-data-loss"]);
        assert!(
            result.is_err(),
            "rollback --allow-data-loss without --reason should fail"
        );
    }

    #[test]
    fn parse_rollback_rejects_reason_without_allow_data_loss() {
        // `--reason` is only meaningful with `--allow-data-loss`; clap should
        // reject it on its own so an operator cannot silently supply a lossy
        // audit reason that has no effect.
        let result = Cli::try_parse_from([
            "djogi",
            "migrations",
            "rollback",
            "--reason",
            "operator confirmed rollback",
        ]);
        assert!(
            result.is_err(),
            "rollback --reason without --allow-data-loss should fail"
        );
    }

    #[test]
    fn parse_rollback_accepts_allow_data_loss_and_reason_together() {
        let result = Cli::try_parse_from([
            "djogi",
            "migrations",
            "rollback",
            "--allow-data-loss",
            "--reason",
            "operator confirmed rollback",
        ]);
        assert!(
            result.is_ok(),
            "rollback --allow-data-loss with --reason should parse"
        );
    }
}
