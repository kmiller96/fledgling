//! SQLite migration scenarios against the public library API.
//!
//! To add a scenario, create a folder under `tests/sqlite/scenarios/` containing:
//!
//! - `target.schema.sql`: the desired schema.
//! - `current.schema.sql` (optional): the starting schema. Omit it to start empty.
//! - `expected.up.sql` and `expected.down.sql`: the SQL to apply and revert.
//!   Leave both empty for a no-change scenario.
//! - Or `error.txt` instead of the Up/Down pair: the expected planner rejection.
//!
//! `dir-test` registers a separate Cargo test for each `expected.up.sql` or
//! `error.txt` file. No manual registration is needed. Run the suite with
//! `cargo test --test sqlite`, or append a scenario folder name to filter it.

use dir_test::{Fixture, dir_test};
use fledgling::{Dialect, generate, migrate, preview, pull, rollback, status};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

// ----------------------------------------------------------------------------
// Scenario tests
// ----------------------------------------------------------------------------

/// Verify the generated SQL and round-trip it against a real database.
#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/sqlite/scenarios",
    glob: "*/expected.up.sql",
)]
fn sqlite(fixture: Fixture<&str>) {
    let case = Path::new(fixture.path()).parent().unwrap();
    assert!(!case.join("error.txt").exists(), "unexpected error.txt");

    let dialect = Dialect::SQLite;
    let workspace = Workspace::new();
    let database = workspace.0.join("target.sqlite");
    let migrations = &workspace.0;
    let target_path = case.join("target.schema.sql");
    let target = reference(&read(&target_path));
    let baseline = reference(&seed_baseline(case, migrations));
    let assert_state = |phase, expected, history: &[u64]| {
        let actual =
            Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_schema(&actual, expected, phase);
        assert_eq!(
            status(dialect, &database).unwrap(),
            history,
            "{phase}: ledger"
        );
    };

    let expected_up = fixture.content();
    let expected_down = read(&case.join("expected.down.sql"));
    let count = fs::read_dir(migrations).unwrap().count();
    let planned = preview(dialect, migrations, &target_path).unwrap();
    assert_eq!(fs::read_dir(migrations).unwrap().count(), count);
    assert!(!database.exists());

    let baseline_history = migrate(dialect, &database, migrations).unwrap();
    assert_state("baseline", &baseline, &baseline_history);
    let generated = generate(dialect, migrations, &target_path).unwrap();
    assert_state("after generation", &baseline, &baseline_history);

    if expected_up.trim().is_empty() {
        assert!(expected_down.trim().is_empty());
        assert_eq!(planned, None);
        assert_eq!(generated, None);
        assert_eq!(fs::read_dir(migrations).unwrap().count(), count + 1); // database only
        assert_state("no changes", &target, &baseline_history);
        return;
    }

    let id = baseline_history.len() as u64 + 1;
    let up_path = generated.unwrap();
    assert_eq!(up_path, migrations.join(format!("{id:06}.up.sql")));
    let up = read(&up_path);
    let down = read(&migrations.join(format!("{id:06}.down.sql")));
    assert_eq!(canonical_sql(&up), canonical_sql(expected_up));
    assert_eq!(canonical_sql(&down), canonical_sql(&expected_down));
    assert_eq!(planned, Some((up, down)));
    assert_eq!(preview(dialect, migrations, &target_path).unwrap(), None);

    let mut target_history = baseline_history.clone();
    target_history.push(id);
    assert_eq!(migrate(dialect, &database, migrations).unwrap(), [id]);
    assert_state("apply", &target, &target_history);
    assert!(migrate(dialect, &database, migrations).unwrap().is_empty());

    let pulled = pull(dialect, &database).unwrap();
    assert!(!pulled.contains("_fledgling_migrations"));
    assert_schema(&reference(&pulled), &target, "pull");

    assert_eq!(rollback(dialect, &database, migrations).unwrap(), Some(id));
    assert_state("revert", &baseline, &baseline_history);

    assert_eq!(migrate(dialect, &database, migrations).unwrap(), [id]);
    assert_state("reapply", &target, &target_history);
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/sqlite/scenarios",
    glob: "*/error.txt",
)]
fn sqlite_rejected(fixture: Fixture<&str>) {
    let case = Path::new(fixture.path()).parent().unwrap();
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    seed_baseline(case, migrations);
    let target = case.join("target.schema.sql");
    let count = fs::read_dir(migrations).unwrap().count();
    for error in [
        preview(Dialect::SQLite, migrations, &target).unwrap_err(),
        generate(Dialect::SQLite, migrations, &target).unwrap_err(),
    ] {
        assert_eq!(error.to_string(), fixture.content().trim());
    }
    assert_eq!(fs::read_dir(migrations).unwrap().count(), count);
}

