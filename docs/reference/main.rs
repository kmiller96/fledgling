//! Fledgling command-line entry point.
//!
//! The entry point parses arguments and dispatches to private command modules. Those
//! modules resolve config, open databases as needed, and report results.
//! The library owns catalog validation, planning, and migration transactions.

use clap::{Args, Parser, Subcommand};
use fledgling::config::CONFIG_FILENAME;
use std::{error::Error, path::PathBuf, process::ExitCode};

mod scaffold;

// ----------------------------------------------------------------------------
// Main Handler
// ----------------------------------------------------------------------------

fn main() -> ExitCode {
    dispatch().unwrap_or_else(|error| {
        eprintln!("Error: {error}");
        ExitCode::FAILURE
    })
}

/// Dispatch a parsed command, preserving the migration runner's reported exit code.
fn dispatch() -> Result<ExitCode, Box<dyn Error>> {
    let Cli { config, command } = Cli::parse();

    match command {
        Command::Init => scaffold::interactive(&config)?,
        Command::Pull(database) => commands::pull(&config, database.path())?,
        Command::Preview { migrations, schema } => {
            commands::preview(&config, migrations.path(), schema.path())?;
        }
        Command::Migrate(DatabaseMigrationsArgs {
            database,
            migrations,
        }) => {
            return commands::migrate(&config, database.path(), migrations.path());
        }
        Command::Status(database) => commands::status(&config, database.path())?,
        Command::Rollback(DatabaseMigrationsArgs {
            database,
            migrations,
        }) => {
            commands::rollback(&config, database.path(), migrations.path())?;
        }
        Command::Generate { migrations, schema } => {
            commands::generate(&config, migrations.path(), schema.path())?
        }
    }

    Ok(ExitCode::SUCCESS)
}

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
    /// Print the current SQLite schema as SQL.
    Pull(DatabaseArgs),
    /// Print the next Up/Down migration offline without writing files.
    Preview {
        #[command(flatten)]
        migrations: MigrationsArgs,
        #[command(flatten)]
        schema: SchemaArgs,
    },
    /// Apply pending SQL migrations from a directory to a SQLite database.
    Migrate(DatabaseMigrationsArgs),
    /// Show applied SQLite migration IDs without initializing the ledger.
    Status(DatabaseArgs),
    /// Roll back the latest applied SQLite migration.
    Rollback(DatabaseMigrationsArgs),
    /// Generate an Up/Down migration offline from migration history and desired schema.
    Generate {
        #[command(flatten)]
        migrations: MigrationsArgs,
        #[command(flatten)]
        schema: SchemaArgs,
    },
}

#[derive(Args)]
struct DatabaseArgs {
    /// Override the configured SQLite database.
    #[arg(long, conflicts_with = "database_pos")]
    database: Option<PathBuf>,
    #[arg(value_name = "DATABASE")]
    database_pos: Option<PathBuf>,
}

impl DatabaseArgs {
    fn path(self) -> Option<PathBuf> {
        self.database.or(self.database_pos)
    }
}

#[derive(Args)]
struct MigrationsArgs {
    /// Override the configured migrations directory.
    #[arg(long, conflicts_with = "migrations_pos")]
    migrations: Option<PathBuf>,
    #[arg(value_name = "MIGRATIONS")]
    migrations_pos: Option<PathBuf>,
}

impl MigrationsArgs {
    fn path(self) -> Option<PathBuf> {
        self.migrations.or(self.migrations_pos)
    }
}

#[derive(Args)]
struct SchemaArgs {
    /// Override the configured schema SQL file.
    #[arg(long, conflicts_with = "schema_pos")]
    schema: Option<PathBuf>,
    #[arg(value_name = "SCHEMA")]
    schema_pos: Option<PathBuf>,
}

impl SchemaArgs {
    fn path(self) -> Option<PathBuf> {
        self.schema.or(self.schema_pos)
    }
}

#[derive(Args)]
struct DatabaseMigrationsArgs {
    #[command(flatten)]
    database: DatabaseArgs,
    #[command(flatten)]
    migrations: MigrationsArgs,
}

