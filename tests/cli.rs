use std::{
    fs,
    path::PathBuf,
    process::{self, Command, Output},
};

fn fledgling() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fledgling"))
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn exposes_commands_and_global_options() {
    let help = stdout(fledgling().arg("--help").output().unwrap());
    for command in [
        "init", "pull", "preview", "migrate", "status", "rollback", "generate",
    ] {
        assert!(
            help.lines()
                .any(|line| line.split_whitespace().next() == Some(command)),
            "missing command: {command}"
        );
    }
    assert!(help.contains("--config <CONFIG>"));
    assert!(help.contains("[default: fledgling.toml]"));
    assert_eq!(
        stdout(fledgling().arg("--version").output().unwrap()),
        concat!("fledgling ", env!("CARGO_PKG_VERSION"), "\n")
    );
}

#[test]
fn rejects_conflicting_path_arguments() {
    let output = fledgling()
        .args(["pull", "positional.db", "--database", "flag.db"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--database <DATABASE>"));
    assert!(stderr.contains("cannot be used with"));
}

#[test]
fn runs_a_migration_from_a_config_relative_to_another_directory() {
    struct Workspace(PathBuf);
    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    let root = std::env::temp_dir().join(format!("fledgling-cli-{}", process::id()));
    fs::create_dir(&root).unwrap();
    let workspace = Workspace(root);
    let project = workspace.0.join("project");
    let migrations = project.join("migrations");
    fs::create_dir_all(&migrations).unwrap();
    fs::write(
        project.join("custom.toml"),
        r#"[fledgling]
dialect = "sqlite"
database_url = "sqlite://project.db"
migrations = "migrations"
schema = "schema.sql"
"#,
    )
    .unwrap();
    fs::write(
        project.join("schema.sql"),
        "CREATE TABLE books (id INTEGER);\n",
    )
    .unwrap();

    let run = |args: &[&str]| {
        stdout(
            fledgling()
                .current_dir(&workspace.0)
                .args(args)
                .args(["--config", "project/custom.toml"])
                .output()
                .unwrap(),
        )
    };
    let preview = run(&["preview"]);
    assert!(!project.join("project.db").exists());
    assert!(fs::read_dir(&migrations).unwrap().next().is_none());

    assert!(run(&["generate"]).ends_with("000001.up.sql\n"));
    let up = fs::read_to_string(migrations.join("000001.up.sql")).unwrap();
    let down = fs::read_to_string(migrations.join("000001.down.sql")).unwrap();
    assert_eq!(preview, format!("-- Up\n{up}\n\n-- Down\n{down}\n"));
    assert!(!project.join("project.db").exists());

    assert_eq!(
        run(&["migrate", "override.db", "project/migrations"]),
        "Applied 000001\n"
    );
    assert!(workspace.0.join("override.db").is_file());
    assert!(!project.join("project.db").exists());
    assert_eq!(run(&["status", "--database", "override.db"]), "000001\n");
    assert_eq!(run(&["migrate"]), "Applied 000001\n");
    assert_eq!(run(&["status"]), "000001\n");
    assert!(run(&["pull"]).contains("CREATE TABLE books"));
    assert_eq!(run(&["preview"]), "No schema changes.\n");
    assert_eq!(run(&["rollback"]), "Rolled back 000001\n");
    assert_eq!(run(&["status"]), "");
    assert_eq!(run(&["pull"]), "");
}
