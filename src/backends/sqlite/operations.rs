//! Native SQLite DDL operations for objects in `main`.

use std::convert::Infallible;

use super::objects::{SqliteObject, SqliteObjectKind};
use crate::migrations::Operation;

// ----------------------------------------------------------------------------
// Operations Macro
// ----------------------------------------------------------------------------

// Generate the enum and render match together, binding fields by reference.
macro_rules! define_operations {
    (
        $(#[$enum_attr:meta])*
        pub enum $name:ident ($prefix:ident) {
            $(
                $(#[$variant_attr:meta])*
                $variant:ident
                $(($arg:ident: $arg_ty:ty))?
                $({ $($field:ident: $field_ty:ty),+ $(,)? })?
                => $sql:expr
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_attr])*
        pub enum $name {
            $(
                $(#[$variant_attr])*
                $variant $(($arg_ty))? $({ $($field: $field_ty),+ })?,
            )+
        }

        impl Operation for $name {
            type Error = Infallible;

            fn render(&self) -> Result<String, Self::Error> {
                Ok(self.render_with_prefix("main."))
            }
        }

        impl $name {
            // The migration driver disallows temporary and attached schemas.
            pub(super) fn render_migration(&self) -> String {
                self.render_with_prefix("")
            }

            fn render_with_prefix(&self, $prefix: &str) -> String {
                match self {
                    $(Self::$variant $(($arg))? $({ $($field),+ })? => $sql,)+
                }
            }
        }
    };
}

// ----------------------------------------------------------------------------
// Operations
// ----------------------------------------------------------------------------

define_operations! {
    /// One executable SQLite schema change. Changes SQLite cannot express
    /// directly require a separate table rebuild.
    ///
    /// Pass identifiers unquoted. Column declarations and CHECK expressions are
    /// trusted SQL fragments instead. The engine validates SQL at execution time.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum SqliteOperation (prefix) {
        Create(object: SqliteObject) => {
            let sql = object.sql();
            if sql.ends_with(';') {
                sql.to_owned()
            } else {
                // A trailing line comment must not swallow the terminator.
                format!("{sql}\n;")
            }
        },
        Drop { kind: SqliteObjectKind, name: String } => {
            use SqliteObjectKind::*;
            let kind = match kind {
                Table | VirtualTable => "TABLE",
                Index => "INDEX",
                View => "VIEW",
                Trigger => "TRIGGER",
            };
            format!("DROP {kind} {prefix}{};", quote(name))
        },
        RenameTable { table: String, new_name: String } =>
            format!("ALTER TABLE {prefix}{} RENAME TO {};", quote(table), quote(new_name)),
        RenameColumn { table: String, column: String, new_name: String } => format!(
            "ALTER TABLE {prefix}{} RENAME COLUMN {} TO {};",
            quote(table),
            quote(column),
            quote(new_name),
        ),
        /// `definition` is a complete SQL column declaration.
        AddColumn { table: String, definition: String } =>
            format!("ALTER TABLE {prefix}{} ADD COLUMN {definition}\n;", quote(table)),
        DropColumn { table: String, column: String } =>
            format!("ALTER TABLE {prefix}{} DROP COLUMN {};", quote(table), quote(column)),
        /// Add a named CHECK constraint (SQLite 3.53+). Pass the expression
        /// without its surrounding parentheses.
        AddCheck { table: String, name: String, expression: String } => format!(
            "ALTER TABLE {prefix}{} ADD CONSTRAINT {} CHECK ({expression}\n);",
            quote(table),
            quote(name),
        ),
        /// Drop a named CHECK constraint (SQLite 3.53+).
        DropCheck { table: String, name: String } =>
            format!("ALTER TABLE {prefix}{} DROP CONSTRAINT {};", quote(table), quote(name)),
        /// Set or drop NOT NULL (SQLite 3.53+).
        SetNotNull { table: String, column: String, not_null: bool } => {
            let verb = if *not_null { "SET" } else { "DROP" };
            format!("ALTER TABLE {prefix}{} ALTER COLUMN {} {verb} NOT NULL;", quote(table), quote(column))
        },
    }
}

// ----------------------------------------------------------------------------
// SQL rendering helpers
// ----------------------------------------------------------------------------

pub(super) fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
