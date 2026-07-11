//! An object-read counter, for asserting incremental-evaluation bounds.

use std::cell::Cell;

use gix_object::Find;
use gix_object::find;

/// Wraps any [`Find`] and counts `try_find` calls, so a test can assert
/// that an "incremental" code path really is bounded — for example, that
/// evaluating a query entry set after a one-commit advance does not
/// re-walk a three-hundred-commit history (`query.incremental`).
///
/// # Examples
///
/// ```
/// use ents_testutil::{CountingFind, ObjectStore, empty_tree};
/// use gix_object::Find as _;
///
/// let objects = ObjectStore::default();
/// let tree = empty_tree(&objects);
///
/// let counting = CountingFind::new(&objects);
/// let mut buf = Vec::new();
/// counting.try_find(&tree, &mut buf).expect("readable");
/// assert_eq!(counting.reads(), 1);
///
/// counting.reset();
/// assert_eq!(counting.reads(), 0);
/// ```
#[derive(Debug)]
pub struct CountingFind<'a, F: Find> {
    inner: &'a F,
    reads: Cell<usize>,
}

impl<'a, F: Find> CountingFind<'a, F> {
    /// Wrap `inner`, starting the counter at zero.
    #[must_use]
    pub fn new(inner: &'a F) -> Self {
        Self {
            inner,
            reads: Cell::new(0),
        }
    }

    /// How many `try_find` calls have happened since the last
    /// [`CountingFind::reset`].
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads.get()
    }

    /// Zero the counter — typically after warming caches, so an assertion
    /// covers only the increment under test.
    pub fn reset(&self) {
        self.reads.set(0);
    }
}

impl<F: Find> Find for CountingFind<'_, F> {
    fn try_find<'b>(
        &self,
        id: &gix_hash::oid,
        buffer: &'b mut Vec<u8>,
    ) -> Result<Option<gix_object::Data<'b>>, find::Error> {
        self.reads.set(self.reads.get().saturating_add(1));
        self.inner.try_find(id, buffer)
    }
}
