//! Conservative SQLite migration planner.

use crate::{
    driver::Driver,
    migrations::{Migration, MigrationCatalog},
    planner::{Plan, Planner},
    schema::{Change, Definition},
};
use sqlite3_parser::{
    Bump, FallibleIterator,
    ast::{Cmd, ColumnConstraint, CreateTableBody, Stmt},
    lexer::{
        Scanner,
        sql::{Parser, TokenType, Tokenizer},
    },
};
use std::{error::Error, path::Path};

use super::{
    driver::SqliteDriver,
    ledger::LEDGER_NAME,
    operators::{Operator, QualifiedName, quote_identifier},
    schema::{SchemaObject, SchemaObjectDefinition, SchemaObjectKind, unquote},
};

/// Plans supported SQLite schema changes.
pub struct SqlitePlanner;

impl SqlitePlanner {
    /// Replay on a throwaway database and verify both directions of the plan.
    pub fn verified_plan(
        catalog: &MigrationCatalog,
        desired: &str,
    ) -> Result<Option<(u64, Plan)>, Box<dyn Error>> {
        let mut replay = SqliteDriver::open_in_memory()?;
        for migration in catalog.iter() {
            replay
                .apply(migration)
                .map_err(|error| format!("replaying {}: {error}", migration.up.path.display()))?;
        }
        let baseline = snapshot(&replay)?;

        validate_desired_sql(desired)?;
        let mut target = SqliteDriver::open_in_memory()?;
        target
            .load_desired_schema(desired)
            .map_err(|error| format!("loading desired schema: {error}"))?;
        let desired = snapshot(&target)?;

        let Some(plan) = Self.plan(&changes(&baseline, &desired))? else {
            return Ok(None);
        };
        let id = catalog.next_id()?;
        let migration = Migration::new(id, Path::new(""), plan.up.clone(), plan.down.clone());
        replay
            .apply(&migration)
            .map_err(|error| format!("verifying generated Up SQL: {error}"))?;
        if !changes(&snapshot(&replay)?, &desired).is_empty() {
            return Err("generated Up SQL does not reproduce the desired schema".into());
        }
        replay
            .revert(&migration)
            .map_err(|error| format!("verifying generated Down SQL: {error}"))?;
        if !changes(&snapshot(&replay)?, &baseline).is_empty() {
            return Err("generated Down SQL does not restore the baseline schema".into());
        }
        Ok(Some((id, plan)))
    }
}

fn snapshot(driver: &SqliteDriver) -> Result<Definition<SchemaObject>, Box<dyn Error>> {
    Ok(Definition::new(
        driver
            .schema_sql()?
            .into_iter()
            .map(SchemaObject::from_sql)
            .collect::<Result<_, _>>()?,
    ))
}

fn validate_desired_sql(sql: &str) -> Result<(), Box<dyn Error>> {
    let bump = Bump::new();
    let mut parser = Parser::new(&bump, sql.as_bytes());
    let mut seen = Vec::new();
    while let Some(command) = parser.next()? {
        let (name, trigger) = match command {
            Cmd::Stmt(
                Stmt::CreateTable {
                    tbl_name: name,
                    temporary: false,
                    body: CreateTableBody::ColumnsAndConstraints { .. },
                    ..
                }
                | Stmt::CreateVirtualTable { tbl_name: name, .. }
                | Stmt::CreateView {
                    view_name: name,
                    temporary: false,
                    ..
                }
                | Stmt::CreateIndex { idx_name: name, .. },
            ) => (name, false),
            Cmd::Stmt(Stmt::CreateTrigger {
                trigger_name,
                temporary: false,
                ..
            }) => (trigger_name, true),
            _ => {
                return Err(
                    "desired schema must contain only persistent CREATE declarations".into(),
                );
            }
        };
        if name
            .db_name
            .as_ref()
            .is_some_and(|db| !unquote(db.0).eq_ignore_ascii_case("main"))
            || unquote(name.name.0).eq_ignore_ascii_case(LEDGER_NAME)
        {
            return Err(format!("unsupported object name in desired schema: {name}").into());
        }
        if seen.contains(&(name.name.clone(), trigger)) {
            return Err(format!("duplicate object in desired schema: {name}").into());
        }
        seen.push((name.name, trigger));
    }
    Ok(())
}

fn changes<'a>(
    before: &'a Definition<SchemaObject>,
    after: &'a Definition<SchemaObject>,
) -> Vec<Change<'a, SchemaObject>> {
    before
        .difference(after)
        .into_iter()
        .filter(|change| match change {
            Change::Modified { before, after } => {
                // ALTER TABLE may reformat the stored CREATE SQL and quote the
                // new column. Compare typed table bodies, not that spelling.
                let bump = Bump::new();
                !matches!(
                    (before.definition(&bump), after.definition(&bump)),
                    (
                        Ok(SchemaObjectDefinition::Table { body: before, .. }),
                        Ok(SchemaObjectDefinition::Table { body: after, .. }),
                    ) if before == after
                )
            }
            _ => true,
        })
        .collect()
}

impl Planner for SqlitePlanner {
    type Object = SchemaObject;
    type Error = String;

