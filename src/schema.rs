//! Abstract database definition
//!
//! Each backend has its own unique set of objects it supports. For example,
//! one database may have a "Table" while another has a "Collection". We don't
//! require an particular nomenclature to use fledgling.
//!
//! This module defines the abstract data structures that each backend
//! instantiates.
//!
//! A backend implements [`Object`] for the database objects it supports and
//! defines how to identify them across schemas. A [`Schema`] collects those
//! objects in declaration order, keeping the first object for each identity.
//!
//! Logical comparison lives in [`crate::planner`].

// ----------------------------------------------------------------------------
// Object
// ----------------------------------------------------------------------------

/// A database object such as a table, view, or index.
///
/// Equality compares object contents. Identity matches objects across schemas,
/// even when their contents differ.
pub trait Object: Eq {
    /// Backend-defined identity, including any applicable namespace.
    type Id: Eq;

    /// Return the object's identity.
    fn id(&self) -> Self::Id;
}

// ----------------------------------------------------------------------------
// Schema
// ----------------------------------------------------------------------------

/// A collection of database objects in declaration order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema<O: Object> {
    objects: Vec<O>,
}

impl<O: Object> Schema<O> {
    /// Keep the first object for each identity, preserving declaration order.
    pub fn new(objects: Vec<O>) -> Self {
        let mut unique = Vec::with_capacity(objects.len());
        for object in objects {
            let id = object.id();
            if !unique.iter().any(|seen: &O| seen.id() == id) {
                unique.push(object);
            }
        }
        Self { objects: unique }
    }

    /// Borrow the objects in declaration order.
    pub fn objects(&self) -> &[O] {
        &self.objects
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // -- Fixtures ------------------------------------------------------------
    use super::{Object, Schema};

    #[derive(Debug, PartialEq, Eq)]
    struct Table(&'static str, u8);

    impl Object for Table {
        type Id = String;

        fn id(&self) -> Self::Id {
            self.0.to_ascii_lowercase()
        }
    }

    // -- Tests ---------------------------------------------------------------
    // TODO: Consider writing more unit tests for this module

    #[test]
    fn schema_keeps_first_object_per_backend_identity() {
        let schema = Schema::new(vec![
            Table("Users", 1),
            Table("orders", 1),
            Table("users", 2),
        ]);
        assert_eq!(schema.objects(), [Table("Users", 1), Table("orders", 1)]);
    }
}
