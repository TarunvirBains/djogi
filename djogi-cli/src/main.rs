//! Djogi CLI — entry point for the `djogi` binary.
//!
//! The CLI is the operator-facing surface for the migration engine,
//! Rhai shell, and database management tooling. Each `clap` leaf
//! delegates to a thin glue function that calls into the `djogi`
//! library; argument parsing is the only meaningful logic here.
//!
//! Phase 7 T6 wires up `migrations compose` and `migrations status`.
//! T7 adds `migrations attune`. T8 adds `db reset`, `db seed`, and
//! the top-level `docs` subcommand.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod db;
mod live;
mod migrations;

#[derive(Parser)]
#[command(name = "djogi", about = "Djogi framework CLI")]
struct Cli {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Subcommand)]
enum TopCommand {
    /// Apply pending migrations.
    Migrate,
    /// Launch interactive Rhai shell.
    Shell,
    /// Database management.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Schema migration tooling (Phase 7).
    Migrations {
        #[command(subcommand)]
        command: MigrationsCommand,
    },
    /// Phase 7.5 live-migration operator surface — drives expand →
    /// backfill → flip → contract sequences for `ExpandContract`-
    /// classified deltas.
    Live {
        #[command(subcommand)]
        command: live::LiveCmd,
    },
    /// Render Markdown documentation from the descriptor inventory.
    ///
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
}

#[derive(Subcommand)]
enum DbCommand {
    /// Drop, recreate, and replay every committed migration against
    /// the application database. **Triple-gated** — refuses unless
    /// (a) `DATABASE_URL` resolves to localhost, (b)
    /// `Djogi.toml::profile != "production"`, and (c) explicit
    /// confirmation is supplied via `--yes` or the interactive
    /// prompt. Logging databases (`crud_log`, `event_log`) are NOT
    /// touched.
    ///
    /// Exit codes: 0 on success, 1 on error (config / network / SQL
    /// / replay), 2 on gate refusal (not localhost, production
    /// profile, missing `--yes`).
    Reset {
        /// Skip the interactive y/N prompt and proceed. Required for
        /// non-interactive invocations (e.g. CI integration suites
        /// that call `db reset` between tests).
        #[arg(long, default_value_t = false)]
        yes: bool,
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
    },
    /// Run operator-authored SQL seed files in `seeds/<database>/`.
    /// Idempotent — re-runs skip seeds whose `V1:<sha256>` checksum
    /// matches the `djogi_seed_runs` ledger; refuses on checksum
    /// drift. Localhost-gated by default.
    ///
    /// `--database <name>` selects BOTH the seed directory and the
    /// connection target. The CLI splices `<name>` into
    /// `database.url`'s path component so seeds always land on the
    /// matching DB; a malformed application URL refuses with exit
    /// code 1.
    ///
    /// Exit codes: 0 on success, 1 on error (config / network / SQL
    /// / checksum drift / malformed URL), 2 on gate refusal
    /// (non-localhost without `--allow-non-localhost`).
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
    ///
    /// Exit codes: 0 on success, 1 on error (config / connect / SQL),
    /// 2 on gate refusal (non-localhost, production profile, missing
    /// `--yes` without `--dry-run`).
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
enum MigrationsCommand {
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
    Status {
        /// Workspace root override (only used when reading
        /// `Djogi.toml`).
        #[arg(long)]
        workspace: Option<PathBuf>,
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
    ///
    /// Exit codes: 0 on success, 1 on runtime error (config / network
    /// / SQL / git), 2 on refusal (gate failure or arg validation).
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        TopCommand::Migrate => {
            eprintln!("djogi migrate: not yet implemented");
            ExitCode::from(0)
        }
        TopCommand::Shell => {
            eprintln!("djogi shell: not yet implemented");
            ExitCode::from(0)
        }
        TopCommand::Db { command } => match command {
            DbCommand::Reset {
                yes,
                maintenance_database,
                workspace,
            } => db::reset_cmd(yes, maintenance_database, workspace),
            DbCommand::Seed {
                database,
                allow_non_localhost,
                workspace,
            } => db::seed_cmd(database, allow_non_localhost, workspace),
            DbCommand::CleanupTestDbs {
                dry_run,
                yes,
                maintenance_database,
                allow_non_localhost,
                workspace,
            } => db::cleanup_test_dbs_cmd(
                dry_run,
                yes,
                maintenance_database,
                allow_non_localhost,
                workspace,
            ),
        },
        TopCommand::Docs { output, workspace } => db::docs_cmd(output, workspace),
        TopCommand::Live { command } => live::dispatch(command),
        TopCommand::Migrations { command } => match command {
            MigrationsCommand::Compose {
                name,
                allow_destructive,
                force_overwrite,
                workspace,
            } => migrations::compose_cmd(&name, allow_destructive, force_overwrite, workspace),
            MigrationsCommand::Status { workspace } => migrations::status_cmd(workspace),
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
                apply,
                record,
                record_ledger,
                &record_reason,
                squash,
                from.as_deref(),
                publish,
                app.as_deref(),
                workspace,
            ),
        },
    }
}
