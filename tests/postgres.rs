//! PostgreSQL migration scenarios. See `tests/scenarios/postgres/README.md` for setup.

use dir_test::{Fixture, dir_test};
use postgres::{Client, NoTls};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{self, Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/scenarios/postgres",
    glob: "*/expected.up.sql",
)]
fn postgres(fixture: Fixture<&str>) {
    let project = Project::new(fixture.path());
    let current = project.seed();
    let output = project.run("preview");
    let expected_up = fixture.content();
    let expected_down = fs::read_to_string(project.case.join("expected.down.sql")).unwrap();
    let id = if current.is_empty() {
        "000001"
    } else {
        "000002"
    };
    let up_path = project.migrations().join(format!("{id}.up.sql"));
    let down_path = project.migrations().join(format!("{id}.down.sql"));
    assert!(!up_path.exists());
    project.run("generate");

    let has_changes = !expected_up.trim().is_empty();
    if !has_changes {
        assert!(expected_down.trim().is_empty());
        assert_eq!(output, "No schema changes.\n");
        assert!(!up_path.exists() && !down_path.exists());
    } else {
        let up = fs::read_to_string(&up_path).unwrap();
        let down = fs::read_to_string(&down_path).unwrap();
        assert_eq!(canonical_sql(&up), canonical_sql(expected_up), "Up SQL");
        assert_eq!(
            canonical_sql(&down),
            canonical_sql(&expected_down),
            "Down SQL"
        );
        assert_eq!(output, format!("-- Up\n{up}\n\n-- Down\n{down}\n"));
    }
    assert_eq!(project.run("preview"), "No schema changes.\n");
    let Some(mut databases) = project.databases() else {
        return;
    };
    let live_url = databases.create("live");
    let mut live = Client::connect(&live_url, NoTls).unwrap();
    project.configure(&live_url);
    assert_eq!(project.run("status"), "");
    assert_eq!(project.run("pull"), "");
    assert!(
        live.query_one(
            "SELECT to_regclass('public._fledgling_migrations') IS NULL",
            &[],
        )
        .unwrap()
        .get::<_, bool>(0),
        "read-only commands initialized the ledger"
    );

    // Compare with independent reference schemas loaded directly into PostgreSQL.
    let mut connection = Client::connect(&databases.create("reference"), NoTls).unwrap();
    connection.batch_execute(&current).unwrap();
    let baseline = catalog(&mut connection);
    connection
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .unwrap();
    let target = fs::read_to_string(project.case.join("target.schema.sql")).unwrap();
    connection.batch_execute(&target).unwrap();
    let expected = catalog(&mut connection);

    project.run("migrate");
    let baseline_history = if current.is_empty() { "" } else { "000001\n" };
    let target_history = if has_changes {
        format!("{baseline_history}{id}\n")
    } else {
        baseline_history.to_owned()
    };
    assert_eq!(project.run("status"), target_history);
    assert_eq!(catalog(&mut live), expected);
    assert_eq!(project.run("migrate"), "");

    let pulled = project.run("pull");
    assert!(!pulled.contains("_fledgling"));
    connection
        .batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .unwrap();
    connection.batch_execute(&pulled).unwrap();
    assert_eq!(catalog(&mut connection), expected, "pull");

    if has_changes {
        assert_eq!(project.run("rollback"), format!("Rolled back {id}\n"));
        assert_eq!(project.run("status"), baseline_history);
        assert_eq!(catalog(&mut live), baseline);
        assert_eq!(project.run("migrate"), format!("Applied {id}\n"));
        assert_eq!(catalog(&mut live), expected);
    }
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/scenarios/postgres",
    glob: "*/error.txt",
)]
fn postgres_rejected(fixture: Fixture<&str>) {
    let project = Project::new(fixture.path());
    project.seed();
    let count = fs::read_dir(project.migrations()).unwrap().count();
    for command in ["preview", "generate"] {
        let error = rejected(project.command(command));
        assert!(error.contains(fixture.content().trim()), "{error}");
        assert_eq!(fs::read_dir(project.migrations()).unwrap().count(), count);
    }
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/scenarios/postgres",
    glob: "_runner/*/error.txt",
)]
fn postgres_runner_rejected(fixture: Fixture<&str>) {
    let project = Project::new(fixture.path());
    let Some(mut databases) = project.databases() else {
        return;
    };
    let url = databases.create("live");
    project.configure(&url);
    let baseline = fs::read_to_string(project.case.join("current.schema.sql")).unwrap();
    fs::write(project.migrations().join("000001.up.sql"), baseline).unwrap();
    project.run("migrate");
    let mut live = Client::connect(&url, NoTls).unwrap();
    let before = catalog(&mut live);
    let up = project.case.join("invalid.up.sql");
    let down = project.case.join("invalid.down.sql");
    let command = match (up.is_file(), down.is_file()) {
        (true, false) => {
            fs::copy(up, project.migrations().join("000002.up.sql")).unwrap();
            "migrate"
        }
        (false, true) => {
            fs::copy(down, project.migrations().join("000001.down.sql")).unwrap();
            "rollback"
        }
        _ => panic!("expected exactly one invalid migration fixture"),
    };
    let error = rejected(project.command(command));
    assert!(error.contains(fixture.content().trim()), "{error}");
    assert_eq!(project.run("status"), "000001\n");
    assert_eq!(catalog(&mut live), before);
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/scenarios/postgres",
    glob: "_pull/*/current.schema.sql",
)]
fn postgres_pull(fixture: Fixture<&str>) {
    let project = Project::new(fixture.path());
    let Some(mut databases) = project.databases() else {
        return;
    };
    let url = databases.create("live");
    project.configure(&url);
    let mut live = Client::connect(&url, NoTls).unwrap();
    live.batch_execute(fixture.content()).unwrap();
    let error_path = project.case.join("error.txt");
    if error_path.exists() {
        let error = rejected(project.command("pull"));
        assert!(
            error.contains(fs::read_to_string(error_path).unwrap().trim()),
            "{error}"
        );
        return;
    }
    let pulled = project.run("pull");
    let mut reference = Client::connect(&databases.create("reference"), NoTls).unwrap();
    reference.batch_execute(&pulled).unwrap_or_else(|error| {
        panic!("could not replay pulled SQL: {error:?}\n{pulled}");
    });
    assert_eq!(catalog(&mut live), catalog(&mut reference));
    let assertions = project.case.join("assert.sql");
    if assertions.exists() {
        let sql = fs::read_to_string(assertions).unwrap();
        for client in [&mut live, &mut reference] {
            assert!(client.query_one(&sql, &[]).unwrap().get::<_, bool>(0));
        }
    }
}

