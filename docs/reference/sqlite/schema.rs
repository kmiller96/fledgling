//! SQLite catalog inspection and lowering of SQL into the typed schema.
//!
//! Raw schema export and planning serve different purposes: export reads
//! SQLite's stored definitions, while planning accepts only a modeled subset
//! of `main` DDL. The desired schema is parsed as declarations, not executed
//! as a script. Generated Up and Down are verified on the replay database.

mod model;

use super::ddl::{self, Ddl};
use crate::MigrationSql;
pub use model::PreviewError;
pub(super) use model::{Column, Index, IndexColumn, SortDirection, SqliteObject, Table, View};
use model::{Schema as SqliteSchema, plan_schema};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sqlite3_parser::{
    Bump, FallibleIterator,
    ast::{
        Cmd, ColFlags, ColumnConstraint, ColumnDefinition, CreateTableBody, Expr, Literal,
        OneSelect, ResultColumn, SelectTable, Stmt, TableConstraint,
    },
    lexer::sql::{Error, Parser},
};
use std::{collections::HashSet, error::Error as StdError, path::Path};

// ----------------------------------------------------------------------------
// Plan rendering and verification
// ----------------------------------------------------------------------------

/// Build and verify a plan against an already-replayed baseline. `connection`
/// must be disposable: verification runs Up and then Down on it and can leave
/// changes behind on error.
pub(super) fn plan(
    connection: &Connection,
    desired_schema_sql: &str,
) -> Result<Option<MigrationSql>, Box<dyn StdError>> {
    let baseline = read_schema(connection)?;
    let (desired, sources) = lower_schema_with_sources(desired_schema_sql)
        .map_err(|error| PreviewError::InvalidDesiredSql(error.to_string()))?;
    model::blocked_changes(&baseline, &desired)?;
    let commands = plan_schema(&baseline, &desired).map_err(PreviewError::BlockedChange)?;
    let operations = commands
        .iter()
        .map(|command| Operation {
            up: ddl::to_migration_sql_with_source(command, &sources),
            down: ddl::to_migration_sql(&command.inverse()),
        })
        .collect::<Vec<_>>();
    let migration = if operations.is_empty() {
        None
    } else {
        let up = operations
            .iter()
            .map(|operation| operation.up.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let down = operations
            .iter()
            .rev()
            .map(|operation| operation.down.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Some(MigrationSql { up, down })
    };
    if let Some(migration) = &migration {
        verify(connection, "Up", &migration.up, &desired)?;
        verify(connection, "Down", &migration.down, &baseline)?;
    }
    Ok(migration)
}

/// A paired SQLite DDL command; Down comes from each selected DDL inverse.
struct Operation {
    up: String,
    down: String,
}

/// Run one direction and require an empty diff to the expected schema.
/// This verifies modeled schema equivalence, not data preservation.
fn verify(
    connection: &Connection,
    direction: &str,
    sql: &str,
    expected: &SqliteSchema,
) -> Result<(), Box<dyn StdError>> {
    let failure = |reason: &str| format!("generated {direction} SQL failed verification: {reason}");
    super::runtime::validate_migration_sql(sql).map_err(|error| failure(&error))?;
    connection
        .execute_batch(sql)
        .map_err(|error| failure(&error.to_string()))?;
    let actual = read_schema(connection).map_err(|error| failure(&error.to_string()))?;
    let remaining = plan_schema(&actual, expected).map_err(|reason| failure(&reason))?;
    if !remaining.is_empty() {
        return Err(failure("resulting schema does not match").into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// SQLite comparison and dependency policy
// ----------------------------------------------------------------------------

fn validate_schema(schema: &SqliteSchema) -> Result<(), String> {
    for view in schema.views() {
        if is_reserved_object_name(&view.name) {
            return Err(format!(
                "view {} uses the reserved sqlite_ prefix",
                view.name
            ));
        }
    }
    for index in schema.indexes() {
        validate_index(index, schema)
            .map_err(|reason| format!("index {}: {reason}", index.name))?;
    }
    Ok(())
}

/// Build scratch tables and indexes, then let SQLite resolve the views against
/// them. Unchanged views seed dependencies before new views are considered.
fn validate_and_order_views(
    schema: &SqliteSchema,
    existing: &[&View],
    additions: &[&View],
) -> Result<Vec<View>, String> {
    let connection = Connection::open_in_memory()
        .map_err(|error| format!("could not open a scratch database to validate views: {error}"))?;
    for table in schema.tables() {
        connection
            .execute_batch(&ddl::to_sql(&Ddl::CreateTable(table.clone())))
            .map_err(|error| {
                format!(
                    "could not validate views against table {}: {error}",
                    table.name
                )
            })?;
    }
    for index in schema.indexes() {
        connection
            .execute_batch(&ddl::to_sql(&Ddl::CreateIndex(index.clone())))
            .map_err(|error| {
                format!(
                    "could not validate views against index {}: {error}",
                    index.name
                )
            })?;
    }
    model::order_views(existing, additions, |view| {
        validate_one_view(&connection, view)
    })
    .map(|views| views.into_iter().cloned().collect())
}

/// SQLite defers resolving a view's query at creation. Prepare a query against
/// it to check readiness; keep successful views and drop failed candidates
/// before retrying them in a later dependency pass.
fn validate_one_view(connection: &Connection, view: &View) -> Result<(), String> {
    let failure = |error| format!("could not resolve view {}: {error}", view.name);
    let sql = ddl::to_sql(&Ddl::CreateView(view.clone()));
    connection.execute_batch(&sql).map_err(failure)?;
    let escaped = view.name.replace('"', "\"\"");
    let result = connection
        .prepare(&format!("SELECT * FROM \"{escaped}\" LIMIT 0"))
        .map(|_| ())
        .map_err(failure);
    if result.is_err() {
        let _ = connection.execute_batch(&ddl::to_sql(&Ddl::DropView(view.clone())));
    }
    result
}

/// Accept non-unique indexes over existing modeled columns, without collations.
fn validate_index(index: &Index, schema: &SqliteSchema) -> Result<(), String> {
    if is_reserved_object_name(&index.name) {
        return Err("the sqlite_ prefix is reserved".into());
    }
    if index.unique {
        return Err("unique indexes are not supported".into());
    }
    if index.columns.is_empty() {
        return Err("at least one indexed column is required".into());
    }
    let table = schema
        .tables()
        .find(|table| table.name.eq_ignore_ascii_case(&index.table))
        .ok_or_else(|| format!("table {} does not exist", index.table))?;
    for key in &index.columns {
        if key.collation.is_some() {
            return Err(format!("collation on column {} is not supported", key.name));
        }
        if !table
            .columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(&key.name))
        {
            return Err(format!("column {}.{} does not exist", table.name, key.name));
        }
    }
    Ok(())
}

/// Check SQLite's case-insensitive `sqlite_` internal-object prefix.
pub(super) fn is_reserved_object_name(name: &str) -> bool {
    name.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlite_"))
}

/// Compare names and parser-normalized queries, not semantic SQL equivalence.
fn view_match(actual: &View, desired: &View) -> bool {
    actual.name.eq_ignore_ascii_case(&desired.name)
        && normalize_view_select(&actual.select) == normalize_view_select(&desired.select)
}

// ----------------------------------------------------------------------------
// Main catalog export and snapshots
// ----------------------------------------------------------------------------

pub(crate) fn open_read_only(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
}

/// Read stored DDL in table, view, then other-object order, sorted by name
/// within each group. Omit internal names, the ledger, and rows without SQL.
fn catalog_rows(connection: &Connection) -> rusqlite::Result<Vec<(String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql FROM main.sqlite_schema \
         WHERE sql IS NOT NULL AND name NOT GLOB 'sqlite_*' \
         AND name != '_fledgling_migrations' \
         ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'view' THEN 1 ELSE 2 END, name",
    )?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

/// Export stored DDL from `main`, excluding SQLite internals and the migration
/// ledger. Append semicolons and separate statements with newlines.
///
/// This is not a data dump or a validated planning input; definitions outside
/// the additive planner's subset may appear in the result.
pub fn pull(connection: &Connection) -> rusqlite::Result<String> {
    Ok(catalog_rows(connection)?
        .into_iter()
        .map(|(_, _, definition)| format!("{definition};"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Reject main-schema features outside the modeled subset: persistent
/// triggers, virtual tables, and declared foreign keys even if disabled.
fn validate_catalog(connection: &Connection) -> rusqlite::Result<()> {
    let trigger = connection
        .query_row(
            "SELECT name FROM main.sqlite_schema \
             WHERE type = 'trigger' ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(name) = trigger {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "unsupported trigger in main: {name}"
        )));
    }

    let virtual_table = connection
        .query_row(
            "SELECT name FROM pragma_table_list \
             WHERE schema = 'main' AND type = 'virtual' AND name NOT GLOB 'sqlite_*' \
             ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(name) = virtual_table {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "unsupported virtual table in main: {name}"
        )));
    }

    let foreign_key_table = connection
        .query_row(
            "SELECT t.name FROM pragma_table_list AS t \
             WHERE t.schema = 'main' AND t.type = 'table' AND t.name NOT GLOB 'sqlite_*' \
             AND EXISTS (SELECT 1 FROM pragma_foreign_key_list(t.name, 'main')) \
             ORDER BY t.name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(name) = foreign_key_table {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "unsupported foreign key in main table: {name}"
        )));
    }

    Ok(())
}

/// Validate and lower the `main` catalog from a single read transaction.
/// Ignore TEMP and attached schemas. The connection must not already be
/// inside a transaction.
pub(crate) fn read_schema(connection: &Connection) -> rusqlite::Result<SqliteSchema> {
    let transaction = connection.unchecked_transaction()?;
    validate_catalog(&transaction)?;
    let definitions = catalog_rows(&transaction)?
        .into_iter()
        .filter_map(|(kind, name, sql)| match kind.as_str() {
            "table" => Some(lower_table(&sql).map(SqliteObject::Table).map_err(|error| {
                rusqlite::Error::InvalidParameterName(format!(
                    "unsupported table declaration for {name}: {error:?}"
                ))
            })),
            "view" => Some(lower_view(&sql).map(SqliteObject::View).map_err(|error| {
                rusqlite::Error::InvalidParameterName(format!(
                    "unsupported view declaration for {name}: {error:?}"
                ))
            })),
            "index" => Some(lower_index(&sql).map(SqliteObject::Index).map_err(|error| {
                rusqlite::Error::InvalidParameterName(format!(
                    "unsupported index declaration for {name}: {error:?}"
                ))
            })),
            _ => None,
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let schema = SqliteSchema::try_new(definitions)
        .map_err(|error| rusqlite::Error::InvalidParameterName(format!("main schema: {error}")))?;
    transaction.commit()?;
    Ok(schema)
}

// ----------------------------------------------------------------------------
// Declaration parsing and statement diagnostics
// ----------------------------------------------------------------------------

fn parse_sql<'a>(arena: &'a Bump, sql: &'a str) -> Result<Vec<Cmd<'a>>, Error> {
    Parser::new(arena, sql.as_bytes()).collect()
}

/// Skip separators and leading comments within a parsed statement's span
/// before retaining authored SQL.
fn skip_statement_prefix(sql: &str, mut start: usize, end: usize) -> usize {
    let bytes = sql.as_bytes();
    while start < end {
        if bytes[start].is_ascii_whitespace() || bytes[start] == b';' {
            start += 1;
        } else if bytes.get(start..start + 2) == Some(b"--") {
            start = bytes[start..end]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(end, |offset| start + offset + 1);
        } else if bytes.get(start..start + 2) == Some(b"/*") {
            start = bytes[start + 2..end]
                .windows(2)
                .position(|window| window == b"*/")
                .map_or(end, |offset| start + offset + 4);
        } else {
            break;
        }
    }
    start
}

/// A parser failure or rejection of a parsed statement in desired SQL.
#[derive(Debug)]
pub(super) enum DesiredSchemaError {
    Parse(Error),
    Unsupported {
        statement_number: usize,
        kind: UnsupportedKind,
    },
}

impl std::fmt::Display for DesiredSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "{error}"),
            Self::Unsupported {
                statement_number,
                kind,
            } => {
                let kind = match kind {
                    UnsupportedKind::Declaration => "declaration",
                    UnsupportedKind::Executable => "executable statement",
                };
                write!(f, "unsupported {kind} at statement {statement_number}")
            }
        }
    }
}

impl std::error::Error for DesiredSchemaError {}

/// Diagnostic classification: unsupported DDL versus a command that is not
/// a declaration of the desired schema.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum UnsupportedKind {
    Declaration,
    Executable,
}

/// Checks desired-schema statement kinds and supported declaration shapes.
#[cfg(test)]
pub(crate) fn check_desired_sql(sql: &str) -> Result<(), DesiredSchemaError> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, sql).map_err(DesiredSchemaError::Parse)?;
    check_desired_commands(&commands)
}

