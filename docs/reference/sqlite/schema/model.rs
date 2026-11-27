//! SQLite's typed schema and additive migration planning.
//!
//! The model represents only what the additive planner can compare and render;
//! it is not a complete SQLite SQL AST.

use super::{validate_and_order_views, validate_schema, validate_view_select_shape, view_match};
use crate::{
    schema::{Change, Definition as SharedDefinition},
    sqlite::ddl::Ddl,
};
use std::{error::Error, fmt};

// ----------------------------------------------------------------------------
// Typed schema model
// ----------------------------------------------------------------------------

/// Table definition with columns in declaration order and an optional table CHECK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) check: Option<String>,
}

/// Column shape supported by the model. SQL fragments such as CHECK and
/// defaults are retained for dialect comparison and rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) declared_type: Option<String>,
    pub(crate) not_null: bool,
    pub(crate) check: Option<String>,
    /// An authored `DEFAULT NULL`, distinct for rendering from an omitted default.
    pub(crate) default_null: bool,
    /// Non-NULL default SQL, absent if `default_null` is set.
    pub(crate) default_value: Option<String>,
    pub(crate) primary_key: bool,
}

/// Explicit ordering of an index key; omission is represented separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortDirection {
    Asc,
    Desc,
}

/// Named index over a table, with keys in declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) unique: bool,
    pub(crate) columns: Vec<IndexColumn>,
}

/// Named view with a dialect SQL fragment for its defining query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub(crate) name: String,
    pub(crate) select: String,
}

/// One named object in SQLite's modeled schema namespace.
#[derive(Clone, Debug)]
pub(crate) enum SqliteObject {
    Table(Table),
    View(View),
    Index(Index),
}

impl SqliteObject {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Table(value) => &value.name,
            Self::View(value) => &value.name,
            Self::Index(value) => &value.name,
        }
    }
}

impl crate::schema::DatabaseObject for SqliteObject {
    type Id = String;

    /// Use the ASCII-folded name across all modeled object kinds.
    fn id(&self) -> Self::Id {
        self.name().to_ascii_lowercase()
    }
}

impl PartialEq for SqliteObject {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Table(a), Self::Table(b)) => tables_match(a, b),
            (Self::View(a), Self::View(b)) => view_match(a, b),
            (Self::Index(a), Self::Index(b)) => indexes_match(a, b),
            _ => false,
        }
    }
}

impl Eq for SqliteObject {}

fn tables_match(a: &Table, b: &Table) -> bool {
    a.name.eq_ignore_ascii_case(&b.name)
        && a.check == b.check
        && a.columns.len() == b.columns.len()
        && a.columns.iter().zip(&b.columns).all(|(x, y)| {
            x.name.eq_ignore_ascii_case(&y.name)
                && x.not_null == y.not_null
                && x.check == y.check
                && x.default_value == y.default_value
                && x.primary_key == y.primary_key
                && match (&x.declared_type, &y.declared_type) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => true,
                    _ => false,
                }
        })
}

/// The second definition of a name under SQLite's identity rules.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DuplicateDefinition {
    /// Zero-based index in the original definition list.
    pub(crate) index: usize,
}

impl std::fmt::Display for DuplicateDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "duplicate definition at position {}", self.index + 1)
    }
}

/// SQLite objects in declaration order, with duplicate names rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Schema {
    definitions: SharedDefinition<SqliteObject>,
}

// ----------------------------------------------------------------------------
// Preview diagnostics and schema access
// ----------------------------------------------------------------------------

/// Rejections while turning desired SQL into a migration plan.
#[derive(Debug)]
pub enum PreviewError {
    /// Desired SQL could not be parsed or lowered into the supported model.
    InvalidDesiredSql(String),
    /// A specifically diagnosed change, such as removing a table or column.
    BlockedChange(String),
    /// Generic planning rejection retained for API compatibility.
    UnsupportedPlan,
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDesiredSql(error) => write!(f, "invalid desired schema SQL: {error}"),
            Self::BlockedChange(reason) => write!(f, "blocked schema change: {reason}"),
            Self::UnsupportedPlan => write!(f, "desired schema change is not supported"),
        }
    }
}
impl Error for PreviewError {}

