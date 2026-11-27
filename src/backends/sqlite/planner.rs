//! Checked additive SQLite migration planning.

use std::{error::Error, path::Path};

use sqlite3_parser::{
    Bump, FallibleIterator,
    ast::{Cmd, ColumnConstraint, CreateTableBody, Stmt, TableConstraint},
    lexer::{
        Scanner,
        sql::{Parser, TokenType, Tokenizer},
    },
};

use super::{
    driver::SqliteDriver,
    objects::{SqliteObject, SqliteObjectKind, unquote, validate_desired_sql},
    operations::{SqliteOperation, quote},
};
use crate::{
    driver::Driver,
    migrations::{MigrationFile, read_sql},
    planner::{Change, diff},
    schema::Schema,
};

pub(crate) fn verified_plan(
    files: &[MigrationFile],
    schema: &Path,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    let mut replay = SqliteDriver::open_in_memory()?;
    for file in files {
        let sql = read_sql(&file.up)?;
        replay
            .execute(&sql)
            .map_err(|error| format!("replaying {}: {error}", file.up.display()))?;
    }
    let baseline = replay.schema()?;

    let sql = read_sql(schema)?;
    validate_desired_sql(&sql)?;
    let mut target = SqliteDriver::open_in_memory()?;
    target
        .execute(&sql)
        .map_err(|error| format!("loading desired schema: {error}"))?;
    let desired = target.schema()?;
    target.validate_views(&desired)?;

    let Some((up, down)) = plan(&baseline, &desired)? else {
        return Ok(None);
    };
    replay
        .execute(&up)
        .map_err(|error| format!("verifying generated Up SQL: {error}"))?;
    let applied = replay.schema()?;
    replay.validate_views(&applied)?;
    if !changes(&applied, &desired)?.is_empty() {
        return Err("generated Up SQL does not reproduce the desired schema".into());
    }
    replay
        .execute(&down)
        .map_err(|error| format!("verifying generated Down SQL: {error}"))?;
    if !changes(&replay.schema()?, &baseline)?.is_empty() {
        return Err("generated Down SQL does not restore the baseline schema".into());
    }
    Ok(Some((up, down)))
}

fn changes<'a>(
    before: &'a Schema<SqliteObject>,
    after: &'a Schema<SqliteObject>,
) -> Result<Vec<Change<'a, SqliteObject>>, Box<dyn Error>> {
    let mut changes = Vec::new();
    for change in diff(before, after).into_changes() {
        // ALTER TABLE can change the spelling of a definition without changing
        // its table body. Keep the generic diff's textual equality for other objects.
        if let Change::Modified { before, after } = change
            && before.kind() == SqliteObjectKind::Table
            && after.kind() == SqliteObjectKind::Table
        {
            let bump = Bump::new();
            if table_body(before, &bump)? == table_body(after, &bump)? {
                continue;
            }
        }
        changes.push(change);
    }
    Ok(changes)
}

fn plan(
    before: &Schema<SqliteObject>,
    after: &Schema<SqliteObject>,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    use Change::*;
    use SqliteObjectKind::*;
    use SqliteOperation::*;

    let mut changes = changes(before, after)?;
    if changes.is_empty() {
        return Ok(None);
    }
    for change in &changes {
        if let Removed(object) = change {
            return Err(format!(
                "removing {} {} is not supported",
                kind_label(object.kind()),
                object.name()
            )
            .into());
        }
    }
    changes.sort_by_key(|change| match change {
        Added(object) => match object.kind() {
            Table | VirtualTable => 0,
            Index => 2,
            View => 3,
            Trigger => 4,
        },
        Modified { .. } => 1,
        Removed(_) => unreachable!("removals rejected above"),
    });

    let mut up = Vec::new();
    let mut down = Vec::new();
    for change in changes {
        let (apply, revert) = match change {
            Added(object) => (
                Create(object.clone()),
                Drop {
                    kind: object.kind(),
                    name: object.name().into(),
                },
            ),
            Modified { before, after } if before.kind() == Table && after.kind() == Table => {
                let (column, definition) = appended_column(before, after)?;
                let table = before.name().to_owned();
                (
                    AddColumn {
                        table: table.clone(),
                        definition,
                    },
                    DropColumn { table, column },
                )
            }
            Modified { before, .. } => {
                return Err(format!(
                    "changing {} {} is not supported",
                    kind_label(before.kind()),
                    before.name()
                )
                .into());
            }
            Removed(_) => unreachable!("removals rejected above"),
        };
        up.push(apply.render_migration());
        down.push(revert.render_migration());
    }
    down.reverse();
    Ok(Some((up.join("\n"), down.join("\n"))))
}

