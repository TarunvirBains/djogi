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
        /// Discard hand-edits to existing migration files (D013
        /// override). Without this flag compose refuses to overwrite
        /// any up or down migration file whose current bytes do NOT
        /// match what the deterministic emitter would freshly produce
        /// — the byte-equality check stands in for a checksum compare
        /// because the emitter is deterministic (same inputs always
        /// produce the same bytes). Per Codex round-2 A-2 the check
        /// is purely byte-level; it does not read the pending JSON's
        /// `checksum_up` field.
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
    /// the ledger. `--record` inserts ledger rows for unrecorded SQL
    /// files. `--squash --from <ver>` collapses local history into a
    /// single migration (localhost + dev profile only).
    Attune {
        /// Insert ledger rows for SQL files present on disk but
        /// absent from the ledger. Records the operator-supplied
        /// reason in `partial_apply_note`. Does NOT execute SQL.
        #[arg(long, default_value_t = false, conflicts_with = "squash")]
        record: bool,
        /// When `--record` is set, the rationale recorded on every
        /// inserted ledger row's `partial_apply_note`.
        #[arg(long, default_value = "operator asserted out-of-band apply")]
        record_reason: String,
        /// Coalesce every committed migration from `--from` to HEAD
        /// into a single squashed migration. HISTORY REWRITE — gated
        /// on localhost + dev profile.
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
        /// multiple buckets (B-5); auto-detected when the version is
        /// unique to one bucket.
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
        },
        TopCommand::Docs { output, workspace } => db::docs_cmd(output, workspace),
        TopCommand::Migrations { command } => match command {
            MigrationsCommand::Compose {
                name,
                allow_destructive,
                force_overwrite,
                workspace,
            } => migrations::compose_cmd(&name, allow_destructive, force_overwrite, workspace),
            MigrationsCommand::Status { workspace } => migrations::status_cmd(workspace),
            MigrationsCommand::Attune {
                record,
                record_reason,
                squash,
                from,
                publish,
                app,
                workspace,
            } => migrations::attune_cmd(
                record,
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
