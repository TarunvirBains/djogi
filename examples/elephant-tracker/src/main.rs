//! elephant-tracker — runnable Djogi example.
//!
//! Subcommands:
//! - `migrate` — apply pending migrations
//! - `seed`    — load `seeds/countries.sql` then run `seeds::herds_and_sightings`
//! - `demo cluster-sightings`
//! - `demo cross-border-herds`
//! - `demo lineage --matriarch=<name>`
//! - `demo herd-summaries`

use anyhow::Result;
use clap::{Parser, Subcommand};

mod models;
mod visages;
mod demos;

#[derive(Parser)]
#[command(name = "elephant-tracker", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Migrate,
    Seed,
    Demo {
        #[command(subcommand)]
        which: DemoCmd,
    },
}

#[derive(Subcommand)]
enum DemoCmd {
    ClusterSightings,
    CrossBorderHerds,
    Lineage {
        #[arg(long)]
        matriarch: String,
    },
    HerdSummaries,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    // ctx wiring — once cluster PRs merge:
    //     let ctx = djogi::DjogiContext::from_env().await?;
    let ctx = todo!("DjogiContext from DATABASE_URL");

    match cli.cmd {
        Cmd::Migrate => todo!("djogi::cli::migrate(&ctx).await"),
        Cmd::Seed    => todo!("seed countries + herds + sightings"),
        Cmd::Demo { which } => match which {
            DemoCmd::ClusterSightings => demos::cluster_sightings::run(&ctx).await?,
            DemoCmd::CrossBorderHerds => demos::cross_border_herds::run(&ctx).await?,
            DemoCmd::Lineage { matriarch } => demos::lineage::run(&ctx, &matriarch).await?,
            DemoCmd::HerdSummaries => demos::herd_summaries::run(&ctx).await?,
        },
    }
    Ok(())
}
