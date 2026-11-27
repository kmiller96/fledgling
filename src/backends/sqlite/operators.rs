//! SQLite schema DDL operators.
//!
//! Identifiers are quoted by these renderers. `column_definition` and
//! `expression` are SQL fragments, not identifiers; callers must provide
//! trusted, syntactically valid SQLite SQL for those fields.
//! SQLite has no `ALTER INDEX` or general constraint/column mutation syntax.
//! SQLite 3.53 adds narrow named-CHECK and NOT NULL operations; other
//! constraint and column mutations require an explicit table rebuild, which
//! this module does not plan automatically.

use super::schema::{SchemaObject, SchemaObjectKind};

/// A supported SQLite schema operation.
// The backend can render more operations than the additive planner emits.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operator {
    /// Create a table, virtual table, view, index, or trigger.
    Create(SchemaObject),
    /// Drop a named schema object.
    Drop {
        kind: SchemaObjectKind,
        name: QualifiedName,
        if_exists: bool,
    },
    RenameTable {
        table: QualifiedName,
        new_name: String,
    },
    RenameColumn {
        table: QualifiedName,
        column: String,
        new_name: String,
    },
    AddColumn {
        table: QualifiedName,
        /// Complete column declaration, e.g. `"email" TEXT NOT NULL`.
        column_definition: String,
    },
    DropColumn {
        table: QualifiedName,
        column: String,
    },
    /// Add a named CHECK constraint (SQLite 3.53+).
    AddCheckConstraint {
        table: QualifiedName,
        name: String,
        /// CHECK expression contents, without the surrounding parentheses.
        expression: String,
        on_conflict: Option<ConflictAction>,
    },
    /// Drop a named CHECK constraint (SQLite 3.53+).
    DropConstraint { table: QualifiedName, name: String },
    /// SQLite 3.53+ `ALTER COLUMN ... SET NOT NULL`.
    SetNotNull {
        table: QualifiedName,
        column: String,
        on_conflict: Option<ConflictAction>,
    },
    /// SQLite 3.53+ `ALTER COLUMN ... DROP NOT NULL`.
    DropNotNull {
        table: QualifiedName,
        column: String,
    },
    /// Rebuild all indexes, or indexes selected by table, index or collation name.
    Reindex(Option<QualifiedName>),
    /// Rebuild all expression indexes (SQLite 3.53+).
    ReindexExpressions,
}

/// An identifier optionally qualified by an explicit SQLite schema
/// (`main`, `temp`, or an attached database name). A dot inside either
/// component is ordinary identifier data, never parsed as qualification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualifiedName {
    pub schema: Option<String>,
    pub name: String,
}

impl QualifiedName {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    #[cfg(test)]
    pub fn in_schema(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn to_sql(&self) -> String {
        match &self.schema {
            Some(schema) => format!(
                "{}.{}",
                quote_identifier(schema),
                quote_identifier(&self.name)
            ),
            None => quote_identifier(&self.name),
        }
    }
}

/// SQLite constraint conflict resolution policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ConflictAction {
    Rollback,
    Abort,
    Fail,
    Ignore,
    Replace,
}

impl Operator {
    /// Render this operation as one SQLite statement.
    pub fn to_sql(&self) -> String {
        match self {
            // A trailing line comment must not swallow the terminator.
            Self::Create(object) => format!("{}\n;", object.to_sql()),
            Self::Drop {
                kind,
                name,
                if_exists,
            } => format!(
                "DROP {}{} {};",
                kind.sql_name(),
                if *if_exists { " IF EXISTS" } else { "" },
                name.to_sql()
            ),
            Self::RenameTable { table, new_name } => format!(
                "ALTER TABLE {} RENAME TO {};",
                table.to_sql(),
                quote_identifier(new_name)
            ),
            Self::RenameColumn {
                table,
                column,
                new_name,
            } => format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                table.to_sql(),
                quote_identifier(column),
                quote_identifier(new_name)
            ),
            Self::AddColumn {
                table,
                column_definition,
            } => format!(
                "ALTER TABLE {} ADD COLUMN {};",
                table.to_sql(),
                column_definition
            ),
            Self::DropColumn { table, column } => format!(
                "ALTER TABLE {} DROP COLUMN {};",
                table.to_sql(),
                quote_identifier(column)
            ),
            Self::AddCheckConstraint {
                table,
                name,
                expression,
                on_conflict,
            } => format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({}){};",
                table.to_sql(),
                quote_identifier(name),
                expression,
                render_conflict(*on_conflict)
            ),
            Self::DropConstraint { table, name } => format!(
                "ALTER TABLE {} DROP CONSTRAINT {};",
                table.to_sql(),
                quote_identifier(name)
            ),
            Self::SetNotNull {
                table,
                column,
                on_conflict,
            } => format!(
                "ALTER TABLE {} ALTER COLUMN {} SET NOT NULL{};",
                table.to_sql(),
                quote_identifier(column),
                render_conflict(*on_conflict)
            ),
            Self::DropNotNull { table, column } => format!(
                "ALTER TABLE {} ALTER COLUMN {} DROP NOT NULL;",
                table.to_sql(),
                quote_identifier(column)
            ),
            Self::Reindex(None) => "REINDEX;".to_owned(),
            Self::Reindex(Some(name)) => format!("REINDEX {};", name.to_sql()),
            Self::ReindexExpressions => "REINDEX EXPRESSIONS;".to_owned(),
        }
    }
}

