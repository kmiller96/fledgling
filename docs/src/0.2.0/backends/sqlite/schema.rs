//! SQLite schema objects.
//!
//! Definitions retain the SQLite parser's typed CREATE clauses and original
//! SQL. This keeps SQLite-specific table, column, and constraint forms intact.
//! Parser acceptance is syntax validation only: SQLite engine validation is
//! still required for module availability, name resolution, and semantic rules.
//! Catalog-generated autoindexes have no CREATE statement and are not represented. Virtual-table
//! shadow tables are represented only if callers explicitly include their
//! CREATE statements; SQLite internal objects such as `sqlite_sequence` and
//! `sqlite_stat*` should generally be filtered by catalog readers. This parser
//! follows sqlite3-parser's grammar and may lag engine syntax/features.

use std::fmt;

use sqlite3_parser::{
    Bump, FallibleIterator,
    ast::{
        Cmd, Expr, IndexedColumn, Name, QualifiedName, Select, SortedColumn, Stmt, TriggerCmd,
        TriggerEvent, TriggerTime,
    },
    lexer::sql::Parser,
};

use crate::schema::DatabaseObject;
use sqlite3_parser::ast::CreateTableBody;

/// SQLite schema object category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaObjectKind {
    /// Ordinary or virtual table.
    Table,
    /// View.
    View,
    /// Explicit index.
    Index,
    /// Trigger (SQLite gives triggers a separate name namespace).
    Trigger,
}

/// SQLite schema identity. Names and schemas compare using SQLite's ASCII
/// case-insensitive identifier rules. Table, view, and index share a namespace;
/// triggers are independent. Unqualified CREATE INDEX and CREATE TRIGGER names
/// default to `main` here; SQLite may infer their schema from the referenced
/// table, so callers needing attached-database identity should qualify them.
#[derive(Clone, Debug)]
pub struct SchemaObjectId {
    schema: String,
    name: String,
    namespace: SchemaObjectKind,
}

impl PartialEq for SchemaObjectId {
    fn eq(&self, other: &Self) -> bool {
        self.namespace == other.namespace
            && self.schema.eq_ignore_ascii_case(&other.schema)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}
impl Eq for SchemaObjectId {}

/// One SQLite object created by a single CREATE statement.
///
/// Content equality compares kind, identity, and CREATE SQL exactly. Identity
/// comparison (`DatabaseObject::id`) is case-insensitive and ignores table /
/// view / index kind because those occupy one SQLite namespace. SQL is retained
/// as supplied (apart from surrounding whitespace and a trailing semicolon),
/// so formatting-only changes count as content modifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaObject {
    kind: SchemaObjectKind,
    id: SchemaObjectId,
    sql: String,
}

/// Borrowed, typed CREATE definition backed by sqlite3-parser's AST.
///
/// Table bodies expose parser `CreateTableBody::ColumnsAndConstraints` and
/// `CreateTableBody::AsSelect`; each column/table constraint retains the
/// parser's typed clause. `Table` and `VirtualTable` are distinct variants.
// Retain typed clauses even where the additive planner only uses object identity.
#[allow(dead_code)]
#[derive(Debug)]
pub enum SchemaObjectDefinition<'a> {
    /// Ordinary CREATE TABLE.
    Table {
        temporary: bool,
        if_not_exists: bool,
        name: QualifiedName<'a>,
        body: CreateTableBody<'a>,
    },
    /// CREATE VIRTUAL TABLE.
    VirtualTable {
        if_not_exists: bool,
        name: QualifiedName<'a>,
        module: Name<'a>,
        arguments: Option<&'a [&'a str]>,
    },
    /// CREATE VIEW.
    View {
        temporary: bool,
        if_not_exists: bool,
        name: QualifiedName<'a>,
        columns: Option<&'a [IndexedColumn<'a>]>,
        query: &'a Select<'a>,
    },
    /// CREATE INDEX.
    Index {
        unique: bool,
        if_not_exists: bool,
        name: QualifiedName<'a>,
        table: Name<'a>,
        columns: &'a [SortedColumn<'a>],
        predicate: Option<Expr<'a>>,
    },
    /// CREATE TRIGGER.
    Trigger {
        temporary: bool,
        if_not_exists: bool,
        name: QualifiedName<'a>,
        time: Option<TriggerTime>,
        event: TriggerEvent<'a>,
        table: QualifiedName<'a>,
        for_each_row: bool,
        when: Option<Expr<'a>>,
        commands: &'a [TriggerCmd<'a>],
    },
}

