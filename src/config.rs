//! Defines the CLI's project configuration and its TOML representation.
//!
//! [`Config`] stores the selected SQL dialect and optional schema, migrations,
//! paths and an optional database location. Paths read from disk are resolved
//! relative to the config file; PostgreSQL connection URLs stay literal.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use fledgling::Dialect;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// Config filename and path resolution
// ----------------------------------------------------------------------------

/// Filename for the fledgling configuration file.
pub const CONFIG_FILENAME: &str = "fledgling.toml";

/// Join relative paths to `base` without canonicalizing or checking existence.
/// Preserve absolute paths, missing values, and SQLite's `:memory:` sentinel.
fn resolve_paths<const N: usize>(base: &Path, paths: [Option<PathBuf>; N]) -> [Option<PathBuf>; N] {
    paths.map(|path| {
        path.map(|path| {
            if path.is_absolute() || path == Path::new(":memory:") {
                path
            } else {
                base.join(path)
            }
        })
    })
}

// ----------------------------------------------------------------------------
/// Decode `sqlite:` with optional `//` as a literal path, not a general URL.
/// Empty paths, query strings, fragments, and NULs are not accepted.
fn sqlite_database_path(url: &str) -> Result<PathBuf, String> {
    let path = url
        .strip_prefix("sqlite:")
        .map(|path| path.strip_prefix("//").unwrap_or(path))
        .filter(|path| !path.trim().is_empty() && !path.contains(['?', '#', '\0']));
    path.map(PathBuf::from).ok_or_else(|| {
        "database_url must be a SQLite URL with a file path (sqlite://example.db) or sqlite::memory:".into()
    })
}

// ----------------------------------------------------------------------------
// TOML Document Struct
// ----------------------------------------------------------------------------

/// On-disk TOML envelope; project settings are nested under `[fledgling]`.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document {
    fledgling: Config,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    dialect: Dialect,
    schema: Option<PathBuf>,
    migrations: Option<PathBuf>,
    database_url: Option<String>,
}

impl TryFrom<Settings> for Config {
    type Error = String;

    fn try_from(settings: Settings) -> Result<Self, Self::Error> {
        let database = settings
            .database_url
            .map(|url| match settings.dialect {
                Dialect::SQLite => sqlite_database_path(&url),
                Dialect::Postgres
                    if url.starts_with("postgres://") || url.starts_with("postgresql://") =>
                {
                    url.parse::<postgres::Config>()
                        .map_err(|_| "database_url is not a valid PostgreSQL URL".to_owned())?;
                    Ok(PathBuf::from(url))
                }
                Dialect::Postgres => Err(
                    "database_url must be a PostgreSQL URL (postgresql://localhost/example)".into(),
                ),
            })
            .transpose()?;
        Ok(Self {
            dialect: settings.dialect,
            schema: settings.schema,
            migrations: settings.migrations,
            database,
        })
    }
}

impl From<Config> for Settings {
    fn from(config: Config) -> Self {
        Self {
            dialect: config.dialect,
            schema: config.schema,
            migrations: config.migrations,
            database_url: config.database.map(|database| match config.dialect {
                Dialect::SQLite => format!("sqlite://{}", database.display()),
                Dialect::Postgres => database.to_string_lossy().into_owned(),
            }),
        }
    }
}

// ----------------------------------------------------------------------------
// Configuration Struct
// ----------------------------------------------------------------------------

const DEFAULT_SCHEMA_PATH: &str = "schema.sql";
const DEFAULT_MIGRATIONS_PATH: &str = "migrations";
const DEFAULT_SQLITE_PATH: &str = "example.db";
const DEFAULT_POSTGRES_URL: &str = "postgresql://localhost/example";

/// Project settings; commands choose which optional paths they require.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "Settings", into = "Settings")]
pub struct Config {
    /// SQL dialect used by the project.
    pub dialect: Dialect,

    /// The path to the schema SQL file, resolved relative to the config file when read.
    pub schema: Option<PathBuf>,

    /// The path to the migrations directory, resolved relative to the config file when read.
    pub migrations: Option<PathBuf>,

