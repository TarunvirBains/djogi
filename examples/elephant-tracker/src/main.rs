//! elephant-tracker — runnable Djogi example.
//!
//! See `README.md` for an overview. This binary exposes a `demo` command
//! group with six feature walkthroughs:
//! `cluster-sightings`, `cross-border-herds`, `lineage`,
//! `herd-summaries`, `mating-pairs`, and `values-scores`.
//! Most demos accept `--format json|mermaid|markdown` plus
//! `--out <path>` (default stdout).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod demos;
mod models;
mod output;
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
        /// from. Compose with `--order` to
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

    /// Wright F coefficient over the materialized ancestry closure,
    /// top-3 candidate pairs per mature female.
    MatingPairs {
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `mermaid`, `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },

    /// Join a process-local score list to Elephant rows using VALUES.
    /// Demonstrates djogi#103 typed inline-relation join.
    ValuesScores {
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format. `json` (default), `markdown`.
        #[arg(long, value_enum, default_value_t = Format::Json)]
        format: Format,
    },
}

#[allow(clippy::disallowed_methods)]
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

    let heer_node_id = std::env::var("HEER_NODE_ID")
        .unwrap_or_else(|_| djogi::migrate::bootstrap::DEFAULT_NODE_ID.to_string());
    let _: i32 = heer_node_id
        .parse()
        .context("HEER_NODE_ID must be an integer when set")?;

    // Build the pool through the `DjogiPool` builder. The
    // `post_connect` hook runs once per physical connection and sets
    // the HeeRanjID node GUCs for that session. This is the recommended
    // adopter shape: the service selects a node id through its
    // environment, while the pool guarantees every new connection sees
    // the same session setup without requiring `ALTER DATABASE`
    // privileges.
    //
    // The migration path seeds HeeRanjID's default node. Multi-node
    // deployments must provision/register their chosen node before
    // startup; if `HEER_NODE_ID` names an unregistered node,
    // HeeRanjID's Postgres functions surface the error when ids are
    // minted.
    let pool = djogi::pg::pool::DjogiPool::builder(&database_url)
        .post_connect({
            let heer_node_id = heer_node_id.clone();
            move |client| {
                let heer_node_id = heer_node_id.clone();
                Box::pin(async move {
                    client
                        .execute(
                            "SELECT set_config('heer.node_id', $1, false), \
     set_config('heer.ranj_node_id', $1, false)",
                            &[&heer_node_id],
                        )
                        .await
                        .map_err(djogi::DjogiError::from)?;
                    Ok(())
                })
            }
        })
        .build()
        .await
        .context("failed to construct DjogiPool")?;
    let mut ctx = djogi::DjogiContext::from_pool(pool);

    match cli.cmd {
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
            DemoCmd::MatingPairs { out, format } => {
                demos::mating_pairs::run(&mut ctx, format, out.as_deref()).await?
            }
            DemoCmd::ValuesScores { out, format } => {
                demos::values_scores::run(&mut ctx, format, out.as_deref()).await?
            }
        },
    }
    Ok(())
}