    fn plan(&self, changes: &[Change<'_, SchemaObject>]) -> Result<Option<Plan>, String> {
        if changes.is_empty() {
            return Ok(None);
        }
        let mut up = Vec::new();
        let mut down = Vec::new();
        // Reject removals before examining modifications, independent of generic
        // diff order. Never return a partial plan.
        for change in changes {
            if let Change::Removed(object) = change {
                return Err(format!(
                    "removing {} {} is not supported",
                    kind_label(object.kind()),
                    object.name()
                ));
            }
        }
        let mut ordered = changes.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|change| match change {
            Change::Added(object) => match object.kind() {
                SchemaObjectKind::Table => 0,
                SchemaObjectKind::Index => 2,
                SchemaObjectKind::View => 3,
                SchemaObjectKind::Trigger => 4,
            },
            Change::Modified { .. } => 1,
            Change::Removed(_) => 5,
        });
        for change in ordered {
            match change {
                Change::Added(object) => {
                    up.push(Operator::Create((*object).clone()).to_sql());
                    down.insert(0, drop_object(object));
                }
                Change::Removed(_) => unreachable!("removals rejected above"),
                Change::Modified { before, after } => {
                    if before.kind() != SchemaObjectKind::Table
                        || after.kind() != SchemaObjectKind::Table
                    {
                        return Err(format!(
                            "changing {} {} is not supported",
                            kind_label(before.kind()),
                            before.name()
                        ));
                    }
                    match appended_column(before, after) {
                        Ok(Some((column, definition, required))) => {
                            if required {
                                return Err(format!(
                                    "cannot add column {}.{}: new columns on existing tables must be nullable; remove NOT NULL or write a migration that backfills existing rows and enforces the constraint",
                                    before.name(),
                                    column
                                ));
                            }
                            let op = Operator::AddColumn {
                                table: QualifiedName::new(before.name()),
                                column_definition: definition,
                            };
                            up.push(op.to_sql());
                            down.insert(
                                0,
                                Operator::DropColumn {
                                    table: QualifiedName::new(before.name()),
                                    column,
                                }
                                .to_sql(),
                            );
                        }
                        Ok(None) => return Err(classify_table_edit(before, after)),
                        Err(message) => return Err(message),
                    }
                }
            }
        }
        Ok(Some(Plan {
            up: up.join("\n"),
            down: down.join("\n"),
        }))
    }
}

fn classify_table_edit(before: &SchemaObject, after: &SchemaObject) -> String {
    let bump = Bump::new();
    let (
        Ok(SchemaObjectDefinition::Table {
            body:
                CreateTableBody::ColumnsAndConstraints {
                    columns: old_columns,
                    constraints: old_constraints,
                    ..
                },
            ..
        }),
        Ok(SchemaObjectDefinition::Table {
            body:
                CreateTableBody::ColumnsAndConstraints {
                    columns: new_columns,
                    constraints: new_constraints,
                    ..
                },
            ..
        }),
    ) = (before.definition(&bump), after.definition(&bump))
    else {
        return format!("changing table {} is not supported", before.name());
    };
    if old_constraints != new_constraints
        || old_columns.iter().zip(new_columns.iter()).any(|(a, b)| {
            a.constraints != b.constraints
                && a.constraints
                    .iter()
                    .chain(b.constraints)
                    .any(|c| matches!(c.constraint, ColumnConstraint::Check(_)))
        })
    {
        return format!(
            "changing CHECK constraint on table {} is not supported",
            before.name()
        );
    }
    for old in old_columns.iter() {
        let name = unquote(old.col_name.0);
        match new_columns.iter().find(|new| new.col_name == old.col_name) {
            None => {
                return format!(
                    "removing column {name} from table {} is not supported",
                    before.name()
                );
            }
            Some(new) if old.col_type != new.col_type => {
                return format!(
                    "changing the declared type of column {}.{name} is not supported",
                    before.name()
                );
            }
            _ => {}
        }
    }
    format!("changing table {} is not supported", before.name())
}

fn kind_label(kind: SchemaObjectKind) -> &'static str {
    match kind {
        SchemaObjectKind::Table => "table",
        SchemaObjectKind::View => "view",
        SchemaObjectKind::Index => "index",
        SchemaObjectKind::Trigger => "trigger",
    }
}

fn drop_object(object: &SchemaObject) -> String {
    Operator::Drop {
        kind: object.kind(),
        name: QualifiedName::new(object.name()),
        if_exists: false,
    }
    .to_sql()
}

// Recognize a final top-level column appended to a simple CREATE TABLE body.
// Unsupported/ambiguous table edits are rejected rather than approximated.
fn appended_column(
    before: &SchemaObject,
    after: &SchemaObject,
) -> Result<Option<(String, String, bool)>, String> {
    let bump = Bump::new();
    let SchemaObjectDefinition::Table {
        body:
            CreateTableBody::ColumnsAndConstraints {
                columns: old_columns,
                constraints: old_constraints,
                flags: old_flags,
            },
        ..
    } = before.definition(&bump).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let SchemaObjectDefinition::Table {
        body:
            CreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                flags,
            },
        ..
    } = after.definition(&bump).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if constraints != old_constraints
        || flags != old_flags
        || columns.len() != old_columns.len() + 1
        || columns[..old_columns.len()] != old_columns[..]
    {
        return Ok(None);
    }
    let col = columns.last().unwrap();
    let name = unquote(col.col_name.0);
    let required = col.constraints.iter().any(|c| {
        matches!(
            c.constraint,
            ColumnConstraint::NotNull {
                nullable: false,
                ..
            } | ColumnConstraint::PrimaryKey { .. }
        )
    });
    // Extract one complete top-level declaration from the original CREATE
    // body. AST equality above proves this is the sole appended declaration.
    let sql = after.to_sql();
    let declaration = column_fragment(&sql, old_columns.len()).ok_or_else(|| {
        format!(
            "cannot safely render appended column {}.{}",
            before.name(),
            name
        )
    })?;
    let rest = declaration.strip_prefix(col.col_name.0).ok_or_else(|| {
        format!(
            "cannot safely render appended column {}.{name}",
            before.name()
        )
    })?;
    let declaration = format!("{}{rest}", quote_identifier(&name));
    Ok(Some((name, declaration, required)))
}

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
            let (first, _) = span.unwrap_or((start, end));
            span = Some((first, end));
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