/// Require supported `main` CREATE declarations, not executable commands.
/// Check common scope, name, and shape restrictions; individual lowerers
/// enforce the rest of the model. Diagnostics use one-based statement numbers.
fn check_desired_commands(commands: &[Cmd<'_>]) -> Result<(), DesiredSchemaError> {
    let mut names = HashSet::new();

    for (index, command) in commands.iter().enumerate() {
        let statement = match command {
            Cmd::Stmt(statement) => statement,
            Cmd::Explain(_) | Cmd::ExplainQueryPlan(_) => {
                return Err(DesiredSchemaError::Unsupported {
                    statement_number: index + 1,
                    kind: UnsupportedKind::Executable,
                });
            }
        };

        if let Stmt::CreateTable {
            temporary: true, ..
        } = statement
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateTable { tbl_name, .. } = statement
            && tbl_name
                .db_name
                .as_ref()
                .is_some_and(|database| database != "main")
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateView {
            temporary: true, ..
        } = statement
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateView { view_name, .. } = statement
            && view_name
                .db_name
                .as_ref()
                .is_some_and(|database| database != "main")
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateTable {
            body: CreateTableBody::AsSelect(_),
            ..
        } = statement
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateTable {
            body: CreateTableBody::ColumnsAndConstraints { columns, .. },
            ..
        } = statement
            && columns
                .iter()
                .any(|column| column.flags.intersects(ColFlags::GENERATED))
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateTable {
            body:
                CreateTableBody::ColumnsAndConstraints {
                    columns,
                    constraints,
                    ..
                },
            ..
        } = statement
            && (columns.iter().any(|column| {
                column.constraints.iter().any(|constraint| {
                    matches!(constraint.constraint, ColumnConstraint::ForeignKey { .. })
                })
            }) || constraints.is_some_and(|constraints| {
                constraints.iter().any(|constraint| {
                    matches!(constraint.constraint, TableConstraint::ForeignKey { .. })
                })
            }))
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateIndex {
            where_clause: Some(_),
            ..
        } = statement
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateIndex { idx_name, .. } = statement
            && idx_name
                .db_name
                .as_ref()
                .is_some_and(|database| database != "main")
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        if let Stmt::CreateIndex { columns, .. } = statement
            && columns.iter().any(|column| {
                !matches!(column.expr, Expr::Id(_))
                    && !matches!(column.expr, Expr::Collate(expr, _) if matches!(expr, Expr::Id(_)))
            })
        {
            return Err(DesiredSchemaError::Unsupported {
                statement_number: index + 1,
                kind: UnsupportedKind::Declaration,
            });
        }

        let name = match statement {
            Stmt::CreateTable { tbl_name, .. } => Some(&tbl_name.name),
            Stmt::CreateIndex { idx_name, .. } => Some(&idx_name.name),
            Stmt::CreateView { view_name, .. } => Some(&view_name.name),
            _ => None,
        };
        if let Some(name) = name {
            if name.0.eq_ignore_ascii_case("_fledgling_migrations") {
                return Err(DesiredSchemaError::Unsupported {
                    statement_number: index + 1,
                    kind: UnsupportedKind::Declaration,
                });
            }
            if !names.insert(name) {
                return Err(DesiredSchemaError::Unsupported {
                    statement_number: index + 1,
                    kind: UnsupportedKind::Declaration,
                });
            }
            continue;
        }

        let kind = if matches!(
            statement,
            Stmt::CreateTrigger { .. } | Stmt::CreateVirtualTable { .. }
        ) {
            UnsupportedKind::Declaration
        } else {
            UnsupportedKind::Executable
        };
        return Err(DesiredSchemaError::Unsupported {
            statement_number: index + 1,
            kind,
        });
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Individual declarations and view query shapes
// ----------------------------------------------------------------------------

/// Lowers one supported `CREATE TABLE` declaration into the shared model.
fn lower_table(sql: &str) -> Result<Table, DesiredSchemaError> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, sql).map_err(DesiredSchemaError::Parse)?;
    check_desired_commands(&commands)?;
    let [Cmd::Stmt(statement)] = commands.as_slice() else {
        return Err(DesiredSchemaError::Unsupported {
            statement_number: 1,
            kind: UnsupportedKind::Declaration,
        });
    };
    lower_table_statement(statement, 1)
}

/// Lowers one ordinary `CREATE VIEW`, retaining its SELECT as owned SQL.
fn lower_view(sql: &str) -> Result<View, DesiredSchemaError> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, sql).map_err(DesiredSchemaError::Parse)?;
    check_desired_commands(&commands)?;
    let [Cmd::Stmt(statement)] = commands.as_slice() else {
        return Err(DesiredSchemaError::Unsupported {
            statement_number: 1,
            kind: UnsupportedKind::Declaration,
        });
    };
    lower_view_statement(statement, 1)
}

/// Lower an unconditional view declaration without a named column list.
fn lower_view_statement(
    statement: &Stmt,
    statement_number: usize,
) -> Result<View, DesiredSchemaError> {
    let Stmt::CreateView {
        if_not_exists: false,
        view_name,
        columns: None,
        select,
        ..
    } = statement
    else {
        return Err(DesiredSchemaError::Unsupported {
            statement_number,
            kind: UnsupportedKind::Declaration,
        });
    };
    Ok(View {
        name: unquote_identifier(view_name.name.0),
        select: format!("{}", sqlite3_parser::ast::Cmd::Stmt(Stmt::Select(select))),
    })
}

fn view_has_integer_literal_select(view: &View) -> bool {
    view_has_select_shape(
        view,
        |expr| matches!(expr, Expr::Literal(sqlite3_parser::ast::Literal::Numeric(value)) if value.parse::<i64>().is_ok()),
    )
}

fn view_has_lower_string_literal_select(view: &View) -> bool {
    view_has_string_literal_function_select(view, "lower")
}

fn view_has_upper_string_literal_select(view: &View) -> bool {
    view_has_string_literal_function_select(view, "upper")
}

/// Accept simple SELECTs over named FROM/JOIN sources, or the supported
/// literal-only forms. Shape alone does not establish dependency readiness.
fn validate_view_select_shape(view: &View) -> Result<(), &'static str> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, &view.select).map_err(|_| "invalid SELECT syntax")?;
    let [Cmd::Stmt(Stmt::Select(select))] = commands.as_slice() else {
        return Err("expected a single SELECT statement");
    };
    let unsupported = if select.with.is_some() {
        Some("WITH clauses are not supported")
    } else if select.body.compounds.is_some() {
        Some("compound SELECTs are not supported")
    } else if select.order_by.is_some() {
        Some("ORDER BY is not supported")
    } else if select.limit.is_some() {
        Some("LIMIT is not supported")
    } else {
        None
    };
    if let Some(reason) = unsupported {
        return Err(reason);
    }
    let OneSelect::Select {
        ref from,
        ref group_by,
        ref having,
        ref window_clause,
        ..
    } = select.body.select
    else {
        return Err("expected a SELECT query, not VALUES");
    };
    if group_by.is_some() || having.is_some() || window_clause.is_some() {
        return Err("GROUP BY, HAVING, and WINDOW clauses are not supported");
    }
    let Some(from) = from else {
        return if view_has_integer_literal_select(view)
            || view_has_lower_string_literal_select(view)
            || view_has_upper_string_literal_select(view)
            || view_has_length_string_literal_select(view)
        {
            Ok(())
        } else {
            Err(
                "without FROM, expected an integer literal or lower(), upper(), or length() applied to a string literal",
            )
        };
    };
    if !matches!(from.select, Some(SelectTable::Table(..))) {
        return Err("FROM must refer to a named table or view");
    }
    if from.joins.as_ref().is_some_and(|joins| {
        !joins
            .iter()
            .all(|join| matches!(&join.table, SelectTable::Table(..)))
    }) {
        return Err("JOIN must refer to a named table or view");
    }
    Ok(())
}

/// Re-render one parsed SELECT for comparison, or return `None` for other SQL.
pub(crate) fn normalize_view_select(sql: &str) -> Option<String> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, sql).ok()?;
    let [Cmd::Stmt(Stmt::Select(select))] = commands.as_slice() else {
        return None;
    };
    Some(format!("{}", Cmd::Stmt(Stmt::Select(select))))
}

fn view_has_length_string_literal_select(view: &View) -> bool {
    view_has_string_literal_function_select(view, "length")
}

fn view_has_string_literal_function_select(view: &View, function: &str) -> bool {
    view_has_select_shape(view, |expr| {
        matches!(
            expr,
            Expr::FunctionCall {
                name,
                distinctness: None,
                args: Some([Expr::Literal(sqlite3_parser::ast::Literal::String(_))]),
                order_by: None,
                filter_over: None,
            } if name.0.eq_ignore_ascii_case(function)
        )
    })
}

