//! PostgreSQL execution steps lowered from logical schema changes.

use std::convert::Infallible;

use crate::migrations::Operation;

use super::{
    objects::{Column, Kind, Name, PostgresObject},
    quote,
};

pub(super) enum PostgresOperation<'a> {
    Create(&'a PostgresObject),
    Drop(Kind, &'a Name),
    AddColumn(&'a Name, &'a Column),
    DropColumn(&'a Name, &'a Column),
}

impl Operation for PostgresOperation<'_> {
    type Error = Infallible;

    fn render(&self) -> Result<String, Self::Error> {
        Ok(match self {
            Self::Create(object) => format!("{};", object.create_sql()),
            Self::Drop(kind, name) => format!("DROP {} {};", kind.sql(), name.sql()),
            Self::AddColumn(table, column) => {
                format!(
                    "ALTER TABLE {} ADD COLUMN {};",
                    table.sql(),
                    column.definition
                )
            }
            Self::DropColumn(table, column) => {
                format!(
                    "ALTER TABLE {} DROP COLUMN {};",
                    table.sql(),
                    quote(&column.name)
                )
            }
        })
    }
}
