//! Fledgling command-line entry point.

mod config;
mod scaffold;

use clap::{Args, Parser, Subcommand};
use config::{CONFIG_FILENAME, Config};
use fledgling::{generate, migrate, preview, pull, rollback, status};
use std::{error::Error, path::PathBuf, process::ExitCode};

// ----------------------------------------------------------------------------
// CLI Definition
// ----------------------------------------------------------------------------

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Use configuration from this file.
    #[arg(long, global = true, default_value = CONFIG_FILENAME)]
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

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(Cli { config, command }: Cli) -> Result<(), Box<dyn Error>> {
    if matches!(command, Command::Init) {
        return scaffold::interactive(&config);
    }
    let Config {
        dialect,
        database,
        postgres_database_url,
        migrations,
        schema,
    } = Config::read(&config).map_err(|error| format!("reading {}: {error}", config.display()))?;

    let apply = matches!(&command, Command::Migrate(_));
    let show_preview = matches!(&command, Command::Preview { .. });
    match command {
        Command::Init => unreachable!(),
        Command::Pull(args) => {
            let database = required(
                args.database
                    .or(args.database_pos)
                    .or_else(|| postgres_database_url.clone().map(PathBuf::from))
                    .or(database),
                "database",
            )?;
            print!("{}", pull(dialect, &database)?);
        }
        Command::Status(args) => {
            let database = required(
                args.database
                    .or(args.database_pos)
                    .or_else(|| postgres_database_url.clone().map(PathBuf::from))
                    .or(database),
                "database",
            )?;
            for id in status(dialect, &database)? {
                println!("{id:06}");
            }
        }
        Command::Migrate(args) | Command::Rollback(args) => {
            let database = required(
                args.database
                    .database
                    .or(args.database.database_pos)
                    .or_else(|| postgres_database_url.clone().map(PathBuf::from))
                    .or(database),
                "database",
            )?;
            let migrations = required(
                args.migrations
                    .migrations
                    .or(args.migrations.migrations_pos)
                    .or(migrations),
                "migrations",
            )?;
            if apply {
                for id in migrate(dialect, &database, &migrations)? {
                    println!("Applied {id:06}");
                }
            } else if let Some(id) = rollback(dialect, &database, &migrations)? {
                println!("Rolled back {id:06}");
            }
        }
        Command::Preview {
            migrations: args_migrations,
            schema: args_schema,
        }
        | Command::Generate {
            migrations: args_migrations,
            schema: args_schema,
        } => {
            let migrations = required(
                args_migrations
                    .migrations
                    .or(args_migrations.migrations_pos)
                    .or(migrations),
                "migrations",
            )?;
            let schema = required(
                args_schema.schema.or(args_schema.schema_pos).or(schema),
                "schema",
            )?;
            if show_preview {
                let plan = preview(dialect, &migrations, &schema)?;
                if let Some((up, down)) = plan {
                    println!("-- Up\n{up}\n\n-- Down\n{down}");
                } else {
                    println!("No schema changes.");
                }
            } else if let Some(up) = generate(dialect, &migrations, &schema)? {
                println!("Generated {}", up.display());
            } else {
                println!("No schema changes.");
            }
        }
    }
    Ok(())
}

fn required(path: Option<PathBuf>, label: &str) -> Result<PathBuf, Box<dyn Error>> {
    path.ok_or_else(|| {
        format!("missing {label} path; set it in the config or pass --{label}").into()
    })
}
