//! Supported SQLite DDL commands and their SQL rendering.
//!
//! Scratch validation uses canonical `main`-qualified targets. Migration
//! artifacts use unqualified targets when rendered canonically and retain
//! matching authored CREATE statements when available. This module neither
//! validates the plan nor executes SQL.

use super::schema::{Column, Index, SortDirection, SqliteObject as Definition, Table, View};

/// A supported SQLite schema operation. Drops retain the definitions needed
/// to invert them, but are not inferred as forward operations.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ddl {
    CreateTable(Table),
    AddColumn { table: String, column: Column },
    CreateIndex(Index),
    CreateView(View),
    DropTable(Table),
    DropColumn { table: String, column: Column },
    DropIndex(Index),
    DropView(View),
}

impl Ddl {
    pub(super) fn inverse(&self) -> Self {
        match self {
            Self::CreateTable(table) => Self::DropTable(table.clone()),
            Self::AddColumn { table, column } => Self::DropColumn {
                table: table.clone(),
                column: column.clone(),
            },
            Self::CreateIndex(index) => Self::DropIndex(index.clone()),
            Self::CreateView(view) => Self::DropView(view.clone()),
            Self::DropTable(table) => Self::CreateTable(table.clone()),
            Self::DropColumn { table, column } => Self::AddColumn {
                table: table.clone(),
                column: column.clone(),
            },
            Self::DropIndex(index) => Self::CreateIndex(index.clone()),
            Self::DropView(view) => Self::CreateView(view.clone()),
        }
    }
}

// ----------------------------------------------------------------------------
// Canonical rendering
// ----------------------------------------------------------------------------

/// Renders DDL with a `main`-qualified target.
pub(super) fn to_sql(operation: &Ddl) -> String {
    to_sql_with_qualification(operation, true)
}

