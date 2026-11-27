//! Fledgling command-line entry point.

use clap::{Args, Parser, Subcommand};
use config::{CONFIG_FILENAME, Config};
use fledgling::{generate, migrate, preview, pull, rollback, status};
use std::{error::Error, path::PathBuf, process::ExitCode};

mod config;
mod scaffold;

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

impl DatabaseArgs {
    fn resolve(self, configured: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
        required(
            self.database.or(self.database_pos).or(configured),
            "database",
        )
    }
}

impl MigrationsArgs {
    fn resolve(self, configured: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
        required(
            self.migrations.or(self.migrations_pos).or(configured),
            "migrations",
        )
    }
}

impl SchemaArgs {
    fn resolve(self, configured: Option<PathBuf>) -> Result<PathBuf, Box<dyn Error>> {
        required(self.schema.or(self.schema_pos).or(configured), "schema")
    }
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
        migrations,
        schema,
    } = Config::read(&config).map_err(|error| format!("reading {}: {error}", config.display()))?;
    match command {
        Command::Init => unreachable!(),
        Command::Pull(args) => {
            print!("{}", pull(dialect, &args.resolve(database)?)?);
        }
        Command::Status(args) => {
            for id in status(dialect, &args.resolve(database)?)? {
                println!("{id:06}");
            }
        }
        Command::Migrate(args) => {
            let database = args.database.resolve(database)?;
            let migrations = args.migrations.resolve(migrations)?;
            for id in migrate(dialect, &database, &migrations)? {
                println!("Applied {id:06}");
            }
        }
        Command::Rollback(args) => {
            let database = args.database.resolve(database)?;
            let migrations = args.migrations.resolve(migrations)?;
            if let Some(id) = rollback(dialect, &database, &migrations)? {
                println!("Rolled back {id:06}");
            }
        }
        Command::Preview {
            migrations: migrations_args,
            schema: schema_args,
        } => {
            let migrations = migrations_args.resolve(migrations)?;
            let schema = schema_args.resolve(schema)?;
            if let Some((up, down)) = preview(dialect, &migrations, &schema)? {
                println!("-- Up\n{up}\n\n-- Down\n{down}");
            } else {
                println!("No schema changes.");
            }
        }
        Command::Generate {
            migrations: migrations_args,
            schema: schema_args,
        } => {
            let migrations = migrations_args.resolve(migrations)?;
            let schema = schema_args.resolve(schema)?;
            if let Some(up) = generate(dialect, &migrations, &schema)? {
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