fn canonical_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Compare PostgreSQL's own deparsed catalog, not Fledgling's schema model.
fn catalog(client: &mut Client) -> Vec<String> {
    client
        .query(
            "SELECT definition FROM (
                SELECT c.relname, 0 AS kind, 0 AS position,
                       concat_ws(' ', c.relkind, c.relname,
                           CASE WHEN c.relkind = 'v' THEN pg_get_viewdef(c.oid) END) AS definition
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                WHERE n.nspname = 'public' AND c.relkind IN ('r', 'v')
                UNION ALL
                SELECT c.relname, 1, a.attnum,
                       concat_ws(' ', a.attname, format_type(a.atttypid, a.atttypmod),
                           a.attnotnull, pg_get_expr(d.adbin, d.adrelid))
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_attribute a ON a.attrelid = c.oid
                LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
                WHERE n.nspname = 'public' AND c.relkind IN ('r', 'v')
                  AND a.attnum > 0 AND NOT a.attisdropped
                UNION ALL
                SELECT c.relname, 2, 0, pg_get_constraintdef(k.oid)
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_constraint k ON k.conrelid = c.oid
                WHERE n.nspname = 'public'
                UNION ALL
                SELECT c.relname, 3, 0, pg_get_indexdef(i.indexrelid)
                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                JOIN pg_index i ON i.indrelid = c.oid
                WHERE n.nspname = 'public'
            ) objects
            WHERE relname <> '_fledgling_migrations'
            ORDER BY relname, kind, position, definition",
            &[],
        )
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect()
}