impl SchemaObject {
    /// Parse and retain a CREATE TABLE, VIRTUAL TABLE, VIEW, INDEX, or TRIGGER.
    pub fn from_sql(sql: impl Into<String>) -> Result<Self, SchemaError> {
        let sql = sql.into().trim().trim_end_matches(';').trim().to_owned();
        let bump = Bump::new();
        use SchemaObjectDefinition::*;
        let (kind, qualified, temporary) = match parse(&sql, &bump)? {
            Table {
                name, temporary, ..
            } => (SchemaObjectKind::Table, name, temporary),
            VirtualTable { name, .. } => (SchemaObjectKind::Table, name, false),
            View {
                name, temporary, ..
            } => (SchemaObjectKind::View, name, temporary),
            Index { name, .. } => (SchemaObjectKind::Index, name, false),
            Trigger {
                name, temporary, ..
            } => (SchemaObjectKind::Trigger, name, temporary),
        };
        let schema = qualified.db_name.map_or_else(
            || if temporary { "temp" } else { "main" }.into(),
            |name| unquote(name.0),
        );
        let name = unquote(qualified.name.0);
        let namespace = if kind == SchemaObjectKind::Trigger {
            kind
        } else {
            SchemaObjectKind::Table
        };
        // Preserve exact CREATE content for use by CREATE operators.
        Ok(Self {
            kind,
            id: SchemaObjectId {
                schema,
                name,
                namespace,
            },
            sql,
        })
    }

    /// Reparse retained SQL into a typed definition borrowing from this object
    /// and `bump`. The parser checks grammar, not SQLite engine semantics.
    pub fn definition<'a>(
        &'a self,
        bump: &'a Bump,
    ) -> Result<SchemaObjectDefinition<'a>, SchemaError> {
        parse(&self.sql, bump)
    }

    /// Object category.
    pub fn kind(&self) -> SchemaObjectKind {
        self.kind
    }

    /// Schema name (`main`, `temp`, or an attached database name).
    #[cfg(test)]
    pub fn schema(&self) -> &str {
        &self.id.schema
    }

    /// Object name.
    pub fn name(&self) -> &str {
        &self.id.name
    }

    /// Original CREATE statement, with trailing semicolons trimmed.
    pub fn to_sql(&self) -> String {
        self.sql.clone()
    }
}

fn parse<'a>(sql: &'a str, bump: &'a Bump) -> Result<SchemaObjectDefinition<'a>, SchemaError> {
    let mut parser = Parser::new(bump, sql.as_bytes());
    let command = parser
        .next()
        .map_err(|e| SchemaError(e.to_string()))?
        .ok_or_else(|| SchemaError("empty CREATE statement".into()))?;
    if parser
        .next()
        .map_err(|e| SchemaError(e.to_string()))?
        .is_some()
    {
        return Err(SchemaError("expected exactly one CREATE statement".into()));
    }
    match command {
        Cmd::Stmt(Stmt::CreateTable {
            temporary,
            if_not_exists,
            tbl_name,
            body,
        }) => Ok(SchemaObjectDefinition::Table {
            temporary,
            if_not_exists,
            name: tbl_name,
            body,
        }),
        Cmd::Stmt(Stmt::CreateVirtualTable {
            if_not_exists,
            tbl_name,
            module_name,
            args,
        }) => Ok(SchemaObjectDefinition::VirtualTable {
            if_not_exists,
            name: tbl_name,
            module: module_name,
            arguments: args,
        }),
        Cmd::Stmt(Stmt::CreateView {
            temporary,
            if_not_exists,
            view_name,
            columns,
            select,
        }) => Ok(SchemaObjectDefinition::View {
            temporary,
            if_not_exists,
            name: view_name,
            columns,
            query: select,
        }),
        Cmd::Stmt(Stmt::CreateIndex {
            unique,
            if_not_exists,
            idx_name,
            tbl_name,
            columns,
            where_clause,
        }) => Ok(SchemaObjectDefinition::Index {
            unique,
            if_not_exists,
            name: idx_name,
            table: tbl_name,
            columns,
            predicate: where_clause,
        }),
        Cmd::Stmt(Stmt::CreateTrigger {
            temporary,
            if_not_exists,
            trigger_name,
            time,
            event,
            tbl_name,
            for_each_row,
            when_clause,
            commands,
        }) => Ok(SchemaObjectDefinition::Trigger {
            temporary,
            if_not_exists,
            name: trigger_name,
            time,
            event,
            table: tbl_name,
            for_each_row,
            when: when_clause,
            commands,
        }),
        _ => Err(SchemaError(
            "expected CREATE TABLE, VIRTUAL TABLE, VIEW, INDEX, or TRIGGER".into(),
        )),
    }
}