/// Report removals and changes to existing CHECK constraints before planning.
/// Passing this check does not establish plan support.
pub(super) fn blocked_changes(actual: &Schema, desired: &Schema) -> Result<(), PreviewError> {
    let removed_tables: Vec<_> = actual
        .tables()
        .filter(|old| {
            !desired
                .tables()
                .any(|new| new.name.eq_ignore_ascii_case(&old.name))
        })
        .map(|item| item.name.as_str())
        .collect();
    if !removed_tables.is_empty() {
        let noun = if removed_tables.len() == 1 {
            "table"
        } else {
            "tables"
        };
        return Err(PreviewError::BlockedChange(format!(
            "removing {noun} {} is not supported",
            removed_tables.join(", ")
        )));
    }
    for old in actual.tables() {
        if let Some(new) = desired
            .tables()
            .find(|item| item.name.eq_ignore_ascii_case(&old.name))
        {
            if let Some(column) = old.columns.iter().find(|column| {
                !new.columns
                    .iter()
                    .any(|item| item.name.eq_ignore_ascii_case(&column.name))
            }) {
                return Err(PreviewError::BlockedChange(format!(
                    "removing column {} from table {} is not supported",
                    column.name, old.name
                )));
            }
            if old.check != new.check
                || old.columns.iter().any(|old_column| {
                    new.columns
                        .iter()
                        .find(|column| column.name.eq_ignore_ascii_case(&old_column.name))
                        .is_some_and(|new_column| old_column.check != new_column.check)
                })
            {
                return Err(PreviewError::BlockedChange(format!(
                    "changing CHECK constraint on table {} is not supported",
                    old.name
                )));
            }
        }
    }
    if let Some(view) = actual.views().find(|old| {
        !desired
            .views()
            .any(|new| new.name.eq_ignore_ascii_case(&old.name))
    }) {
        return Err(PreviewError::BlockedChange(format!(
            "removing view {} is not supported",
            view.name
        )));
    }
    let removed: Vec<_> = actual
        .indexes()
        .filter(|old| {
            !desired
                .indexes()
                .any(|new| new.name.eq_ignore_ascii_case(&old.name))
        })
        .map(|item| item.name.as_str())
        .collect();
    if !removed.is_empty() {
        let noun = if removed.len() == 1 {
            "index"
        } else {
            "indexes"
        };
        return Err(PreviewError::BlockedChange(format!(
            "removing {noun} {} is not supported",
            removed.join(", ")
        )));
    }
    Ok(())
}

impl Schema {
    /// Preserve input order and reject the first repeated name, even across object kinds.
    pub(crate) fn try_new(definitions: Vec<SqliteObject>) -> Result<Self, DuplicateDefinition> {
        let mut names = std::collections::HashSet::with_capacity(definitions.len());
        for (index, definition) in definitions.iter().enumerate() {
            if !names.insert(definition.name().to_ascii_lowercase()) {
                return Err(DuplicateDefinition { index });
            }
        }
        Ok(Self {
            definitions: SharedDefinition::new(definitions),
        })
    }
    #[cfg(test)]
    pub(crate) fn definitions(&self) -> &[SqliteObject] {
        self.definitions.objects()
    }
    pub(crate) fn tables(&self) -> impl Iterator<Item = &Table> {
        self.definitions.objects().iter().filter_map(|d| {
            if let SqliteObject::Table(v) = d {
                Some(v)
            } else {
                None
            }
        })
    }
    pub(crate) fn views(&self) -> impl Iterator<Item = &View> {
        self.definitions.objects().iter().filter_map(|d| {
            if let SqliteObject::View(v) = d {
                Some(v)
            } else {
                None
            }
        })
    }
    pub(crate) fn indexes(&self) -> impl Iterator<Item = &Index> {
        self.definitions.objects().iter().filter_map(|d| {
            if let SqliteObject::Index(v) = d {
                Some(v)
            } else {
                None
            }
        })
    }
}

// ----------------------------------------------------------------------------
// Additive diff and dependency order
// ----------------------------------------------------------------------------

