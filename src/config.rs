//! Defines the CLI's project configuration and its TOML representation.
//!
//! [`Config`] stores the selected SQL dialect and optional schema, migrations,
//! and database paths. This module reads and writes the `[fledgling]` TOML
//! section, including SQLite `database_url` conversion. Paths read from disk
//! are resolved relative to the config file; reading does not check or open
//! the referenced files or database.

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
// SQLite URL serialization
// ----------------------------------------------------------------------------

/// Decode `sqlite:` with optional `//` as a literal path, not a general URL.
/// Empty paths, query strings, fragments, and NULs are not accepted.
fn sqlite_database_path<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PathBuf>, D::Error> {
    let Some(url) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let path = url
        .strip_prefix("sqlite:")
        .map(|path| path.strip_prefix("//").unwrap_or(path))
        .filter(|path| !path.trim().is_empty() && !path.contains(['?', '#', '\0']));
    path.map(PathBuf::from).map(Some).ok_or_else(|| {
        serde::de::Error::custom(
            "database_url must be a SQLite URL with a file path (sqlite://example.db) or sqlite::memory:",
        )
    })
}

/// Serialize a present path with the `sqlite://` prefix, without URL escaping.
fn sqlite_database_url<S: serde::Serializer>(
    database: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    database
        .as_ref()
        .map(|path| format!("sqlite://{}", path.display()))
        .serialize(serializer)
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

// ----------------------------------------------------------------------------
// Configuration Struct
// ----------------------------------------------------------------------------

const DEFAULT_SCHEMA_PATH: &str = "schema.sql";
const DEFAULT_MIGRATIONS_PATH: &str = "migrations";
const DEFAULT_SQLITE_PATH: &str = "example.db";

/// Project settings; commands choose which optional paths they require.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// SQL dialect used by the project.
    pub dialect: Dialect,

    /// The path to the schema SQL file, resolved relative to the config file when read.
    pub schema: Option<PathBuf>,

    /// The path to the migrations directory, resolved relative to the config file when read.
    pub migrations: Option<PathBuf>,

    /// SQLite path from TOML `database_url`, resolved like the other paths
    /// except for `:memory:`, which stays literal.
    #[serde(
        rename = "database_url",
        default,
        deserialize_with = "sqlite_database_path",
        serialize_with = "sqlite_database_url"
    )]
    pub database: Option<PathBuf>,
}

impl Config {
    /// Create a project config, using `schema.sql`, `migrations`, and `example.db`
    /// for paths passed as `None`. Paths are stored as-is.
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
            database: database.or_else(|| Some(DEFAULT_SQLITE_PATH.into())),
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
        let [schema, migrations, database] =
            resolve_paths(base, [config.schema, config.migrations, config.database]);

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
            fledgling: Config {
                dialect: self.dialect,
                schema: self.schema.clone(),
                migrations: self.migrations.clone(),
                database: self.database.clone(),
            },
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
    fn requires_a_sqlite_dialect_inside_the_project_table() {
        let config = parse(MINIMAL).unwrap();

        assert!(matches!(config.dialect, Dialect::SQLite));
        assert!(config.schema.is_none());
        assert!(config.migrations.is_none());
        assert!(config.database.is_none());

        for (source, expected) in [
            ("", "missing field `fledgling`"),
            ("dialect = 'sqlite'", "unknown field `dialect`"),
            ("[fledgling]", "missing field `dialect`"),
            (
                "[fledgling]\ndialect = 'postgres'",
                "unknown variant `postgres`",
            ),
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
