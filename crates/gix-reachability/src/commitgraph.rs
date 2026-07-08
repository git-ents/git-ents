//! [`CommitGraph`]: OID -> (tree, parents, generation number), the
//! commit-parent accelerator `docs/scale-out.adoc`'s "Reachability" section
//! calls for ("Maintenance effects generate commit-graph and reachability
//! bitmaps"). `gix-commitgraph` (survey, WS6/Q4) reads git's own
//! `commit-graph` file format but offers no writer, and the format itself is
//! a stock-git interop surface this crate doesn't need to match — artifacts
//! here are accelerators for the native backends, not stock-git interop
//! (`docs/scale-out.adoc`'s reachability artifacts are explicitly allowed to
//! be workspace-private), so this module defines its own minimal, versioned
//! binary format instead of a git-compatible one.
//!
//! # Format (version 1)
//!
//! ```text
//! magic       "RGCG" (4 bytes)
//! version     1 (1 byte)
//! count       u32 LE
//! oids        count * 20 bytes, sorted ascending — the index table
//!             `parents` below refers into
//! trees       count * 20 bytes, `trees[i]` is `oids[i]`'s root tree
//! generations count * u32 LE, `generations[i]` is `oids[i]`'s generation
//!             number (1 + max(parent generations), or 1 for a root commit)
//! parents     count entries, each: parent_count (1 byte) then
//!             parent_count * u32 LE indices into `oids`
//! ```
//!
//! Parents are stored as indices into the sorted OID table (git's own
//! commit-graph format does the same) rather than as OIDs again: a lookup
//! by OID is one `binary_search` over `oids`, and every parent reference
//! costs 4 bytes instead of 20.

use std::collections::BTreeMap;

use gix_hash::ObjectId;

use crate::codec::{Reader, Writer};
use crate::walk::ObjectSource;
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"RGCG";
const VERSION: u8 = 1;

/// One commit's data as read back from a [`CommitGraph`]: its tree,
/// parents, and generation number, exactly what [`crate::walk::reachable`]
/// needs to descend a commit without ever reading it from the object store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphEntry {
    /// The commit's root tree.
    pub tree: ObjectId,
    /// The commit's parents, in the order the commit object listed them.
    pub parents: Vec<ObjectId>,
    /// `1 + max(parent generations)`, or `1` for a commit with no parents.
    pub generation: u32,
}

/// A serialized commit graph: every commit reachable (via parent edges
/// alone) from the tips it was [`build`](CommitGraph::build) against,
/// mapped to its tree, parents, and generation number.
///
/// Coverage is partial by construction whenever new commits have landed
/// since the graph was built — [`entry`](CommitGraph::entry) simply returns
/// `None` for anything it doesn't have, so a caller degrades to reading
/// that one commit from the object store instead of failing
/// (`docs/scale-out.adoc`: "absence or staleness degrades speed, never
/// answers").
#[derive(Debug, Clone, Default)]
pub struct CommitGraph {
    /// Sorted ascending; `oids.binary_search` is how every other field is
    /// looked up by OID.
    oids: Vec<ObjectId>,
    trees: Vec<ObjectId>,
    generations: Vec<u32>,
    /// `parents[i]` holds indices into `oids` for `oids[i]`'s parents.
    parents: Vec<Vec<u32>>,
}

impl CommitGraph {
    /// Build a commit graph covering every commit reachable from `tips` via
    /// parent edges alone (trees/blobs are never visited — this is a
    /// commit-only structure). A tip or ancestor `source` cannot resolve, or
    /// that is not itself a commit, is simply not included rather than
    /// treated as an error: this is a best-effort maintenance artifact, not
    /// a correctness gate (`crate::maintenance::regenerate` is free to
    /// re-run and does not block anything on this being complete).
    ///
    /// # Errors
    ///
    /// Returns an error if `source` fails outright (not: "doesn't have it"),
    /// or if a resolved commit fails to decode.
    pub fn build(
        tips: impl IntoIterator<Item = ObjectId>,
        source: &dyn ObjectSource,
    ) -> Result<Self> {
        let mut commits: BTreeMap<ObjectId, (ObjectId, Vec<ObjectId>)> = BTreeMap::new();
        let mut seen: std::collections::BTreeSet<ObjectId> = std::collections::BTreeSet::new();
        let mut stack: Vec<ObjectId> = tips.into_iter().collect();

        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some((kind, data)) = source.find(&id)? else {
                continue;
            };
            if kind != gix_object::Kind::Commit {
                continue;
            }
            let commit = gix_object::CommitRef::from_bytes(&data, gix_hash::Kind::Sha1)
                .map_err(|error| Error::Decode(error.to_string()))?;
            let tree = commit.tree();
            let parent_oids: Vec<ObjectId> = commit.parents().collect();
            stack.extend(parent_oids.iter().copied());
            commits.insert(id, (tree, parent_oids));
        }