fn kind_label(kind: SqliteObjectKind) -> &'static str {
    match kind {
        SqliteObjectKind::Table => "table",
        SqliteObjectKind::VirtualTable => "virtual table",
        SqliteObjectKind::Index => "index",
        SqliteObjectKind::View => "view",
        SqliteObjectKind::Trigger => "trigger",
    }
}

fn table_body<'a>(
    object: &'a SqliteObject,
    bump: &'a Bump,
) -> Result<CreateTableBody<'a>, Box<dyn Error>> {
    match Parser::new(bump, object.sql().as_bytes())
        .next()
        .map_err(|e| e.to_string())?
    {
        Some(Cmd::Stmt(Stmt::CreateTable { body, .. })) => Ok(body),
        _ => Err(format!("expected a table definition for {}", object.name()).into()),
    }
}

fn appended_column(
    before: &SqliteObject,
    after: &SqliteObject,
) -> Result<(String, String), Box<dyn Error>> {
    let table = before.name();
    let unsupported = || format!("changing table {table} is not supported");
    let bump = Bump::new();
    let (
        CreateTableBody::ColumnsAndConstraints {
            columns: old,
            constraints: old_constraints,
            flags: old_flags,
        },
        CreateTableBody::ColumnsAndConstraints {
            columns: new,
            constraints: new_constraints,
            flags: new_flags,
        },
    ) = (table_body(before, &bump)?, table_body(after, &bump)?)
    else {
        return Err(unsupported().into());
    };

    if old_constraints == new_constraints
        && old_flags == new_flags
        && new.len() == old.len() + 1
        && new.starts_with(&old)
    {
        let column = &new[old.len()];
        let name = unquote(column.col_name.0);
        if column.constraints.iter().any(|c| {
            matches!(
                c.constraint,
                ColumnConstraint::NotNull {
                    nullable: false,
                    ..
                } | ColumnConstraint::PrimaryKey { .. }
            )
        }) {
            return Err(format!("cannot add column {table}.{name}: new columns on existing tables must be nullable; remove NOT NULL or write a migration that backfills existing rows and enforces the constraint").into());
        }
        let rest = column_fragment(after.sql(), old.len())
            .and_then(|sql| sql.strip_prefix(column.col_name.0))
            .ok_or_else(|| format!("cannot safely render appended column {table}.{name}"))?;
        return Ok((name.clone(), format!("{}{rest}", quote(&name))));
    }

    if !old_constraints
        .into_iter()
        .flatten()
        .filter(|c| matches!(c.constraint, TableConstraint::Check(..)))
        .eq(new_constraints
            .into_iter()
            .flatten()
            .filter(|c| matches!(c.constraint, TableConstraint::Check(..))))
        || old.iter().zip(new.iter()).any(|(a, b)| {
            !a.constraints
                .iter()
                .filter(|c| matches!(c.constraint, ColumnConstraint::Check(_)))
                .eq(b
                    .constraints
                    .iter()
                    .filter(|c| matches!(c.constraint, ColumnConstraint::Check(_))))
        })
    {
        return Err(format!("changing CHECK constraint on table {table} is not supported").into());
    }
    for column in &old {
        let name = unquote(column.col_name.0);
        match new.iter().find(|new| new.col_name == column.col_name) {
            None => {
                return Err(
                    format!("removing column {name} from table {table} is not supported").into(),
                );
            }
            Some(new) if new.col_type != column.col_type => {
                return Err(format!(
                    "changing the declared type of column {table}.{name} is not supported"
                )
                .into());
            }
            _ => {}
        }
    }
    Err(unsupported().into())
}

// Extract the original declaration, honoring quoted names, comments and
// nested commas (e.g. DEFAULT expressions and CHECK constraints).
fn column_fragment(sql: &str, index: usize) -> Option<&str> {
    use TokenType::{TK_COMMA, TK_LP, TK_RP};

    let mut scanner = Scanner::new(Tokenizer::new());
    let mut depth = 0;
    let mut column = 0;
    let mut span = None;
    while let (start, Some((_, token)), end) = scanner.scan(sql.as_bytes()).ok()? {
        if depth == 1 && matches!(token, TK_COMMA | TK_RP) {
            if column == index {
                return span.and_then(|(first, last)| sql.get(first..last));
            }
            column += 1;
        } else if depth > 0 && column == index {
            span = Some((span.map_or(start, |(first, _)| first), end));
        }
        match token {
            TK_LP => depth += 1,
            TK_RP if depth == 1 => break,
            TK_RP => depth -= 1,
            _ => {}
        }
    }
    None
}
