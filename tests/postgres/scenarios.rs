//! Fixture-based PostgreSQL tests. See `scenarios/README.md` for setup.

use std::{fs, path::Path};

use dir_test::{Fixture, dir_test};
use fledgling::{Dialect::Postgres, generate, migrate, preview, pull, rollback, status};
use postgres::Client;

use super::{Database, Project};

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/postgres/scenarios",
    glob: "*/expected.up.sql",
)]
fn plan_offline(fixture: Fixture<&str>) {
    let project = Project::new();
    let case = case_dir(&fixture);
    let baseline = seed(&project, case);
    assert_plan(&project, case, fixture.content(), next_id(&baseline));
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/postgres/scenarios",
    glob: "*/error.txt",
)]
fn reject_offline(fixture: Fixture<&str>) {
    let project = Project::new();
    seed(&project, case_dir(&fixture));
    let count = fs::read_dir(&project.migrations).unwrap().count();
    for result in [
        preview(Postgres, &project.migrations, &project.schema).unwrap_err(),
        generate(Postgres, &project.migrations, &project.schema).unwrap_err(),
    ] {
        assert_error(&result.to_string(), fixture.content());
        assert_eq!(fs::read_dir(&project.migrations).unwrap().count(), count);
    }
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/postgres/scenarios",
    glob: "*/expected.up.sql",
)]
#[dir_test_attr(#[ignore = "requires FLEDGLING_TEST_POSTGRES_URL"])]
fn round_trip_live(fixture: Fixture<&str>) {
    let project = Project::new();
    let case = case_dir(&fixture);
    let baseline = seed(&project, case);
    let db = Database::new();
    let path = db.path();
    let mut live = db.connect();
    let history = &project.migrations;
    assert!(status(Postgres, path).unwrap().is_empty());
    assert!(pull(Postgres, path).unwrap().is_empty());
    assert!(
        live.query_one(
            "SELECT to_regclass('public._fledgling_migrations') IS NULL",
            &[],
        )
        .unwrap()
        .get::<_, bool>(0)
    );

    let baseline_catalog = reference_catalog(&baseline);
    let target_catalog = reference_catalog(&read(&project.schema));
    let baseline_ids = if baseline.trim().is_empty() {
        vec![]
    } else {
        vec![1]
    };
    assert_eq!(migrate(Postgres, path, history).unwrap(), baseline_ids);
    let assert_state = |live: &mut Client, phase, catalog: &[String], ids: &[u64]| {
        assert_eq!(server_catalog(live), catalog, "{phase}: catalog");
        assert_eq!(status(Postgres, path).unwrap(), ids, "{phase}: ledger");
    };
    assert_state(&mut live, "baseline", &baseline_catalog, &baseline_ids);

    let id = next_id(&baseline);
    let generated = assert_plan(&project, case, fixture.content(), id);
    assert_state(
        &mut live,
        "after planning",
        &baseline_catalog,
        &baseline_ids,
    );
    let mut target_ids = baseline_ids.clone();
    if generated {
        target_ids.push(id);
        assert_eq!(migrate(Postgres, path, history).unwrap(), [id]);
    }
    assert!(migrate(Postgres, path, history).unwrap().is_empty());
    assert_state(&mut live, "target", &target_catalog, &target_ids);

    let pulled = pull(Postgres, path).unwrap();
    assert!(!pulled.contains("_fledgling_migrations"));
    assert_eq!(reference_catalog(&pulled), target_catalog, "pulled schema");
    if generated {
        assert_eq!(rollback(Postgres, path, history).unwrap(), Some(id));
        assert_state(&mut live, "rollback", &baseline_catalog, &baseline_ids);
        assert_eq!(migrate(Postgres, path, history).unwrap(), [id]);
        assert_state(&mut live, "reapply", &target_catalog, &target_ids);
    }
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/postgres/scenarios",
    glob: "_runner/*/error.txt",
)]
#[dir_test_attr(#[ignore = "requires FLEDGLING_TEST_POSTGRES_URL"])]
fn reject_runner_live(fixture: Fixture<&str>) {
    let case = case_dir(&fixture);
    let project = Project::new();
    let db = Database::new();
    let path = db.path();
    let history = &project.migrations;
    fs::copy(
        case.join("current.schema.sql"),
        history.join("000001.up.sql"),
    )
    .unwrap();
    assert_eq!(migrate(Postgres, path, history).unwrap(), [1]);
    let mut live = db.connect();
    let before = server_catalog(&mut live);
    let up = case.join("invalid.up.sql");
    let down = case.join("invalid.down.sql");
    let error = match (up.is_file(), down.is_file()) {
        (true, false) => {
            fs::copy(up, history.join("000002.up.sql")).unwrap();
            migrate(Postgres, path, history).unwrap_err()
        }
        (false, true) => {
            fs::copy(down, history.join("000001.down.sql")).unwrap();
            rollback(Postgres, path, history).unwrap_err()
        }
        _ => panic!("expected exactly one invalid migration fixture"),
    };
    assert_error(&error.to_string(), fixture.content());
    assert_eq!(status(Postgres, path).unwrap(), [1]);
    assert_eq!(server_catalog(&mut live), before);
}