// ----------------------------------------------------------------------------
// Command handlers
// ----------------------------------------------------------------------------

mod commands {
    //! Handles database inspection, offline migration planning, and migration execution.

    use super::settings::{DATABASE, MIGRATIONS, SCHEMA, required};
    use fledgling::{
        FailureOutcome, MigrationCatalog, MigrationExecutor, MigrationSql,
        config::{Config, Dialect},
        plan_and_publish, sqlite,
    };
    use rusqlite::{Connection, OpenFlags};
    use std::{
        error::Error,
        fs, io,
        path::{Path, PathBuf},
        process::ExitCode,
    };

    // Database inspection

    /// Print the existing database's main schema using a read-only connection.
    pub(super) fn pull(config: &Path, database: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
        let config = Config::read(config)?;
        let database = required(database.or(config.database), DATABASE)?;
        let schema = match config.dialect {
            Dialect::SQLite => sqlite::pull(&open_pull_connection(database)?)?,
        };
        println!("{schema}");
        Ok(())
    }

    /// Print applied IDs in order, distinguishing an absent ledger from an empty one.
    pub(super) fn status(config: &Path, database: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
        let config = Config::read(config)?;
        let database = required(database.or(config.database), DATABASE)?;
        let status = match config.dialect {
            Dialect::SQLite => sqlite::status(&open_pull_connection(database)?)?,
        };
        match status {
            None => println!("uninitialized"),
            Some(ids) if ids.is_empty() => println!("no migrations applied"),
            Some(ids) => {
                for id in ids {
                    println!("{id:06}");
                }
            }
        }
        Ok(())
    }

    fn open_pull_connection(database: PathBuf) -> rusqlite::Result<Connection> {
        sqlite::open_read_only(database)
    }

    // Offline migration planning

    /// Print the next Up/Down pair (or `No changes`) using only the catalog and
    /// desired SQL file; do not open the configured database or write artifacts.
    pub(super) fn preview(
        config: &Path,
        directory: Option<PathBuf>,
        schema: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        let config = Config::read(config)?;
        let directory = required(directory.or(config.migrations), MIGRATIONS)?;
        let schema = required(schema.or(config.schema), SCHEMA)?;
        let catalog = MigrationCatalog::read(directory)?;
        let desired_schema = fs::read_to_string(schema)?;
        let plan = match config.dialect {
            Dialect::SQLite => sqlite::plan(&catalog, &desired_schema)?,
        };
        match plan {
            Some(MigrationSql { up, down }) => println!("-- Up\n{up}\n\n-- Down\n{down}"),
            None => println!("No changes"),
        }
        Ok(())
    }

    /// Generate the next numbered migration offline.
    /// Print the new Up path, or `No changes` if no migration is needed.
    pub(super) fn generate(
        config: &Path,
        directory: Option<PathBuf>,
        schema: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        let config = Config::read(config)?;
        let directory = required(directory.or(config.migrations), MIGRATIONS)?;
        let schema = required(schema.or(config.schema), SCHEMA)?;
        let catalog = MigrationCatalog::read(&directory)?;
        let desired_schema = fs::read_to_string(schema)?;
        let published = match config.dialect {
            Dialect::SQLite => {
                plan_and_publish(&sqlite::SqliteAdaptor, directory, &catalog, &desired_schema)?
            }
        };
        match published {
            Some(path) => println!("{}", path.display()),
            None => println!("No changes"),
        }
        Ok(())
    }

    // Migration execution

    /// Apply pending migrations and report committed progress and failure outcomes.
    pub(super) fn migrate(
        config: &Path,
        database: Option<PathBuf>,
        directory: Option<PathBuf>,
    ) -> Result<ExitCode, Box<dyn Error>> {
        let config = Config::read(config)?;
        let database = required(database.or(config.database), DATABASE)?;
        let directory = required(directory.or(config.migrations), MIGRATIONS)?;
        match config.dialect {
            Dialect::SQLite => apply_migrations(
                database,
                directory,
                &mut io::stdout().lock(),
                &mut io::stderr().lock(),
            ),
        }
    }

