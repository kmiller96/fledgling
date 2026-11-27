//! Offline additive PostgreSQL planning using the PostgreSQL SQL grammar.

use std::{error::Error, path::Path};

use pg_query::{
    NodeEnum,
    protobuf::{AlterTableType, DropBehavior, ObjectType},
};

use crate::{
    migrations::{MigrationFile, Operation, read_sql},
    planner::{Change, diff},
    schema::Schema,
};

use super::{
    PostgresError,
    objects::{Column, Definition, Kind, Name, PostgresObject, foreign_tables, invalid},
    operations::PostgresOperation,
};

pub(crate) fn verified_plan(
    files: &[MigrationFile],
    schema: &Path,
) -> Result<Option<(String, String)>, Box<dyn Error>> {
    let mut history = Vec::new();
    for file in files {
        replay(&mut history, &read_sql(&file.up)?, Replay::History)
            .map_err(|e| format!("replaying {}: {e}", file.up.display()))?;
    }
    let mut desired = Vec::new();
    replay(&mut desired, &read_sql(schema)?, Replay::Desired)
        .map_err(|e| format!("loading desired schema: {e}"))?;

    let baseline = Schema::new(history.clone());
    let desired = Schema::new(desired);
    let Some((up, down)) = plan(&baseline, &desired)? else {
        return Ok(None);
    };
    replay(&mut history, &up, Replay::History)
        .map_err(|e| format!("verifying generated Up SQL: {e}"))?;
    if !diff(&Schema::new(history.clone()), &desired)
        .changes()
        .is_empty()
    {
        return Err("generated PostgreSQL Up SQL does not reproduce the desired model".into());
    }
    replay(&mut history, &down, Replay::GeneratedDown)
        .map_err(|e| format!("verifying generated Down SQL: {e}"))?;
    if !diff(&Schema::new(history), &baseline).changes().is_empty() {
        return Err("generated PostgreSQL Down SQL does not restore the baseline model".into());
    }
    Ok(Some((up, down)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Replay {
    Desired,
    History,
    GeneratedDown,
}

fn replay(objects: &mut Vec<PostgresObject>, sql: &str, mode: Replay) -> Result<(), PostgresError> {
    let parsed = pg_query::parse(sql).map_err(|error| invalid(error.to_string()))?;
    for raw in parsed.protobuf.stmts {
        let node = raw
            .stmt
            .and_then(|stmt| stmt.node)
            .ok_or_else(|| invalid("missing PostgreSQL statement"))?;
        match node {
            NodeEnum::CreateStmt(_) | NodeEnum::IndexStmt(_) | NodeEnum::ViewStmt(_) => {
                let object = PostgresObject::create(node)?;
                if objects.iter().any(|existing| existing.name == object.name) {
                    return Err(invalid(format!(
                        "duplicate PostgreSQL relation {}",
                        object.name.sql()
                    )));
                }
                for name in &object.dependencies {
                    if !objects.iter().any(|source| {
                        source.name == *name
                            && (source.kind() == Kind::Table
                                || (object.kind() == Kind::View && source.kind() == Kind::View))
                    }) {
                        return Err(invalid(format!(
                            "{} references an unknown or unsupported relation {}",
                            object.name.sql(),
                            name.sql()
                        )));
                    }
                }
                objects.push(object);
            }
            NodeEnum::AlterTableStmt(alter) if mode != Replay::Desired => {
                let name = Name::from_range(alter.relation.as_ref())?;
                if alter.missing_ok
                    || alter.objtype != ObjectType::ObjectTable as i32
                    || alter.relation.as_ref().is_none_or(|r| !r.inh)
                {
                    return Err(invalid("unsupported options on PostgreSQL ALTER TABLE"));
                }
                let has_dependents = objects
                    .iter()
                    .any(|object| object.dependencies.contains(&name));
                let has_dependent_views = has_dependent_view(objects, &name);
                let position = objects
                    .iter()
                    .position(|object| object.name == name && object.kind() == Kind::Table)
                    .ok_or_else(|| invalid(format!("missing PostgreSQL table {}", name.sql())))?;
                let (before, after) = objects.split_at_mut(position);
                let (table, after) = after.split_first_mut().expect("validated table position");
                let Definition::Table {
                    columns,
                    constraints,
                } = &mut table.definition
                else {
                    unreachable!("validated PostgreSQL table")
                };
                for cmd in alter.cmds {
                    let Some(NodeEnum::AlterTableCmd(cmd)) = cmd.node else {
                        return Err(invalid("unsupported PostgreSQL ALTER TABLE command"));
                    };
                    match AlterTableType::try_from(cmd.subtype).ok() {
                        Some(AlterTableType::AtAddColumn) if !cmd.missing_ok => {
                            if has_dependent_views {
                                return Err(add_with_view(&name));
                            }
                            let node = cmd
                                .def
                                .as_deref()
                                .ok_or_else(|| invalid("missing column definition"))?;
                            let Some(NodeEnum::ColumnDef(column)) = &node.node else {
                                return Err(invalid("unsupported PostgreSQL column definition"));
                            };
                            for source in foreign_tables(&name, &column.constraints)? {
                                if !before.iter().chain(after.iter()).any(|table| {
                                    table.name == source && table.kind() == Kind::Table
                                }) {
                                    return Err(invalid(format!(
                                        "{} references an unknown or unsupported relation {}",
                                        name.sql(),
                                        source.sql()
                                    )));
                                }
                                if let Err(position) = table.dependencies.binary_search(&source) {
                                    table.dependencies.insert(position, source);
                                }
                            }
                            let column = Column::parse(node, column)?;
                            if columns.iter().any(|existing| existing.name == column.name) {
                                return Err(invalid(format!(
                                    "duplicate PostgreSQL column {}",
                                    column.name
                                )));
                            }
                            columns.push(column);
                        }
                        Some(AlterTableType::AtDropColumn)
                            if !cmd.missing_ok
                                && cmd.behavior == DropBehavior::DropRestrict as i32 =>
                        {
                            let pos = columns.iter().position(|c| c.name == cmd.name).ok_or_else(
                                || invalid(format!("missing PostgreSQL column {}", cmd.name)),
                            )?;
                            if !columns[pos].plain_nullable
                                || (mode != Replay::GeneratedDown
                                    && (has_dependents
                                        || !constraints.is_empty()
                                        || columns.iter().any(|c| !c.plain_nullable)))
                            {
                                return Err(invalid(
                                    "cannot model PostgreSQL DROP COLUMN with possible dependents",
                                ));
                            }
                            columns.remove(pos);
                        }
                        _ => {
                            return Err(invalid(
                                "unsupported PostgreSQL ALTER TABLE command in offline planning",
                            ));
                        }
                    }
                }
            }
            NodeEnum::DropStmt(drop) if mode != Replay::Desired => {
                let kind = match ObjectType::try_from(drop.remove_type).ok() {
                    Some(ObjectType::ObjectTable) => Kind::Table,
                    Some(ObjectType::ObjectIndex) => Kind::Index,
                    Some(ObjectType::ObjectView) => Kind::View,
                    _ => return Err(invalid("unsupported PostgreSQL DROP target")),
                };
                if drop.concurrent
                    || drop.missing_ok
                    || drop.behavior != DropBehavior::DropRestrict as i32
                {
                    return Err(invalid("unsupported options on PostgreSQL DROP"));
                }
                for target in drop.objects {
                    let Some(NodeEnum::List(parts)) = target.node else {
                        return Err(invalid("unsupported PostgreSQL DROP name"));
                    };
                    let parts = parts
                        .items
                        .iter()
                        .map(|part| match &part.node {
                            Some(NodeEnum::String(value)) => Ok(value.sval.as_str()),
                            _ => Err(invalid("unsupported PostgreSQL DROP name")),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let name = match parts.as_slice() {
                        [relation] => Name::new("", relation)?,
                        [schema, relation] => Name::new(schema, relation)?,
                        _ => return Err(invalid("unsupported PostgreSQL DROP name")),
                    };
                    let pos = objects
                        .iter()
                        .position(|object| object.name == name && object.kind() == kind)
                        .ok_or_else(|| {
                            invalid(format!("missing PostgreSQL {} {}", kind.sql(), name.sql()))
                        })?;
                    if objects.iter().any(|object| {
                        object.kind() != Kind::Index && object.dependencies.contains(&name)
                    }) {
                        return Err(invalid(format!(
                            "cannot model dropping {} with dependents",
                            name.sql()
                        )));
                    }
                    objects.remove(pos);
                    if kind == Kind::Table {
                        objects.retain(|object| {
                            object.kind() != Kind::Index || !object.dependencies.contains(&name)
                        });
                    }
                }
            }
            _ => {
                return Err(invalid(
                    "unsupported PostgreSQL statement in offline planning",
                ));
            }
        }
    }
    Ok(())
}

fn plan(
    before: &Schema<PostgresObject>,
    after: &Schema<PostgresObject>,
) -> Result<Option<(String, String)>, PostgresError> {
    use PostgresOperation::*;

    let mut changes = diff(before, after).into_changes();
    if changes.is_empty() {
        return Ok(None);
    }
    // Update existing tables before adding dependent objects. Keep additions in
    // declaration order; replay already checked relation dependencies.
    changes.sort_by_key(|change| matches!(change, Change::Added(_)));
    let mut up = Vec::new();
    let mut down = Vec::new();
    for change in changes {
        let (apply, revert) = match change {
            Change::Added(object) => (Create(object), Drop(object.kind(), &object.name)),
            Change::Removed(object) => {
                return Err(invalid(format!(
                    "removing PostgreSQL {} {} is not supported",
                    object.kind().sql(),
                    object.name.sql()
                )));
            }
            Change::Modified {
                before: old_object,
                after: new_object,
            } => {
                let name = &old_object.name;
                match (&old_object.definition, &new_object.definition) {
                    (
                        Definition::Table {
                            columns: old,
                            constraints: old_constraints,
                        },
                        Definition::Table {
                            columns: new,
                            constraints: new_constraints,
                        },
                    ) if old_constraints == new_constraints => {
                        if let Some((_, column)) = old
                            .iter()
                            .enumerate()
                            .find(|(i, column)| new.get(*i) != Some(*column))
                        {
                            return Err(invalid(format!(
                                "changing PostgreSQL TABLE {} is not supported: cannot change, move, or remove column {}",
                                name.sql(),
                                super::quote(&column.name)
                            )));
                        }
                        if new.len() != old.len() + 1 {
                            return Err(invalid(format!(
                                "changing PostgreSQL TABLE {} is not supported: append one column at a time",
                                name.sql()
                            )));
                        }
                        let column = &new[old.len()];
                        if !column.plain_nullable {
                            return Err(invalid(format!(
                                "cannot add column {}.{}: new columns on existing PostgreSQL tables must be plain nullable",
                                name.sql(),
                                column.name
                            )));
                        }
                        if has_dependent_view(before.objects(), name) {
                            return Err(add_with_view(name));
                        }
                        (AddColumn(name, column), DropColumn(name, column))
                    }
                    _ => {
                        return Err(invalid(format!(
                            "changing PostgreSQL {} {} is not supported",
                            old_object.kind().sql(),
                            name.sql()
                        )));
                    }
                }
            }
        };
        up.push(apply.render().expect("infallible PostgreSQL operation"));
        down.push(revert.render().expect("infallible PostgreSQL operation"));
    }
    down.reverse();
    Ok(Some((up.join("\n"), down.join("\n"))))
}

fn has_dependent_view(objects: &[PostgresObject], table: &Name) -> bool {
    objects
        .iter()
        .any(|object| object.kind() == Kind::View && object.dependencies.contains(table))
}

fn add_with_view(table: &Name) -> PostgresError {
    // PostgreSQL binds a view's columns at creation time. Replay cannot
    // establish whether adding a column would change a fresh view definition.
    invalid(format!(
        "cannot add a column to PostgreSQL table {} with existing dependent views in offline planning",
        table.sql()
    ))
}
