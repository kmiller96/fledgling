//! Defines the generic traits and structures which define a database configuration.
//!
//! Fledgling can be thought of mangaging the following abstract data structures:
//!
//! 1. **Definition**: this is a collection of database objects. It contains
//!    things like the tables, the views, etc. This is a generic collection as
//!    it doesn't assume any particular set of objects. Rather, it just contains
//!    a collection of DatabaseObject traits which each backend implements.
//! 2. **DatabaseObject**: this represets a specific object in the database. That
//!    could be a table, a view, an index, a role, etc.

// ----------------------------------------------------------------------------
// Database Objects & Definitions
// ----------------------------------------------------------------------------

/// A particular object in the database.
///
/// This is a generic, Fledgling-specific data structure that encapsulates a
/// unique database "object". This could be a table, a view, an index, etc. Each
/// backend defines its own set of database objects.
pub trait DatabaseObject: Eq {
    /// Key used to match objects within the backend's namespaces.
    ///
    /// This is because equivalence changes between databases. Some databases
    /// might be case sensitive, others case insensitive, so we don't try to
    /// figure this out for the user but instead require them to define it for
    /// us.
    type Id: Eq;

    /// Return the ID of the object, which can be used in checking equality.
    fn id(&self) -> Self::Id;
}

/// Unique schema objects in declaration order.
///
/// Equality compares objects in order; [`Self::difference`] ignores order
/// when matching objects by identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition<Object: DatabaseObject> {
    objects: Vec<Object>,
}

impl<Object: DatabaseObject> Definition<Object> {
    /// Keep the first object for each identity, preserving input order.
    ///
    /// Callers that reject duplicates must check before constructing a definition.
    pub fn new(objects: Vec<Object>) -> Self {
        let mut unique = Vec::with_capacity(objects.len());
        for object in objects {
            if !unique.iter().any(|seen: &Object| seen.id() == object.id()) {
                unique.push(object);
            }
        }
        Self { objects: unique }
    }

    /// Borrow the objects in declaration order.
    pub fn objects(&self) -> &[Object] {
        &self.objects
    }

    /// Describe the transition from this definition to `after`.
    ///
    /// Equivalent objects are omitted. Additions and modifications follow
    /// `after`'s order, then removals follow this definition's order. This is
    /// not an execution order.
    pub fn difference<'a>(&'a self, after: &'a Self) -> Diff<'a, Object> {
        let mut changes = Vec::new();
        for object in after.objects() {
            match self.objects.iter().find(|old| old.id() == object.id()) {
                None => changes.push(Change::Added(object)),
                Some(old) if old != object => {
                    changes.push(Change::Modified {
                        before: old,
                        after: object,
                    });
                }
                Some(_) => {}
            }
        }
        for object in &self.objects {
            if !after.objects.iter().any(|new| new.id() == object.id()) {
                changes.push(Change::Removed(object));
            }
        }
        changes
    }
}

// ----------------------------------------------------------------------------
// Changes
// ----------------------------------------------------------------------------

/// A difference between database definitions.
///
/// This Change enum only supports database objects. Objects can be added,
/// removed, or modified.
#[derive(Debug, PartialEq, Eq)]
pub enum Change<'a, Object: DatabaseObject> {
    /// The object exists only in the target definition.
    Added(&'a Object),

    /// The object exists only in the source definition.
    Removed(&'a Object),

    /// Both definitions have the same identity but differ in content.
    Modified {
        before: &'a Object,
        after: &'a Object,
    },
}

impl<Object: DatabaseObject> Change<'_, Object> {
    /// Describe the opposite change, without promising it can be executed.
    // TODO: Revisit whether this is needed once the migration design is finalized.
    #[allow(dead_code)]
    pub fn invert(self) -> Self {
        match self {
            Self::Added(object) => Self::Removed(object),
            Self::Removed(object) => Self::Added(object),
            Self::Modified { before, after } => Self::Modified {
                before: after,
                after: before,
            },
        }
    }
}

// ----------------------------------------------------------------------------
// Schema diff
// ----------------------------------------------------------------------------

/// Logical changes needed to reach a target schema.
///
/// The backend determines how to execute the changes and in which order.
pub type Diff<'a, Object> = Vec<Change<'a, Object>>;

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{Change, DatabaseObject, Definition};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Table {
        name: &'static str,
        columns: Vec<String>,
    }

    impl DatabaseObject for Table {
        type Id = &'static str;
        fn id(&self) -> Self::Id {
            self.name
        }
    }

    #[test]
    fn invert_reverses_all_change_variants() {
        let before = Table {
            name: "items",
            columns: vec!["id".into()],
        };
        let after = Table {
            name: "items",
            columns: vec!["id".into(), "name".into()],
        };

        assert_eq!(Change::Added(&after).invert(), Change::Removed(&after));
        assert_eq!(Change::Removed(&before).invert(), Change::Added(&before));
        assert_eq!(
            Change::Modified {
                before: &before,
                after: &after,
            }
            .invert(),
            Change::Modified {
                before: &after,
                after: &before,
            }
        );
    }

    #[test]
    fn difference_reports_only_tables_that_changed() {
        // Define out table objects
        let accounts = Table {
            name: "accounts",
            columns: vec!["id".into()],
        };
        let users = Table {
            name: "users",
            columns: vec!["id".into()],
        };
        let users_with_email = Table {
            name: "users",
            columns: vec!["id".into(), "email".into()],
        };
        let orders = Table {
            name: "orders",
            columns: vec!["id".into()],
        };
        let sessions = Table {
            name: "sessions",
            columns: vec!["id".into()],
        };

        // Define our before/after definitions
        let before = Definition::new(vec![accounts.clone(), users.clone(), orders.clone()]);
        let after = Definition::new(vec![
            accounts.clone(),
            users_with_email.clone(),
            sessions.clone(),
        ]);

        // Compare changes match expectations
        assert_eq!(
            before.difference(&after),
            [
                Change::Modified {
                    before: &users,
                    after: &users_with_email,
                },
                Change::Added(&sessions),
                Change::Removed(&orders),
            ]
        );
    }
}
