use std::{
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicUsize, Ordering},
};

use fledgling::{Dialect::Postgres, generate, migrate, preview, pull, rollback, status};
use postgres::{Client, NoTls};

mod scenarios;

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn unique_name() -> String {
    format!(
        "fledgling_{}_{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct Project {
    root: PathBuf,
    migrations: PathBuf,
    schema: PathBuf,
}

impl Project {
    fn new() -> Self {
        let root = std::env::temp_dir().join(unique_name());
        fs::create_dir(&root).unwrap();
        let migrations = root.join("migrations");
        fs::create_dir(&migrations).unwrap();
        let schema = root.join("schema.sql");
        Self {
            root,
            migrations,
            schema,
        }
    }

    fn write_migration(&self, id: u64, up: &str, down: &str) {
        fs::write(self.migrations.join(format!("{id:06}.up.sql")), up).unwrap();
        fs::write(self.migrations.join(format!("{id:06}.down.sql")), down).unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn previews_and_generates_offline_from_config_and_history() {
    let project = Project::new();
    let config = project.root.join("fledgling.toml");
    fs::write(
        &config,
        r#"[fledgling]
dialect = "postgres"
schema = "schema.sql"
migrations = "migrations"
"#,
    )
    .unwrap();
    let cli = |command| {
        let output = Command::new(env!("CARGO_BIN_EXE_fledgling"))
            .args([command, "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    fs::write(
        &project.schema,
        "CREATE TABLE books (id INTEGER PRIMARY KEY);",
    )
    .unwrap();
    assert!(cli("generate").ends_with("000001.up.sql\n"));
    assert_eq!(cli("preview"), "No schema changes.\n");

    fs::write(
        &project.schema,
        "
        CREATE TABLE public.books (id INTEGER PRIMARY KEY, title TEXT);
        CREATE INDEX by_title ON books (title);
        CREATE VIEW titles AS SELECT title FROM books;
        CREATE VIEW short_titles AS SELECT title FROM titles;
    ",
    )
    .unwrap();
    let (up, down) = preview(Postgres, &project.migrations, &project.schema)
        .unwrap()
        .unwrap();
    assert!(
        up.starts_with("ALTER TABLE \"public\".\"books\" ADD COLUMN "),
        "{up}"
    );
    assert!(
        up.find("CREATE INDEX").unwrap() < up.find("CREATE VIEW").unwrap(),
        "{up}"
    );
    assert!(
        down.starts_with("DROP VIEW \"public\".\"short_titles\";"),
        "{down}"
    );
    assert!(down.ends_with("DROP COLUMN \"title\";"), "{down}");
    assert_eq!(fs::read_dir(&project.migrations).unwrap().count(), 2);
    assert_eq!(cli("preview"), format!("-- Up\n{up}\n\n-- Down\n{down}\n"));
    assert!(cli("generate").ends_with("000002.up.sql\n"));
    assert_eq!(cli("preview"), "No schema changes.\n");
}

#[test]
fn rejects_destructive_ambiguous_and_unsupported_plans_without_publishing() {
    let project = Project::new();
    project.write_migration(1, "CREATE TABLE books (id int);", "DROP TABLE books;");
    for (target, expected) in [
        ("", "removing PostgreSQL TABLE"),
        ("CREATE TABLE books (id text);", "changing PostgreSQL TABLE"),
        (
            "CREATE TABLE books (id int, title text NOT NULL);",
            "must be plain nullable",
        ),
        (
            "CREATE TABLE books (id int, seq serial);",
            "must be plain nullable",
        ),
        (
            "CREATE TABLE books (id int, title text DEFAULT '');",
            "must be plain nullable",
        ),
        (
            "CREATE TABLE books (id int); CREATE VIEW broken AS SELECT * FROM absent;",
            "unknown or unsupported relation",
        ),
        (
            "CREATE TABLE books (id int); CREATE VIEW broken AS WITH t AS (SELECT 1) SELECT * FROM t;",
            "not supported",
        ),
        (
            "CREATE TABLE books (id int); CREATE INDEX books ON books (id);",
            "duplicate PostgreSQL relation",
        ),
        (
            "CREATE TABLE books (id int) PARTITION BY RANGE (id);",
            "unsupported options",
        ),
    ] {
        fs::write(&project.schema, target).unwrap();
        let error = generate(Postgres, &project.migrations, &project.schema).unwrap_err();
        assert!(error.to_string().contains(expected), "{target}: {error}");
        assert_eq!(fs::read_dir(&project.migrations).unwrap().count(), 2);
    }
}

#[test]
fn replays_handwritten_add_column_with_constraints() {
    let project = Project::new();
    project.write_migration(
        1,
        "CREATE TABLE authors (id int PRIMARY KEY); CREATE TABLE books (id int PRIMARY KEY);",
        "DROP TABLE books; DROP TABLE authors;",
    );
    project.write_migration(
        2,
        "ALTER TABLE books ADD COLUMN author int NOT NULL REFERENCES authors (id);",
        "ALTER TABLE books DROP COLUMN author;",
    );
    fs::write(
        &project.schema,
        "
        CREATE TABLE authors (id int PRIMARY KEY);
        CREATE TABLE books (id int PRIMARY KEY, author int NOT NULL REFERENCES authors (id));
    ",
    )
    .unwrap();
    assert!(
        preview(Postgres, &project.migrations, &project.schema)
            .unwrap()
            .is_none()
    );
}

// The test server must allow the configured role to create test databases.
// Tests never run migration SQL in the database from the admin URL itself.
struct Database {
    admin: Client,
    name: String,
    url: String,
}

impl Database {
    fn new() -> Self {
        let admin_url = std::env::var("FLEDGLING_TEST_POSTGRES_URL")
            .expect("set FLEDGLING_TEST_POSTGRES_URL to a disposable local PostgreSQL server");
        let (base, query) = admin_url.split_once('?').unwrap_or((&admin_url, ""));
        let prefix = base
            .rsplit_once('/')
            .expect("admin URL must include a database name")
            .0;
        let name = unique_name();
        let url = if query.is_empty() {
            format!("{prefix}/{name}")
        } else {
            format!("{prefix}/{name}?{query}")
        };
        let mut admin = Client::connect(&admin_url, NoTls).unwrap();
        admin
            .batch_execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
            .unwrap();
        Self { admin, name, url }
    }

    fn path(&self) -> &Path {
        Path::new(&self.url)
    }

    fn connect(&self) -> Client {
        Client::connect(&self.url, NoTls).unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        self.admin
            .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .unwrap();
    }
}

#[test]
#[ignore = "requires FLEDGLING_TEST_POSTGRES_URL"]
fn applies_generated_pairs_rolls_back_and_preserves_ledger_on_failure() {
    let db = Database::new();
    let path = db.path();
    let mut client = db.connect();
    let project = Project::new();
    let history = &project.migrations;
    assert!(status(Postgres, path).unwrap().is_empty());
    assert_eq!(rollback(Postgres, path, history).unwrap(), None);
    let has_relation = |client: &mut Client, relation: &str| {
        client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&relation])
            .unwrap()
            .get::<_, bool>(0)
    };
    assert!(!has_relation(&mut client, "public._fledgling_migrations"));

    project.write_migration(1, "CREATE TABLE bad (id int); SELECT missing FROM bad;", "");
    assert!(migrate(Postgres, path, history).is_err());
    assert!(!has_relation(&mut client, "public.bad"));
    assert!(!has_relation(&mut client, "public._fledgling_migrations"));
    fs::remove_file(history.join("000001.up.sql")).unwrap();
    fs::remove_file(history.join("000001.down.sql")).unwrap();

    fs::write(
        &project.schema,
        "CREATE TABLE books (id bigint PRIMARY KEY);",
    )
    .unwrap();
    generate(Postgres, history, &project.schema)
        .unwrap()
        .unwrap();
    fs::write(
        &project.schema,
        "
        CREATE TABLE books (id bigint PRIMARY KEY, title text);
        CREATE INDEX books_title ON books (title);
        CREATE VIEW titles AS SELECT title FROM books;
    ",
    )
    .unwrap();
    generate(Postgres, history, &project.schema)
        .unwrap()
        .unwrap();
    assert_eq!(migrate(Postgres, path, history).unwrap(), [1, 2]);
    assert!(migrate(Postgres, path, history).unwrap().is_empty());
    assert_eq!(status(Postgres, path).unwrap(), [1, 2]);
    let config = project.root.join("fledgling.toml");
    fs::write(&config, "[fledgling]\ndialect = 'postgres'\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_fledgling"))
        .args(["status", "--config"])
        .arg(&config)
        .args(["--database", &db.url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"000001\n000002\n");
    assert!(has_relation(&mut client, "public.titles"));

    for sql in [
        "CREATE TABLE bad (id int); COMMIT; BEGIN;",
        "SELECT * FROM public._fledgling_migrations;",
        "DO $$BEGIN EXECUTE 'DELETE FROM public._fledgling_migrations'; END$$;",
        "CREATE INDEX CONCURRENTLY bad_idx ON books (id);",
    ] {
        project.write_migration(3, sql, "");
        assert!(migrate(Postgres, path, history).is_err(), "{sql}");
        assert_eq!(status(Postgres, path).unwrap(), [1, 2]);
        assert!(!has_relation(&mut client, "public.bad"));
        assert!(!has_relation(&mut client, "public.bad_idx"));
    }
    let down_path = history.join("000002.down.sql");
    let down = fs::read_to_string(&down_path).unwrap();
    fs::write(&down_path, format!("{down}\nSELECT missing FROM books;")).unwrap();
    assert!(rollback(Postgres, path, history).is_err());
    assert_eq!(status(Postgres, path).unwrap(), [1, 2]);
    assert!(has_relation(&mut client, "public.titles"));
    fs::write(&down_path, down).unwrap();
    assert_eq!(rollback(Postgres, path, history).unwrap(), Some(2));
    assert!(!has_relation(&mut client, "public.titles"));
    assert_eq!(status(Postgres, path).unwrap(), [1]);
    assert_eq!(rollback(Postgres, path, history).unwrap(), Some(1));
    assert!(!has_relation(&mut client, "public.books"));

    fs::remove_file(history.join("000003.up.sql")).unwrap();
    fs::remove_file(history.join("000003.down.sql")).unwrap();
    project.write_migration(u64::MAX, "CREATE TABLE last (id int);", "DROP TABLE last;");
    assert_eq!(migrate(Postgres, path, history).unwrap(), [1, 2, u64::MAX]);
    assert_eq!(status(Postgres, path).unwrap(), [1, 2, u64::MAX]);
    assert_eq!(rollback(Postgres, path, history).unwrap(), Some(u64::MAX));
}

#[test]
#[ignore = "requires FLEDGLING_TEST_POSTGRES_URL"]
fn pulls_dependency_ordered_ddl_without_ledger_objects() {
    let source = Database::new();
    let target = Database::new();
    let project = Project::new();
    project.write_migration(
        1,
        "
        CREATE SCHEMA inventory;
        CREATE TABLE inventory.books (id serial, name text);
        CREATE UNIQUE INDEX books_id ON inventory.books (id);
        CREATE TABLE public.books (
            id bigint GENERATED ALWAYS AS IDENTITY (START WITH 100 CACHE 5) PRIMARY KEY,
            inventory_id int REFERENCES inventory.books (id)
        );
        CREATE VIEW public.z_names AS SELECT name FROM inventory.books;
        CREATE VIEW public.a_names AS SELECT name FROM public.z_names;
    ",
        "DROP SCHEMA inventory CASCADE;",
    );
    migrate(Postgres, source.path(), &project.migrations).unwrap();
    let sql = pull(Postgres, source.path()).unwrap();
    assert!(!sql.contains("_fledgling_migrations"), "{sql}");
    assert!(
        sql.find("CREATE VIEW public.z_names").unwrap()
            < sql.find("CREATE VIEW public.a_names").unwrap(),
        "{sql}"
    );
    target.connect().batch_execute(&sql).unwrap();
    assert_eq!(pull(Postgres, target.path()).unwrap(), sql);

    source
        .connect()
        .batch_execute("CREATE TABLE unsupported (id int) PARTITION BY RANGE (id)")
        .unwrap();
    assert!(
        pull(Postgres, source.path())
            .unwrap_err()
            .to_string()
            .contains("cannot represent PostgreSQL relation public.unsupported")
    );
}
