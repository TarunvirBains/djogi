use clap::Parser;

#[derive(Parser)]
#[command(name = "djogi", about = "Djogi framework CLI")]
enum Cli {
    /// Apply pending migrations
    Migrate,
    /// Launch interactive Rhai shell
    Shell,
    /// Database management
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
}

#[derive(Parser)]
enum DbCommand {
    /// Drop, recreate, and migrate the database (dev only)
    Reset,
    /// Run seed script
    Seed,
}

fn main() {
    let cli = Cli::parse();
    match cli {
        Cli::Migrate => eprintln!("djogi migrate: not yet implemented"),
        Cli::Shell => eprintln!("djogi shell: not yet implemented"),
        Cli::Db { command } => match command {
            DbCommand::Reset => eprintln!("djogi db reset: not yet implemented"),
            DbCommand::Seed => eprintln!("djogi db seed: not yet implemented"),
        },
    }
}