/// Seed the readiness environment with unchanged views, then order additions.
/// `ready` must make a successful view visible to later calls and leave failed
/// attempts unavailable so they can be retried. Only additions are returned.
pub(super) fn order_views<'a>(
    existing: &[&'a View],
    additions: &[&'a View],
    mut ready: impl FnMut(&View) -> Result<(), String>,
) -> Result<Vec<&'a View>, String> {
    let mut ordered = Vec::new();
    let mut pending = existing.to_vec();
    order_view_batch(&mut pending, &mut ordered, &mut ready, false)?;
    pending = additions.to_vec();
    order_view_batch(&mut pending, &mut ordered, &mut ready, true)?;
    Ok(ordered)
}

/// Validate and compare two SQLite schemas before planning their transition.
pub(super) fn plan_schema(actual: &Schema, desired: &Schema) -> Result<Vec<Ddl>, String> {
    validate_schema(actual)?;
    validate_schema(desired)?;
    let plan = actual.definitions.difference(&desired.definitions);
    plan_changes(&plan, actual, desired)
}

/// Translate a batch of changes into ordered SQLite DDL.
///
/// Accept new tables, indexes, and views, and eligible column appends. Emit
/// table operations first, then indexes, then views in dependency order.
fn plan_changes(
    changes: &[Change<'_, SqliteObject>],
    actual: &Schema,
    desired: &Schema,
) -> Result<Vec<Ddl>, String> {
    let mut new_indexes: Vec<&Index> = Vec::new();
    for change in changes {
        match change {
            Change::Added(SqliteObject::Index(index)) => new_indexes.push(index),
            Change::Modified {
                before: SqliteObject::Index(old),
                ..
            }
            | Change::Removed(SqliteObject::Index(old)) => {
                return Err(format!(
                    "changing or removing index {} is not supported",
                    old.name
                ));
            }
            _ => {}
        }
    }
    new_indexes.sort_by_key(|index| (&index.table, &index.name));

    let mut new_views: Vec<&View> = Vec::new();
    for change in changes {
        match change {
            Change::Added(SqliteObject::View(view)) => new_views.push(view),
            Change::Modified {
                before: SqliteObject::View(old),
                ..
            }
            | Change::Removed(SqliteObject::View(old)) => {
                return Err(format!(
                    "changing or removing view {} is not supported",
                    old.name
                ));
            }
            _ => {}
        }
    }
    // The diff omits unchanged views, but new views may depend on them.
    let existing_views: Vec<_> = actual.views().collect();

    let table_operations = additive_table_changes(changes)?;

    for view in &new_views {
        validate_view_select_shape(view)
            .map_err(|reason| format!("view {}: {reason}", view.name))?;
    }
    let ordered_views = if desired.views().next().is_none() {
        Vec::new()
    } else {
        validate_and_order_views(desired, &existing_views, &new_views)?
    };
    Ok(table_operations
        .into_iter()
        .chain(new_indexes.into_iter().cloned().map(Ddl::CreateIndex))
        .chain(ordered_views.into_iter().map(Ddl::CreateView))
        .collect())
}

/// Retry deferred views in input order; a pass with no progress means a cycle,
/// unresolved dependency, or otherwise invalid view.
fn order_view_batch<'a>(
    pending: &mut Vec<&'a View>,
    ordered: &mut Vec<&'a View>,
    ready: &mut impl FnMut(&View) -> Result<(), String>,
    include: bool,
) -> Result<(), String> {
    while !pending.is_empty() {
        let pending_count = pending.len();
        let mut rest = Vec::new();
        let mut first_error = None;
        for view in pending.drain(..) {
            match ready(view) {
                Ok(()) => {
                    if include {
                        ordered.push(view);
                    }
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    rest.push(view);
                }
            }
        }
        if rest.len() == pending_count
            && let Some(error) = first_error
        {
            return Err(error);
        }
        *pending = rest;
    }
    Ok(())
}

/// A modeled index key, preserving explicit order and collation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexColumn {
    pub(crate) name: String,
    pub(crate) order: Option<SortDirection>,
    pub(crate) collation: Option<String>,
}

/// Compare key position and modifiers as well as object and column names.
fn indexes_match(actual: &Index, desired: &Index) -> bool {
    actual.name.eq_ignore_ascii_case(&desired.name)
        && actual.table.eq_ignore_ascii_case(&desired.table)
        && actual.unique == desired.unique
        && actual.columns.len() == desired.columns.len()
        && actual
            .columns
            .iter()
            .zip(&desired.columns)
            .all(|(actual, desired)| {
                actual.name.eq_ignore_ascii_case(&desired.name)
                    && actual.order == desired.order
                    && match (&actual.collation, &desired.collation) {
                        (Some(actual), Some(desired)) => actual.eq_ignore_ascii_case(desired),
                        (None, None) => true,
                        _ => false,
                    }
            })
}