#[test]
fn reads_do_not_create_a_database_or_ledger() {
    let workspace = Workspace::new();
    let database = workspace.0.join("db.sqlite");
    let dialect = Dialect::SQLite;
    assert!(status(dialect, &database).is_err());
    assert!(pull(dialect, &database).is_err());
    assert!(!database.exists());

    assert!(
        migrate(dialect, &database, &workspace.0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(status(dialect, &database).unwrap(), []);
    assert_eq!(rollback(dialect, &database, &workspace.0).unwrap(), None);
    let db = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(catalog_rows(&db, "SELECT name FROM main.sqlite_schema").is_empty());
}

#[test]
fn applies_in_numeric_order_and_resumes_after_a_failed_migration() {
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    let database = migrations.join("db.sqlite");
    let dialect = Dialect::SQLite;
    let later = migrations.join("10.up.sql");
    fs::write(&later, "CREATE TABLE later (id); SELECT * FROM missing;").unwrap();
    fs::write(migrations.join("2.up.sql"), "CREATE TABLE first (id);").unwrap();
    let error = migrate(dialect, &database, migrations).unwrap_err();
    assert!(error.to_string().contains("10.up.sql"));
    assert_eq!(status(dialect, &database).unwrap(), [2]);
    assert_eq!(
        pull(dialect, &database).unwrap(),
        "CREATE TABLE first (id);\n"
    );

    fs::write(&later, "CREATE TABLE later AS SELECT id FROM first;").unwrap();
    assert_eq!(migrate(dialect, &database, migrations).unwrap(), [10]);
    assert_eq!(status(dialect, &database).unwrap(), [2, 10]);
    let error = rollback(dialect, &database, migrations).unwrap_err();
    assert!(error.to_string().contains("no Down SQL"));

    let down = migrations.join("10.down.sql");
    fs::write(&down, "DROP TABLE later; DROP TABLE missing;").unwrap();
    assert!(rollback(dialect, &database, migrations).is_err());
    assert!(
        pull(dialect, &database)
            .unwrap()
            .contains("CREATE TABLE later")
    );
    assert_eq!(status(dialect, &database).unwrap(), [2, 10]);

    fs::write(down, "DROP TABLE later;").unwrap();
    assert_eq!(rollback(dialect, &database, migrations).unwrap(), Some(10));
    fs::write(migrations.join("1.up.sql"), "CREATE TABLE inserted (id);").unwrap();
    assert!(migrate(dialect, &database, migrations).is_err());
    assert_eq!(status(dialect, &database).unwrap(), [2]);
}

#[test]
fn planning_rejects_duplicate_ids_and_generates_after_the_highest_id() {
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    let schema = migrations.join("schema.sql");
    let dialect = Dialect::SQLite;
    fs::write(migrations.join("2.up.sql"), "CREATE TABLE first (id);").unwrap();
    fs::write(migrations.join("10.up.sql"), "CREATE TABLE second (id);").unwrap();
    fs::write(
        &schema,
        "CREATE TABLE first (id); CREATE TABLE second (id); CREATE TABLE third (id);",
    )
    .unwrap();

    let duplicate = migrations.join("000010.up.sql");
    fs::write(&duplicate, "CREATE TABLE repeated (id);").unwrap();
    let error = preview(dialect, migrations, &schema).unwrap_err();
    assert!(error.to_string().contains("duplicate migration ID 000010"));

    fs::remove_file(duplicate).unwrap();
    assert_eq!(
        generate(dialect, migrations, &schema).unwrap(),
        Some(migrations.join("000011.up.sql"))
    );
}

#[test]
fn generation_refuses_to_replace_existing_files() {
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    let schema = migrations.join("schema.sql");
    let down = migrations.join("000001.down.sql");
    fs::write(&schema, "CREATE TABLE books (id);").unwrap();
    fs::write(&down, "keep this file").unwrap();
    assert!(generate(Dialect::SQLite, migrations, &schema).is_err());
    assert_eq!(read(&down), "keep this file");
    assert!(!migrations.join("000001.up.sql").exists());
}

#[test]
fn generation_replays_triggers_and_appends_to_a_previously_altered_table() {
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    let schema = migrations.join("schema.sql");
    let dialect = Dialect::SQLite;
    let trigger = "CREATE TRIGGER log_insert AFTER INSERT ON \"log(table)\" BEGIN
        INSERT INTO audit VALUES ('hello; world');
        INSERT INTO audit VALUES ('done');
    END;";
    for columns in [
        "id INTEGER",
        "id INTEGER, /* comma, parenthesis ) */ [first note] TEXT",
        "id INTEGER, [first note] TEXT, second_note TEXT",
    ] {
        fs::write(
            &schema,
            format!(
                "CREATE TABLE \"log(table)\" ({columns}, CHECK (id > 0));
                 CREATE TABLE audit (message TEXT);
                 {trigger}"
            ),
        )
        .unwrap();
        assert!(generate(dialect, migrations, &schema).unwrap().is_some());
        assert!(preview(dialect, migrations, &schema).unwrap().is_none());
    }
    let database = migrations.join("db.sqlite");
    assert_eq!(migrate(dialect, &database, migrations).unwrap(), [1, 2, 3]);
    assert!(pull(dialect, &database).unwrap().contains(trigger));
}

#[test]
fn rejects_executable_sql_and_unresolved_views_in_the_desired_schema() {
    let workspace = Workspace::new();
    let migrations = &workspace.0;
    let schema = migrations.join("schema.sql");
    for sql in [
        "CREATE TABLE books (id); INSERT INTO books VALUES (1);",
        "CREATE VIEW books AS SELECT * FROM missing;",
        "CREATE TABLE books (id); CREATE TABLE IF NOT EXISTS BOOKS (other);",
    ] {
        fs::write(&schema, sql).unwrap();
        assert!(
            generate(Dialect::SQLite, migrations, &schema).is_err(),
            "{sql}"
        );
    }
    assert!(!migrations.join("000001.up.sql").exists());
}

// ----------------------------------------------------------------------------
// Fixture helpers
// ----------------------------------------------------------------------------

fn seed_baseline(case: &Path, migrations: &Path) -> String {
    let path = case.join("current.schema.sql");
    if !path.is_file() {
        return String::new();
    }
    let sql = read(&path);
    fs::write(migrations.join("000001.up.sql"), &sql).unwrap();
    sql
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Build the expected schema independently of the migration driver.
fn reference(sql: &str) -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(sql).unwrap();
    db
}

fn canonical_sql(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(str::split_whitespace)
        .map(|parts| parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase())
        .filter(|statement| !statement.is_empty())
        .collect()
}

// ----------------------------------------------------------------------------
// Schema assertions
// ----------------------------------------------------------------------------

/// Compare user objects and table columns, excluding backend-owned history.
/// ALTER TABLE can leave different quoting and whitespace than a fresh
/// CREATE TABLE, so table columns are compared via SQLite's catalog.
fn assert_schema(actual: &Connection, expected: &Connection, phase: &str) {
    for (label, sql) in [
        (
            "objects",
            "SELECT type, name, tbl_name, CASE WHEN type = 'table' THEN NULL ELSE rtrim(sql, char(10) || char(13) || ' ') END
               FROM main.sqlite_schema
              WHERE name <> '_fledgling_migrations' AND sql IS NOT NULL
              ORDER BY type, name",
        ),
        (
            "table columns",
            "SELECT s.name, p.cid, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk, p.hidden
               FROM main.sqlite_schema AS s JOIN pragma_table_xinfo(s.name, 'main') AS p
              WHERE s.type = 'table' AND s.name <> '_fledgling_migrations'
              ORDER BY s.name, p.cid",
        ),
    ] {
        assert_eq!(
            catalog_rows(actual, sql),
            catalog_rows(expected, sql),
            "{phase}: {label}"
        );
    }
}

fn catalog_rows(db: &Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = db.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |row| (0..width).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// ----------------------------------------------------------------------------
// Temporary workspace
// ----------------------------------------------------------------------------

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fledgling-acceptance-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