impl SchemaObjectKind {
    const fn sql_name(self) -> &'static str {
        match self {
            Self::Table => "TABLE",
            Self::View => "VIEW",
            Self::Index => "INDEX",
            Self::Trigger => "TRIGGER",
        }
    }
}

fn render_conflict(action: Option<ConflictAction>) -> String {
    match action {
        None => String::new(),
        Some(action) => format!(
            " ON CONFLICT {}",
            match action {
                ConflictAction::Rollback => "ROLLBACK",
                ConflictAction::Abort => "ABORT",
                ConflictAction::Fail => "FAIL",
                ConflictAction::Ignore => "IGNORE",
                ConflictAction::Replace => "REPLACE",
            }
        ),
    }
}

pub(super) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{ConflictAction, Operator, QualifiedName, SchemaObjectKind};
    use rusqlite::Connection;

    #[test]
    fn quotes_identifiers_and_renders_drop() {
        assert_eq!(
            Operator::Drop {
                kind: SchemaObjectKind::Index,
                name: QualifiedName::new("odd\" index"),
                if_exists: true,
            }
            .to_sql(),
            "DROP INDEX IF EXISTS \"odd\"\" index\";"
        );
    }

    #[test]
    fn renders_native_alter_and_reindex_forms() {
        assert_eq!(
            Operator::RenameColumn {
                table: QualifiedName::new("user"),
                column: "old".into(),
                new_name: "new".into(),
            }
            .to_sql(),
            "ALTER TABLE \"user\" RENAME COLUMN \"old\" TO \"new\";"
        );
        assert_eq!(
            Operator::AddColumn {
                table: QualifiedName::new("user"),
                column_definition: "\"active\" INTEGER NOT NULL DEFAULT 1".into(),
            }
            .to_sql(),
            "ALTER TABLE \"user\" ADD COLUMN \"active\" INTEGER NOT NULL DEFAULT 1;"
        );
        assert_eq!(
            Operator::Reindex(Some(QualifiedName::new("by_name"))).to_sql(),
            "REINDEX \"by_name\";"
        );
    }

    #[test]
    fn renders_sqlite_353_constraint_and_not_null_forms() {
        assert_eq!(
            Operator::AddCheckConstraint {
                table: QualifiedName::new("items"),
                name: "positive".into(),
                expression: "amount > 0".into(),
                on_conflict: Some(ConflictAction::Abort),
            }
            .to_sql(),
            "ALTER TABLE \"items\" ADD CONSTRAINT \"positive\" CHECK (amount > 0) ON CONFLICT ABORT;"
        );
        assert_eq!(
            Operator::DropConstraint {
                table: QualifiedName::new("items"),
                name: "positive".into(),
            }
            .to_sql(),
            "ALTER TABLE \"items\" DROP CONSTRAINT \"positive\";"
        );
        assert_eq!(
            Operator::SetNotNull {
                table: QualifiedName::new("items"),
                column: "amount".into(),
                on_conflict: Some(ConflictAction::Fail),
            }
            .to_sql(),
            "ALTER TABLE \"items\" ALTER COLUMN \"amount\" SET NOT NULL ON CONFLICT FAIL;"
        );
        assert_eq!(
            Operator::DropNotNull {
                table: QualifiedName::new("items"),
                column: "amount".into(),
            }
            .to_sql(),
            "ALTER TABLE \"items\" ALTER COLUMN \"amount\" DROP NOT NULL;"
        );
    }

    #[test]
    fn qualification_is_explicit_and_quotes_each_component() {
        let table = QualifiedName::in_schema("attached.db", "items.with.dot");
        assert_eq!(
            Operator::DropColumn {
                table,
                column: "value".into(),
            }
            .to_sql(),
            "ALTER TABLE \"attached.db\".\"items.with.dot\" DROP COLUMN \"value\";"
        );
    }

    #[test]
    fn executes_constraint_changes_and_reindexes_on_bundled_sqlite() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE items (amount INTEGER);")
            .unwrap();
        let table = QualifiedName::in_schema("main", "items");
        for op in [
            Operator::AddCheckConstraint {
                table: table.clone(),
                name: "positive".into(),
                expression: "amount > 0".into(),
                on_conflict: Some(ConflictAction::Abort),
            },
            Operator::SetNotNull {
                table: table.clone(),
                column: "amount".into(),
                on_conflict: None,
            },
        ] {
            db.execute_batch(&op.to_sql()).unwrap();
        }
        assert!(db.execute_batch("INSERT INTO items VALUES (NULL)").is_err());
        assert!(db.execute_batch("INSERT INTO items VALUES (0)").is_err());

        for op in [
            Operator::DropConstraint {
                table: table.clone(),
                name: "positive".into(),
            },
            Operator::DropNotNull {
                table,
                column: "amount".into(),
            },
            Operator::ReindexExpressions,
        ] {
            db.execute_batch(&op.to_sql()).unwrap();
        }
        db.execute_batch("INSERT INTO items VALUES (NULL), (0)")
            .unwrap();
    }
}