    /// Discover artifacts before opening (and potentially creating) the database.
    /// Runner failures are printed to `errors` and produce a failure exit code;
    /// discovery, open, and reporting errors propagate separately.
    fn apply_migrations(
        database: PathBuf,
        directory: PathBuf,
        output: &mut impl io::Write,
        errors: &mut impl io::Write,
    ) -> Result<ExitCode, Box<dyn Error>> {
        let catalog = MigrationCatalog::read(directory)?;
        let connection = Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        match sqlite::SqliteExecutor::new(&connection).apply(&catalog) {
            Ok(committed) => {
                if committed.is_empty() {
                    writeln!(output, "No changes to apply.")?;
                } else {
                    for &id in &committed {
                        writeln!(output, "✓ {id:06}")?;
                    }
                    if committed.len() == 1 {
                        writeln!(output, "\nApplied 1 migration.")?;
                    } else {
                        writeln!(output, "\nApplied {} migrations.", committed.len())?;
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let applied = error.committed_migration_ids.len();
                for id in error.committed_migration_ids {
                    writeln!(errors, "✓ {id:06}")?;
                }
                if let Some(id) = error.failed_migration_id {
                    writeln!(errors, "✗ {id:06}")?;
                    for line in error.source.to_string().lines() {
                        writeln!(errors, "  {line}")?;
                    }
                    // Preflight can fail before reaching this migration in execution order.
                    if error.outcome == FailureOutcome::NotStarted && applied == 0 {
                        writeln!(errors, "\nMigration stopped. No migrations applied.")?;
                    } else {
                        let not_run =
                            catalog.len() - catalog.partition_point(|migration| migration.id <= id);
                        writeln!(
                            errors,
                            "\nMigration stopped. {applied} applied, 1 failed, {not_run} not run."
                        )?;
                    }
                } else {
                    writeln!(errors, "Migration stopped: {}", error.source)?;
                }
                match error.outcome {
                    FailureOutcome::NotStarted => {}
                    FailureOutcome::KnownRollback => {
                        writeln!(errors, "The failed migration was rolled back.")?
                    }
                    FailureOutcome::Uncertain => writeln!(
                        errors,
                        "Migration outcome uncertain. Inspect the migration ledger and database before retrying."
                    )?,
                }
                if let Some(cleanup_error) = error.cleanup_error {
                    writeln!(errors, "Rollback/cleanup failed: {cleanup_error}")?;
                }
                Ok(ExitCode::FAILURE)
            }
        }
    }

    /// Run the latest applied migration's Down file on an existing database.
    /// An absent ledger, empty history, or missing Down file is an error.
    pub(super) fn rollback(
        config: &Path,
        database: Option<PathBuf>,
        directory: Option<PathBuf>,
    ) -> Result<(), Box<dyn Error>> {
        let config = Config::read(config)?;
        let database = required(database.or(config.database), DATABASE)?;
        let directory = required(directory.or(config.migrations), MIGRATIONS)?;
        match config.dialect {
            Dialect::SQLite => rollback_latest(database, directory),
        }
    }

    fn rollback_latest(database: PathBuf, directory: PathBuf) -> Result<(), Box<dyn Error>> {
        let catalog = MigrationCatalog::read(directory)?;
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        sqlite::SqliteExecutor::new(&connection).revert(&catalog)?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{
            apply_migrations, generate, open_pull_connection, pull, rollback, rollback_latest,
            status,
        };
        use crate::tests::{temp_path, test_config};
        use fledgling::{MigrationCatalog, sqlite};
        use rusqlite::{Connection, OpenFlags};
        use std::{fs, process::ExitCode};

        #[test]
        fn pull_rejects_missing_database_without_creating_it() {
            let path = temp_path("pull").with_extension("sqlite");

            let config = test_config(&path);
            assert!(pull(&config, Some(path.clone())).is_err());
            fs::remove_file(config).unwrap();
            assert!(!path.exists());
        }

        #[test]
        fn pull_opens_existing_database_read_only() {
            let path = temp_path("pull").with_extension("sqlite");
            let setup = Connection::open(&path).unwrap();
            setup
                .execute_batch("CREATE TABLE example (id INTEGER);")
                .unwrap();
            drop(setup);

            let connection = open_pull_connection(path.clone()).unwrap();
            assert!(
                sqlite::pull(&connection)
                    .unwrap()
                    .contains("CREATE TABLE example")
            );
            assert!(
                connection
                    .execute("CREATE TABLE rejected (id INTEGER)", [])
                    .is_err()
            );

            drop(connection);
            fs::remove_file(path).unwrap();
        }

        #[test]
        fn status_reads_without_initializing_the_ledger() {
            let path = temp_path("status").with_extension("sqlite");
            let setup = Connection::open(&path).unwrap();
            drop(setup);

            let config = test_config(&path);
            status(&config, Some(path.clone())).unwrap();
            fs::remove_file(config).unwrap();
            let check =
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert_eq!(
                check
                    .query_row(
                        "SELECT count(*) FROM main.sqlite_schema \
                     WHERE type = 'table' AND name = '_fledgling_migrations'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );

            drop(check);
            fs::remove_file(path).unwrap();
        }

        #[test]
        fn generate_command_publishes_migration_from_catalog_and_schema_file() {
            let directory = temp_path("generate");
            fs::create_dir(&directory).unwrap();
            let schema = temp_path("schema").with_extension("sql");
            fs::write(&schema, "CREATE TABLE example (id INTEGER);").unwrap();

            let config = test_config(&schema);
            generate(&config, Some(directory.clone()), Some(schema.clone())).unwrap();

            let catalog = MigrationCatalog::read(&directory).unwrap();
            let [migration] = &catalog[..] else {
                panic!("expected one migration");
            };
            assert_eq!(migration.up.path, directory.join("000001.up.sql"));
            assert!(
                fs::read_to_string(&migration.up.path)
                    .unwrap()
                    .contains("CREATE TABLE")
            );
            assert!(migration.down.is_some());
            fs::remove_file(config).unwrap();
            fs::remove_file(schema).unwrap();
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn preview_rejects_invalid_check_and_reserved_ledger_name() {
            for sql in [
                "CREATE TABLE items (id INTEGER CHECK (missing > 0));",
                "CREATE TABLE _FLEDGLING_MIGRATIONS (id INTEGER);",
                "CREATE VIEW _fledgling_migrations AS SELECT 1;",
                "CREATE INDEX _fledgling_migrations ON items (id);",
            ] {
                assert!(
                    sqlite::plan(&MigrationCatalog::default(), sql).is_err(),
                    "preview accepted {sql}"
                );
            }
            assert!(
                sqlite::plan(
                    &MigrationCatalog::default(),
                    "CREATE TABLE items (id INTEGER);"
                )
                .is_ok()
            );
        }

        #[test]
        fn rollback_command_path_rolls_back_latest_migration() {
            let database = temp_path("rollback").with_extension("sqlite");
            let directory = temp_path("rollback");
            fs::create_dir(&directory).unwrap();
            fs::write(
                directory.join("000001.up.sql"),
                "CREATE TABLE example (id INTEGER);",
            )
            .unwrap();
            fs::write(directory.join("000001.down.sql"), "DROP TABLE example;").unwrap();

            let connection = Connection::open(&database).unwrap();
            let catalog = MigrationCatalog::read(&directory).unwrap();
            sqlite::migrate(&connection, &catalog).unwrap();
            drop(connection);

            let config = test_config(&database);
            rollback(&config, Some(database.clone()), Some(directory.clone())).unwrap();
            fs::remove_file(config).unwrap();

            let check =
                Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert_eq!(
                check
                    .query_row(
                        "SELECT count(*) FROM main.sqlite_schema \
                     WHERE type = 'table' AND name = 'example'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
            assert_eq!(sqlite::status(&check).unwrap(), Some(vec![]));
            drop(check);
            fs::remove_file(database).unwrap();
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn migrate_command_path_records_and_skips_applied_migrations() {
            let database = temp_path("migrate").with_extension("sqlite");
            drop(Connection::open(&database).unwrap());
            let directory = temp_path("migrate");
            fs::create_dir(&directory).unwrap();
            fs::write(
                directory.join("000001.up.sql"),
                "CREATE TABLE example (id INTEGER);",
            )
            .unwrap();

            fs::write(
                directory.join("000010.up.sql"),
                "CREATE TABLE second (id INTEGER);",
            )
            .unwrap();

            let mut output = Vec::new();
            let mut errors = Vec::new();
            for expected in [
                "✓ 000001\n✓ 000010\n\nApplied 2 migrations.\n",
                "No changes to apply.\n",
            ] {
                assert_eq!(
                    apply_migrations(
                        database.clone(),
                        directory.clone(),
                        &mut output,
                        &mut errors
                    )
                    .unwrap(),
                    ExitCode::SUCCESS
                );
                assert_eq!(output, expected.as_bytes());
                assert!(errors.is_empty());
                output.clear();
            }

            let connection =
                Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert_eq!(sqlite::status(&connection).unwrap(), Some(vec![1, 10]));
            assert!(
                connection
                    .query_row(
                        "SELECT count(*) FROM main.sqlite_schema \
                     WHERE type = 'table' AND name = 'example'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
                    == 1
            );
            drop(connection);
            fs::remove_file(database).unwrap();
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn migrate_command_failure_reports_failed_and_committed_migrations() {
            let database = temp_path("migrate-failure").with_extension("sqlite");
            drop(Connection::open(&database).unwrap());
            let directory = temp_path("migrate-failure");
            fs::create_dir(&directory).unwrap();
            fs::write(
                directory.join("000001.up.sql"),
                "CREATE TABLE first (id INTEGER);",
            )
            .unwrap();
            fs::write(
                directory.join("000002.up.sql"),
                "CREATE TABLE second (id INTEGER); SELECT * FROM missing;",
            )
            .unwrap();
            fs::write(
                directory.join("000003.up.sql"),
                "CREATE TABLE third (id INTEGER);",
            )
            .unwrap();

            let mut output = Vec::new();
            let mut errors = Vec::new();
            for (prefix, applied) in [("✓ 000001\n", 1), ("", 0)] {
                assert_eq!(
                    apply_migrations(
                        database.clone(),
                        directory.clone(),
                        &mut output,
                        &mut errors
                    )
                    .unwrap(),
                    ExitCode::FAILURE
                );
                assert!(output.is_empty());
                let report = std::str::from_utf8(&errors).unwrap();
                assert!(
                    report.starts_with(&format!("{prefix}✗ 000002\n  no such table: missing")),
                    "{report}"
                );
                assert!(
                report.ends_with(&format!(
                    "\n\nMigration stopped. {applied} applied, 1 failed, 1 not run.\nThe failed migration was rolled back.\n"
                )),
                "{report}"
            );
                errors.clear();
            }
            let connection = Connection::open(&database).unwrap();
            assert_eq!(sqlite::status(&connection).unwrap(), Some(vec![1]));
            drop(connection);
            fs::remove_file(database).unwrap();
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn migrate_creates_missing_database_but_rollback_does_not() {
            let database = temp_path("missing-db").with_extension("sqlite");
            let directory = temp_path("missing-db");
            fs::create_dir(&directory).unwrap();
            fs::write(
                directory.join("000001.up.sql"),
                "CREATE TABLE example (id INTEGER);",
            )
            .unwrap();

            let mut output = Vec::new();
            assert_eq!(
                apply_migrations(
                    database.clone(),
                    directory.clone(),
                    &mut output,
                    &mut Vec::new()
                )
                .unwrap(),
                ExitCode::SUCCESS
            );
            assert!(database.exists());
            assert_eq!(
                std::str::from_utf8(&output).unwrap(),
                "✓ 000001\n\nApplied 1 migration.\n"
            );
            assert!(rollback_latest(database.clone(), directory.clone()).is_err());
            fs::remove_file(database).unwrap();

            fs::remove_dir_all(directory).unwrap();
        }
    }
}

// ----------------------------------------------------------------------------
// Shared command settings
// ----------------------------------------------------------------------------

mod settings {
    //! Validates required paths after handlers combine CLI overrides with config defaults.

    use std::{error::Error, path::PathBuf};

    pub(super) const DATABASE: &str = "--database / database_url";
    pub(super) const MIGRATIONS: &str = "--migrations / migrations";
    pub(super) const SCHEMA: &str = "--schema / schema";

    /// Require a path after combining command-line overrides with config defaults.
    pub(super) fn required(value: Option<PathBuf>, label: &str) -> Result<PathBuf, Box<dyn Error>> {
        value.ok_or_else(|| {
            format!("missing required {label}; provide it on the command line or in [fledgling]")
                .into()
        })
    }
}

// ----------------------------------------------------------------------------
// Cross-command tests and shared fixtures
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Cross-command tests for config resolution and the migration workflow,
    //! plus shared temporary paths and config fixtures.

    use super::commands::{generate, migrate, preview, pull, rollback, status};
    use fledgling::{MigrationCatalog, sqlite};
    use rusqlite::Connection;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::ExitCode,
        sync::atomic::{AtomicUsize, Ordering},
    };

    pub(super) fn temp_path(name: &str) -> PathBuf {
        static NEXT_PATH: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "fledgling-{name}-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    pub(super) fn test_config(path: &Path) -> PathBuf {
        let config = path.with_extension("toml");
        fs::write(&config, "[fledgling]\ndialect = 'sqlite'\n").unwrap();
        config
    }

    #[test]
    fn commands_report_missing_effective_settings_before_io() {
        let config = test_config(&temp_path("missing-settings"));
        let error = pull(&config, None).unwrap_err().to_string();
        assert!(error.contains("--database"));
        assert!(error.contains("database_url"));

        let error = preview(&config, Some("migrations".into()), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--schema"));
        assert!(error.contains("schema"));
        fs::remove_file(config).unwrap();
    }

    #[test]
    fn workflow_commands_run_from_config_without_extra_arguments() {
        let project = temp_path("workflow");
        let migrations = project.join("migrations");
        fs::create_dir_all(&migrations).unwrap();
        let config = project.join("fledgling.toml");
        fs::write(
            &config,
            "[fledgling]\n\
             dialect = 'sqlite'\n\
             schema = 'schema.sql'\n\
             migrations = 'migrations'\n\
             database_url = 'sqlite://db.sqlite'\n",
        )
        .unwrap();
        let schema = project.join("schema.sql");
        fs::write(&schema, "CREATE TABLE example (id INTEGER);").unwrap();
        let database = project.join("db.sqlite");

        generate(&config, None, None).unwrap();

        fs::write(&schema, "CREATE TABLE example (id INTEGER, note TEXT);").unwrap();
        preview(&config, None, None).unwrap();
        generate(&config, None, None).unwrap(); // First migration isn't live yet.
        let catalog = MigrationCatalog::read(&migrations).unwrap();
        let [first, second] = &catalog[..] else {
            panic!("expected two migrations");
        };
        assert_eq!((first.id, second.id), (1, 2));
        assert_eq!(first.up.path, migrations.join("000001.up.sql"));
        assert_eq!(second.up.path, migrations.join("000002.up.sql"));
        generate(&config, None, None).unwrap();
        assert_eq!(fs::read_dir(&migrations).unwrap().count(), 4);
        assert!(!database.exists());

        drop(Connection::open(&database).unwrap());
        assert_eq!(migrate(&config, None, None).unwrap(), ExitCode::SUCCESS);
        status(&config, None).unwrap();
        assert_eq!(
            sqlite::status(&Connection::open(&database).unwrap()).unwrap(),
            Some(vec![first.id, second.id])
        );
        pull(&config, None).unwrap();
        rollback(&config, None, None).unwrap();
        assert_eq!(
            sqlite::status(&Connection::open(&database).unwrap()).unwrap(),
            Some(vec![first.id])
        );
        fs::remove_dir_all(project).unwrap();
    }
}