    /// SQLite path or literal PostgreSQL URL, as accepted by the library.
    pub database: Option<PathBuf>,
}

impl Config {
    /// Create a project config with default paths and a dialect-specific
    /// database location for values passed as `None`.
    pub fn new(
        dialect: Dialect,
        schema: Option<PathBuf>,
        migrations: Option<PathBuf>,
        database: Option<PathBuf>,
    ) -> Self {
        Self {
            dialect,
            schema: schema.or_else(|| Some(DEFAULT_SCHEMA_PATH.into())),
            migrations: migrations.or_else(|| Some(DEFAULT_MIGRATIONS_PATH.into())),
            database: database.or_else(|| {
                Some(
                    match dialect {
                        Dialect::SQLite => DEFAULT_SQLITE_PATH,
                        Dialect::Postgres => DEFAULT_POSTGRES_URL,
                    }
                    .into(),
                )
            }),
        }
    }

    /// Read a TOML config file.
    ///
    /// Relative paths are resolved against the config file's parent directory.
    /// The file must contain `[fledgling]` with a `dialect`; unknown fields
    /// are rejected. Referenced paths are not opened or checked.
    pub fn read(path: &Path) -> Result<Self, Box<dyn Error>> {
        let doc: Document = toml::from_str(&fs::read_to_string(path)?)?;
        let Document { fledgling: config } = doc;

        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let [schema, migrations] = resolve_paths(base, [config.schema, config.migrations]);
        let database = match config.dialect {
            Dialect::SQLite => {
                let [database] = resolve_paths(base, [config.database]);
                database
            }
            Dialect::Postgres => config.database,
        };

        Ok(Config {
            dialect: config.dialect,
            schema,
            migrations,
            database,
        })
    }