struct Project {
    case: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new(fixture: &str) -> Self {
        let case = Path::new(fixture).parent().unwrap().to_path_buf();
        let key = format!(
            "fledgling_{}_{}_{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        );
        let root = env::temp_dir().join(&key);
        let project = Self { case, root };
        fs::create_dir(&project.root).unwrap();
        fs::create_dir(project.migrations()).unwrap();
        project.configure("postgresql://127.0.0.1:1/unused");
        project
    }

    fn databases(&self) -> Option<Databases> {
        let admin_url = match env::var("FLEDGLING_TEST_POSTGRES_URL") {
            Ok(url) => url,
            Err(env::VarError::NotPresent) => {
                eprintln!("skipping live checks: FLEDGLING_TEST_POSTGRES_URL is not set");
                return None;
            }
            Err(error) => panic!("{error}"),
        };
        Some(Databases {
            admin: Client::connect(&admin_url, NoTls).unwrap(),
            admin_url,
            key: self.root.file_name().unwrap().to_str().unwrap().into(),
            names: Vec::new(),
        })
    }

    fn configure(&self, live_url: &str) {
        fs::write(
            self.root.join("fledgling.toml"),
            format!(
                "[fledgling]\ndialect = 'postgres'\ndatabase_url = {live_url:?}\nmigrations = 'migrations'\nschema = 'schema.sql'\n",
            ),
        )
        .unwrap();
    }

    fn seed(&self) -> String {
        fs::copy(
            self.case.join("target.schema.sql"),
            self.root.join("schema.sql"),
        )
        .unwrap();
        let path = self.case.join("current.schema.sql");
        if !path.exists() {
            return String::new();
        }
        let current = fs::read_to_string(path).unwrap();
        if !current.is_empty() {
            fs::write(self.migrations().join("000001.up.sql"), &current).unwrap();
        }
        current
    }

    fn migrations(&self) -> PathBuf {
        self.root.join("migrations")
    }

    fn command(&self, command: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fledgling"))
            .current_dir(&self.root)
            .arg(command)
            .output()
            .unwrap()
    }

    fn run(&self, command: &str) -> String {
        let output = self.command(command);
        assert!(
            output.status.success(),
            "{command} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Databases {
    admin: Client,
    admin_url: String,
    key: String,
    names: Vec<String>,
}

impl Databases {
    fn create(&mut self, suffix: &str) -> String {
        let name = format!("{}_{suffix}", self.key);
        let (scheme, rest) = self
            .admin_url
            .split_once("://")
            .expect("test PostgreSQL URL must have a scheme");
        let (server, _) = rest
            .split_once('/')
            .expect("test PostgreSQL URL must have a host and database");
        let query = self
            .admin_url
            .split_once('?')
            .map_or("", |(_, query)| query);
        let mut url = format!("{scheme}://{server}/{name}");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        let config: postgres::Config = url.parse().expect("invalid test PostgreSQL URL");
        assert_eq!(config.get_dbname(), Some(name.as_str()));
        self.admin
            .batch_execute(&format!("CREATE DATABASE {name} TEMPLATE template0"))
            .unwrap();
        self.names.push(name);
        url
    }
}

impl Drop for Databases {
    fn drop(&mut self) {
        for name in &self.names {
            if let Err(error) = self.admin.batch_execute(&format!("DROP DATABASE {name}")) {
                eprintln!("could not remove test database {name}: {error}");
            }
        }
    }
}

fn rejected(output: Output) -> String {
    assert!(!output.status.success(), "command unexpectedly succeeded");
    String::from_utf8(output.stderr).unwrap()
}
