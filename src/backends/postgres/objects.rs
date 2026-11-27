//! PostgreSQL objects supported by the offline schema model.

use pg_query::{
    Node, NodeEnum, NodeRef,
    protobuf::{ColumnDef, ConstrType, CreateStmt, OnCommitAction, RangeVar},
};

use super::{PostgresError, ledger::LEDGER_NAME, quote};
use crate::schema::Object;

pub(super) fn invalid(message: impl Into<String>) -> PostgresError {
    PostgresError::Invalid(message.into())
}

pub(super) fn identifier(name: &str) -> Result<&str, PostgresError> {
    if name.is_empty() || name.len() > 63 {
        return Err(invalid(format!(
            "unsupported PostgreSQL identifier {name:?}"
        )));
    }
    Ok(name)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Name {
    pub schema: String,
    pub relation: String,
}

impl Name {
    pub fn new(schema: &str, relation: &str) -> Result<Self, PostgresError> {
        if relation == LEDGER_NAME {
            return Err(invalid("the PostgreSQL migration ledger name is reserved"));
        }
        Ok(Self {
            schema: identifier(if schema.is_empty() { "public" } else { schema })?.into(),
            relation: identifier(relation)?.into(),
        })
    }

    pub fn from_range(range: Option<&RangeVar>) -> Result<Self, PostgresError> {
        let range = range.ok_or_else(|| invalid("missing PostgreSQL relation"))?;
        if !range.catalogname.is_empty() {
            return Err(invalid("cross-database PostgreSQL names are not supported"));
        }
        Self::new(&range.schemaname, &range.relname)
    }

    pub fn sql(&self) -> String {
        format!("{}.{}", quote(&self.schema), quote(&self.relation))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Table,
    Index,
    View,
}

impl Kind {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Table => "TABLE",
            Self::Index => "INDEX",
            Self::View => "VIEW",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Column {
    pub name: String,
    pub definition: String,
    pub plain_nullable: bool,
}

impl Column {
    pub fn parse(node: &Node, column: &ColumnDef) -> Result<Self, PostgresError> {
        let name = identifier(&column.colname)?.into();
        let mut normalized = node.clone();
        if let Some(NodeEnum::ColumnDef(column)) = &mut normalized.node
            && let Some(ty) = &mut column.type_name
            && let [
                Node {
                    node: Some(NodeEnum::String(alias)),
                },
            ] = ty.names.as_slice()
            && matches!(
                alias.sval.as_str(),
                "int2" | "int4" | "int8" | "float4" | "float8" | "bool"
            )
        {
            // Migrations use the default catalog lookup ahead of public.
            // PostgreSQL already qualifies the corresponding SQL type names.
            ty.names.insert(
                0,
                Node {
                    node: Some(NodeEnum::String(pg_query::protobuf::String {
                        sval: "pg_catalog".into(),
                    })),
                },
            );
        }
        let definition = element_sql(&normalized)?;
        let plain_nullable = !column.is_not_null
            && column.raw_default.is_none()
            && column.identity.is_empty()
            && column.generated.is_empty()
            && column.constraints.iter().all(|constraint| {
                matches!(&constraint.node, Some(NodeEnum::Constraint(c)) if c.contype == ConstrType::ConstrNull as i32)
            })
            // SERIAL is parser sugar for a sequence, a default, and NOT NULL.
            && !column.type_name.as_ref().is_some_and(|ty| matches!(
                ty.names.as_slice(),
                [Node { node: Some(NodeEnum::String(s)) }] if matches!(
                    s.sval.as_str(), "smallserial" | "serial2" | "serial" | "serial4" | "bigserial" | "serial8"
                )
            ));
        Ok(Self {
            name,
            definition,
            plain_nullable,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Definition {
    Table {
        columns: Vec<Column>,
        constraints: Vec<String>,
    },
    Index(String),
    View(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PostgresObject {
    pub name: Name,
    pub definition: Definition,
    pub dependencies: Vec<Name>,
}

impl Object for PostgresObject {
    type Id = Name;

    fn id(&self) -> Name {
        self.name.clone()
    }
}

impl PostgresObject {
    pub fn create(node: NodeEnum) -> Result<Self, PostgresError> {
        let (name, definition, mut dependencies) = match node {
            NodeEnum::CreateStmt(table) => {
                let name = Name::from_range(table.relation.as_ref())?;
                if table.if_not_exists
                    || !table.inh_relations.is_empty()
                    || table.partbound.is_some()
                    || table.partspec.is_some()
                    || table.of_typename.is_some()
                    || !table.options.is_empty()
                    || !table.constraints.is_empty()
                    || !table.tablespacename.is_empty()
                    || !table.access_method.is_empty()
                    || table.oncommit != OnCommitAction::OncommitNoop as i32
                    || table
                        .relation
                        .as_ref()
                        .is_none_or(|r| r.relpersistence != "p")
                {
                    return Err(invalid("unsupported options on PostgreSQL CREATE TABLE"));
                }
                let mut columns: Vec<Column> = Vec::new();
                let mut constraints = Vec::new();
                let mut dependencies = Vec::new();
                for element in &table.table_elts {
                    match &element.node {
                        Some(NodeEnum::ColumnDef(column)) => {
                            let column = Column::parse(element, column)?;
                            if columns.iter().any(|existing| existing.name == column.name) {
                                return Err(invalid(format!(
                                    "duplicate PostgreSQL column {}",
                                    column.name
                                )));
                            }
                            columns.push(column);
                        }
                        Some(NodeEnum::Constraint(_)) => constraints.push(element_sql(element)?),
                        _ => return Err(invalid("unsupported PostgreSQL table element")),
                    }
                    let candidates = match &element.node {
                        Some(NodeEnum::ColumnDef(column)) => column.constraints.as_slice(),
                        _ => std::slice::from_ref(element),
                    };
                    dependencies.extend(foreign_tables(&name, candidates)?);
                }
                (
                    name,
                    Definition::Table {
                        columns,
                        constraints,
                    },
                    dependencies,
                )
            }
            NodeEnum::IndexStmt(mut index) => {
                if index.concurrent || index.if_not_exists {
                    return Err(invalid("unsupported options on PostgreSQL CREATE INDEX"));
                }
                let source = Name::from_range(index.relation.as_ref())?;
                let name = Name::new(&source.schema, &index.idxname)?;
                index
                    .relation
                    .as_mut()
                    .expect("validated relation")
                    .schemaname
                    .clone_from(&source.schema);
                (
                    name,
                    Definition::Index(deparse(NodeEnum::IndexStmt(index))?),
                    vec![source],
                )
            }
            NodeEnum::ViewStmt(mut view) => {
                let name = Name::from_range(view.view.as_ref())?;
                if view.replace || view.view.as_ref().is_none_or(|r| r.relpersistence != "p") {
                    return Err(invalid("unsupported options on PostgreSQL CREATE VIEW"));
                }
                view.view
                    .as_mut()
                    .expect("validated relation")
                    .schemaname
                    .clone_from(&name.schema);
                let mut dependencies = Vec::new();
                let query = view
                    .query
                    .as_ref()
                    .and_then(|query| query.node.as_ref())
                    .ok_or_else(|| invalid("missing PostgreSQL view query"))?;
                for (node, ..) in query.nodes() {
                    match node {
                        NodeRef::RangeVar(range) => {
                            dependencies.push(Name::from_range(Some(range))?)
                        }
                        NodeRef::CommonTableExpr(_) => {
                            return Err(invalid(
                                "PostgreSQL views with CTEs are not supported in offline planning",
                            ));
                        }
                        _ => {}
                    }
                }
                (
                    name,
                    Definition::View(deparse(NodeEnum::ViewStmt(view))?),
                    dependencies,
                )
            }
            _ => {
                return Err(invalid(
                    "unsupported PostgreSQL statement in offline planning",
                ));
            }
        };
        dependencies.sort();
        dependencies.dedup();
        Ok(Self {
            name,
            definition,
            dependencies,
        })
    }

    pub fn kind(&self) -> Kind {
        match self.definition {
            Definition::Table { .. } => Kind::Table,
            Definition::Index(_) => Kind::Index,
            Definition::View(_) => Kind::View,
        }
    }

    pub fn create_sql(&self) -> String {
        match &self.definition {
            Definition::Table {
                columns,
                constraints,
            } => {
                let elements = columns
                    .iter()
                    .map(|column| column.definition.as_str())
                    .chain(constraints.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("CREATE TABLE {} ({elements})", self.name.sql())
            }
            Definition::Index(sql) | Definition::View(sql) => sql.clone(),
        }
    }
}

pub(super) fn foreign_tables(
    table: &Name,
    constraints: &[Node],
) -> Result<Vec<Name>, PostgresError> {
    let mut tables = Vec::new();
    for node in constraints {
        if let Some(NodeEnum::Constraint(constraint)) = &node.node
            && constraint.pktable.is_some()
        {
            let source = Name::from_range(constraint.pktable.as_ref())?;
            if source != *table && !tables.contains(&source) {
                tables.push(source);
            }
        }
    }
    Ok(tables)
}

fn deparse(node: NodeEnum) -> Result<String, PostgresError> {
    node.deparse().map_err(|error| invalid(error.to_string()))
}

fn element_sql(element: &Node) -> Result<String, PostgresError> {
    // The PostgreSQL deparser renders statements, not isolated column definitions.
    let node = NodeEnum::CreateStmt(CreateStmt {
        relation: Some(RangeVar {
            relname: "fledgling_element".into(),
            relpersistence: "p".into(),
            inh: true,
            ..Default::default()
        }),
        table_elts: vec![element.clone()],
        oncommit: OnCommitAction::OncommitNoop as i32,
        ..Default::default()
    });
    let sql = deparse(node)?;
    sql.strip_prefix("CREATE TABLE fledgling_element (")
        .and_then(|sql| sql.strip_suffix(')'))
        .map(str::to_owned)
        .ok_or_else(|| invalid("cannot render PostgreSQL table element"))
}
