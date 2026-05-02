//! elephant-tracker — runnable Djogi example.
//!
//! See `README.md` for an overview. This binary exposes three
//! subcommand groups:
//!
//! - `migrate` — drop and recreate the example tables. The example
//!   pre-dates the full Phase 7 migration runner integration that
//!   adopters will eventually use; the CLI here applies hand-written
//!   DDL via `ctx.raw_ddl` and `ctx.raw_execute`.
//! - `seed` — load `seeds/countries.sql`, then insert herds,
//!   herd ranges, elephants, and sightings programmatically.
//! - `demo <which>` — run one of four feature walkthroughs:
//!   `cluster-sightings`, `cross-border-herds`, `lineage`, or
//!   `herd-summaries`. Most demos accept `--format json|mermaid|markdown`
//!   plus `--out <path>` (default stdout).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod demos;
mod migrate;
mod models;
mod output;
mod seed;
mod visages;

use output::Format;

#[derive(Parser)]
#[command(
    name = "elephant-tracker",
    version,
    about = "Djogi feature walkthrough."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Drop + recreate the example tables. Idempotent.
    Migrate,

    /// Load `seeds/countries.sql` then seed herds + sightings programmatically.
    Seed,

    /// Run a feature walkthrough.
    Demo {
        #[command(subcommand)]
        which: DemoCmd,
    },
}

#[derive(Subcommand)]
enum DemoCmd {
    /// DBSCAN-style spatial clustering over `Sighting.location`.
    ClusterSightings {
        /// Output path; omit to write to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Herds whose ranges span >=2 countries within the same season.
    CrossBorderHerds {
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `mermaid`, `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Walk the matriarchal lineage starting from a named matriarch.
    Lineage {
        /// Matriarch name (e.g. `Wema`). Case-sensitive.
        #[arg(long)]
        matriarch: String,
        /// Maximum descent depth to traverse (default 8).
        #[arg(long, default_value_t = 8)]
        max_depth: i32,
        /// Switch from raw recursive-CTE SQL to the typed
        /// `tree_descendants(ElephantRelated::mother(), id)` builder
        /// from Phase 8-Zero Cluster B. Compose with `--order` to
        /// pick BFS / DFS traversal.
        #[arg(long, default_value_t = false)]
        typed: bool,
        /// Traversal order for `--typed` mode. `default` lets
        /// Postgres pick (typically depth-first per-recursion-step);
        /// `bfs` adds `SEARCH BREADTH FIRST BY estimated_birth_year`
        /// for clean top-down generation bands; `dfs` uses
        /// `SEARCH DEPTH FIRST BY estimated_birth_year` to walk one
        /// matriline chain at a time. Ignored unless `--typed` is set.
        #[arg(long, value_enum, default_value_t = demos::lineage::Order::Default)]
        order: demos::lineage::Order,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `mermaid`, `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// `HerdSummary` projection plus the `herd_size` side query per herd.
    HerdSummaries {
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cli = Cli::parse();

    let database_url = std::env::var("DATABASE_URL").context(
        "DATABASE_URL must be set, e.g. \
         postgres://djogi:djogi@localhost:5432/djogi_test",
    )?;

    // Build the pool through the `DjogiPool` builder. The
    // `post_connect` hook runs once per physical connection and pins
    // `heer.node_id = '1'` for the session — node 1 is the default
    // seeded by `heeranjid::postgres_schema::seed_default_node` in the
    // migrate path. Setting it here at the session level means the
    // example also runs against a database the connecting role does
    // not own (where ALTER DATABASE would fail) — useful for CI
    // sandboxes.
    //
    // The example pins to `"1"` deliberately — supporting an
    // arbitrary `HEER_NODE_ID` would require also seeding the
    // requested node row before persisting the GUC, otherwise
    // `current_heer_node_id()` would later refuse to mint IDs for an
    // unregistered node. Production deployments register their nodes
    // through HeeRanjID's own provisioning path, not through this
    // example.
    let pool = djogi::pg::pool::DjogiPool::builder(&database_url)
        .post_connect(|client| {
            Box::pin(async move {
                client
                    .batch_execute("SET heer.node_id = '1'")
                    .await
                    .map_err(djogi::DjogiError::from)
            })
        })
        .build()
        .await
        .context("failed to construct DjogiPool")?;
    let mut ctx = djogi::DjogiContext::from_pool(pool);

    match cli.cmd {
        Cmd::Migrate => migrate::run(&mut ctx).await?,
        Cmd::Seed => seed::run(&mut ctx).await?,
        Cmd::Demo { which } => match which {
            DemoCmd::ClusterSightings { out, format } => {
                demos::cluster_sightings::run(&mut ctx, format, out.as_deref()).await?
            }
            DemoCmd::CrossBorderHerds { out, format } => {
                demos::cross_border_herds::run(&mut ctx, format, out.as_deref()).await?
            }
            DemoCmd::Lineage {
                matriarch,
                max_depth,
                typed,
                order,
                out,
                format,
            } => {
                demos::lineage::run(
                    &mut ctx,
                    &matriarch,
                    max_depth,
                    typed,
                    order,
                    format,
                    out.as_deref(),
                )
                .await?
            }
            DemoCmd::HerdSummaries { out, format } => {
                demos::herd_summaries::run(&mut ctx, format, out.as_deref()).await?
            }
        },
    }
    Ok(())
}