        let oids: Vec<ObjectId> = commits.keys().copied().collect();
        let mut trees = Vec::with_capacity(oids.len());
        let mut parent_oid_lists = Vec::with_capacity(oids.len());
        for id in &oids {
            let (tree, parent_oids) = commits
                .get(id)
                .ok_or_else(|| Error::Format("commit graph build lost an entry".to_owned()))?;
            trees.push(*tree);
            parent_oid_lists.push(parent_oids.clone());
        }

        let parents: Vec<Vec<u32>> = parent_oid_lists
            .iter()
            .map(|parent_oids| {
                parent_oids
                    .iter()
                    .filter_map(|parent| {
                        oids.binary_search(parent)
                            .ok()
                            .and_then(|index| u32::try_from(index).ok())
                    })
                    .collect()
            })
            .collect();

        let generations = compute_generations(oids.len(), &parents)?;

        Ok(Self {
            oids,
            trees,
            generations,
            parents,
        })
    }

    /// `id`'s tree, parents, and generation number, or `None` if this graph
    /// does not cover `id`.
    #[must_use]
    pub fn entry(&self, id: &ObjectId) -> Option<GraphEntry> {
        let index = self.oids.binary_search(id).ok()?;
        let tree = *self.trees.get(index)?;
        let generation = *self.generations.get(index)?;
        let parent_indices = self.parents.get(index)?;
        let parents = parent_indices
            .iter()
            .filter_map(|&parent_index| self.oids.get(usize::try_from(parent_index).ok()?).copied())
            .collect();
        Some(GraphEntry {
            tree,
            parents,
            generation,
        })
    }

    /// `id`'s generation number alone, or `None` if this graph does not
    /// cover `id`.
    #[must_use]
    pub fn generation(&self, id: &ObjectId) -> Option<u32> {
        let index = self.oids.binary_search(id).ok()?;
        self.generations.get(index).copied()
    }

    /// How many commits this graph covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.oids.len()
    }

    /// Whether this graph covers no commits at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.oids.is_empty()
    }

    /// Serialize to this module's binary format (version 1).
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.header(MAGIC, VERSION);
        let count = u32::try_from(self.oids.len()).unwrap_or(u32::MAX);
        writer.u32(count);
        for oid in &self.oids {
            writer.oid(oid);
        }
        for tree in &self.trees {
            writer.oid(tree);
        }
        for generation in &self.generations {
            writer.u32(*generation);
        }
        for parent_indices in &self.parents {
            let parent_count = u8::try_from(parent_indices.len()).unwrap_or(u8::MAX);
            writer.u8(parent_count);
            for &parent_index in parent_indices.iter().take(usize::from(parent_count)) {
                writer.u32(parent_index);
            }
        }
        writer.into_bytes()
    }

    /// Parse this module's binary format back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Format`] if the header, length, or any entry is
    /// malformed or truncated.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        reader.header(MAGIC, VERSION)?;
        let count = usize::try_from(reader.u32()?)
            .map_err(|_error| Error::Format("commit graph count overflowed usize".to_owned()))?;

        let mut oids = Vec::with_capacity(count);
        for _ in 0..count {
            oids.push(reader.oid()?);
        }
        let mut trees = Vec::with_capacity(count);
        for _ in 0..count {
            trees.push(reader.oid()?);
        }
        let mut generations = Vec::with_capacity(count);
        for _ in 0..count {
            generations.push(reader.u32()?);
        }
        let mut parents = Vec::with_capacity(count);
        for _ in 0..count {
            let parent_count = reader.u8()?;
            let mut indices = Vec::with_capacity(usize::from(parent_count));
            for _ in 0..parent_count {
                let index = reader.u32()?;
                let in_range = usize::try_from(index).is_ok_and(|index| index < count);
                if !in_range {
                    return Err(Error::Format(
                        "commit graph parent index out of range".to_owned(),
                    ));
                }
                indices.push(index);
            }
            parents.push(indices);
        }

        if !reader.at_end() {
            return Err(Error::Format(
                "trailing bytes after commit graph artifact".to_owned(),
            ));
        }

        Ok(Self {
            oids,
            trees,
            generations,
            parents,
        })
    }
}

