//! Fledgling command-line entry point.

use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode};

mod config;
mod scaffold;

// ----------------------------------------------------------------------------
// CLI Definition
// ----------------------------------------------------------------------------

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Use configuration from this file.
    #[arg(long, global = true, default_value = "fledgling.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interactively scaffold a Fledgling project.
    Init,
    /// Print the current database schema as SQL.
    Pull(DatabaseArgs),
    /// Print the next migration offline without writing files.
    Preview {
        #[command(flatten)]
        migrations: MigrationsArgs,
        #[command(flatten)]
        schema: SchemaArgs,
    },
    /// Apply pending migrations.
    Migrate(DatabaseMigrationsArgs),
    /// Show applied migration IDs.
    Status(DatabaseArgs),
    /// Roll back the latest applied migration.
    Rollback(DatabaseMigrationsArgs),
    /// Generate a migration offline from migration history and desired schema.
    Generate {
        #[command(flatten)]
        migrations: MigrationsArgs,
        #[command(flatten)]
        schema: SchemaArgs,
    },
}

#[derive(Args)]
struct DatabaseArgs {
    /// Override the configured database.
    #[arg(long, conflicts_with = "database_pos")]
    database: Option<PathBuf>,
    #[arg(value_name = "DATABASE")]
    database_pos: Option<PathBuf>,
}

#[derive(Args)]
struct MigrationsArgs {
    /// Override the configured migrations directory.
    #[arg(long, conflicts_with = "migrations_pos")]
    migrations: Option<PathBuf>,
    #[arg(value_name = "MIGRATIONS")]
    migrations_pos: Option<PathBuf>,
}

#[derive(Args)]
struct SchemaArgs {
    /// Override the configured schema SQL file.
    #[arg(long, conflicts_with = "schema_pos")]
    schema: Option<PathBuf>,
    #[arg(value_name = "SCHEMA")]
    schema_pos: Option<PathBuf>,
}

#[derive(Args)]
struct DatabaseMigrationsArgs {
    #[command(flatten)]
    database: DatabaseArgs,
    #[command(flatten)]
    migrations: MigrationsArgs,
}

// ----------------------------------------------------------------------------
// Main Handler
// ----------------------------------------------------------------------------

/// Main routine.
fn main() -> ExitCode {
    // Parse CLI
    let cli = Cli::parse();

    // Handle CLI
    let result = {
        match cli.command {
            Command::Init => scaffold::interactive(&cli.config),
            _ => Err("commands are not implemented".into()),
        }
    };

    // Handle handler response
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}
