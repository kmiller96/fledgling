//! Definitions and identities for objects in SQLite's main schema.

use crate::schema::Object;
use sqlite3_parser::{
    Bump, FallibleIterator,
    ast::{Cmd, CreateTableBody, QualifiedName, Stmt},
    lexer::sql::Parser,
};

use super::ledger::LEDGER_NAME;
pub use crate::backends::errors::ObjectError;

// ----------------------------------------------------------------------------
// Object kind and identity
// ----------------------------------------------------------------------------

// Declare each kind alongside the parser variant and fields that identify it.
macro_rules! define_objects {
    ($($kind:ident => $stmt:ident { $name:ident $(, $field:ident: $pattern:pat)* }),+ $(,)?) => {
        /// The kind of schema object described by a CREATE statement.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum SqliteObjectKind {
            $($kind,)+
        }

        impl SqliteObjectKind {
            fn from_create(command: Cmd<'_>) -> Option<(Self, QualifiedName<'_>)> {
                match command {
                    $(
                        Cmd::Stmt(Stmt::$stmt { $name $(, $field: $pattern)*, .. }) =>
                            Some((Self::$kind, $name)),
                    )+
                    _ => None,
                }
            }
        }
    };
}

define_objects! {
    Table        => CreateTable        { tbl_name, temporary: false },
    VirtualTable => CreateVirtualTable { tbl_name },
    Index        => CreateIndex        { idx_name },
    View         => CreateView         { view_name, temporary: false },
    Trigger      => CreateTrigger      { trigger_name, temporary: false },
}

/// ASCII case-insensitive identity within `main`.
///
/// Tables, views, and indexes share a namespace; triggers have their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteObjectId {
    name: String,
    trigger: bool,
}

// ----------------------------------------------------------------------------
// Object
// ----------------------------------------------------------------------------

/// One object with its original CREATE SQL. Content equality is textual;
/// formatting-only edits can still differ even when identities match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteObject {
    kind: SqliteObjectKind,
    name: String,
    sql: String,
}

impl SqliteObject {
    /// Parse exactly one CREATE statement. Rejects temporary objects, objects
    /// outside `main`, and reserved names. SQLite itself checks engine semantics.
    pub fn from_sql(sql: &str) -> Result<Self, ObjectError> {
        let sql = sql.trim();
        if sql.contains('\0') {
            return Err(ObjectError("SQL contains a NUL byte".into()));
        }
        let bump = Bump::new();
        let mut parser = Parser::new(&bump, sql.as_bytes());
        let command = parser
            .next()
            .map_err(|e| ObjectError(e.to_string()))?
            .and_then(SqliteObjectKind::from_create);
        let (kind, qualified) = command.ok_or_else(|| {
            ObjectError(
                "expected CREATE TABLE, VIRTUAL TABLE, INDEX, VIEW, or TRIGGER in main".into(),
            )
        })?;
        if parser
            .next()
            .map_err(|e| ObjectError(e.to_string()))?
            .is_some()
        {
            return Err(ObjectError("expected exactly one CREATE statement".into()));
        }
        Ok(Self {
            kind,
            name: main_name(qualified)?,
            sql: sql.to_owned(),
        })
    }

    /// Return the CREATE statement's object kind.
    pub fn kind(&self) -> SqliteObjectKind {
        self.kind
    }

    /// Return the identifier without quoting or schema qualification.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the original CREATE SQL, with surrounding whitespace trimmed.
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

fn main_name(qualified: QualifiedName<'_>) -> Result<String, ObjectError> {
    if qualified
        .db_name
        .is_some_and(|schema| !unquote(schema.0).eq_ignore_ascii_case("main"))
    {
        return Err(ObjectError(
            "only the main SQLite schema is supported".into(),
        ));
    }
    let name = unquote(qualified.name.0);
    if name.eq_ignore_ascii_case(LEDGER_NAME) || name.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(ObjectError(format!("reserved SQLite object name: {name}")));
    }
    Ok(name)
}

pub(super) fn validate_desired_sql(sql: &str) -> Result<(), ObjectError> {
    if sql.contains('\0') {
        return Err(ObjectError("SQL contains a NUL byte".into()));
    }
    let bump = Bump::new();
    let mut parser = Parser::new(&bump, sql.as_bytes());
    let mut seen = std::collections::HashSet::new();
    while let Some(command) = parser.next().map_err(|e| ObjectError(e.to_string()))? {
        let declaration = match command {
            Cmd::Stmt(Stmt::CreateTable {
                body: CreateTableBody::AsSelect(_),
                ..
            }) => None,
            command => SqliteObjectKind::from_create(command),
        };
        let (kind, qualified) = declaration.ok_or_else(|| {
            ObjectError("desired schema must contain only persistent CREATE declarations".into())
        })?;
        let name = main_name(qualified)?;
        if !seen.insert((name.to_ascii_lowercase(), kind == SqliteObjectKind::Trigger)) {
            return Err(ObjectError(format!(
                "duplicate object in desired schema: {name}"
            )));
        }
    }
    Ok(())
}

impl Object for SqliteObject {
    type Id = SqliteObjectId;

    fn id(&self) -> Self::Id {
        SqliteObjectId {
            name: self.name.to_ascii_lowercase(),
            trigger: self.kind == SqliteObjectKind::Trigger,
        }
    }
}

// ----------------------------------------------------------------------------
// Identifier handling
// ----------------------------------------------------------------------------

pub(super) fn unquote(name: &str) -> String {
    for (quote, escaped) in [('"', "\"\""), ('`', "``"), ('\'', "''")] {
        if let Some(inner) = name.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
            return inner.replace(escaped, &quote.to_string());
        }
    }
    name.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(name)
        .to_owned()
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_sqlite_namespaces_and_ascii_case() {
        let table = SqliteObject::from_sql("CREATE TABLE [MAIN].[It\"ems](id)").unwrap();
        let index = SqliteObject::from_sql("CREATE INDEX \"it\"\"ems\" ON t(id)").unwrap();
        let trigger = SqliteObject::from_sql(
            "CREATE TRIGGER [IT\"EMS] AFTER INSERT ON t BEGIN SELECT 1; END;",
        )
        .unwrap();
        assert_eq!(table.name(), "It\"ems");
        assert_eq!(table.id(), index.id());
        assert_ne!(table.id(), trigger.id());
    }

    #[test]
    fn rejects_non_schema_sql_and_non_main_objects() {
        for sql in [
            "CREATE TEMP TABLE t(x)",
            "CREATE TABLE other.t(x)",
            "CREATE TABLE __MIGRATIONS(x)",
            "CREATE TABLE t(x); DELETE FROM t",
        ] {
            assert!(SqliteObject::from_sql(sql).is_err(), "{sql}");
        }
    }
}
