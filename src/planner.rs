//! Logical changes between database schemas.
//!
//! Similar to the [`crate::schema`] module, the planner module is an abstract
//! implementation of diff detection and [`ChangeSet`] creation.
//!
//! [`diff`] matches objects by backend-defined identity and collects additions,
//! removals, and modifications into a [`ChangeSet`]. The result borrows from
//! both schemas and describes what changed, not how to apply those changes.

use crate::schema::{Object, Schema};

// ----------------------------------------------------------------------------
// Changes
// ----------------------------------------------------------------------------

/// A logical change to a database object.
///
/// TODO: Can we generalise this to not use the verbs of Added/Removed/Modified
/// but rather an abstract collection of verbs? This is a bit hard to think
/// about in the early stages of the project, but why can't we have verbs like
/// `Invalidate` or `Replace`? This is artifically limiting.
#[derive(Debug, PartialEq, Eq)]
pub enum Change<'a, O: Object> {
    Added(&'a O),
    Removed(&'a O),
    Modified { before: &'a O, after: &'a O },
}

impl<'a, O: Object> Change<'a, O> {
    /// Return the opposite logical change.
    ///
    /// TODO: Again, should this be generalised more like a trait and require
    /// each implementer to define how to invert a Change? Do we want to have
    /// the concept of a Reversable vs. Irreversable change? something to think
    /// about as I build my brain around this problem.
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
// ChangeSet
// ----------------------------------------------------------------------------

/// An ordered collection of logical changes, not an execution plan.
#[derive(Debug, PartialEq, Eq)]
pub struct ChangeSet<'a, O: Object> {
    changes: Vec<Change<'a, O>>,
}

impl<'a, O: Object> ChangeSet<'a, O> {
    /// Collect changes in the given order.
    pub fn new(changes: Vec<Change<'a, O>>) -> Self {
        Self { changes }
    }

    /// Borrow the changes in order.
    pub fn changes(&self) -> &[Change<'a, O>] {
        &self.changes
    }

    /// Invert every change and reverse their order.
    ///
    /// This does not guarantee that a backend can undo the changes.
    pub fn invert(self) -> Self {
        Self::new(self.changes.into_iter().rev().map(Change::invert).collect())
    }
}

// ----------------------------------------------------------------------------
// Diff Algorithm
// ----------------------------------------------------------------------------

/// Computes a diff between two schemas to create a [`ChangeSet`].
///
/// Additions and modifications follow target order; removals follow source order.
pub fn diff<'a, O: Object>(before: &'a Schema<O>, after: &'a Schema<O>) -> ChangeSet<'a, O> {
    // Setup vector to collect changes.
    let mut changes = Vec::new();

    // Figure out additions & changes
    for object in after.objects() {
        let id = object.id();
        match before.objects().iter().find(|old| old.id() == id) {
            None => changes.push(Change::Added(object)),
            Some(old) if old != object => changes.push(Change::Modified {
                before: old,
                after: object,
            }),
            Some(_) => {}
        }
    }

    // Figure out removals
    for object in before.objects() {
        let id = object.id();
        if !after.objects().iter().any(|new| new.id() == id) {
            changes.push(Change::Removed(object));
        }
    }

    // Return ChangeSet
    ChangeSet::new(changes)
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // -- Fixtures ------------------------------------------------------------
    use super::{Change, ChangeSet, diff};
    use crate::schema::{Object, Schema};

    #[derive(Debug, PartialEq, Eq)]
    struct Table(&'static str, u8);

    impl Object for Table {
        type Id = &'static str;

        fn id(&self) -> Self::Id {
            self.0
        }
    }

    // -- Tests ---------------------------------------------------------------
    #[test]
    fn diff_reports_only_changed_objects() {
        let before = Schema::new(vec![
            Table("accounts", 1),
            Table("users", 1),
            Table("orders", 1),
        ]);
        let after = Schema::new(vec![
            Table("users", 2),
            Table("accounts", 1),
            Table("sessions", 1),
        ]);

        assert_eq!(
            diff(&before, &after).changes(),
            [
                Change::Modified {
                    before: &Table("users", 1),
                    after: &Table("users", 2),
                },
                Change::Added(&Table("sessions", 1)),
                Change::Removed(&Table("orders", 1)),
            ]
        );
    }

    #[test]
    fn inverting_a_change_set_inverts_each_change_in_reverse_order() {
        let added = Table("added", 1);
        let removed = Table("removed", 1);
        let before = Table("modified", 1);
        let after = Table("modified", 2);
        let changes = ChangeSet::new(vec![
            Change::Added(&added),
            Change::Modified {
                before: &before,
                after: &after,
            },
            Change::Removed(&removed),
        ]);

        assert_eq!(
            changes.invert().changes(),
            [
                Change::Added(&removed),
                Change::Modified {
                    before: &after,
                    after: &before,
                },
                Change::Removed(&added),
            ]
        );
    }
}