    /// Serialize this config as TOML.
    ///
    /// Writes paths as held in the config, without rebasing them against `path`.
    /// Existing files are overwritten; the parent directory must already exist.
    pub fn write(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let source = toml::to_string(&Document {
            fledgling: self.clone(),
        })?;

        fs::write(path, source)?;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{CONFIG_FILENAME, Config, Dialect, Document};
    use std::{
        fs, io,
        path::{Path, PathBuf},
    };

    const MINIMAL: &str = "[fledgling]\ndialect = 'sqlite'\n";

    fn parse(source: &str) -> Result<Config, toml::de::Error> {
        toml::from_str::<Document>(source).map(|doc| doc.fledgling)
    }

    #[test]
    fn new_uses_project_defaults_for_omitted_paths() {
        let config = Config::new(Dialect::SQLite, None, None, None);

        assert!(matches!(config.dialect, Dialect::SQLite));
        assert_eq!(config.schema.as_deref(), Some(Path::new("schema.sql")));
        assert_eq!(config.migrations.as_deref(), Some(Path::new("migrations")));
        assert_eq!(config.database.as_deref(), Some(Path::new("example.db")));
    }

    #[test]
    fn requires_a_supported_dialect_inside_the_project_table() {
        let config = parse(MINIMAL).unwrap();

        assert!(matches!(config.dialect, Dialect::SQLite));
        assert!(config.schema.is_none());
        assert!(config.migrations.is_none());
        assert!(config.database.is_none());

        for (source, expected) in [
            ("", "missing field `fledgling`"),
            ("dialect = 'sqlite'", "unknown field `dialect`"),
            ("[fledgling]", "missing field `dialect`"),
            ("[fledgling]\ndialect = 'mysql'", "unknown variant `mysql`"),
            (
                "[fledgling]\ndialect = 'sqlite'\ndatabase = 'db.sqlite'",
                "unknown field `database`",
            ),
            (
                "[fledgling]\ndialect = 'sqlite'\nschema = 4",
                "invalid type",
            ),
            (
                "[fledgling]\ndialect = 'sqlite'\nmigrations = 4",
                "invalid type",
            ),
            (
                "[fledgling]\ndialect = 'sqlite'\ndatabase_url = 4",
                "invalid type",
            ),
        ] {
            assert_rejected(source, expected);
        }
    }

    #[test]
    fn decodes_sqlite_urls_as_literal_paths() {
        for (url, expected) in [
            ("sqlite:db/example.db", "db/example.db"),
            ("sqlite://db/a%20b.db", "db/a%20b.db"),
            ("sqlite:///var/db/example.db", "/var/db/example.db"),
        ] {
            let config = parse(&format!("{MINIMAL}database_url = '{url}'")).unwrap();
            assert_eq!(
                config.database.as_deref(),
                Some(Path::new(expected)),
                "{url}"
            );
        }
    }

    #[test]
    fn rejects_invalid_sqlite_urls() {
        for url in [
            "",
            "sqlite://",
            "sqlite://   ",
            "postgres://localhost/db",
            "sqlite://db?mode=memory",
            "sqlite://db#fragment",
            r"sqlite://db\u0000",
        ] {
            assert_rejected(
                &format!("{MINIMAL}database_url = \"{url}\""),
                "database_url must be a SQLite URL",
            );
        }
    }

    fn assert_rejected(source: &str, expected: &str) {
        let error = parse(source).unwrap_err();
        assert!(error.message().contains(expected), "{source:?}: {error}");
    }

    #[test]
    fn reads_paths_relative_to_the_config_file() {
        let dir = TempDir::new();
        let path = dir.0.join(CONFIG_FILENAME);
        fs::write(
            &path,
            format!("{MINIMAL}schema = 'sql/schema.sql'\nmigrations = 'sql/migrations'\ndatabase_url = 'sqlite://db/example.db'"),
        )
        .unwrap();

        let config = Config::read(&path).unwrap();
        assert_eq!(config.schema, Some(dir.0.join("sql/schema.sql")));
        assert_eq!(config.migrations, Some(dir.0.join("sql/migrations")));
        assert_eq!(config.database, Some(dir.0.join("db/example.db")));

        let absolute = dir.0.join("absolute.sql");
        let absolute_toml = toml::Value::String(absolute.to_str().unwrap().into());
        fs::write(
            &path,
            format!("{MINIMAL}schema = {absolute_toml}\ndatabase_url = 'sqlite::memory:'"),
        )
        .unwrap();
        let config = Config::read(&path).unwrap();
        assert_eq!(config.schema, Some(absolute));
        assert!(config.migrations.is_none());
        assert_eq!(config.database.as_deref(), Some(Path::new(":memory:")));
    }

    #[test]
    fn writes_project_toml_without_rebasing_paths() {
        let dir = TempDir::new();
        let path = dir.0.join(CONFIG_FILENAME);
        fs::write(&path, "old contents").unwrap();

        Config {
            dialect: Dialect::SQLite,
            schema: Some("sql/schema.sql".into()),
            migrations: None,
            database: Some("db/example.db".into()),
        }
        .write(&path)
        .unwrap();

        let actual: toml::Table = fs::read_to_string(path).unwrap().parse().unwrap();
        let expected: toml::Table =
            format!("{MINIMAL}schema = 'sql/schema.sql'\ndatabase_url = 'sqlite://db/example.db'")
                .parse()
                .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn keeps_postgres_urls_literal_and_checks_the_dialect() {
        let dir = TempDir::new();
        let path = dir.0.join(CONFIG_FILENAME);
        let url = "postgresql://user@localhost:5432/example?sslmode=require";
        let config = Config::new(Dialect::Postgres, None, None, Some(url.into()));
        config.write(&path).unwrap();

        let read = Config::read(&path).unwrap();
        assert!(matches!(read.dialect, Dialect::Postgres));
        assert_eq!(read.schema, Some(dir.0.join("schema.sql")));
        assert_eq!(read.database.as_deref(), Some(Path::new(url)));
        assert_rejected(
            "[fledgling]\ndialect = 'postgres'\ndatabase_url = 'sqlite://example.db'",
            "database_url must be a PostgreSQL URL",
        );
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let root = std::env::temp_dir();
            for id in 0.. {
                let path = root.join(format!("fledgling-config-{}-{id}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("failed to create {}: {error}", path.display()),
                }
            }
            unreachable!()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