/// Compute every commit's generation number: `1 + max(parent generations)`,
/// or `1` for a commit with no parents. `parents[i]` (indices into a virtual
/// `0..n` id space) must reference only indices `< n` — [`CommitGraph::
/// build`] guarantees this by construction (every index came from a
/// successful `binary_search` into the same table).
///
/// Iterative post-order DFS rather than recursion: a commit history can be
/// far deeper than Rust's default stack tolerates, and this workspace's
/// lints forbid the indexing/unwrapping a naive recursive version would
/// otherwise reach for just as much as this iterative one avoids.
fn compute_generations(n: usize, parents: &[Vec<u32>]) -> Result<Vec<u32>> {
    let mut generation: Vec<u32> = vec![0; n];
    let mut done: Vec<bool> = vec![false; n];

    for start in 0..n {
        if *done
            .get(start)
            .ok_or_else(|| Error::Format("generation computation index out of range".to_owned()))?
        {
            continue;
        }
        // Each stack frame is (node index, how many of its parents this
        // frame has already pushed for processing).
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&(index, next_parent)) = stack.last() {
            let node_parents = parents.get(index).ok_or_else(|| {
                Error::Format("generation computation index out of range".to_owned())
            })?;

            if let Some(&parent_index) = node_parents.get(next_parent) {
                if let Some(frame) = stack.last_mut() {
                    frame.1 = next_parent.saturating_add(1);
                }
                let parent_index = usize::try_from(parent_index).map_err(|_error| {
                    Error::Format("generation computation index overflowed usize".to_owned())
                })?;
                if !*done.get(parent_index).ok_or_else(|| {
                    Error::Format("generation computation index out of range".to_owned())
                })? {
                    stack.push((parent_index, 0));
                }
                continue;
            }

            // Every parent has already been assigned a generation.
            let mut max_parent_generation = 0u32;
            for &parent_index in node_parents {
                let parent_index = usize::try_from(parent_index).map_err(|_error| {
                    Error::Format("generation computation index overflowed usize".to_owned())
                })?;
                let parent_generation = *generation.get(parent_index).ok_or_else(|| {
                    Error::Format("generation computation index out of range".to_owned())
                })?;
                max_parent_generation = max_parent_generation.max(parent_generation);
            }
            let this_generation = if node_parents.is_empty() {
                1
            } else {
                max_parent_generation
                    .checked_add(1)
                    .ok_or_else(|| Error::Format("generation number overflowed u32".to_owned()))?
            };
            let generation_slot = generation.get_mut(index).ok_or_else(|| {
                Error::Format("generation computation index out of range".to_owned())
            })?;
            *generation_slot = this_generation;
            let done_slot = done.get_mut(index).ok_or_else(|| {
                Error::Format("generation computation index out of range".to_owned())
            })?;
            *done_slot = true;
            stack.pop();
        }
    }

    Ok(generation)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use std::collections::HashMap;

    use gix_object::Kind;
    use gix_object::WriteTo as _;

    use super::*;

    /// An in-memory [`ObjectSource`] over a fixed set of commits, keyed by
    /// id, for building [`CommitGraph`]s in tests without a real repo.
    struct FakeCommits(HashMap<ObjectId, Vec<u8>>);

    impl ObjectSource for FakeCommits {
        fn find(&self, id: &ObjectId) -> Result<Option<(Kind, Vec<u8>)>> {
            Ok(self.0.get(id).map(|data| (Kind::Commit, data.clone())))
        }
    }

    fn commit_bytes(tree: ObjectId, parents: &[ObjectId]) -> Vec<u8> {
        let identity = gix_actor::Signature {
            name: "test".into(),
            email: "test@example.com".into(),
            time: gix_date::Time::default(),
        };
        let commit = gix_object::Commit {
            tree,
            parents: parents.iter().copied().collect(),
            author: identity.clone(),
            committer: identity,
            encoding: None,
            message: "message".into(),
            extra_headers: Vec::new(),
        };
        let mut buf = Vec::new();
        commit.write_to(&mut buf).unwrap();
        buf
    }

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0_u8; 20];
        if let Some(last) = bytes.last_mut() {
            *last = byte;
        }
        ObjectId::from(bytes)
    }

    #[test]
    fn generation_is_one_for_a_root_commit() {
        let tree = oid(0xAA);
        let root = oid(1);
        let mut commits = HashMap::new();
        commits.insert(root, commit_bytes(tree, &[]));
        let source = FakeCommits(commits);

        let graph = CommitGraph::build([root], &source).unwrap();
        assert_eq!(graph.generation(&root), Some(1));
        let entry = graph.entry(&root).unwrap();
        assert_eq!(entry.tree, tree);
        assert!(entry.parents.is_empty());
    }

    #[test]
    fn generation_increases_along_a_chain() {
        let tree = oid(0xAA);
        let root = oid(1);
        let child = oid(2);
        let grandchild = oid(3);
        let mut commits = HashMap::new();
        commits.insert(root, commit_bytes(tree, &[]));
        commits.insert(child, commit_bytes(tree, &[root]));
        commits.insert(grandchild, commit_bytes(tree, &[child]));
        let source = FakeCommits(commits);

        let graph = CommitGraph::build([grandchild], &source).unwrap();
        assert_eq!(graph.generation(&root), Some(1));
        assert_eq!(graph.generation(&child), Some(2));
        assert_eq!(graph.generation(&grandchild), Some(3));
    }

    #[test]
    fn generation_at_a_merge_is_one_plus_the_max_parent() {
        let tree = oid(0xAA);
        let root = oid(1);
        let left = oid(2);
        let right_chain_a = oid(3);
        let right_chain_b = oid(4);
        let merge = oid(5);
        let mut commits = HashMap::new();
        commits.insert(root, commit_bytes(tree, &[]));
        commits.insert(left, commit_bytes(tree, &[root]));
        commits.insert(right_chain_a, commit_bytes(tree, &[root]));
        commits.insert(right_chain_b, commit_bytes(tree, &[right_chain_a]));
        commits.insert(merge, commit_bytes(tree, &[left, right_chain_b]));
        let source = FakeCommits(commits);

        let graph = CommitGraph::build([merge], &source).unwrap();
        assert_eq!(graph.generation(&left), Some(2));
        assert_eq!(graph.generation(&right_chain_b), Some(3));
        assert_eq!(graph.generation(&merge), Some(4));
    }

    #[test]
    fn entry_is_none_for_an_uncovered_commit() {
        let tree = oid(0xAA);
        let root = oid(1);
        let mut commits = HashMap::new();
        commits.insert(root, commit_bytes(tree, &[]));
        let source = FakeCommits(commits);

        let graph = CommitGraph::build([root], &source).unwrap();
        assert_eq!(graph.entry(&oid(99)), None);
    }

    #[test]
    fn round_trips_through_serialize_and_deserialize() {
        let tree = oid(0xAA);
        let root = oid(1);
        let child = oid(2);
        let mut commits = HashMap::new();
        commits.insert(root, commit_bytes(tree, &[]));
        commits.insert(child, commit_bytes(tree, &[root]));
        let source = FakeCommits(commits);

        let graph = CommitGraph::build([child], &source).unwrap();
        let bytes = graph.serialize();
        let read_back = CommitGraph::deserialize(&bytes).unwrap();

        assert_eq!(read_back.entry(&root), graph.entry(&root));
        assert_eq!(read_back.entry(&child), graph.entry(&child));
        assert_eq!(read_back.len(), graph.len());
    }

    #[test]
    fn deserialize_rejects_garbage() {
        let _error = CommitGraph::deserialize(b"not a commit graph").unwrap_err();
    }
}