/// Render one operation from an already-validated plan. In particular, the
/// caller must restrict ADD COLUMN to the supported nullable suffix columns.
fn to_sql_with_qualification(operation: &Ddl, qualify: bool) -> String {
    let schema = if qualify { "\"main\"." } else { "" };
    match operation {
        Ddl::CreateTable(table) => {
            let columns = table
                .columns
                .iter()
                .map(|column| {
                    let mut definition =
                        column_definition(&column.name, column.declared_type.as_deref());
                    if column.not_null {
                        definition.push_str(" NOT NULL");
                    }
                    if let Some(check) = &column.check {
                        definition.push_str(" CHECK (");
                        definition.push_str(check);
                        definition.push(')');
                    }
                    if column.default_null {
                        definition.push_str(" DEFAULT NULL");
                    }
                    if let Some(default) = &column.default_value {
                        definition.push_str(" DEFAULT ");
                        definition.push_str(default);
                    }
                    if column.primary_key {
                        definition.push_str(" PRIMARY KEY");
                    }
                    definition
                })
                .collect::<Vec<_>>()
                .join(", ");
            let check = table
                .check
                .as_ref()
                .map(|check| format!(", CHECK ({check})"))
                .unwrap_or_default();
            format!(
                "CREATE TABLE {schema}{} ({columns}{check});",
                quote_identifier(&table.name)
            )
        }
        Ddl::AddColumn { table, column } => {
            let mut definition = column_definition(&column.name, column.declared_type.as_deref());
            if column.default_null {
                definition.push_str(" DEFAULT NULL");
            }
            if let Some(default) = &column.default_value {
                definition.push_str(" DEFAULT ");
                definition.push_str(default);
            }
            format!(
                "ALTER TABLE {schema}{} ADD COLUMN {definition};",
                quote_identifier(table)
            )
        }
        Ddl::CreateIndex(index) => format!(
            "CREATE {}INDEX {schema}{} ON {} ({});",
            if index.unique { "UNIQUE " } else { "" },
            quote_identifier(&index.name),
            quote_identifier(&index.table),
            index
                .columns
                .iter()
                .map(|column| {
                    let mut key = quote_identifier(&column.name);
                    if let Some(collation) = &column.collation {
                        key.push_str(" COLLATE ");
                        key.push_str(collation);
                    }
                    match column.order {
                        Some(SortDirection::Asc) => key.push_str(" ASC"),
                        Some(SortDirection::Desc) => key.push_str(" DESC"),
                        None => {}
                    }
                    key
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ddl::CreateView(view) => format!(
            "CREATE VIEW {schema}{} AS {};",
            quote_identifier(&view.name),
            view.select.trim_end_matches(';')
        ),
        Ddl::DropTable(table) => {
            format!("DROP TABLE {schema}{};", quote_identifier(&table.name))
        }
        Ddl::DropColumn { table, column } => format!(
            "ALTER TABLE {schema}{} DROP COLUMN {};",
            quote_identifier(table),
            quote_identifier(&column.name)
        ),
        Ddl::DropIndex(index) => {
            format!("DROP INDEX {schema}{};", quote_identifier(&index.name))
        }
        Ddl::DropView(view) => {
            format!("DROP VIEW {schema}{};", quote_identifier(&view.name))
        }
    }
}

/// Artifact SQL uses unqualified targets for compatibility with authored migrations.
pub(super) fn to_migration_sql(operation: &Ddl) -> String {
    to_sql_with_qualification(operation, false)
}

// ----------------------------------------------------------------------------
// Authored CREATE statements
// ----------------------------------------------------------------------------

/// Preserve authored SQL when the CREATE kind and name match. `sources` must
/// come from the same desired schema that produced the operation. Other
/// operations fall back to canonical migration SQL.
pub(super) fn to_migration_sql_with_source(
    operation: &Ddl,
    sources: &[(&str, Definition)],
) -> String {
    let source = sources.iter().find_map(|(source, definition)| {
        let name = match (operation, definition) {
            (Ddl::CreateTable(table), Definition::Table(_)) => &table.name,
            (Ddl::CreateIndex(index), Definition::Index(_)) => &index.name,
            (Ddl::CreateView(view), Definition::View(_)) => &view.name,
            _ => return None,
        };
        name.eq_ignore_ascii_case(definition.name())
            .then_some(*source)
    });
    source.map_or_else(
        || to_migration_sql(operation),
        |source| {
            if source.trim_end().ends_with(';') {
                source.to_owned()
            } else {
                // Keep a trailing line comment from swallowing the terminator.
                format!("{source}\n;")
            }
        },
    )
}

// ----------------------------------------------------------------------------
// Identifier and column fragments
// ----------------------------------------------------------------------------

/// Quote an identifier, escaping embedded double quotes for SQLite.
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn column_definition(name: &str, declared_type: Option<&str>) -> String {
    let mut definition = quote_identifier(name);
    if let Some(declared_type) = declared_type {
        definition.push(' ');
        definition.push_str(declared_type);
    }
    definition
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{Column, Ddl, Index, SortDirection, Table, to_migration_sql, to_sql};
    use crate::sqlite::schema::{IndexColumn, lower_index};

    #[test]
    fn renders_index_column_sort_order() {
        let operation = Ddl::CreateIndex(Index {
            name: "items_by_id".into(),
            table: "items".into(),
            unique: false,
            columns: vec![IndexColumn {
                name: "id".into(),
                order: Some(SortDirection::Desc),
                collation: None,
            }],
        });
        let sql = to_sql(&operation);

        assert_eq!(
            sql,
            "CREATE INDEX \"main\".\"items_by_id\" ON \"items\" (\"id\" DESC);"
        );
    }

    #[test]
    fn renders_inverse_with_quoted_sqlite_identifiers() {
        let table = Table {
            name: "item\"s".into(),
            check: None,
            columns: vec![],
        };
        let operation = Ddl::CreateTable(table);

        let inverse = operation.inverse();
        assert_eq!(to_sql(&inverse), "DROP TABLE \"main\".\"item\"\"s\";");
        assert_eq!(to_migration_sql(&inverse), "DROP TABLE \"item\"\"s\";");
        assert_eq!(inverse.inverse(), operation);
    }

    #[test]
    fn renders_and_executes_create_table_preserving_modeled_fields() {
        let operation = Ddl::CreateTable(Table {
            name: "item\"s".into(),
            check: Some("\"kind\"\"value\" <> ''".into()),
            columns: vec![
                Column {
                    name: "kind\"value".into(),
                    declared_type: Some("TEXT".into()),
                    not_null: true,
                    check: Some("length(\"kind\"\"value\") > 0".into()),
                    default_null: false,
                    default_value: None,
                    primary_key: false,
                },
                Column {
                    name: "optional".into(),
                    declared_type: None,
                    not_null: false,
                    check: None,
                    default_null: true,
                    default_value: None,
                    primary_key: false,
                },
            ],
        });

        let sql = to_sql(&operation);
        assert_eq!(
            sql,
            "CREATE TABLE \"main\".\"item\"\"s\" (\"kind\"\"value\" TEXT NOT NULL CHECK (length(\"kind\"\"value\") > 0), \"optional\" DEFAULT NULL, CHECK (\"kind\"\"value\" <> ''));"
        );

        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(&sql).unwrap();
        let columns = connection
            .prepare("PRAGMA main.table_info(\"item\"\"s\")")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            columns,
            vec![
                ("kind\"value".into(), Some("TEXT".into()), 1, None),
                ("optional".into(), Some("".into()), 0, Some("NULL".into())),
            ]
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO \"item\"\"s\" (\"kind\"\"value\") VALUES ('')",
                    [],
                )
                .is_err()
        );
    }

    #[test]
    fn renders_and_executes_add_column_with_quoted_identifiers() {
        let operation = Ddl::AddColumn {
            table: "item\"s".into(),
            column: Column {
                name: "note\"text".into(),
                declared_type: Some("TEXT".into()),
                not_null: false,
                check: None,
                default_null: false,
                default_value: None,
                primary_key: false,
            },
        };
        let sql = to_sql(&operation);
        assert_eq!(
            sql,
            "ALTER TABLE \"main\".\"item\"\"s\" ADD COLUMN \"note\"\"text\" TEXT;"
        );

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE \"item\"\"s\" (id INTEGER)")
            .unwrap();
        connection.execute_batch(&sql).unwrap();
        let name: String = connection
            .query_row(
                "SELECT name FROM pragma_table_info('item\"s') WHERE cid = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "note\"text");
    }

    #[test]
    fn renders_and_executes_add_column_with_explicit_null_default() {
        let operation = Ddl::AddColumn {
            table: "items".into(),
            column: Column {
                name: "note".into(),
                declared_type: Some("TEXT".into()),
                not_null: false,
                check: None,
                default_null: true,
                default_value: None,
                primary_key: false,
            },
        };
        let sql = to_sql(&operation);
        assert_eq!(
            sql,
            "ALTER TABLE \"main\".\"items\" ADD COLUMN \"note\" TEXT DEFAULT NULL;"
        );

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE items (id INTEGER)")
            .unwrap();
        connection.execute_batch(&sql).unwrap();
        let default: Option<String> = connection
            .query_row(
                "SELECT dflt_value FROM pragma_table_info('items') WHERE name = 'note'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(default.as_deref(), Some("NULL"));
    }

    #[test]
    fn renders_and_executes_create_index_with_quoted_identifiers() {
        let operation = Ddl::CreateIndex(Index {
            name: "items\"kind".into(),
            table: "item\"s".into(),
            unique: false,
            columns: vec![IndexColumn {
                name: "kind\"value".into(),
                order: None,
                collation: None,
            }],
        });
        let sql = to_sql(&operation);
        assert_eq!(
            sql,
            "CREATE INDEX \"main\".\"items\"\"kind\" ON \"item\"\"s\" (\"kind\"\"value\");"
        );
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE \"item\"\"s\" (\"kind\"\"value\" TEXT)")
            .unwrap();
        connection.execute_batch(&sql).unwrap();
        let name: String = connection
            .query_row(
                "SELECT name FROM sqlite_schema WHERE type='index'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "items\"kind");
    }

    #[test]
    fn renders_unique_index_with_key_collation() {
        let index =
            lower_index("CREATE UNIQUE INDEX by_name ON items (name COLLATE NOCASE)").unwrap();
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE items (name TEXT); INSERT INTO items VALUES ('a');")
            .unwrap();
        connection
            .execute_batch(&to_sql(&Ddl::CreateIndex(index)))
            .unwrap();
        assert!(
            connection
                .execute("INSERT INTO items VALUES ('A')", [])
                .is_err()
        );
    }

    #[test]
    fn renders_multi_column_index_keys_in_declared_order() {
        let operation = Ddl::CreateIndex(Index {
            name: "items_kind_label".into(),
            table: "items".into(),
            unique: false,
            columns: ["kind", "label"]
                .into_iter()
                .map(|name| IndexColumn {
                    name: name.into(),
                    order: None,
                    collation: None,
                })
                .collect(),
        });
        assert_eq!(
            to_sql(&operation),
            "CREATE INDEX \"main\".\"items_kind_label\" ON \"items\" (\"kind\", \"label\");"
        );
    }
}
