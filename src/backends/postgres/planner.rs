//! Offline PostgreSQL schema replay and additive migration planning.

use crate::{migrations::MigrationCatalog, planner::Plan};
use pg_query::{
    Node, NodeEnum, NodeRef,
    protobuf::{
        AlterTableType, ConstrType, CreateStmt, DropBehavior, ObjectType, OnCommitAction, RangeVar,
    },
};
use std::{collections::BTreeMap, error::Error};

pub(crate) struct PostgresPlanner;

impl PostgresPlanner {
    pub(crate) fn verified_plan(
        catalog: &MigrationCatalog,
        desired: &str,
    ) -> Result<Option<(u64, Plan)>, Box<dyn Error>> {
        let mut baseline = Model::default();
        for migration in catalog.iter() {
            baseline
                .replay(&migration.up.sql, Replay::History)
                .map_err(|e| format!("replaying {}: {e}", migration.up.path.display()))?;
        }
        let mut target = Model::default();
        target
            .replay(desired, Replay::Target)
            .map_err(|e| format!("desired schema: {e}"))?;
        let Some(plan) = plan(&baseline, &target)? else {
            return Ok(None);
        };
        let mut result = baseline.clone();
        result.replay(&plan.up, Replay::History)?;
        if result.objects != target.objects {
            return Err("generated Up SQL does not reproduce the desired schema".into());
        }
        result.replay(&plan.down, Replay::GeneratedDown)?;
        if result.objects != baseline.objects {
            return Err("generated Down SQL does not restore the baseline schema".into());
        }
        Ok(Some((catalog.next_id()?, plan)))
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Name {
    schema: String,
    object: String,
}

impl Name {
    fn new(schema: &str, object: &str) -> Result<Self, String> {
        let schema = if schema.is_empty() { "public" } else { schema };
        if object.len() > 63 || schema.len() > 63 {
            return Err("PostgreSQL identifiers longer than 63 bytes are unsupported".into());
        }
        if object == "_fledgling_migrations" {
            return Err("the PostgreSQL migration ledger name is reserved".into());
        }
        Ok(Self {
            schema: schema.into(),
            object: object.into(),
        })
    }

    fn relation(relation: &RangeVar) -> Result<Self, String> {
        if !relation.catalogname.is_empty() {
            return Err("cross-database PostgreSQL relations are unsupported".into());
        }
        Self::new(&relation.schemaname, &relation.relname)
    }

    fn sql(&self) -> String {
        format!("{}.{}", quote(&self.schema), quote(&self.object))
    }
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Column {
    name: String,
    sql: String,
    plain_nullable: bool,
}

impl Column {
    fn parse(element: &Node) -> Result<Self, String> {
        let mut element = element.clone();
        let Some(NodeEnum::ColumnDef(column)) = element.node.as_mut() else {
            return Err("unsupported PostgreSQL column definition".into());
        };
        let name = column.colname.clone();
        if name.len() > 63 {
            return Err(format!("column name {name} exceeds 63 bytes"));
        }
        if let Some(ty) = &mut column.type_name
            && let [part] = ty.names.as_slice()
            && let Some(NodeEnum::String(part)) = &part.node
            && matches!(
                part.sval.as_str(),
                "int2" | "int4" | "int8" | "float4" | "float8" | "bool"
            )
        {
            ty.names.insert(
                0,
                Node {
                    node: Some(NodeEnum::String(pg_query::protobuf::String {
                        sval: "pg_catalog".into(),
                    })),
                },
            );
        }
        let plain_nullable = !column.is_not_null
            && column.raw_default.is_none()
            && column.identity.is_empty()
            && column.generated.is_empty()
            && column.constraints.iter().all(|node| {
                matches!(&node.node, Some(NodeEnum::Constraint(c)) if c.contype == ConstrType::ConstrNull as i32)
            });
        Ok(Self {
            name,
            plain_nullable,
            sql: table_element(element)?,
        })
    }
}

// The PostgreSQL deparser takes statements, not isolated column definitions.
fn table_element(element: Node) -> Result<String, String> {
    let sql = NodeEnum::CreateStmt(CreateStmt {
        relation: Some(RangeVar {
            relname: "fledgling_element".into(),
            relpersistence: "p".into(),
            inh: true,
            ..Default::default()
        }),
        table_elts: vec![element],
        oncommit: OnCommitAction::OncommitNoop as i32,
        ..Default::default()
    })
    .deparse()
    .map_err(|e| e.to_string())?;
    sql.strip_prefix("CREATE TABLE fledgling_element (")
        .and_then(|sql| sql.strip_suffix(')'))
        .map(str::to_owned)
        .ok_or_else(|| format!("could not render PostgreSQL table element: {sql}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Table {
    columns: Vec<Column>,
    constraints: Vec<String>,
}

impl Table {
    fn parse(stmt: &CreateStmt) -> Result<Self, String> {
        if !stmt.inh_relations.is_empty()
            || stmt.partspec.is_some()
            || stmt.partbound.is_some()
            || stmt.of_typename.is_some()
            || !stmt.constraints.is_empty()
            || !stmt.options.is_empty()
            || !stmt.tablespacename.is_empty()
            || !stmt.access_method.is_empty()
            || stmt.oncommit != OnCommitAction::OncommitNoop as i32
            || stmt.if_not_exists
            || stmt
                .relation
                .as_ref()
                .is_none_or(|r| r.relpersistence != "p")
        {
            return Err("unsupported PostgreSQL CREATE TABLE options".into());
        }
        let mut table = Self {
            columns: Vec::new(),
            constraints: Vec::new(),
        };
        for element in &stmt.table_elts {
            match &element.node {
                Some(NodeEnum::ColumnDef(_)) => table.add(Column::parse(element)?)?,
                Some(NodeEnum::Constraint(_)) => {
                    table.constraints.push(table_element(element.clone())?);
                }
                _ => return Err("unsupported PostgreSQL table element".into()),
            }
        }
        Ok(table)
    }

    fn add(&mut self, column: Column) -> Result<(), String> {
        if self
            .columns
            .iter()
            .any(|existing| existing.name == column.name)
        {
            return Err(format!("duplicate PostgreSQL column {}", column.name));
        }
        self.columns.push(column);
        Ok(())
    }

    fn sql(&self, name: &Name) -> String {
        let elements = self
            .columns
            .iter()
            .map(|column| column.sql.as_str())
            .chain(self.constraints.iter().map(String::as_str))
            .collect::<Vec<_>>();
        format!("CREATE TABLE {} ({});", name.sql(), elements.join(", "))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Object {
    Table(Table),
    Index { table: Name, sql: String },
    View(String),
}

impl Object {
    fn kind(&self) -> &str {
        match self {
            Self::Table(_) => "TABLE",
            Self::Index { .. } => "INDEX",
            Self::View(_) => "VIEW",
        }
    }

    fn create(&self, name: &Name) -> String {
        match self {
            Self::Table(table) => table.sql(name),
            Self::Index { sql, .. } | Self::View(sql) => format!("{sql};"),
        }
    }
}

#[derive(Clone, Default)]
struct Model {
    objects: BTreeMap<Name, Object>,
    order: Vec<Name>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Replay {
    Target,
    History,
    GeneratedDown,
}

impl Model {
    fn insert(&mut self, name: Name, object: Object) -> Result<(), String> {
        if self.objects.contains_key(&name) {
            return Err(format!("duplicate PostgreSQL object {}", name.sql()));
        }
        self.order.push(name.clone());
        self.objects.insert(name, object);
        Ok(())
    }

    fn replay(&mut self, sql: &str, mode: Replay) -> Result<(), String> {
        let parsed = pg_query::parse(sql).map_err(|e| e.to_string())?;
        for raw in parsed.protobuf.stmts {
            let Some(node) = raw.stmt.and_then(|stmt| stmt.node) else {
                continue;
            };
            match node {
                NodeEnum::CreateStmt(stmt) => {
                    let name = Name::relation(stmt.relation.as_ref().ok_or("missing table name")?)?;
                    self.insert(name, Object::Table(Table::parse(&stmt)?))?;
                }
                NodeEnum::IndexStmt(mut stmt) => {
                    if stmt.idxname.is_empty() || stmt.concurrent || stmt.if_not_exists {
                        return Err("PostgreSQL indexes must be named and cannot use CONCURRENTLY or IF NOT EXISTS in offline planning".into());
                    }
                    let relation = stmt.relation.as_mut().ok_or("missing index table")?;
                    let table = Name::relation(relation)?;
                    if !matches!(self.objects.get(&table), Some(Object::Table(_))) {
                        return Err(format!("missing PostgreSQL index table {}", table.sql()));
                    }
                    let name = Name::new(&table.schema, &stmt.idxname)?;
                    relation.schemaname.clone_from(&table.schema);
                    let sql = NodeEnum::IndexStmt(stmt)
                        .deparse()
                        .map_err(|e| e.to_string())?;
                    self.insert(name, Object::Index { table, sql })?;
                }
                NodeEnum::ViewStmt(mut stmt) => {
                    if stmt.replace {
                        return Err(
                            "CREATE OR REPLACE VIEW is unsupported in offline PostgreSQL planning"
                                .into(),
                        );
                    }
                    let relation = stmt.view.as_mut().ok_or("missing view name")?;
                    let name = Name::relation(relation)?;
                    relation.schemaname.clone_from(&name.schema);
                    let query = stmt
                        .query
                        .as_ref()
                        .and_then(|query| query.node.as_ref())
                        .ok_or("missing PostgreSQL view query")?;
                    for (node, ..) in query.nodes() {
                        if let NodeRef::RangeVar(relation) = node {
                            let source = Name::relation(relation)?;
                            if !matches!(
                                self.objects.get(&source),
                                Some(Object::Table(_) | Object::View(_))
                            ) {
                                return Err(format!(
                                    "view {} references an unknown or unsupported relation {}",
                                    name.sql(),
                                    source.sql()
                                ));
                            }
                        }
                    }
                    let sql = NodeEnum::ViewStmt(stmt)
                        .deparse()
                        .map_err(|e| e.to_string())?;
                    self.insert(name, Object::View(sql))?;
                }
                NodeEnum::AlterTableStmt(stmt) if mode != Replay::Target => {
                    let name = Name::relation(stmt.relation.as_ref().ok_or("missing table name")?)?;
                    if !matches!(self.objects.get(&name), Some(Object::Table(_))) {
                        return Err(format!("missing PostgreSQL table {}", name.sql()));
                    }
                    if stmt.missing_ok || stmt.objtype != ObjectType::ObjectTable as i32 {
                        return Err("unsupported PostgreSQL ALTER TABLE options".into());
                    }
                    for command in stmt.cmds {
                        let Some(NodeEnum::AlterTableCmd(command)) = command.node else {
                            return Err("unsupported PostgreSQL ALTER TABLE command".into());
                        };
                        match AlterTableType::try_from(command.subtype).ok() {
                            Some(AlterTableType::AtAddColumn) if !command.missing_ok => {
                                let Some(Object::Table(table)) = self.objects.get_mut(&name) else {
                                    unreachable!()
                                };
                                table.add(Column::parse(
                                    command.def.as_deref().ok_or("missing column definition")?,
                                )?)?;
                            }
                            Some(AlterTableType::AtDropColumn)
                                if !command.missing_ok
                                    && command.behavior != DropBehavior::DropCascade as i32 =>
                            {
                                let Some(Object::Table(table)) = self.objects.get(&name) else {
                                    unreachable!()
                                };
                                let pos = table
                                    .columns
                                    .iter()
                                    .position(|c| c.name == command.name)
                                    .ok_or_else(|| {
                                        format!("missing PostgreSQL column {}", command.name)
                                    })?;
                                if mode != Replay::GeneratedDown
                                    && (!table.columns[pos].plain_nullable
                                        || !table.constraints.is_empty()
                                        || self.objects.iter().any(|object| match object.1 {
                                            Object::Index { table, .. } => table == &name,
                                            Object::View(_) => true,
                                            _ => false,
                                        }))
                                {
                                    return Err("cannot model PostgreSQL DROP COLUMN with possible dependents".into());
                                }
                                let Some(Object::Table(table)) = self.objects.get_mut(&name) else {
                                    unreachable!()
                                };
                                table.columns.remove(pos);
                            }
                            _ => return Err("unsupported PostgreSQL ALTER TABLE command".into()),
                        }
                    }
                }
                NodeEnum::DropStmt(stmt) if mode != Replay::Target => {
                    let kind = match ObjectType::try_from(stmt.remove_type).ok() {
                        Some(ObjectType::ObjectTable) => "TABLE",
                        Some(ObjectType::ObjectIndex) => "INDEX",
                        Some(ObjectType::ObjectView) => "VIEW",
                        _ => return Err("unsupported PostgreSQL DROP target".into()),
                    };
                    if stmt.concurrent
                        || stmt.missing_ok
                        || stmt.behavior == DropBehavior::DropCascade as i32
                    {
                        return Err("unsupported PostgreSQL DROP options".into());
                    }
                    for item in stmt.objects {
                        let Some(NodeEnum::List(list)) = item.node else {
                            return Err("unsupported PostgreSQL DROP target".into());
                        };
                        let parts = list
                            .items
                            .iter()
                            .map(|part| match &part.node {
                                Some(NodeEnum::String(s)) => Ok(s.sval.as_str()),
                                _ => Err("unsupported PostgreSQL DROP target"),
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let name = match parts.as_slice() {
                            [object] => Name::new("", object)?,
                            [schema, object] => Name::new(schema, object)?,
                            _ => return Err("unsupported PostgreSQL DROP target".into()),
                        };
                        if self
                            .objects
                            .get(&name)
                            .is_none_or(|object| object.kind() != kind)
                        {
                            return Err(format!("missing PostgreSQL {kind} {}", name.sql()));
                        }
                        self.objects.remove(&name);
                        self.order.retain(|candidate| candidate != &name);
                        if kind == "TABLE" {
                            self.objects.retain(|_, object| {
                                !matches!(object, Object::Index { table, .. } if table == &name)
                            });
                            self.order
                                .retain(|candidate| self.objects.contains_key(candidate));
                        }
                    }
                }
                _ => {
                    let description = sql.get(raw.stmt_location as usize..).unwrap_or_default();
                    let keyword = description
                        .split_whitespace()
                        .take(2)
                        .collect::<Vec<_>>()
                        .join(" ");
                    return Err(format!(
                        "unsupported PostgreSQL statement in offline planning: {keyword}"
                    ));
                }
            }
        }
        Ok(())
    }
}

fn plan(before: &Model, after: &Model) -> Result<Option<Plan>, String> {
    let mut up = Vec::new();
    let mut down = Vec::new();
    for (name, existing) in &before.objects {
        let target = after.objects.get(name).ok_or_else(|| {
            format!(
                "cannot remove PostgreSQL {} {}",
                existing.kind(),
                name.sql()
            )
        })?;
        if existing == target {
            continue;
        }
        let (Object::Table(existing), Object::Table(target)) = (existing, target) else {
            return Err(format!(
                "cannot change PostgreSQL {} {}",
                existing.kind(),
                name.sql()
            ));
        };
        if existing.constraints != target.constraints {
            return Err(format!(
                "cannot change constraints on PostgreSQL table {}",
                name.sql()
            ));
        }
        for (i, column) in existing.columns.iter().enumerate() {
            if target.columns.get(i) != Some(column) {
                return Err(format!(
                    "cannot change, move, or remove PostgreSQL column {}.{}",
                    name.sql(),
                    quote(&column.name)
                ));
            }
        }
        for column in &target.columns[existing.columns.len()..] {
            if !column.plain_nullable {
                return Err(format!(
                    "cannot add PostgreSQL column {}.{}: new columns must be nullable and unconstrained",
                    name.sql(),
                    quote(&column.name)
                ));
            }
            // A stored SELECT * view doesn't gain columns when its source does.
            // Without resolving projected columns, replay cannot prove equivalence.
            if before
                .objects
                .values()
                .any(|object| matches!(object, Object::View(_)))
            {
                return Err(format!(
                    "cannot add PostgreSQL column {}.{} with existing views in offline planning",
                    name.sql(),
                    quote(&column.name)
                ));
            }
            up.push(format!(
                "ALTER TABLE {} ADD COLUMN {};",
                name.sql(),
                column.sql
            ));
            down.push(format!(
                "ALTER TABLE {} DROP COLUMN {};",
                name.sql(),
                quote(&column.name)
            ));
        }
    }
    for name in &after.order {
        if !before.objects.contains_key(name) {
            let object = &after.objects[name];
            up.push(object.create(name));
            down.push(format!("DROP {} {};", object.kind(), name.sql()));
        }
    }
    down.reverse();
    Ok((!up.is_empty()).then(|| Plan {
        up: up.join("\n"),
        down: down.join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_columns_alongside_existing_constraints_indexes_and_new_objects() {
        let current =
            "CREATE TABLE books (id bigint, PRIMARY KEY (id)); CREATE INDEX lookup ON books (id);";
        let desired = "CREATE TABLE books (id bigint, label text, PRIMARY KEY (id)); CREATE INDEX lookup ON books (id); CREATE TABLE audit (id bigint);";
        let mut before = Model::default();
        before.replay(current, Replay::History).unwrap();
        let mut target = Model::default();
        target.replay(desired, Replay::Target).unwrap();
        let plan = plan(&before, &target).unwrap().unwrap();
        let mut replayed = before.clone();
        replayed.replay(&plan.up, Replay::History).unwrap();
        assert_eq!(replayed.objects, target.objects);
        replayed.replay(&plan.down, Replay::GeneratedDown).unwrap();
        assert_eq!(replayed.objects, before.objects);
    }

    #[test]
    fn rejects_a_view_with_a_missing_source() {
        let error = Model::default()
            .replay("CREATE VIEW v AS SELECT * FROM missing;", Replay::Target)
            .unwrap_err();
        assert!(error.contains("unknown or unsupported relation"));
    }
}