#[dir_test(
    dir: "$CARGO_MANIFEST_DIR/tests/postgres/scenarios",
    glob: "_pull/*/current.schema.sql",
)]
#[dir_test_attr(#[ignore = "requires FLEDGLING_TEST_POSTGRES_URL"])]
fn pull_live(fixture: Fixture<&str>) {
    let case = case_dir(&fixture);
    let db = Database::new();
    let mut live = db.connect();
    live.batch_execute(fixture.content()).unwrap();
    let error = case.join("error.txt");
    let result = pull(Postgres, db.path());
    if error.exists() {
        assert_error(&result.unwrap_err().to_string(), &read(&error));
        return;
    }
    let restored = Database::new();
    let mut reference = restored.connect();
    reference.batch_execute(&result.unwrap()).unwrap();
    assert_eq!(server_catalog(&mut live), server_catalog(&mut reference));
    let assertions = case.join("assert.sql");
    if assertions.exists() {
        let sql = read(&assertions);
        for database in [&mut live, &mut reference] {
            assert!(
                database.query_one(&sql, &[]).unwrap().get::<_, bool>(0),
                "{sql}"
            );
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn case_dir<'a>(fixture: &'a Fixture<&str>) -> &'a Path {
    Path::new(fixture.path()).parent().unwrap()
}

fn seed(project: &Project, case: &Path) -> String {
    fs::copy(case.join("target.schema.sql"), &project.schema).unwrap();
    let current = case.join("current.schema.sql");
    if current.exists() {
        let baseline = read(&current);
        if !baseline.trim().is_empty() {
            fs::write(project.migrations.join("000001.up.sql"), &baseline).unwrap();
        }
        baseline
    } else {
        String::new()
    }
}

fn next_id(baseline: &str) -> u64 {
    if baseline.trim().is_empty() { 1 } else { 2 }
}

fn assert_plan(project: &Project, case: &Path, expected_up: &str, id: u64) -> bool {
    let migrations = &project.migrations;
    let count = fs::read_dir(migrations).unwrap().count();
    let planned = preview(Postgres, migrations, &project.schema).unwrap();
    assert_eq!(fs::read_dir(migrations).unwrap().count(), count);
    let generated = generate(Postgres, migrations, &project.schema).unwrap();
    let expected_down = read(&case.join("expected.down.sql"));
    if expected_up.trim().is_empty() {
        assert!(expected_down.trim().is_empty());
        assert_eq!(planned, None);
        assert_eq!(generated, None);
        assert_eq!(fs::read_dir(migrations).unwrap().count(), count);
        return false;
    }
    let up_path = generated.unwrap();
    assert_eq!(up_path, migrations.join(format!("{id:06}.up.sql")));
    let up = read(&up_path);
    let down = read(&migrations.join(format!("{id:06}.down.sql")));
    assert_eq!(up.trim(), expected_up.trim());
    assert_eq!(down.trim(), expected_down.trim());
    assert_eq!(planned, Some((up, down)));
    assert_eq!(
        preview(Postgres, migrations, &project.schema).unwrap(),
        None
    );
    assert_eq!(fs::read_dir(migrations).unwrap().count(), count + 2);
    true
}

fn assert_error(actual: &str, expected: &str) {
    // Multiple lines can independently check operation context and the cause.
    for line in expected
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        assert!(actual.contains(line), "expected {line:?}; got {actual}");
    }
}

fn reference_catalog(sql: &str) -> Vec<String> {
    let db = Database::new();
    let mut client = db.connect();
    if !sql.trim().is_empty() {
        client.batch_execute(sql).unwrap();
    }
    server_catalog(&mut client)
}

// Query PostgreSQL directly so planning and pull cannot mask their own mistakes.
fn server_catalog(client: &mut Client) -> Vec<String> {
    client
        .query(
            "WITH relations AS (
             SELECT c.oid, c.relname, c.relkind FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'public' AND c.relkind IN ('r', 'v')
               AND c.relname <> '_fledgling_migrations'
         )
         SELECT definition FROM (
             SELECT r.relname, 0 AS kind, 0 AS position,
                    concat_ws(' ', r.relkind, r.relname,
                        CASE WHEN r.relkind = 'v' THEN pg_get_viewdef(r.oid) END) AS definition
             FROM relations r
             UNION ALL
             SELECT r.relname, 1, a.attnum, concat_ws(' ', a.attname,
                        format_type(a.atttypid, a.atttypmod), a.attnotnull,
                        pg_get_expr(d.adbin, d.adrelid))
             FROM relations r JOIN pg_attribute a ON a.attrelid = r.oid
             LEFT JOIN pg_attrdef d ON d.adrelid = r.oid AND d.adnum = a.attnum
             WHERE a.attnum > 0 AND NOT a.attisdropped
             UNION ALL
             SELECT r.relname, 2, 0, pg_get_constraintdef(c.oid)
             FROM relations r JOIN pg_constraint c ON c.conrelid = r.oid
             UNION ALL
             SELECT r.relname, 3, 0, pg_get_indexdef(i.indexrelid)
             FROM relations r JOIN pg_index i ON i.indrelid = r.oid
         ) objects ORDER BY relname, kind, position, definition",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}