/// Require old columns to remain an equivalent prefix of each table.
/// Emit new tables first, then column appends, each grouped by table name
/// as spelled in the desired schema, with appended columns in declaration order.
fn additive_table_changes(changes: &[Change<'_, SqliteObject>]) -> Result<Vec<Ddl>, String> {
    let mut creates = Vec::new();
    let mut modified = Vec::new();
    let mut tables: Vec<&Table> = Vec::new();
    for change in changes {
        match change {
            Change::Added(SqliteObject::Table(table)) => tables.push(table),
            Change::Modified {
                before: SqliteObject::Table(old),
                after: SqliteObject::Table(new),
            } => {
                modified.push((old, new));
            }
            Change::Removed(SqliteObject::Table(old)) => {
                return Err(format!("removing table {} is not supported", old.name));
            }
            _ => {}
        }
    }
    tables.sort_unstable_by_key(|table| &table.name);
    modified.sort_unstable_by_key(|(_, table)| &table.name);
    for table in tables {
        creates.push(Ddl::CreateTable(table.clone()));
    }
    let mut appends = Vec::new();
    for (old, new) in modified {
        appends.extend(table_append(old, new)?);
    }
    creates.extend(appends);
    Ok(creates)
}

fn table_append(previous: &Table, table: &Table) -> Result<Vec<Ddl>, String> {
    let mut creates = Vec::new();
    if previous.check != table.check {
        return Err(format!(
            "changing CHECK constraint on table {} is not supported",
            table.name
        ));
    }
    for (position, old) in previous.columns.iter().enumerate() {
        let Some(new) = table.columns.get(position) else {
            return Err(format!(
                "removing column {} from table {} is not supported",
                old.name, table.name
            ));
        };
        if !old.name.eq_ignore_ascii_case(&new.name) {
            return Err(format!(
                "expected column {} at position {} in table {}, found {}; new columns must be appended without reordering existing columns",
                old.name,
                position + 1,
                table.name,
                new.name
            ));
        }
        let changed = if old.not_null != new.not_null {
            Some("NOT NULL constraint")
        } else if old.check != new.check {
            Some("CHECK constraint")
        } else if old.primary_key != new.primary_key {
            Some("PRIMARY KEY constraint")
        } else {
            compare_type_and_default(old, new)
        };
        if let Some(property) = changed {
            return Err(format!(
                "changing the {property} of column {}.{} is not supported",
                table.name, new.name
            ));
        }
    }
    for column in &table.columns[previous.columns.len()..] {
        check_added_column(column).map_err(|reason| {
            format!("cannot add column {}.{}: {reason}", table.name, column.name)
        })?;
        creates.push(Ddl::AddColumn {
            table: previous.name.clone(),
            column: column.clone(),
        });
    }
    Ok(creates)
}

/// Compare declared types ignoring ASCII case, and treat an omitted default as
/// equivalent to an explicit DEFAULT NULL.
fn compare_type_and_default(old: &Column, new: &Column) -> Option<&'static str> {
    let types_match = match (&old.declared_type, &new.declared_type) {
        (Some(old), Some(new)) => old.eq_ignore_ascii_case(new),
        (None, None) => true,
        _ => false,
    };
    if !types_match {
        return Some("declared type");
    }
    if old.default_value != new.default_value {
        return Some("default value");
    }
    None
}

fn check_added_column(column: &Column) -> Result<(), &'static str> {
    if column.not_null {
        return Err(
            "new columns on existing tables must be nullable; remove NOT NULL or write a migration that backfills existing rows and enforces the constraint",
        );
    }
    if column.check.is_some() {
        return Err("adding a column with a CHECK constraint is not supported");
    }
    if column.default_value.is_some() {
        return Err("adding a column with a non-NULL default is not supported");
    }
    if column.primary_key {
        return Err("adding a PRIMARY KEY column is not supported");
    }
    Ok(())
}