impl DatabaseObject for SchemaObject {
    type Id = SchemaObjectId;
    fn id(&self) -> Self::Id {
        self.id.clone()
    }
}

/// Error while parsing or validating a CREATE statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaError(pub String);
impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SchemaError {}

pub(super) fn unquote(name: &str) -> String {
    let bytes = name.as_bytes();
    if bytes.len() >= 2 {
        let close = match bytes[0] {
            b'[' => b']',
            b'"' | b'`' | b'\'' => bytes[0],
            _ => 0,
        };
        if close != 0 && bytes[bytes.len() - 1] == close {
            let inner = &name[1..name.len() - 1];
            let escaped = [char::from(close), char::from(close)]
                .iter()
                .collect::<String>();
            return inner.replace(&escaped, &char::from(close).to_string());
        }
    }
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use sqlite3_parser::ast::ColumnConstraint;

    #[test]
    fn identities_follow_sqlite_namespaces_and_schemas() {
        let table = SchemaObject::from_sql("CREATE TABLE Foo (id INTEGER PRIMARY KEY)").unwrap();
        let view = SchemaObject::from_sql("CREATE VIEW foo AS SELECT 1").unwrap();
        let trigger =
            SchemaObject::from_sql("CREATE TRIGGER foo AFTER INSERT ON Foo BEGIN SELECT 1; END")
                .unwrap();
        assert_eq!(table.id(), view.id());
        assert_ne!(table.id(), trigger.id());
        assert_ne!(
            table.id(),
            SchemaObject::from_sql("CREATE TABLE aux.foo (id)")
                .unwrap()
                .id()
        );
        assert_eq!(
            SchemaObject::from_sql("CREATE TEMP TABLE x (id)")
                .unwrap()
                .schema(),
            "temp"
        );
    }

    #[test]
    fn retains_all_create_clauses_and_executes_in_bundled_sqlite() {
        let sql = "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, parent INTEGER REFERENCES p(id) ON DELETE CASCADE, name TEXT NOT NULL UNIQUE CHECK(length(name)>0) DEFAULT 'x', CONSTRAINT uq UNIQUE(parent,name), CHECK(id > 0)) STRICT";
        let object = SchemaObject::from_sql(sql).unwrap();
        assert_eq!(object.to_sql(), sql);
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE p(id);").unwrap();
        db.execute_batch(&object.to_sql()).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='items'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn parses_virtual_tables_partial_indexes_views_and_triggers() {
        for sql in [
            "CREATE VIRTUAL TABLE docs USING fts5(body)",
            "CREATE INDEX ix ON docs(body) WHERE body <> ''",
            "CREATE VIEW v AS SELECT body FROM docs",
            "CREATE TRIGGER tr AFTER INSERT ON docs BEGIN UPDATE docs SET body=body; END",
        ] {
            let obj = SchemaObject::from_sql(sql).unwrap();
            assert_eq!(obj.to_sql(), sql);
        }
    }

    #[test]
    fn exposes_typed_table_constraints_and_virtual_table_definition() {
        let table = SchemaObject::from_sql(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, parent INT REFERENCES p(id), name TEXT NOT NULL CHECK(length(name)>0), UNIQUE(parent,name), CHECK(id > 0))",
        )
        .unwrap();
        let bump = Bump::new();
        let SchemaObjectDefinition::Table { body, .. } = table.definition(&bump).unwrap() else {
            panic!("expected ordinary table");
        };
        let CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints,
            ..
        } = body
        else {
            panic!("expected column definitions");
        };
        assert_eq!(columns.len(), 3);
        assert!(matches!(
            &columns[1].constraints[0].constraint,
            ColumnConstraint::ForeignKey { .. }
        ));
        assert!(matches!(
            &columns[2].constraints[0].constraint,
            ColumnConstraint::NotNull { .. }
        ));
        assert_eq!(constraints.unwrap().len(), 2);

        let virtual_table =
            SchemaObject::from_sql("CREATE VIRTUAL TABLE docs USING fts5(body)").unwrap();
        assert!(matches!(
            virtual_table.definition(&bump).unwrap(),
            SchemaObjectDefinition::VirtualTable { .. }
        ));
    }
}
