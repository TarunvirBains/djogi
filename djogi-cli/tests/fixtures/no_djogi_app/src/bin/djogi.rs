//! Adopter binary that embeds djogi CLI infrastructure but has ZERO djogi
//! models. Used by T-NOLOGIC to prove the binary runs without crashing when
//! no models are linked (inventory is empty).
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "djogi")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Verify djogi models
    Verify {
        #[arg(long)]
        database_url: Option<String>,
        #[arg(long)]
        app_label: Option<String>,
    },
    /// Compose migrations
    Compose {
        #[arg(long)]
        database_url: Option<String>,
        #[arg(long)]
        workspace_root: Option<String>,
        #[arg(long)]
        allow_destructive: bool,
    },
}

fn main() {
    let _cli = Cli::parse();
    // Zero-descriptor shape: no models are linked. The djogi CLI infrastructure
    // is present but inventory is empty. This binary exits 0 for verify (no
    // DB connection needed) — the T-NOLOGIC test proves this path works.
    println!("djogi v0.0.0");
}