/// Match an unaliased, single-expression SELECT without FROM or other clauses.
fn view_has_select_shape(view: &View, matches_expr: impl FnOnce(&Expr<'_>) -> bool) -> bool {
    let arena = Bump::new();
    let Ok(commands) = parse_sql(&arena, &view.select) else {
        return false;
    };
    let [Cmd::Stmt(Stmt::Select(select))] = commands.as_slice() else {
        return false;
    };
    matches!(
        select.body.select,
        OneSelect::Select {
            distinctness: None,
            columns: [ResultColumn::Expr(expr, None)],
            from: None,
            where_clause: None,
            group_by: None,
            having: None,
            window_clause: None,
        } if matches_expr(expr)
    ) && select.with.is_none()
        && select.body.compounds.is_none()
        && select.order_by.is_none()
        && select.limit.is_none()
}

/// Lowers one ordinary named `CREATE INDEX` declaration.
pub(super) fn lower_index(sql: &str) -> Result<Index, DesiredSchemaError> {
    let arena = Bump::new();
    let commands = parse_sql(&arena, sql).map_err(DesiredSchemaError::Parse)?;
    check_desired_commands(&commands)?;
    let [Cmd::Stmt(statement)] = commands.as_slice() else {
        return Err(unsupported_declaration(1));
    };
    lower_index_statement(statement, 1)
}

/// Lower ordered column keys and their modifiers. The model retains uniqueness
/// and collations even though the additive planner rejects those options.
fn lower_index_statement(
    statement: &Stmt,
    statement_number: usize,
) -> Result<Index, DesiredSchemaError> {
    let Stmt::CreateIndex {
        unique,
        if_not_exists: false,
        idx_name,
        tbl_name,
        columns,
        where_clause: None,
    } = statement
    else {
        return Err(DesiredSchemaError::Unsupported {
            statement_number,
            kind: UnsupportedKind::Declaration,
        });
    };
    let columns = columns
        .iter()
        .map(|column| {
            let (name, collation) = match &column.expr {
                Expr::Id(name) => (unquote_identifier(name.0), None),
                Expr::Collate(expr, collation) => {
                    let Expr::Id(name) = *expr else {
                        return Err(unsupported_declaration(statement_number));
                    };
                    (unquote_identifier(name.0), Some((*collation).to_owned()))
                }
                _ => return Err(unsupported_declaration(statement_number)),
            };
            if column.nulls.is_some() {
                return Err(unsupported_declaration(statement_number));
            }
            Ok(IndexColumn {
                name,
                order: column.order.map(|order| match order {
                    sqlite3_parser::ast::SortOrder::Asc => SortDirection::Asc,
                    sqlite3_parser::ast::SortOrder::Desc => SortDirection::Desc,
                }),
                collation,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(Index {
        name: unquote_identifier(idx_name.name.0),
        table: unquote_identifier(tbl_name.0),
        unique: *unique,
        columns,
    })
}

/// Strip recognized identifier quotes and decode doubled closing delimiters.
fn unquote_identifier(identifier: &str) -> String {
    let Some((first, last)) = identifier.chars().next().zip(identifier.chars().last()) else {
        return identifier.to_owned();
    };
    let closing = match first {
        '"' => Some('"'),
        '`' => Some('`'),
        '[' => Some(']'),
        _ => None,
    };
    if closing == Some(last) && identifier.len() >= 2 {
        identifier[1..identifier.len() - 1].replace(&format!("{last}{last}"), &last.to_string())
    } else {
        identifier.to_owned()
    }
}

fn unsupported_declaration(statement_number: usize) -> DesiredSchemaError {
    DesiredSchemaError::Unsupported {
        statement_number,
        kind: UnsupportedKind::Declaration,
    }
}

// ----------------------------------------------------------------------------
// Desired schema and authored source spans
// ----------------------------------------------------------------------------

/// Lowers a desired SQLite document into its typed schema model.
#[cfg(test)]
fn lower_schema(sql: &str) -> Result<SqliteSchema, DesiredSchemaError> {
    lower_schema_with_sources(sql).map(|(schema, _)| schema)
}

/// Authored SQL slices paired with their lowered definitions in source order.
type SchemaSources<'a> = Vec<(&'a str, SqliteObject)>;

/// Lower a declaration-only document and extract matching authored CREATE
/// statements in the same parser pass. Reject names that collide under SQLite
/// identity rules rather than selecting one source to render.
pub(crate) fn lower_schema_with_sources(
    sql: &str,
) -> Result<(SqliteSchema, SchemaSources<'_>), DesiredSchemaError> {
    // Derive both command and source span from the same parser cursor.
    let arena = Bump::new();
    let mut parser = Parser::new(&arena, sql.as_bytes());
    let line_starts = std::iter::once(0)
        .chain(
            sql.bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
        )
        .collect::<Vec<_>>();
    let mut commands = Vec::new();
    let mut sources = Vec::new();
    loop {
        let position = parser.position();
        let start = line_starts
            .get(position.line.saturating_sub(1))
            .copied()
            .unwrap_or(sql.len())
            .saturating_add(position.column.saturating_sub(1))
            .min(sql.len());
        let Some(command) = parser.next().map_err(DesiredSchemaError::Parse)? else {
            break;
        };
        let position = parser.position();
        let end = line_starts
            .get(position.line.saturating_sub(1))
            .copied()
            .unwrap_or(sql.len())
            .saturating_add(position.column.saturating_sub(1))
            .min(sql.len());
        sources.push(sql[skip_statement_prefix(sql, start, end)..end].trim());
        commands.push(command);
    }
    check_desired_commands(&commands)?;
    let mut definitions = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        let statement_number = index + 1;
        let Cmd::Stmt(statement) = command else {
            return Err(DesiredSchemaError::Unsupported {
                statement_number,
                kind: UnsupportedKind::Executable,
            });
        };
        definitions.push(match statement {
            Stmt::CreateTable { .. } => {
                SqliteObject::Table(lower_table_statement(statement, statement_number)?)
            }
            Stmt::CreateView { .. } => {
                SqliteObject::View(lower_view_statement(statement, statement_number)?)
            }
            Stmt::CreateIndex { .. } => {
                SqliteObject::Index(lower_index_statement(statement, statement_number)?)
            }
            _ => {
                return Err(DesiredSchemaError::Unsupported {
                    statement_number,
                    kind: UnsupportedKind::Declaration,
                });
            }
        });
    }
    let paired = sources
        .into_iter()
        .zip(definitions.iter().cloned())
        .collect();
    let schema = SqliteSchema::try_new(definitions)
        .map_err(|duplicate| unsupported_declaration(duplicate.index + 1))?;
    Ok((schema, paired))
}

// ----------------------------------------------------------------------------
// Table and column lowering
// ----------------------------------------------------------------------------

/// Lower a column-list table, rejecting options and constraints the model
/// cannot faithfully represent. Preserve column order and modeled SQL fragments.
fn lower_table_statement(
    statement: &Stmt,
    statement_number: usize,
) -> Result<Table, DesiredSchemaError> {
    let Stmt::CreateTable {
        tbl_name,
        body:
            CreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                flags,
            },
        ..
    } = statement
    else {
        return Err(DesiredSchemaError::Unsupported {
            statement_number,
            kind: UnsupportedKind::Declaration,
        });
    };
    if !supports_table_checks(*constraints)
        || flags.intersects(
            sqlite3_parser::ast::TabFlags::WithoutRowid | sqlite3_parser::ast::TabFlags::Strict,
        )
    {
        return Err(unsupported_declaration(statement_number));
    }

    Ok(Table {
        name: unquote_identifier(tbl_name.name.0),
        check: constraints.and_then(|constraints| {
            constraints.iter().find_map(|constraint| {
                if let sqlite3_parser::ast::TableConstraint::Check(expression, _) =
                    &constraint.constraint
                {
                    Some(expression.to_string())
                } else {
                    None
                }
            })
        }),
        columns: columns
            .iter()
            .map(|column| lower_column(column, statement_number))
            .collect::<Result<_, _>>()?,
    })
}

fn lower_column(
    definition: &ColumnDefinition<'_>,
    statement_number: usize,
) -> Result<Column, DesiredSchemaError> {
    let unsupported = || unsupported_declaration(statement_number);
    let declared_type = definition.col_type.as_ref();
    if definition.flags.contains(ColFlags::GENERATED)
        || declared_type.is_some_and(|ty| ty.size.is_some())
    {
        return Err(unsupported());
    }

    let mut column = Column {
        name: unquote_identifier(definition.col_name.0),
        declared_type: declared_type.map(|ty| ty.name.to_owned()),
        not_null: false,
        check: None,
        default_null: false,
        default_value: None,
        primary_key: false,
    };
    for constraint in definition.constraints {
        if constraint.name.is_some() {
            return Err(unsupported());
        }
        match &constraint.constraint {
            ColumnConstraint::Check(expr) if column.check.is_none() => {
                column.check = Some(format!("{expr}"));
            }
            ColumnConstraint::NotNull {
                nullable: false,
                conflict_clause: None,
            } if !column.not_null => column.not_null = true,
            ColumnConstraint::Default(Expr::Literal(literal))
                if !column.default_null && column.default_value.is_none() =>
            {
                match literal {
                    Literal::Null => column.default_null = true,
                    Literal::String(value) => column.default_value = Some((*value).to_owned()),
                    _ => return Err(unsupported()),
                }
            }
            ColumnConstraint::PrimaryKey {
                order: None,
                conflict_clause: None,
                auto_increment: false,
            } if !column.primary_key
                && declared_type.is_some_and(|ty| ty.name.eq_ignore_ascii_case("INTEGER")) =>
            {
                column.primary_key = true;
            }
            _ => return Err(unsupported()),
        }
    }
    Ok(column)
}

/// Permit no table constraints or one anonymous CHECK with no conflict policy.
fn supports_table_checks(
    constraints: Option<&[sqlite3_parser::ast::NamedTableConstraint<'_>]>,
) -> bool {
    constraints.is_none_or(|constraints| {
        constraints.len() == 1
            && constraints[0].name.is_none()
            && matches!(
                constraints[0].constraint,
                sqlite3_parser::ast::TableConstraint::Check(_, None)
            )
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, Error};
    use sqlite3_parser::{
        Bump,
        ast::{Cmd, Stmt},
    };

    use super::{
        Column, DesiredSchemaError, Index, IndexColumn, SqliteObject, SqliteSchema, Table,
        UnsupportedKind, View, check_desired_sql, lower_index, lower_schema,
        lower_schema_with_sources, lower_table, lower_view, open_read_only, parse_sql, read_schema,
    };
    use super::{SortDirection, model::DuplicateDefinition};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestDb(PathBuf);

    impl TestDb {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fledgling-read-only-{}-{id}.sqlite",
                std::process::id()
            ));
            Self(path)
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn loading_rejects_duplicates_by_normalized_identity() {
        let table = lower_schema("CREATE TABLE items (id INTEGER);").unwrap();
        let view = lower_schema("CREATE VIEW ITEMS AS SELECT 1;").unwrap();
        let other = lower_schema("CREATE TABLE other (id INTEGER);").unwrap();
        let duplicate = SqliteSchema::try_new(
            [&table, &other, &view]
                .into_iter()
                .flat_map(|schema| schema.definitions().iter().cloned())
                .collect(),
        );
        assert_eq!(duplicate, Err(DuplicateDefinition { index: 2 }));
        assert!(matches!(
            lower_schema("CREATE TABLE items (id INTEGER); CREATE VIEW \"ITEMS\" AS SELECT 1;"),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                ..
            })
        ));
    }

    #[test]
    fn opens_existing_database_for_reads_but_rejects_writes() {
        let db = TestDb::new();
        {
            let connection = Connection::open(&db.0).unwrap();
            connection
                .execute_batch("CREATE TABLE items (id INTEGER); INSERT INTO items VALUES (1);")
                .unwrap();
        }

        let connection = open_read_only(&db.0).unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            connection.execute("INSERT INTO items VALUES (2)", []),
            Err(Error::SqliteFailure(_, _))
        ));
    }

    #[test]
    fn missing_path_returns_error_without_creating_database() {
        let db = TestDb::new();
        assert!(open_read_only(&db.0).is_err());
        assert!(!db.0.exists());
    }

    #[test]
    fn reports_main_virtual_tables_as_unsupported() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE VIRTUAL TABLE search USING fts5(content);")
            .unwrap();

        let error = read_schema(&connection).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid parameter name: unsupported virtual table in main: search"
        );
    }

    #[test]
    fn reports_persistent_main_triggers_as_unsupported() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER);
                 CREATE TRIGGER \"odd trigger\" AFTER INSERT ON items BEGIN SELECT 1; END;",
            )
            .unwrap();

        let error = read_schema(&connection).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid parameter name: unsupported trigger in main: odd trigger"
        );
    }

    #[test]
    fn reports_inline_and_table_level_foreign_keys_as_unsupported_when_disabled() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 CREATE TABLE inline_fk (parent_id INTEGER REFERENCES parent(id));
                 CREATE TABLE table_fk (
                     parent_id INTEGER,
                     FOREIGN KEY (parent_id) REFERENCES parent(id)
                 );",
            )
            .unwrap();

        let error = read_schema(&connection).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid parameter name: unsupported foreign key in main table: inline_fk"
        );
    }

    #[test]
    fn snapshot_ignores_temporary_and_attached_unsupported_objects() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE main.items (id INTEGER);
                 CREATE TEMP TRIGGER temp_trigger AFTER INSERT ON items BEGIN SELECT 1; END;
                 CREATE TEMP TABLE temp_child (parent_id INTEGER REFERENCES parent(id));
                 ATTACH ':memory:' AS other;
                 CREATE TABLE other.parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE other.child (parent_id INTEGER REFERENCES parent(id));
                 CREATE TRIGGER other.attached_trigger AFTER INSERT ON child BEGIN SELECT 1; END;",
            )
            .unwrap();

        assert_eq!(
            read_schema(&connection)
                .unwrap()
                .tables()
                .map(|table| table.name.as_str())
                .collect::<Vec<_>>(),
            ["items"]
        );
    }

    #[test]
    fn reads_sorted_main_tables_into_schema_model_and_handles_empty_schema() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE z_table (id INTEGER);
                 CREATE TABLE a_table (label TEXT);
                  CREATE VIEW z_view AS SELECT id FROM z_table;
                  CREATE VIEW a_view AS SELECT label FROM a_table;
                  CREATE INDEX z_index ON z_table(id);
                  CREATE INDEX a_index ON a_table(label);
                  CREATE TEMP TABLE temp_table (id INTEGER);
                 CREATE TEMP VIEW temp_view AS SELECT 1;
                 ATTACH ':memory:' AS other;
                 CREATE TABLE other.attached_table (id INTEGER);
                 CREATE VIEW other.attached_view AS SELECT 1;",
            )
            .unwrap();

        let schema = read_schema(&connection).unwrap();
        assert_eq!(
            schema
                .tables()
                .map(|table| table.name.as_str())
                .collect::<Vec<_>>(),
            ["a_table", "z_table"]
        );
        assert_eq!(
            schema
                .views()
                .map(|view| view.name.as_str())
                .collect::<Vec<_>>(),
            ["a_view", "z_view"]
        );
        assert_eq!(
            schema
                .indexes()
                .map(|index| index.name.as_str())
                .collect::<Vec<_>>(),
            ["a_index", "z_index"]
        );

        let empty = Connection::open_in_memory().unwrap();
        assert!(read_schema(&empty).unwrap().definitions().is_empty());
    }

    #[test]
    fn snapshot_reports_unsupported_views_and_indexes_with_object_context() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE VIEW unsupported (named) AS SELECT 1;")
            .unwrap();
        let error = read_schema(&connection).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported view declaration for unsupported")
        );

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE items (id INTEGER, label TEXT);
                 CREATE INDEX expression_index ON items(lower(label));",
            )
            .unwrap();
        let error = read_schema(&connection).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported index declaration for expression_index")
        );
    }

    #[test]
    fn lowering_retains_authored_sources_in_statement_order() {
        let sql = "/* lead */ CREATE TABLE café (id); -- next\nCREATE VIEW v AS SELECT 'x;y';";
        let (schema, sources) = lower_schema_with_sources(sql).unwrap();
        assert_eq!(schema.definitions().len(), 2);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].0, "CREATE TABLE café (id);");
        assert_eq!(sources[1].0, "CREATE VIEW v AS SELECT 'x;y';");
    }

    #[test]
    fn parses_schema_statements_in_source_order() {
        let arena = Bump::new();
        let sql = include_str!("../../tests/sqlite/scenarios/multiple_changes/target.schema.sql");

        assert!(matches!(
            parse_sql(&arena, sql).unwrap().as_slice(),
            [
                Cmd::Stmt(Stmt::CreateTable { .. }),
                Cmd::Stmt(Stmt::CreateTable { .. }),
                Cmd::Stmt(Stmt::CreateIndex { .. }),
                Cmd::Stmt(Stmt::CreateView { .. }),
            ]
        ));
    }

    #[test]
    fn lowers_representative_sqlite_acceptance_schemas() {
        for sql in [
            include_str!("../../tests/sqlite/scenarios/fresh_migration/target.schema.sql"),
            include_str!("../../tests/sqlite/scenarios/multiple_changes/target.schema.sql"),
        ] {
            lower_schema(sql).unwrap();
        }
    }

    #[test]
    fn parses_trigger_body_as_one_statement() {
        let arena = Bump::new();
        let sql = "CREATE TRIGGER log_insert AFTER INSERT ON items BEGIN
                       INSERT INTO log VALUES ('first; entry');
                       INSERT INTO log VALUES ('second entry');
                   END;
                   SELECT 1";

        assert!(matches!(
            parse_sql(&arena, sql).unwrap().as_slice(),
            [
                Cmd::Stmt(Stmt::CreateTrigger { .. }),
                Cmd::Stmt(Stmt::Select(_))
            ]
        ));
    }

    #[test]
    fn rejects_invalid_sql_after_a_valid_statement() {
        let arena = Bump::new();
        assert!(parse_sql(&arena, "CREATE TABLE items (id); CREATE TABLE").is_err());
    }

    #[test]
    fn accepts_empty_input_and_allowed_statement_kinds() {
        assert!(check_desired_sql("").is_ok());
        assert!(
            check_desired_sql(
                "CREATE TABLE t (id); CREATE INDEX i ON t(id); CREATE VIEW v AS SELECT id FROM t;"
            )
            .is_ok()
        );
    }

    #[test]
    fn lowers_plain_table_and_preserves_column_order_and_declared_types() {
        assert_eq!(
            lower_table("CREATE TABLE items (id INTEGER, label TEXT, payload)").unwrap(),
            Table {
                name: "items".into(),
                check: None,
                columns: vec![
                    Column {
                        name: "id".into(),
                        declared_type: Some("INTEGER".into()),
                        not_null: false,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    },
                    Column {
                        name: "label".into(),
                        declared_type: Some("TEXT".into()),
                        not_null: false,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    },
                    Column {
                        name: "payload".into(),
                        declared_type: None,
                        not_null: false,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    },
                ],
            }
        );
    }

    #[test]
    fn lowers_inline_integer_primary_key_and_text_default() {
        let table = lower_table(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'done')))",
        )
        .unwrap();
        assert!(table.columns[0].primary_key);
        assert_eq!(table.columns[1].default_value.as_deref(), Some("'draft'"));
        assert_eq!(
            table.columns[1].check.as_deref(),
            Some("status IN ('draft', 'done')")
        );
    }

    #[test]
    fn rejects_unsupported_primary_key_and_default_forms() {
        for sql in [
            "CREATE TABLE t (id TEXT PRIMARY KEY)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY DESC)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)",
            "CREATE TABLE t (id INTEGER DEFAULT 7)",
            "CREATE TABLE t (id INTEGER DEFAULT (1))",
        ] {
            assert!(
                matches!(
                    lower_table(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn lowers_one_unnamed_column_check_expression() {
        assert_eq!(
            lower_table("CREATE TABLE items (kind TEXT CHECK (kind IN ('a', 'b')))").unwrap(),
            Table {
                name: "items".into(),
                check: None,
                columns: vec![Column {
                    name: "kind".into(),
                    declared_type: Some("TEXT".into()),
                    not_null: false,
                    check: Some("kind IN ('a', 'b')".into()),
                    default_null: false,
                    default_value: None,
                    primary_key: false,
                }],
            }
        );
        for sql in [
            "CREATE TABLE t (a CHECK (a > 0) CHECK (a < 10))",
            "CREATE TABLE t (a CONSTRAINT named CHECK (a > 0))",
        ] {
            assert!(
                matches!(
                    lower_table(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    }),
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn lowers_one_table_check_expression() {
        let table =
            lower_table("CREATE TABLE items (kind TEXT, CHECK (kind IN ('a', 'b')))").unwrap();
        assert_eq!(table.check.as_deref(), Some("kind IN ('a', 'b')"));
    }

    #[test]
    fn lowers_only_one_unnamed_unparenthesized_default_null() {
        assert_eq!(
            lower_table("CREATE TABLE t (absent TEXT, explicit TEXT DEFAULT NULL)")
                .unwrap()
                .columns
                .iter()
                .map(|column| column.default_null)
                .collect::<Vec<_>>(),
            [false, true]
        );
        for sql in [
            "CREATE TABLE t (a DEFAULT NULL DEFAULT NULL)",
            "CREATE TABLE t (a CONSTRAINT named DEFAULT NULL)",
            "CREATE TABLE t (a DEFAULT (NULL))",
            "CREATE TABLE t (a DEFAULT 1)",
        ] {
            assert!(
                matches!(
                    lower_table(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn rejects_repeated_column_constraints_at_their_statement() {
        for column in [
            "a TEXT NOT NULL NOT NULL",
            "a INTEGER PRIMARY KEY PRIMARY KEY",
            "a TEXT DEFAULT NULL DEFAULT 'draft'",
            "a TEXT DEFAULT 'draft' DEFAULT NULL",
        ] {
            let sql = format!("CREATE TABLE first (id); CREATE TABLE second ({column});");
            assert!(
                matches!(
                    lower_schema(&sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn lowers_plain_view_and_preserves_builtin_calls_in_its_select() {
        assert_eq!(
            lower_view(
                "CREATE VIEW main.names AS SELECT lower(name), coalesce(label, 'none') FROM people"
            )
            .unwrap(),
            View {
                name: "names".into(),
                select: "SELECT lower (name), coalesce (label, 'none') FROM people;".into(),
            }
        );
    }

    #[test]
    fn lowers_named_index_and_preserves_order_unique_and_key_modifiers() {
        assert_eq!(
        lower_index(
            "CREATE UNIQUE INDEX main.idx ON items (\"first col\" COLLATE NOCASE DESC, second ASC)"
        )
        .unwrap(),
        Index {
            name: "idx".into(),
            table: "items".into(),
            unique: true,
            columns: vec![
                IndexColumn {
                    name: "first col".into(),
                    order: Some(SortDirection::Desc),
                    collation: Some("NOCASE".into()),
                },
                IndexColumn {
                    name: "second".into(),
                    order: Some(SortDirection::Asc),
                    collation: None,
                },
            ],
        }
    );
    }

    #[test]
    fn rejects_index_shapes_not_represented_by_the_index_model() {
        for sql in [
            "CREATE INDEX IF NOT EXISTS idx ON t(id)",
            "CREATE INDEX idx ON t(id) WHERE id > 0",
            "CREATE INDEX idx ON t(lower(id))",
            "CREATE INDEX idx ON t(id + 1)",
            "CREATE INDEX idx ON t(id NULLS FIRST)",
            "CREATE INDEX first ON t(id); CREATE INDEX second ON t(id)",
        ] {
            assert!(
                matches!(
                    lower_index(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn rejects_view_shapes_not_represented_by_the_view_model() {
        for sql in [
            "CREATE VIEW v (name) AS SELECT lower(name) FROM people",
            "CREATE VIEW IF NOT EXISTS v AS SELECT 1",
            "CREATE VIEW first AS SELECT 1; CREATE VIEW second AS SELECT 2",
        ] {
            assert!(
                matches!(
                    lower_view(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn lowers_mixed_documents_into_schema_collections_and_accepts_empty_input() {
        assert!(
            lower_schema(" -- empty\n /* document */ ")
                .unwrap()
                .definitions()
                .is_empty()
        );
        assert_eq!(
            lower_schema("CREATE VIEW v AS SELECT 1; CREATE TABLE t (id); CREATE INDEX i ON t(id)")
                .unwrap()
                .definitions(),
            [
                SqliteObject::View(View {
                    name: "v".into(),
                    select: "SELECT 1;".into(),
                }),
                SqliteObject::Table(Table {
                    name: "t".into(),
                    check: None,
                    columns: vec![Column {
                        name: "id".into(),
                        declared_type: None,
                        not_null: false,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    }],
                }),
                SqliteObject::Index(Index {
                    name: "i".into(),
                    table: "t".into(),
                    unique: false,
                    columns: vec![IndexColumn {
                        name: "id".into(),
                        order: None,
                        collation: None,
                    }],
                }),
            ]
        );
    }

    #[test]
    fn schema_lowering_reports_shape_failures_at_actual_statement() {
        assert!(matches!(
            lower_schema(
                "CREATE TABLE t (id); CREATE TABLE u (id); CREATE INDEX i ON u(id NULLS FIRST)"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 3,
                kind: UnsupportedKind::Declaration,
            })
        ));
        assert!(matches!(
            lower_schema("CREATE TABLE t (id); SELECT 1"),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                ..
            })
        ));
    }

    #[test]
    fn refuses_lowering_features_not_represented_by_the_table_model() {
        for sql in [
            "CREATE TABLE t (id INTEGER, UNIQUE(id))",
            "CREATE TABLE t (id INTEGER) WITHOUT ROWID",
            "CREATE TABLE t (id INTEGER) STRICT",
            "CREATE TABLE t (id VARCHAR(20))",
            "CREATE INDEX i ON t(id)",
            "CREATE TABLE t (id); CREATE TABLE u (id)",
        ] {
            assert!(
                matches!(
                    lower_table(sql),
                    Err(DesiredSchemaError::Unsupported { .. })
                ),
                "{sql}"
            );
        }
        for sql in [
            "CREATE TABLE t (id CONSTRAINT required NOT NULL)",
            "CREATE TABLE t (id NOT NULL ON CONFLICT REPLACE)",
            "CREATE TABLE t (id NULL)",
        ] {
            assert!(
                matches!(
                    lower_table(sql),
                    Err(DesiredSchemaError::Unsupported {
                        kind: UnsupportedKind::Declaration,
                        ..
                    })
                ),
                "{sql}"
            );
        }
    }

    #[test]
    fn validates_create_table_database_qualifier() {
        for declaration in [
            "CREATE TABLE t (id)",
            "CREATE TABLE main.t (id)",
            "CREATE TABLE MAIN.t (id)",
            "CREATE TABLE MaIn.t (id)",
            "CREATE TABLE \"MaIn\".t (id)",
            "CREATE TABLE \"main.t\" (id)",
        ] {
            assert!(check_desired_sql(declaration).is_ok(), "{declaration}");
        }

        for declaration in [
            "CREATE TABLE temp.t (id)",
            "CREATE TABLE other.t (id)",
            "CREATE TABLE \"Temp\".t (id)",
        ] {
            let sql = format!("CREATE TABLE ordinary (id); {declaration}");
            assert!(
                matches!(
                    check_desired_sql(&sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{declaration}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_table_names_in_main_at_second_declaration() {
        for sql in [
            "CREATE TABLE t (id); CREATE TABLE T (id)",
            "CREATE TABLE t (id); CREATE TABLE main.t (id)",
            "CREATE TABLE main.t (id); CREATE TABLE \"MAIN\".T (id)",
            "CREATE TABLE \"a.b\" (id); CREATE TABLE main.\"A.B\" (id)",
            "CREATE TABLE caféName (id); CREATE TABLE [caféNAME] (id)",
            "CREATE TABLE \"a\"\".b\" (id); CREATE TABLE [A\".B] (id)",
        ] {
            assert!(
                matches!(
                    check_desired_sql(sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{sql}"
            );
        }

        assert!(check_desired_sql("CREATE TABLE \"a.b\" (id); CREATE TABLE a (id)").is_ok());
        assert!(check_desired_sql("CREATE TABLE Ä (id); CREATE TABLE ä (id)").is_ok());
    }

    #[test]
    fn rejects_create_table_as_select_as_an_unsupported_declaration() {
        assert!(matches!(
            check_desired_sql(
                "CREATE TABLE source (id); CREATE TABLE copy AS SELECT id FROM source"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Declaration,
            })
        ));
        assert!(check_desired_sql("CREATE TABLE copy (id)").is_ok());
    }

    #[test]
    fn rejects_generated_columns_as_unsupported_declarations() {
        for declaration in [
            "CREATE TABLE generated_table (source INTEGER, derived INTEGER AS (source + 1))",
            "CREATE TABLE generated_table (source INTEGER, derived INTEGER GENERATED ALWAYS AS (source + 1) VIRTUAL)",
            "CREATE TABLE generated_table (source INTEGER, derived INTEGER AS (source + 1) STORED)",
        ] {
            let sql = format!("CREATE TABLE ordinary_table (id); {declaration}");
            assert!(matches!(
                check_desired_sql(&sql),
                Err(DesiredSchemaError::Unsupported {
                    statement_number: 2,
                    kind: UnsupportedKind::Declaration,
                })
            ));
        }
        assert!(check_desired_sql("CREATE TABLE ordinary_table (id, name TEXT)").is_ok());
    }

    #[test]
    fn rejects_foreign_key_declarations_as_unsupported() {
        for declaration in [
            "CREATE TABLE child (parent_id REFERENCES parent(id))",
            "CREATE TABLE child (parent_id, FOREIGN KEY (parent_id) REFERENCES parent(id))",
            "CREATE TABLE child (parent_id REFERENCES parent)",
            "CREATE TABLE child (parent_id, CONSTRAINT fk FOREIGN KEY (parent_id) REFERENCES parent(id))",
        ] {
            let sql = format!("CREATE TABLE ordinary_table (id); {declaration}");
            assert!(
                matches!(
                    check_desired_sql(&sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{declaration}"
            );
        }
        assert!(
            check_desired_sql(
                "CREATE TABLE ordinary_table (id); CREATE TABLE child (parent_id INTEGER)"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_temporary_tables_as_unsupported_declarations() {
        for declaration in [
            "CREATE TEMP TABLE temporary_table (id)",
            "CREATE TEMPORARY TABLE temporary_table (id)",
        ] {
            let sql = format!("CREATE TABLE ordinary_table (id); {declaration}");
            assert!(matches!(
                check_desired_sql(&sql),
                Err(DesiredSchemaError::Unsupported {
                    statement_number: 2,
                    kind: UnsupportedKind::Declaration,
                })
            ));
        }
        assert!(check_desired_sql("CREATE TABLE ordinary_table (id)").is_ok());
    }

    #[test]
    fn rejects_temporary_views_as_unsupported_declarations() {
        for declaration in [
            "CREATE TEMP VIEW temporary_view AS SELECT 1",
            "CREATE TEMPORARY VIEW temporary_view AS SELECT 1",
        ] {
            let sql = format!("CREATE TABLE ordinary_table (id); {declaration}");
            assert!(matches!(
                check_desired_sql(&sql),
                Err(DesiredSchemaError::Unsupported {
                    statement_number: 2,
                    kind: UnsupportedKind::Declaration,
                })
            ));
        }
        assert!(check_desired_sql("CREATE VIEW ordinary_view AS SELECT 1").is_ok());
    }

    #[test]
    fn validates_create_view_database_qualifier() {
        for declaration in [
            "CREATE VIEW v AS SELECT 1",
            "CREATE VIEW main.v AS SELECT 1",
            "CREATE VIEW MAIN.v AS SELECT 1",
            "CREATE VIEW MaIn.v AS SELECT 1",
            "CREATE VIEW \"MaIn\".v AS SELECT 1",
            "CREATE VIEW \"main.v\" AS SELECT 1",
        ] {
            assert!(check_desired_sql(declaration).is_ok(), "{declaration}");
        }

        for declaration in [
            "CREATE VIEW temp.v AS SELECT 1",
            "CREATE VIEW other.v AS SELECT 1",
            "CREATE VIEW \"Temp\".v AS SELECT 1",
        ] {
            let sql = format!("CREATE TABLE ordinary (id); {declaration}");
            assert!(
                matches!(
                    check_desired_sql(&sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{declaration}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_view_names_in_main_at_second_declaration() {
        for sql in [
            "CREATE VIEW v AS SELECT 1; CREATE VIEW V AS SELECT 2",
            "CREATE VIEW v AS SELECT 1; CREATE VIEW main.v AS SELECT 2",
            "CREATE VIEW main.v AS SELECT 1; CREATE VIEW \"MAIN\".V AS SELECT 2",
            "CREATE VIEW \"a.b\" AS SELECT 1; CREATE VIEW main.\"A.B\" AS SELECT 2",
        ] {
            assert!(
                matches!(
                    check_desired_sql(sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{sql}"
            );
        }

        assert!(
            check_desired_sql(
                "CREATE VIEW first_view AS SELECT 1; CREATE VIEW second_view AS SELECT 2"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_partial_indexes_as_unsupported_declarations() {
        assert!(matches!(
            check_desired_sql(
                "CREATE TABLE ordinary_table (id); CREATE INDEX partial_idx ON ordinary_table(id) WHERE id > 0"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Declaration,
            })
        ));
        assert!(
            check_desired_sql(
                "CREATE TABLE ordinary_table (id); CREATE INDEX ordinary_idx ON ordinary_table(id)"
            )
            .is_ok()
        );
    }

    #[test]
    fn validates_create_index_database_qualifier() {
        for declaration in [
            "CREATE INDEX idx ON t(id)",
            "CREATE INDEX main.idx ON t(id)",
            "CREATE INDEX MAIN.idx ON t(id)",
            "CREATE INDEX MaIn.idx ON t(id)",
            "CREATE INDEX \"MaIn\".idx ON t(id)",
        ] {
            assert!(check_desired_sql(declaration).is_ok(), "{declaration}");
        }

        for declaration in [
            "CREATE INDEX temp.idx ON t(id)",
            "CREATE INDEX other.idx ON t(id)",
            "CREATE INDEX \"Temp\".idx ON t(id)",
        ] {
            let sql = format!("CREATE TABLE ordinary (id); {declaration}");
            assert!(
                matches!(
                    check_desired_sql(&sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{declaration}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_index_names_in_main_at_second_declaration() {
        for sql in [
            "CREATE INDEX idx ON first_table(id); CREATE INDEX IDX ON second_table(id)",
            "CREATE INDEX idx ON first_table(id); CREATE INDEX IF NOT EXISTS main.idx ON second_table(id)",
            "CREATE INDEX \"a.b\" ON first_table(id); CREATE INDEX main.\"A.B\" ON second_table(id)",
        ] {
            assert!(
                matches!(
                    check_desired_sql(sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{sql}"
            );
        }

        assert!(
        check_desired_sql(
            "CREATE INDEX first_idx ON first_table(id); CREATE INDEX second_idx ON second_table(id)"
        )
        .is_ok()
    );
    }

    #[test]
    fn rejects_index_names_colliding_with_tables_or_views_in_either_order() {
        for sql in [
            "CREATE TABLE Thing (id); CREATE INDEX thing ON other_table(id)",
            "CREATE INDEX Thing ON other_table(id); CREATE TABLE thing (id)",
            "CREATE VIEW Thing AS SELECT 1; CREATE INDEX thing ON other_table(id)",
            "CREATE INDEX Thing ON other_table(id); CREATE VIEW thing AS SELECT 1",
        ] {
            assert!(
                matches!(
                    check_desired_sql(sql),
                    Err(DesiredSchemaError::Unsupported {
                        statement_number: 2,
                        kind: UnsupportedKind::Declaration,
                    })
                ),
                "{sql}"
            );
        }

        assert!(check_desired_sql(
        "CREATE TABLE table_name (id); CREATE VIEW view_name AS SELECT 1; CREATE INDEX index_name ON other_table(id)"
    ).is_ok());
    }

    #[test]
    fn rejects_expression_index_keys_as_unsupported_declarations() {
        assert!(matches!(
            check_desired_sql(
                "CREATE TABLE ordinary_table (id, name); CREATE INDEX expression_idx ON ordinary_table(lower(name))"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Declaration,
            })
        ));
        assert!(matches!(
            check_desired_sql(
                "CREATE TABLE ordinary_table (id); CREATE INDEX expression_idx ON ordinary_table(id + 1)"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Declaration,
            })
        ));
    }

    #[test]
    fn accepts_column_indexes_with_sort_directions_and_quoted_identifiers() {
        assert!(check_desired_sql(
        "CREATE TABLE \"ordinary table\" (\"first col\", second); CREATE INDEX ordinary_idx ON \"ordinary table\"(\"first col\" DESC, second ASC)"
    )
    .is_ok());
    }

    #[test]
    fn reports_the_first_unsupported_statement_number_and_category() {
        assert!(matches!(
            check_desired_sql(
                "CREATE TABLE t (id); CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END; SELECT 1"
            ),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Declaration,
            })
        ));
        assert!(matches!(
            check_desired_sql("CREATE TABLE t (id); DROP TABLE t; SELECT 1"),
            Err(DesiredSchemaError::Unsupported {
                statement_number: 2,
                kind: UnsupportedKind::Executable,
            })
        ));
    }

    #[test]
    fn preserves_parse_failures_as_a_distinct_error() {
        match check_desired_sql("CREATE TABLE t (id); CREATE TABLE") {
            Err(DesiredSchemaError::Parse(error)) => {
                let _ = error;
            }
            _ => panic!("expected a syntax parse failure"),
        }
    }
}

#[cfg(test)]
mod plan_tests {

    use super::super::ddl::{self, Ddl as Operation};
    use super::{
        Column, Index, IndexColumn, SortDirection, SqliteObject, SqliteSchema, Table, View,
        is_reserved_object_name, model::DuplicateDefinition, read_schema,
    };

    // Grouped only for fixture setup. The planner takes validated SqliteSchemas.
    #[derive(Clone, Debug)]
    struct Schema {
        tables: Vec<Table>,
        views: Vec<View>,
        indexes: Vec<Index>,
    }

    impl Schema {
        fn build(&self) -> Result<SqliteSchema, DuplicateDefinition> {
            SqliteSchema::try_new(
                self.tables
                    .iter()
                    .cloned()
                    .map(SqliteObject::Table)
                    .chain(self.views.iter().cloned().map(SqliteObject::View))
                    .chain(self.indexes.iter().cloned().map(SqliteObject::Index))
                    .collect(),
            )
        }
    }

    fn plan_schema(actual: &Schema, desired: &Schema) -> Result<Vec<Operation>, ()> {
        super::plan_schema(
            &actual.build().map_err(|_| ())?,
            &desired.build().map_err(|_| ())?,
        )
        .map_err(|_| ())
    }

    fn empty_schema() -> Schema {
        Schema {
            tables: vec![],
            views: vec![],
            indexes: vec![],
        }
    }

    #[test]
    fn reserved_object_name_prefix_is_ascii_case_insensitive() {
        assert!(is_reserved_object_name("sqlite_sequence"));
        assert!(is_reserved_object_name("SQLITE_sequence"));
        assert!(!is_reserved_object_name("sqlit_sequence"));
        assert!(!is_reserved_object_name("my_sqlite_sequence"));
    }

    fn empty_table(name: &str) -> Table {
        table(name, [])
    }

    fn table(name: &str, columns: impl Into<Vec<Column>>) -> Table {
        Table {
            name: name.into(),
            check: None,
            columns: columns.into(),
        }
    }

    fn with_appended_columns(table: &Table, columns: impl IntoIterator<Item = Column>) -> Table {
        let mut table = table.clone();
        table.columns.extend(columns);
        table
    }

    fn column(name: &str, declared_type: Option<&str>) -> Column {
        Column {
            name: name.into(),
            declared_type: declared_type.map(str::to_owned),
            not_null: false,
            check: None,
            default_null: false,
            default_value: None,
            primary_key: false,
        }
    }

    fn plain_index(name: &str, table: &str, column: &str) -> Index {
        Index {
            name: name.into(),
            table: table.into(),
            columns: vec![key(column)],
            unique: false,
        }
    }

    #[test]
    fn accepts_index_references_with_sqlite_identifier_casing() {
        let actual = Schema {
            tables: vec![table("Items", [column("ItemId", Some("INTEGER"))])],
            ..empty_schema()
        };
        let index = plain_index("items_by_id", "items", "itemid");
        let desired = Schema {
            indexes: vec![index.clone()],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(index)])
        );
    }

    #[test]
    fn plans_multiple_new_views_over_an_unchanged_table_catalog() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let views = vec![
            View {
                name: "item_ids".into(),
                select: "SELECT id FROM items".into(),
            },
            View {
                name: "item_count".into(),
                select: "SELECT count(*) FROM items".into(),
            },
        ];
        let desired = Schema {
            views: views.clone(),
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(views.into_iter().map(Operation::CreateView).collect())
        );
    }

    #[test]
    fn applies_mixed_tables_index_and_fixture_shaped_view_and_rolls_back() {
        let desired = Schema {
            tables: vec![
                table("suppliers", [column("id", Some("INTEGER")), column("name", Some("TEXT"))]),
                table("purchase_orders", [
                    column("id", Some("INTEGER")),
                    column("supplier_id", Some("INTEGER")),
                    column("status", Some("TEXT")),
                ]),
            ],
            indexes: vec![plain_index("orders_by_status", "purchase_orders", "status")],
            views: vec![
                View {
                    name: "open_order_ids".into(),
                    select: "SELECT id FROM open_purchase_orders".into(),
                },
                View {
                    name: "open_purchase_orders".into(),
                    select: "SELECT purchase_orders.id, lower(suppliers.name) AS supplier_name FROM purchase_orders JOIN suppliers ON suppliers.id = purchase_orders.supplier_id WHERE purchase_orders.status = 'open'".into(),
                },
            ],
        };
        let operations = plan_schema(&empty_schema(), &desired).unwrap();
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        for operation in &operations {
            connection.execute_batch(&ddl::to_sql(operation)).unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO suppliers VALUES (1, 'Acme');\
             INSERT INTO purchase_orders VALUES (7, 1, 'open');",
            )
            .unwrap();
        let supplier: String = connection
            .query_row(
                "SELECT supplier_name FROM open_purchase_orders WHERE id = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(supplier, "acme");
        let order_id: i64 = connection
            .query_row("SELECT id FROM open_order_ids", [], |row| row.get(0))
            .unwrap();
        assert_eq!(order_id, 7);
        let introspected = read_schema(&connection).unwrap();
        assert_eq!(
            super::plan_schema(&introspected, &desired.build().unwrap()),
            Ok(vec![]),
            "actual={introspected:#?} desired={desired:#?}"
        );

        for operation in operations.into_iter().rev() {
            connection
                .execute_batch(&ddl::to_sql(&operation.inverse()))
                .unwrap();
        }
        assert_eq!(
            read_schema(&connection).unwrap(),
            empty_schema().build().unwrap()
        );
    }

    #[test]
    fn rejects_views_with_missing_sources_columns_or_unsupported_from_shapes() {
        let actual = Schema {
            tables: vec![table("purchase_orders", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        for select in [
            "SELECT id FROM missing_table",
            "SELECT missing_column FROM purchase_orders",
            "SELECT id FROM (SELECT id FROM purchase_orders)",
        ] {
            let desired = Schema {
                views: vec![View {
                    name: "bad_view".into(),
                    select: select.into(),
                }],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()), "accepted {select}");
        }
    }

    #[test]
    fn rejects_unsupported_table_change_mixed_with_supported_additions() {
        let actual = Schema {
            tables: vec![Table {
                check: Some("id > 0".into()),
                ..table("items", [column("id", Some("INTEGER"))])
            }],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                Table {
                    check: Some("id >= 0".into()),
                    columns: vec![column("id", Some("INTEGER")), column("note", Some("TEXT"))],
                    ..actual.tables[0].clone()
                },
                table("other", [column("id", Some("INTEGER"))]),
            ],
            indexes: vec![plain_index("other_by_id", "other", "id")],
            ..actual.clone()
        };
        assert_eq!(plan_schema(&actual, &desired), Err(()));
    }

    #[test]
    fn blocks_changes_to_existing_primary_key_default_or_check() {
        let original = super::lower_schema_with_sources(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, status TEXT DEFAULT 'draft' CHECK (status = 'draft'))",
        )
        .unwrap()
        .0;
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        for operation in super::plan_schema(&empty_schema().build().unwrap(), &original).unwrap() {
            connection.execute_batch(&ddl::to_sql(&operation)).unwrap();
        }
        assert_eq!(read_schema(&connection).unwrap(), original);

        for (sql, expected) in [
            (
                "CREATE TABLE items (id INTEGER, status TEXT DEFAULT 'draft' CHECK (status = 'draft'))",
                "changing the PRIMARY KEY constraint of column items.id is not supported",
            ),
            (
                "CREATE TABLE items (id INTEGER PRIMARY KEY, status TEXT DEFAULT 'open' CHECK (status = 'draft'))",
                "changing the default value of column items.status is not supported",
            ),
            (
                "CREATE TABLE items (id INTEGER PRIMARY KEY, status TEXT DEFAULT 'draft' CHECK (status = 'open'))",
                "changing the CHECK constraint of column items.status is not supported",
            ),
        ] {
            let desired = super::lower_schema_with_sources(sql).unwrap().0;
            assert_eq!(
                super::plan_schema(&original, &desired).unwrap_err(),
                expected,
                "{sql}"
            );
        }
    }

    #[test]
    fn composes_incremental_append_index_and_view_with_unchanged_objects() {
        let actual = Schema {
            tables: vec![table(
                "purchase_orders",
                [
                    column("id", Some("INTEGER")),
                    column("status", Some("TEXT")),
                ],
            )],
            indexes: vec![plain_index("orders_by_status", "purchase_orders", "status")],
            views: vec![View {
                name: "all_orders".into(),
                select: "SELECT id FROM purchase_orders".into(),
            }],
        };
        let desired = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [column("customer", Some("TEXT"))],
            )],
            indexes: vec![
                actual.indexes[0].clone(),
                plain_index("orders_by_customer", "purchase_orders", "customer"),
            ],
            views: vec![
                actual.views[0].clone(),
                View {
                    name: "customer_orders".into(),
                    select: "SELECT id, customer FROM purchase_orders WHERE status = 'open'".into(),
                },
            ],
        };
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        for operation in plan_schema(&empty_schema(), &actual).unwrap() {
            connection.execute_batch(&ddl::to_sql(&operation)).unwrap();
        }
        let prior_schema = read_schema(&connection).unwrap();
        let operations = plan_schema(&actual, &desired).unwrap();
        assert!(matches!(
            operations.first(),
            Some(Operation::AddColumn { .. })
        ));
        assert!(matches!(operations.last(), Some(Operation::CreateView(_))));
        for operation in &operations {
            connection.execute_batch(&ddl::to_sql(operation)).unwrap();
        }
        assert_eq!(
            super::plan_schema(
                &read_schema(&connection).unwrap(),
                &desired.build().unwrap()
            ),
            Ok(vec![])
        );
        for operation in operations.into_iter().rev() {
            connection
                .execute_batch(&ddl::to_sql(&operation.inverse()))
                .unwrap();
        }
        assert_eq!(read_schema(&connection).unwrap(), prior_schema);
    }

    #[test]
    fn treats_declared_type_casing_as_equivalent() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![table("items", [column("id", Some("integer"))])],
            ..empty_schema()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn treats_index_name_casing_as_equivalent() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            indexes: vec![plain_index("items_by_id", "items", "id")],
            ..empty_schema()
        };
        let desired = Schema {
            indexes: vec![plain_index("ITEMS_BY_ID", "items", "id")],
            ..actual.clone()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn treats_explicit_null_default_as_equivalent_to_no_default() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![Table {
                columns: vec![Column {
                    default_null: true,
                    default_value: None,
                    primary_key: false,
                    ..actual.tables[0].columns[0].clone()
                }],
                ..actual.tables[0].clone()
            }],
            ..empty_schema()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn plans_ordinary_indexes_with_descending_column_order() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let index = Index {
            name: "items_by_id".into(),
            table: "items".into(),
            columns: vec![IndexColumn {
                name: "id".into(),
                order: Some(SortDirection::Desc),
                collation: None,
            }],
            unique: false,
        };
        let desired = Schema {
            indexes: vec![index.clone()],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(index)])
        );
    }

    fn key(name: impl Into<String>) -> IndexColumn {
        IndexColumn {
            name: name.into(),
            order: None,
            collation: None,
        }
    }

    #[test]
    fn plans_one_table_from_an_empty_schema_without_losing_model_data() {
        let desired = Schema {
            tables: vec![table(
                "items",
                [Column {
                    name: "kind".into(),
                    declared_type: Some("TEXT".into()),
                    not_null: true,
                    check: Some("kind IN ('a', 'b')".into()),
                    default_null: true,
                    default_value: None,
                    primary_key: false,
                }],
            )],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![Operation::CreateTable(desired.tables[0].clone())])
        );
    }

    #[test]
    fn empty_actual_and_desired_schema_has_no_operations() {
        assert_eq!(plan_schema(&empty_schema(), &empty_schema()), Ok(vec![]));
    }

    #[test]
    fn creates_only_supported_standalone_views_from_an_empty_schema() {
        for select in [
            "SELECT 1",
            "SELECT 2",
            "SELECT lower('ABC')",
            "SELECT upper('abc')",
            "SELECT length('abc')",
        ] {
            let view = View {
                name: "v".into(),
                select: select.into(),
            };
            let desired = Schema {
                views: vec![view.clone()],
                ..empty_schema()
            };
            assert_eq!(
                plan_schema(&empty_schema(), &desired),
                Ok(vec![Operation::CreateView(view)])
            );
        }

        for (name, select) in [
            ("SQLite_reserved", "SELECT 1"),
            ("v", "SELECT 1 + 2"),
            ("v", "SELECT upper('ABC') || '!'"),
            ("v", "SELECT length('ABC') + 1"),
        ] {
            let desired = Schema {
                views: vec![View {
                    name: name.into(),
                    select: select.into(),
                }],
                ..empty_schema()
            };
            assert_eq!(plan_schema(&empty_schema(), &desired), Err(()));
        }
    }

    #[test]
    fn plans_multiple_supported_standalone_views_from_an_empty_schema() {
        let desired = Schema {
            views: vec![
                View {
                    name: "first".into(),
                    select: "SELECT 1;".into(),
                },
                View {
                    name: "second".into(),
                    select: "SELECT lower('ABC');".into(),
                },
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateView(desired.views[0].clone()),
                Operation::CreateView(desired.views[1].clone()),
            ])
        );
    }

    #[test]
    fn plans_standalone_views_after_new_tables() {
        let desired = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            views: vec![View {
                name: "item_count".into(),
                select: "SELECT 1".into(),
            }],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::CreateView(desired.views[0].clone()),
            ])
        );
    }

    #[test]
    fn plans_table_dependent_view_with_new_table() {
        let desired = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            views: vec![View {
                name: "item_ids".into(),
                select: "SELECT id FROM items".into(),
            }],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::CreateView(desired.views[0].clone()),
            ])
        );
    }

    #[test]
    fn creates_one_view_over_an_unchanged_existing_table() {
        let table = table("items", [column("name", Some("TEXT"))]);
        let actual = Schema {
            tables: vec![table],
            ..empty_schema()
        };
        let view = View {
            name: "item_names".into(),
            select: "SELECT name FROM items".into(),
        };
        let desired = Schema {
            views: vec![view.clone()],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateView(view)])
        );

        let missing_column = Schema {
            views: vec![View {
                select: "SELECT missing FROM items".into(),
                ..desired.views[0].clone()
            }],
            ..desired
        };
        assert_eq!(plan_schema(&actual, &missing_column), Err(()));
    }

    #[test]
    fn rejects_view_names_colliding_with_existing_tables_or_indexes() {
        let table = table("items", [column("id", Some("INTEGER"))]);
        let index = plain_index("items_by_id", "items", "id");
        let actual = Schema {
            tables: vec![table],
            indexes: vec![index],
            ..empty_schema()
        };

        for name in ["ITEMS", "ITEMS_BY_ID"] {
            let desired = Schema {
                views: vec![View {
                    name: name.into(),
                    select: "SELECT 1".into(),
                }],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }
    }

    #[test]
    fn plans_multiple_tables_in_name_order_independent_of_declaration_order() {
        let desired = Schema {
            tables: vec![
                empty_table("zebra"),
                empty_table("alpha"),
                empty_table("middle"),
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[1].clone()),
                Operation::CreateTable(desired.tables[2].clone()),
                Operation::CreateTable(desired.tables[0].clone()),
            ])
        );
    }

    #[test]
    fn identical_nonempty_catalog_is_noop_independent_of_table_order() {
        let actual = Schema {
            tables: vec![
                table(
                    "items",
                    [Column {
                        name: "id".into(),
                        declared_type: Some("INTEGER".into()),
                        not_null: true,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    }],
                ),
                empty_table("tags"),
            ],
            ..empty_schema()
        };
        let desired = Schema {
            tables: actual.tables.iter().rev().cloned().collect(),
            ..empty_schema()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn plans_only_new_tables_in_name_order_against_nonempty_catalog() {
        let actual = Schema {
            tables: vec![empty_table("base")],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                empty_table("zebra"),
                actual.tables[0].clone(),
                empty_table("alpha"),
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[2].clone()),
                Operation::CreateTable(desired.tables[0].clone()),
            ])
        );
    }

    #[test]
    fn plans_one_new_table_beside_one_unchanged_view() {
        let existing = table("base", [column("id", Some("INTEGER"))]);
        let view = View {
            name: "base_view".into(),
            select: "SELECT * FROM base".into(),
        };
        let actual = Schema {
            tables: vec![existing.clone()],
            views: vec![view.clone()],
            indexes: vec![],
        };
        let new_table = table("extra", [column("id", Some("INTEGER"))]);
        let desired = Schema {
            tables: vec![existing, new_table.clone()],
            views: vec![view],
            indexes: vec![],
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateTable(new_table)])
        );
    }

    #[test]
    fn plans_new_tables_before_one_appended_column() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                empty_table("zebra"),
                with_appended_columns(&actual.tables[0], [column("note", Some("TEXT"))]),
                empty_table("alpha"),
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[2].clone()),
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::AddColumn {
                    table: "items".into(),
                    column: desired.tables[1].columns[1].clone(),
                },
            ])
        );
    }

    #[test]
    fn rejects_non_additive_changes_against_nonempty_catalog() {
        let actual_table = empty_table("base");
        let actual = Schema {
            tables: vec![actual_table.clone()],
            ..empty_schema()
        };
        let changed = Table {
            columns: vec![Column {
                name: "id".into(),
                declared_type: None,
                not_null: true,
                check: None,
                default_null: false,
                default_value: None,
                primary_key: false,
            }],
            ..actual_table.clone()
        };

        for desired in [
            empty_schema(),
            Schema {
                tables: vec![changed],
                ..empty_schema()
            },
        ] {
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }
    }

    #[test]
    fn matches_table_names_ascii_case_insensitively_and_renders_catalog_name() {
        let actual = Schema {
            tables: vec![table("Items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let same = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        assert_eq!(plan_schema(&actual, &same), Ok(vec![]));

        let desired = Schema {
            tables: vec![table(
                "ITEMS",
                [column("id", Some("INTEGER")), column("note", Some("TEXT"))],
            )],
            ..empty_schema()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "Items".into(),
                column: column("note", Some("TEXT")),
            }])
        );
    }

    #[test]
    fn matches_existing_column_names_ascii_case_insensitively() {
        let actual = Schema {
            tables: vec![table(
                "items",
                [column("ID", Some("INTEGER")), column("Label", Some("TEXT"))],
            )],
            ..empty_schema()
        };
        let same = Schema {
            tables: vec![table(
                "items",
                [column("id", Some("INTEGER")), column("label", Some("TEXT"))],
            )],
            ..empty_schema()
        };
        assert_eq!(plan_schema(&actual, &same), Ok(vec![]));

        let desired = Schema {
            tables: vec![table(
                "items",
                [
                    column("id", Some("INTEGER")),
                    column("label", Some("TEXT")),
                    column("Note", Some("TEXT")),
                ],
            )],
            ..empty_schema()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: column("Note", Some("TEXT")),
            }])
        );
    }

    #[test]
    fn plans_one_plain_nullable_appended_column_only() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [column("note", Some("TEXT"))],
            )],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: desired.tables[0].columns[1].clone(),
            }])
        );
    }

    #[test]
    fn plans_nullable_column_addition_with_an_unchanged_ordinary_index() {
        let mut actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        actual
            .indexes
            .push(plain_index("items_by_id", "items", "id"));
        let desired = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [column("note", Some("TEXT"))],
            )],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: desired.tables[0].columns[1].clone(),
            }])
        );

        let mut with_new_table = desired.clone();
        with_new_table.tables.push(empty_table("items_by_id"));
        assert_eq!(plan_schema(&actual, &with_new_table), Err(()));

        let mut with_second_appended_column = desired.clone();
        with_second_appended_column.tables[0]
            .columns
            .push(column("tag", Some("TEXT")));
        assert_eq!(
            plan_schema(&actual, &with_second_appended_column),
            Ok(vec![
                Operation::AddColumn {
                    table: "items".into(),
                    column: column("note", Some("TEXT")),
                },
                Operation::AddColumn {
                    table: "items".into(),
                    column: column("tag", Some("TEXT")),
                },
            ])
        );

        let mut actual_with_unindexed_table = actual.clone();
        actual_with_unindexed_table
            .tables
            .push(table("other", [column("id", Some("INTEGER"))]));
        let mut desired_with_unindexed_change = desired.clone();
        desired_with_unindexed_change.tables.push(Table {
            name: "other".into(),
            check: None,
            columns: vec![
                actual_with_unindexed_table.tables[1].columns[0].clone(),
                column("note", Some("TEXT")),
            ],
        });
        assert_eq!(
            plan_schema(&actual_with_unindexed_table, &desired_with_unindexed_change),
            Ok(vec![
                Operation::AddColumn {
                    table: "items".into(),
                    column: column("note", Some("TEXT")),
                },
                Operation::AddColumn {
                    table: "other".into(),
                    column: column("note", Some("TEXT")),
                },
            ])
        );
    }

    #[test]
    fn plans_nullable_column_on_unindexed_table_with_unrelated_unchanged_index() {
        let index = plain_index("indexed_by_id", "indexed", "id");
        let actual = Schema {
            tables: vec![
                table("unindexed", [column("id", Some("INTEGER"))]),
                table("indexed", [column("id", Some("INTEGER"))]),
            ],
            indexes: vec![index.clone()],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                Table {
                    columns: vec![column("id", Some("INTEGER")), column("note", Some("TEXT"))],
                    ..actual.tables[0].clone()
                },
                actual.tables[1].clone(),
            ],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "unindexed".into(),
                column: column("note", Some("TEXT")),
            }])
        );

        let changed_index = Schema {
            indexes: vec![Index {
                name: "different_index".into(),
                ..index
            }],
            ..desired.clone()
        };
        assert_eq!(plan_schema(&actual, &changed_index), Err(()));
    }

    #[test]
    fn plans_multiple_plain_nullable_columns_in_declaration_order() {
        let actual = Schema {
            tables: vec![table("items", [column("id", Some("INTEGER"))])],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [column("note", Some("TEXT")), column("tag", Some("TEXT"))],
            )],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::AddColumn {
                    table: "items".into(),
                    column: desired.tables[0].columns[1].clone(),
                },
                Operation::AddColumn {
                    table: "items".into(),
                    column: desired.tables[0].columns[2].clone(),
                },
            ])
        );
    }

    #[test]
    fn plans_additions_across_tables_in_name_order_after_new_tables() {
        let column = |name: &str| column(name, Some("TEXT"));
        let actual = Schema {
            tables: vec![
                table("zebra", [column("id")]),
                table("alpha", [column("id")]),
            ],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                table("zebra", [column("id"), column("z1"), column("z2")]),
                empty_table("new_table"),
                table("alpha", [column("id"), column("a1"), column("a2")]),
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[1].clone()),
                Operation::AddColumn {
                    table: "alpha".into(),
                    column: column("a1")
                },
                Operation::AddColumn {
                    table: "alpha".into(),
                    column: column("a2")
                },
                Operation::AddColumn {
                    table: "zebra".into(),
                    column: column("z1")
                },
                Operation::AddColumn {
                    table: "zebra".into(),
                    column: column("z2")
                },
            ])
        );
    }

    #[test]
    fn rejects_all_changes_when_any_table_addition_is_unsupported() {
        let existing = |name: &str| table(name, [column("id", None)]);
        let actual = Schema {
            tables: vec![existing("alpha"), existing("zebra")],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![
                Table {
                    columns: vec![
                        existing("zebra").columns[0].clone(),
                        Column {
                            not_null: true,
                            ..column("required", None)
                        },
                    ],
                    ..existing("zebra")
                },
                Table {
                    columns: vec![existing("alpha").columns[0].clone(), column("note", None)],
                    ..existing("alpha")
                },
            ],
            ..empty_schema()
        };

        assert_eq!(plan_schema(&actual, &desired), Err(()));
    }

    #[test]
    fn rejects_reordered_or_constrained_column_additions() {
        let actual = Schema {
            tables: vec![table(
                "items",
                [column("id", Some("INTEGER")), column("name", Some("TEXT"))],
            )],
            ..empty_schema()
        };
        let appended = column("note", Some("TEXT"));
        let reordered = Schema {
            tables: vec![Table {
                columns: vec![
                    actual.tables[0].columns[1].clone(),
                    actual.tables[0].columns[0].clone(),
                    appended.clone(),
                ],
                ..actual.tables[0].clone()
            }],
            ..empty_schema()
        };
        assert_eq!(plan_schema(&actual, &reordered), Err(()));

        let explicit_null = Column {
            default_null: true,
            default_value: None,
            primary_key: false,
            ..appended.clone()
        };
        let desired = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [explicit_null.clone()],
            )],
            ..empty_schema()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: explicit_null,
            }])
        );

        for column in [
            Column {
                not_null: true,
                ..appended.clone()
            },
            Column {
                check: Some("length(note) > 0".into()),
                ..appended.clone()
            },
        ] {
            let desired = Schema {
                tables: vec![with_appended_columns(&actual.tables[0], [column.clone()])],
                ..empty_schema()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }

        let unsupported_suffix = Schema {
            tables: vec![with_appended_columns(
                &actual.tables[0],
                [
                    appended,
                    Column {
                        name: "required".into(),
                        declared_type: Some("TEXT".into()),
                        not_null: true,
                        check: None,
                        default_null: false,
                        default_value: None,
                        primary_key: false,
                    },
                ],
            )],
            ..empty_schema()
        };
        assert_eq!(plan_schema(&actual, &unsupported_suffix), Err(()));
    }

    #[test]
    fn rejects_inputs_outside_the_supported_initial_subset() {
        let table = empty_table("items");
        let cases = [
            Schema {
                tables: vec![table.clone()],
                ..empty_schema()
            },
            Schema {
                views: vec![View {
                    name: "v".into(),
                    select: "SELECT 1".into(),
                }],
                ..empty_schema()
            },
            Schema {
                indexes: vec![Index {
                    name: "i".into(),
                    table: "items".into(),
                    unique: false,
                    columns: vec![],
                }],
                ..empty_schema()
            },
        ];

        for actual in &cases {
            assert_eq!(plan_schema(actual, &empty_schema()), Err(()));
        }

        for desired in [
            Schema {
                tables: vec![table.clone(), table.clone()],
                ..empty_schema()
            },
            Schema {
                tables: vec![table.clone()],
                indexes: vec![Index {
                    name: "i".into(),
                    table: "items".into(),
                    unique: false,
                    columns: vec![],
                }],
                ..empty_schema()
            },
        ] {
            assert_eq!(plan_schema(&empty_schema(), &desired), Err(()));
        }

        for names in [["items", "ITEMS"], ["items", "Items"]] {
            let desired = Schema {
                tables: names.into_iter().map(empty_table).collect(),
                ..empty_schema()
            };
            assert_eq!(plan_schema(&empty_schema(), &desired), Err(()));
        }
    }

    #[test]
    fn identical_multiple_views_on_unchanged_schema_are_noop_regardless_of_order() {
        let views = [
            View {
                name: "item_names".into(),
                select: "SELECT name FROM items".into(),
            },
            View {
                name: "item_kinds".into(),
                select: "SELECT name FROM items".into(),
            },
        ];
        let actual = Schema {
            tables: vec![table("items", [column("name", Some("TEXT"))])],
            views: views.to_vec(),
            ..empty_schema()
        };
        let desired = Schema {
            views: views.into_iter().rev().collect(),
            ..actual.clone()
        };
        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn matches_view_names_ascii_case_insensitively() {
        let actual = Schema {
            tables: vec![table("items", [column("name", Some("TEXT"))])],
            views: vec![View {
                name: "Item_Names".into(),
                select: "SELECT name FROM items".into(),
            }],
            ..empty_schema()
        };
        let desired = Schema {
            views: vec![View {
                name: "item_names".into(),
                select: "SELECT name FROM items".into(),
            }],
            ..actual.clone()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
    }

    #[test]
    fn plans_single_nullable_column_with_unchanged_views_and_no_indexes() {
        let actual = Schema {
            tables: vec![table(
                "items",
                [column("name", Some("TEXT")), column("kind", Some("TEXT"))],
            )],
            views: vec![View {
                name: "item_names".into(),
                select: "SELECT name FROM items".into(),
            }],
            ..empty_schema()
        };
        let column = column("note", Some("TEXT"));
        let desired = Schema {
            tables: vec![with_appended_columns(&actual.tables[0], [column.clone()])],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: column.clone(),
            }])
        );

        for invalid in [
            Schema {
                views: vec![],
                ..desired.clone()
            },
            Schema {
                views: vec![View {
                    select: "SELECT 1".into(),
                    ..actual.views[0].clone()
                }],
                ..desired.clone()
            },
            Schema {
                tables: vec![with_appended_columns(
                    &actual.tables[0],
                    [Column {
                        not_null: true,
                        ..column.clone()
                    }],
                )],
                ..actual.clone()
            },
        ] {
            assert_eq!(plan_schema(&actual, &invalid), Err(()));
        }
    }

    #[test]
    fn plans_one_nullable_column_with_unchanged_views_and_supported_indexes() {
        let column = |name: &str| column(name, Some("TEXT"));
        let index = plain_index("items_by_name", "items", "name");
        let actual = Schema {
            tables: vec![table("items", [column("name")])],
            views: vec![View {
                name: "item_names".into(),
                select: "SELECT name FROM items".into(),
            }],
            indexes: vec![index.clone()],
        };
        let desired = Schema {
            tables: vec![Table {
                columns: vec![column("name"), column("note")],
                ..actual.tables[0].clone()
            }],
            // Reordered catalog collections remain equivalent under planner conventions.
            ..actual.clone()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::AddColumn {
                table: "items".into(),
                column: column("note"),
            }])
        );

        let mut invalid = desired.clone();
        invalid.indexes[0].name = "renamed_index".into();
        assert_eq!(plan_schema(&actual, &invalid), Err(()));

        let mut invalid = desired.clone();
        invalid.indexes[0].unique = true;
        assert_eq!(plan_schema(&actual, &invalid), Err(()));

        let mut invalid = desired;
        invalid.indexes.push(index);
        assert_eq!(plan_schema(&actual, &invalid), Err(()));
    }

    #[test]
    fn plans_one_ordinary_index_with_unchanged_views_and_tables() {
        let table = table("items", [column("name", Some("TEXT"))]);
        let view = View {
            name: "item_names".into(),
            select: "SELECT name FROM items".into(),
        };
        let index = plain_index("items_by_name", "items", "name");
        let actual = Schema {
            tables: vec![table.clone()],
            views: vec![view.clone()],
            indexes: vec![],
        };
        let desired = Schema {
            indexes: vec![index.clone()],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(index.clone())])
        );

        for invalid_index in [
            Index {
                table: "missing".into(),
                ..index.clone()
            },
            Index {
                columns: vec![],
                ..index.clone()
            },
            Index {
                unique: true,
                ..index.clone()
            },
            Index {
                name: table.name.clone(),
                ..index.clone()
            },
            Index {
                name: "ITEM_NAMES".into(),
                ..index.clone()
            },
        ] {
            let invalid = Schema {
                indexes: vec![invalid_index],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &invalid), Err(()));
        }

        let with_extra_index = Schema {
            indexes: vec![
                index.clone(),
                Index {
                    name: "second".into(),
                    ..index.clone()
                },
            ],
            ..actual.clone()
        };
        assert_eq!(
            plan_schema(&actual, &with_extra_index),
            Ok(vec![
                Operation::CreateIndex(index.clone()),
                Operation::CreateIndex(with_extra_index.indexes[1].clone()),
            ])
        );
        let changed_view = Schema {
            views: vec![View {
                select: "SELECT 1".into(),
                ..view
            }],
            indexes: vec![index],
            ..actual.clone()
        };
        assert_eq!(plan_schema(&actual, &changed_view), Err(()));
        let changed_table = Schema {
            tables: vec![Table {
                name: "other".into(),
                ..table
            }],
            indexes: desired.indexes,
            ..actual.clone()
        };
        assert_eq!(plan_schema(&actual, &changed_table), Err(()));
    }

    #[test]
    fn plans_one_new_index_beside_unchanged_view_and_supported_index() {
        let table = table(
            "items",
            [column("name", Some("TEXT")), column("kind", Some("TEXT"))],
        );
        let view = View {
            name: "item_names".into(),
            select: "SELECT name FROM items".into(),
        };
        let index = |name: &str, column: &str| plain_index(name, "items", column);
        let existing = index("items_by_name", "name");
        let added = index("items_by_kind", "kind");
        let actual = Schema {
            tables: vec![table],
            views: vec![view],
            indexes: vec![existing.clone()],
        };
        let desired = Schema {
            indexes: vec![existing.clone(), added.clone()],
            ..actual.clone()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(added.clone())])
        );

        for invalid in [
            Schema {
                indexes: vec![added.clone()],
                ..actual.clone()
            },
            Schema {
                indexes: vec![index("replacement", "kind")],
                ..actual.clone()
            },
            Schema {
                indexes: vec![existing.clone(), index("ITEMS_BY_NAME", "kind")],
                ..actual.clone()
            },
            Schema {
                indexes: vec![
                    existing.clone(),
                    Index {
                        unique: true,
                        ..added.clone()
                    },
                ],
                ..actual.clone()
            },
            Schema {
                indexes: vec![
                    existing.clone(),
                    Index {
                        columns: vec![],
                        ..added.clone()
                    },
                ],
                ..actual.clone()
            },
            Schema {
                indexes: vec![
                    existing.clone(),
                    Index {
                        name: "ITEMS".into(),
                        ..added.clone()
                    },
                ],
                ..actual.clone()
            },
            Schema {
                indexes: vec![
                    existing.clone(),
                    Index {
                        name: "ITEM_NAMES".into(),
                        ..added.clone()
                    },
                ],
                ..actual.clone()
            },
        ] {
            assert_eq!(plan_schema(&actual, &invalid), Err(()));
        }
    }

    #[test]
    fn rejects_view_changes_multiple_views_and_schema_changes_with_views() {
        let view = View {
            name: "item_names".into(),
            select: "SELECT name FROM items".into(),
        };
        let actual = Schema {
            views: vec![view.clone()],
            ..empty_schema()
        };
        let cases = [
            empty_schema(),
            Schema {
                views: vec![View {
                    select: "SELECT 1".into(),
                    ..view.clone()
                }],
                ..empty_schema()
            },
            Schema {
                views: vec![view.clone(), view.clone()],
                ..empty_schema()
            },
            Schema {
                views: vec![
                    view.clone(),
                    View {
                        name: "ITEM_NAMES".into(),
                        select: "SELECT 2".into(),
                    },
                ],
                ..empty_schema()
            },
            Schema {
                views: vec![
                    view.clone(),
                    View {
                        name: "another".into(),
                        select: "SELECT 1".into(),
                    },
                ],
                ..empty_schema()
            },
            Schema {
                tables: vec![empty_table("items")],
                views: vec![view.clone()],
                ..empty_schema()
            },
            Schema {
                indexes: vec![Index {
                    name: "i".into(),
                    table: "items".into(),
                    unique: false,
                    columns: vec![],
                }],
                views: vec![view.clone()],
                ..empty_schema()
            },
        ];
        for desired in cases {
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }
    }

    #[test]
    fn plans_only_a_single_plain_index_on_an_unchanged_existing_table() {
        let table = table("items", [column("kind", Some("TEXT"))]);
        let actual = Schema {
            tables: vec![table.clone()],
            ..empty_schema()
        };
        let index = plain_index("items_kind", "items", "kind");
        let desired = Schema {
            indexes: vec![index.clone()],
            ..actual.clone()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(index.clone())])
        );
        assert_eq!(plan_schema(&desired, &desired), Ok(vec![]));

        for invalid in [
            Index {
                unique: true,
                ..index.clone()
            },
            Index {
                columns: vec![IndexColumn {
                    collation: Some("NOCASE".into()),
                    ..index.columns[0].clone()
                }],
                ..index.clone()
            },
            Index {
                table: "missing".into(),
                ..index.clone()
            },
            Index {
                columns: vec![IndexColumn {
                    name: "missing".into(),
                    ..index.columns[0].clone()
                }],
                ..index.clone()
            },
        ] {
            let desired = Schema {
                indexes: vec![invalid],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }
        assert_eq!(
            plan_schema(
                &actual,
                &Schema {
                    tables: vec![],
                    ..desired
                }
            ),
            Err(())
        );
    }

    #[test]
    fn plans_one_new_table_with_unchanged_supported_indexes_only() {
        let existing = table("items", [column("kind", Some("TEXT"))]);
        let index = plain_index("items_by_kind", "items", "kind");
        let new_table = table("notes", [column("body", Some("TEXT"))]);
        let actual = Schema {
            tables: vec![existing.clone()],
            indexes: vec![index.clone()],
            ..empty_schema()
        };
        let desired = Schema {
            tables: vec![existing, new_table.clone()],
            indexes: vec![index],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateTable(new_table)])
        );

        let mut collision = desired.clone();
        collision.tables[1].name = "ITEMS_BY_KIND".into();
        assert_eq!(plan_schema(&actual, &collision), Err(()));

        let mut duplicate_table = desired.clone();
        duplicate_table.tables[1].name = "ITEMS".into();
        assert_eq!(plan_schema(&actual, &duplicate_table), Err(()));

        let mut multiple = desired.clone();
        multiple.tables.push(Table {
            name: "aardvark".into(),
            ..multiple.tables[1].clone()
        });
        assert_eq!(
            plan_schema(&actual, &multiple),
            Ok(vec![
                Operation::CreateTable(multiple.tables[2].clone()),
                Operation::CreateTable(multiple.tables[1].clone()),
            ])
        );

        let mut later_collision = multiple.clone();
        later_collision.tables[2].name = "ITEMS_BY_KIND".into();
        assert_eq!(plan_schema(&actual, &later_collision), Err(()));

        let mut changed_table = desired;
        changed_table.tables[0].columns[0].name = "changed".into();
        assert_eq!(plan_schema(&actual, &changed_table), Err(()));
    }

    #[test]
    fn rejects_duplicate_names_when_adding_indexes_to_unchanged_existing_table() {
        let table = Table {
            name: "items".into(),
            check: None,
            columns: ["kind", "label"]
                .into_iter()
                .map(|name| column(name, Some("TEXT")))
                .collect(),
        };
        let actual = Schema {
            tables: vec![table],
            ..empty_schema()
        };
        let index = |name: &str, column: &str| plain_index(name, "items", column);

        for duplicate_name in ["same", "SAME"] {
            let desired = Schema {
                indexes: vec![index("same", "kind"), index(duplicate_name, "label")],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()));
        }

        let desired = Schema {
            indexes: vec![index("z_label", "label"), index("a_kind", "kind")],
            ..actual.clone()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateIndex(index("a_kind", "kind")),
                Operation::CreateIndex(index("z_label", "label")),
            ])
        );
    }

    #[test]
    fn rejects_new_index_names_colliding_with_desired_tables_ascii_case_insensitively() {
        let table = table("Users", [column("id", Some("INTEGER"))]);
        let index = |name: &str| plain_index(name, "Users", "id");
        let actual = Schema {
            tables: vec![table.clone()],
            ..empty_schema()
        };

        for name in ["Users", "users", "USERS"] {
            let desired = Schema {
                indexes: vec![index(name)],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&actual, &desired), Err(()));

            let initial = Schema {
                indexes: vec![index(name)],
                ..actual.clone()
            };
            assert_eq!(plan_schema(&empty_schema(), &initial), Err(()));
        }

        let desired = Schema {
            indexes: vec![index("users_by_id")],
            ..actual.clone()
        };
        assert!(plan_schema(&actual, &desired).is_ok());
    }

    #[test]
    fn matching_multiple_indexes_are_noop_independent_of_index_order() {
        let table = Table {
            name: "items".into(),
            check: None,
            columns: ["kind", "label"]
                .into_iter()
                .map(|name| column(name, Some("TEXT")))
                .collect(),
        };
        let index = |name: &str, column: &str| plain_index(name, "items", column);
        let indexes = vec![index("items_kind", "kind"), index("items_label", "label")];
        let actual = Schema {
            tables: vec![table],
            indexes: indexes.clone(),
            ..empty_schema()
        };
        let desired = Schema {
            indexes: indexes.into_iter().rev().collect(),
            ..actual.clone()
        };

        assert_eq!(plan_schema(&actual, &desired), Ok(vec![]));
        let additions = vec![
            index("z_items_label", "label"),
            index("a_items_kind", "kind"),
        ];
        let desired_with_additions = Schema {
            indexes: [actual.indexes.clone(), additions.clone()].concat(),
            ..actual.clone()
        };
        let mut expected = additions;
        expected.sort_by(|left, right| (&left.table, &left.name).cmp(&(&right.table, &right.name)));
        assert_eq!(
            plan_schema(&actual, &desired_with_additions),
            Ok(expected.into_iter().map(Operation::CreateIndex).collect())
        );

        for indexes in [
            vec![index("items_kind", "kind")],
            vec![index("items_kind", "label"), index("items_label", "label")],
            vec![
                index("items_kind", "kind"),
                index("items_kind", "label"),
                index("items_label", "label"),
            ],
        ] {
            assert_eq!(
                plan_schema(
                    &actual,
                    &Schema {
                        indexes,
                        ..actual.clone()
                    }
                ),
                Err(())
            );
        }
    }

    #[test]
    fn plans_plain_multi_column_indexes_and_rejects_missing_keys() {
        let table = Table {
            name: "items".into(),
            check: None,
            columns: ["kind", "label"]
                .into_iter()
                .map(|name| column(name, Some("TEXT")))
                .collect(),
        };
        let index = Index {
            name: "items_kind_label".into(),
            table: "items".into(),
            unique: false,
            columns: ["kind", "label"].into_iter().map(key).collect(),
        };
        let actual = Schema {
            tables: vec![table],
            ..empty_schema()
        };
        let desired = Schema {
            indexes: vec![index.clone()],
            ..actual.clone()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![Operation::CreateIndex(index.clone())])
        );
        assert_eq!(plan_schema(&desired, &desired), Ok(vec![]));

        let invalid = Index {
            columns: vec![
                index.columns[0].clone(),
                IndexColumn {
                    name: "missing".into(),
                    ..index.columns[1].clone()
                },
            ],
            ..index
        };
        assert_eq!(
            plan_schema(
                &actual,
                &Schema {
                    indexes: vec![invalid],
                    ..actual.clone()
                }
            ),
            Err(())
        );
    }

    #[test]
    fn plans_one_new_table_before_its_single_plain_index_from_empty_catalog() {
        let desired = Schema {
            tables: vec![table("items", [column("kind", Some("TEXT"))])],
            indexes: vec![plain_index("items_kind", "items", "kind")],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::CreateIndex(desired.indexes[0].clone()),
            ])
        );

        for index in [
            Index {
                table: "missing".into(),
                ..desired.indexes[0].clone()
            },
            Index {
                columns: vec![IndexColumn {
                    name: "missing".into(),
                    ..desired.indexes[0].columns[0].clone()
                }],
                ..desired.indexes[0].clone()
            },
        ] {
            assert_eq!(
                plan_schema(
                    &empty_schema(),
                    &Schema {
                        indexes: vec![index],
                        ..desired.clone()
                    }
                ),
                Err(())
            );
        }
    }

    #[test]
    fn plans_one_new_table_and_index_with_existing_tables() {
        let existing = table("existing", [column("kind", Some("TEXT"))]);
        let mut desired = Schema {
            tables: vec![
                existing.clone(),
                table("items", [column("kind", Some("TEXT"))]),
            ],
            indexes: vec![plain_index("items_kind", "items", "kind")],
            ..empty_schema()
        };
        let actual = Schema {
            tables: vec![existing],
            ..empty_schema()
        };
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[1].clone()),
                Operation::CreateIndex(desired.indexes[0].clone()),
            ])
        );

        desired.indexes[0].table = "existing".into();
        assert_eq!(
            plan_schema(&actual, &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[1].clone()),
                Operation::CreateIndex(desired.indexes[0].clone()),
            ])
        );
        desired.indexes[0].table = "items".into();
        desired.indexes[0].name = "EXISTING".into();
        assert_eq!(plan_schema(&actual, &desired), Err(()));
    }

    #[test]
    fn plans_new_table_before_multiple_plain_indexes_in_name_order() {
        let mut desired = Schema {
            tables: vec![Table {
                name: "items".into(),
                check: None,
                columns: ["kind", "label"]
                    .into_iter()
                    .map(|name| column(name, Some("TEXT")))
                    .collect(),
            }],
            indexes: ["z_label", "a_kind"]
                .into_iter()
                .zip(["label", "kind"])
                .map(|(name, column)| plain_index(name, "items", column))
                .collect(),
            ..empty_schema()
        };
        let planned = plan_schema(&empty_schema(), &desired).unwrap();
        assert_eq!(
            planned,
            vec![
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::CreateIndex(desired.indexes[1].clone()),
                Operation::CreateIndex(desired.indexes[0].clone()),
            ]
        );

        desired.indexes[1].unique = true;
        assert_eq!(plan_schema(&empty_schema(), &desired), Err(()));
    }

    #[test]
    fn plans_multiple_new_tables_before_indexes_in_stable_order() {
        let make_table = |name: &str, column: &str| {
            table(
                name,
                [Column {
                    name: column.into(),
                    declared_type: Some("TEXT".into()),
                    not_null: false,
                    check: None,
                    default_null: false,
                    default_value: None,
                    primary_key: false,
                }],
            )
        };
        let make_index = |name: &str, table: &str, column: &str| plain_index(name, table, column);
        let desired = Schema {
            tables: vec![make_table("z_items", "name"), make_table("a_items", "kind")],
            indexes: vec![
                make_index("z_name", "z_items", "name"),
                make_index("a_kind", "a_items", "kind"),
            ],
            ..empty_schema()
        };

        assert_eq!(
            plan_schema(&empty_schema(), &desired),
            Ok(vec![
                Operation::CreateTable(desired.tables[1].clone()),
                Operation::CreateTable(desired.tables[0].clone()),
                Operation::CreateIndex(desired.indexes[1].clone()),
                Operation::CreateIndex(desired.indexes[0].clone()),
            ])
        );

        let invalid = Schema {
            indexes: vec![make_index("bad", "z_items", "missing")],
            ..desired.clone()
        };
        assert_eq!(plan_schema(&empty_schema(), &invalid), Err(()));
    }
}
