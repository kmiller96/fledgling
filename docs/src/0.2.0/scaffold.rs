//! Script to help the user scaffold fledgling.
//!
//! Fledgling requires a few files and folders to be setup to work. Rather than
//! make documentation to follow, we just script it.

use crate::config::{CONFIG_FILENAME, Config};
use fledgling::Dialect;
use inquire::{InquireError, Select, Text};
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

// ----------------------------------------------------------------------------
// Constants
// ----------------------------------------------------------------------------

const NEXT_STEPS: &str = "
Next steps (from the project directory):

1. Define your schema in schema.sql. For example:

   \x1b[1;35mCREATE TABLE books (
       id INTEGER PRIMARY KEY,
       title TEXT NOT NULL
   );\x1b[0m

2. Generate your first migration:

   \x1b[1;36mfledgling generate\x1b[0m

3. Apply the migration (creates the SQLite database if needed):

   \x1b[1;36mfledgling migrate\x1b[0m";

// ----------------------------------------------------------------------------
// Initialization
// ----------------------------------------------------------------------------
//
// TODO: Consider moving the interactive part(s) into the CLI and just define
// the scaffolding logic here.

const PROJECT_PATH_PROMPT: &str = "Where would you like to install the project?";
const DIALECT_PROMPT: &str = "What database are you using?";

/// Prompts the user for choices during the installer.
pub fn prompt_for_user_choices() -> Result<(String, Config), InquireError> {
    let path = Text::new(PROJECT_PATH_PROMPT).with_default(".").prompt()?;
    let dialect = Select::new(DIALECT_PROMPT, vec![Dialect::SQLite]).prompt()?;

    Ok((path, Config::new(dialect, None, None, None)))
}

/// Run the interactive project setup wizard.
///
/// There are a small collection of files you need to define to have Fledgling
/// work. Rather than having the users do all of this themselves, they should
/// have the ability for the CLI to scaffold most of this for them.
///
/// This function performs that scaffolding.
pub fn interactive(config: &Path) -> Result<(), Box<dyn Error>> {
    if config != Path::new(CONFIG_FILENAME) {
        return Err(format!(
            "init writes {CONFIG_FILENAME} in the project directory; omit --config"
        )
        .into());
    }

    // Guard the user choices
    let (path, config) = match prompt_for_user_choices() {
        Ok(choices) => choices,
        Err(InquireError::OperationCanceled) => return Ok(()),
        Err(error) => return Err(format!("could not run the init wizard: {error}").into()),
    };

    // Run scaffolding script with user choices
    let path = Path::new(&path);
    scaffold(path, &config)?;

    // Print information & return
    println!("Initialized Fledgling in {}", path.display());
    println!("{NEXT_STEPS}");
    Ok(())
}

// ----------------------------------------------------------------------------
// Scaffolder
// ----------------------------------------------------------------------------

/// Create the configured schema and migrations paths, but no database.
fn scaffold(path: &Path, config: &Config) -> Result<(), Box<dyn Error>> {
    let schema = path.join(config.schema.as_deref().ok_or("missing schema path")?);
    let migrations = path.join(
        config
            .migrations
            .as_deref()
            .ok_or("missing migrations path")?,
    );
    let config_path = path.join(CONFIG_FILENAME);
    for file in [&schema, &config_path] {
        match file.symlink_metadata() {
            Ok(_) => return Err(format!("{} already exists", file.display()).into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not check {}: {error}", file.display()).into());
            }
        }
    }
    fs::create_dir_all(&migrations)
        .map_err(|error| format!("could not create {}: {error}", migrations.display()))?;
    if let Some(parent) = schema.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&schema)
        .and_then(|mut output| output.write_all(b""))
        .map_err(|error| format!("could not write {}: {error}", schema.display()))?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
        .map_err(|error| format!("could not create {}: {error}", config_path.display()))?;
    config.write(&config_path)?;
    Ok(())
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{CONFIG_FILENAME, Config, Dialect, scaffold};
    use std::{fs, path::Path};

    // ------------------------------------------------------------------------
    // TempDir
    // ------------------------------------------------------------------------

    /// Temporary Directory utility to help clean up test code.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("fledgling-init-{name}-{}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn test_config() -> Config {
        Config::new(
            Dialect::SQLite,
            Some("sql/project.sql".into()),
            Some("history".into()),
            Some("project.db".into()),
        )
    }

    // ------------------------------------------------------------------------
    // Unit Tests
    // ------------------------------------------------------------------------

    #[test]
    fn creates_a_project_with_loadable_config() {
        let root = TempDir::new("scaffold");
        let path = root.path().join("nested project");
        scaffold(&path, &test_config()).unwrap();

        let config = Config::read(&path.join(CONFIG_FILENAME)).unwrap();
        let schema = config.schema.unwrap();
        let migrations = config.migrations.unwrap();
        let database = config.database.unwrap();
        assert_eq!(schema, path.join("sql/project.sql"));
        assert_eq!(migrations, path.join("history"));
        assert_eq!(database, path.join("project.db"));
        assert!(fs::read(schema).unwrap().is_empty());
        assert!(fs::read_dir(migrations).unwrap().next().is_none());
        assert!(!database.exists());
    }

    #[test]
    fn refuses_to_scaffold_a_project_twice() {
        let root = TempDir::new("conflict");
        let config = test_config();
        scaffold(root.path(), &config).unwrap();
        assert!(scaffold(root.path(), &config).is_err());
    }
}
