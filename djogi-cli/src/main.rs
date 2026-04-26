//! Djogi CLI — entry point for the `djogi` binary.
//!
//! The CLI is the operator-facing surface for the migration engine,
//! Rhai shell, and database management tooling. Each `clap` leaf
//! delegates to a thin glue function that calls into the `djogi`
//! library; argument parsing is the only meaningful logic here.
//!
//! Phase 7 T6 wires up `migrations compose` and `migrations status`.
//! Other subcommands remain stubbed out — they land with their
//! respective phases.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
}

#[derive(Subcommand)]
enum DbCommand {
    /// Drop, recreate, and migrate the database (dev only).
    Reset,
    /// Run seed script.
    Seed,
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
            DbCommand::Reset => {
                eprintln!("djogi db reset: not yet implemented");
                ExitCode::from(0)
            }
            DbCommand::Seed => {
                eprintln!("djogi db seed: not yet implemented");
                ExitCode::from(0)
            }
        },
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
                workspace,
            } => migrations::attune_cmd(
                record,
                &record_reason,
                squash,
                from.as_deref(),
                publish,
                workspace,
            ),
        },
    }
}
