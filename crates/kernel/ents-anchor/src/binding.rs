//! [`Binding`]: the single typed-reference vocabulary into the object
//! graph — history-bound, content-bound, transformation-bound,
//! position-bound, or relational — plus read-time [`revalidate`] of a
//! binding against a revision under evaluation.
//!
//! [`Binding::Position`] is [`crate::Anchor`] unchanged: every anchor is a
//! binding, but not every binding is an anchor. The other four variants
//! name a target without a line-level position at all — a commit itself, a
//! tree (optionally at an advisory path), a `(base_tree, head_tree)`
//! transformation, or a commit-plus-tree pair.
//!
//! Every binding carries at least one *witness* — a commit whose ancestry
//! reaches the bound object(s) — so a claim's ledger commit can carry the
//! witness as an extra parent and keep the bound objects reachable. The
//! witness is provenance, not identity: [`Binding::same_target`] ignores it
//! entirely.
//!
//! `Binding` itself is a plain Rust enum, not a `facet::Facet` type — the
//! generic derive would encode an enum externally tagged (a tree with one
//! variant-named entry), which would not round-trip the existing anchor
//! storage format byte for byte. Instead [`Binding::serialize_into`] and
//! [`Binding::deserialize`] hand-encode each variant as a *bare* tree (no
//! variant tag), inferring the variant back from which entry names are
//! present on read.

use facet::Facet;
use gix::ObjectId;
use gix_object::{Find, Kind, TreeRef, Write};

use crate::anchor::Anchor;
use crate::error::{Error, Result};
use crate::projection::{Projection, project};
use crate::util::resolve_commit;

/// The single typed-reference vocabulary into the object graph: what a
/// claim, comment, or review is *about*.
///
/// # Examples
///
/// ```
/// use ents_anchor::Binding;
///
/// let commit = gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
/// let binding = Binding::Commit { commit };
/// assert_eq!(binding.witnesses(), vec![commit]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// History-bound: the commit itself is the target. Its own witness is
    /// itself.
    Commit {
        /// The commit named by this binding.
        commit: ObjectId,
    },
    /// Content-bound: a tree, independent of any particular commit or
    /// path. `path` is advisory metadata only — identity is `tree` alone,
    /// [`Binding::same_target`] ignores `path` — because the same tree can
    /// sit at different paths across history without changing what is
    /// bound.
    Tree {
        /// The bound tree's identity.
        tree: ObjectId,
        /// Where `tree` was found when this binding was made, retained for
        /// display only — never part of identity.
        path: String,
        /// A commit whose ancestry reaches `tree`.
        witness: ObjectId,
    },
    /// Transformation-bound: the pair `(base_tree, head_tree)` — an edit,
    /// not either endpoint alone. A commit range is *evidence for* a
    /// `Delta`; it is never a binding itself. `path` is advisory, same as
    /// [`Binding::Tree`]'s.
    Delta {
        /// The tree before the transformation.
        base_tree: ObjectId,
        /// The tree after the transformation.
        head_tree: ObjectId,
        /// Where the transformation was found when this binding was made,
        /// retained for display only — never part of identity.
        path: String,
        /// A commit whose ancestry reaches `base_tree`.
        base_witness: ObjectId,
        /// A commit whose ancestry reaches `head_tree`.
        head_witness: ObjectId,
    },
    /// Position-bound: [`Anchor`] verbatim — a durable pointer to specific
    /// lines (or a whole file) at a specific blob, retained and
    /// projectable exactly as [`crate::project`] describes.
    Position(Anchor),
    /// Relational: a parent commit plus a body tree, bound as a pair
    /// distinct from either [`Binding::Commit`] or [`Binding::Tree`] alone.
    Hybrid {
        /// The parent commit.
        commit: ObjectId,
        /// The body tree.
        tree: ObjectId,
    },
}

/// `[u8; 20]` payload for [`Binding::Commit`], encoded bare (no variant
/// tag) so [`Binding::serialize_into`] reproduces the exact byte layout
/// [`Binding::deserialize`] sniffs on.
#[derive(Debug, Clone, Facet)]
struct CommitPayload {
    commit: [u8; 20],
}

/// `[u8; 20]`/`String` payload for [`Binding::Tree`], encoded bare.
#[derive(Debug, Clone, Facet)]
struct TreePayload {
    tree: [u8; 20],
    path: String,
    witness: [u8; 20],
}

/// `[u8; 20]`/`String` payload for [`Binding::Delta`], encoded bare.
#[derive(Debug, Clone, Facet)]
struct DeltaPayload {
    base_tree: [u8; 20],
    head_tree: [u8; 20],
    path: String,
    base_witness: [u8; 20],
    head_witness: [u8; 20],
}

/// `[u8; 20]` payload for [`Binding::Hybrid`], encoded bare.
#[derive(Debug, Clone, Facet)]
struct HybridPayload {
    commit: [u8; 20],
    tree: [u8; 20],
}

/// `id`'s raw 20 bytes, for embedding in a `Facet`-derived payload struct —
/// [`Anchor`]'s own `commit`/`blob` pattern, applied to every oid field a
/// [`Binding`] variant carries.
fn oid_bytes(id: ObjectId) -> [u8; 20] {
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(id.as_slice());
    bytes
}

impl Binding {
    /// Every commit whose ancestry must reach the bound object(s) for this
    /// binding to stay alive — never empty: exactly the commit itself for
    /// [`Binding::Commit`] and [`Binding::Hybrid`], the anchor's own commit
    /// for [`Binding::Position`], the recorded witness for
    /// [`Binding::Tree`], and both witnesses for [`Binding::Delta`].
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_anchor::Binding;
    ///
    /// let base_witness = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
    /// let head_witness = gix::ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
    /// let binding = Binding::Delta {
    ///     base_tree: gix::ObjectId::from_hex(b"3333333333333333333333333333333333333333").unwrap(),
    ///     head_tree: gix::ObjectId::from_hex(b"4444444444444444444444444444444444444444").unwrap(),
    ///     path: "src/lib.rs".to_owned(),
    ///     base_witness,
    ///     head_witness,
    /// };
    /// assert_eq!(binding.witnesses(), vec![base_witness, head_witness]);
    /// ```
    #[must_use]
    pub fn witnesses(&self) -> Vec<ObjectId> {
        match self {
            Self::Commit { commit } | Self::Hybrid { commit, .. } => vec![*commit],
            Self::Tree { witness, .. } => vec![*witness],
            Self::Delta {
                base_witness,
                head_witness,
                ..
            } => vec![*base_witness, *head_witness],
            Self::Position(anchor) => vec![anchor.commit()],
        }
    }

    /// Whether `self` and `other` name the same target, ignoring
    /// provenance: derived [`PartialEq`] is full structural equality (every
    /// field, including advisory `path` and `witness`/`base_witness`/
    /// `head_witness`), while `same_target` compares identity only —
    /// `commit` for [`Binding::Commit`]; the tree oid alone for
    /// [`Binding::Tree`] (`path` and `witness` ignored); the
    /// `(base_tree, head_tree)` pair for [`Binding::Delta`] (`path` and
    /// both witnesses ignored); the anchor's `(blob, lines, commit)` for
    /// [`Binding::Position`]; `(commit, tree)` for [`Binding::Hybrid`].
    /// Bindings of different variants are never the same target, even when
    /// they happen to name overlapping objects.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_anchor::Binding;
    ///
    /// let tree = gix::ObjectId::from_hex(b"5555555555555555555555555555555555555555").unwrap();
    /// let a = Binding::Tree {
    ///     tree,
    ///     path: "a.rs".to_owned(),
    ///     witness: gix::ObjectId::from_hex(b"6666666666666666666666666666666666666666").unwrap(),
    /// };
    /// let b = Binding::Tree {
    ///     tree,
    ///     path: "b.rs".to_owned(),
    ///     witness: gix::ObjectId::from_hex(b"7777777777777777777777777777777777777777").unwrap(),
    /// };
    /// assert_ne!(a, b, "different path and witness: not structurally equal");
    /// assert!(a.same_target(&b), "same tree oid: same target regardless of path/witness");
    /// ```
    #[must_use]
    pub fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Commit { commit: a }, Self::Commit { commit: b }) => a == b,
            (Self::Tree { tree: a, .. }, Self::Tree { tree: b, .. }) => a == b,
            (
                Self::Delta {
                    base_tree: base_a,
                    head_tree: head_a,
                    ..
                },
                Self::Delta {
                    base_tree: base_b,
                    head_tree: head_b,
                    ..
                },
            ) => base_a == base_b && head_a == head_b,
            (Self::Position(a), Self::Position(b)) => {
                a.blob() == b.blob() && a.lines == b.lines && a.commit() == b.commit()
            }
            (
                Self::Hybrid {
                    commit: ca,
                    tree: ta,
                },
                Self::Hybrid {
                    commit: cb,
                    tree: tb,
                },
            ) => ca == cb && ta == tb,
            _ => false,
        }
    }

    /// Write `self` into `store` as a *bare* tree — no variant tag —
    /// keyed by the variant's own field names: [`Binding::Position`]
    /// writes exactly what [`facet_git_tree::serialize_into`] has always
    /// written for an [`Anchor`] (the existing stored format, unchanged
    /// byte for byte); every other variant writes its payload struct's
    /// fields the same way. [`Binding::deserialize`] recovers the variant
    /// by sniffing which entry names are present, since no discriminant is
    /// stored.
    ///
    /// `store` takes the same bound `facet_git_tree::serialize_into` does:
    /// any `gix` object-write sink — a real repository's object database,
    /// an in-memory [`facet_git_tree::ObjectStore`], or any other
    /// `gix_object::Write` implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use ents_anchor::Binding;
    /// use facet_git_tree::ObjectStore;
    ///
    /// let store = ObjectStore::default();
    /// let binding = Binding::Commit {
    ///     commit: gix::ObjectId::from_hex(b"8888888888888888888888888888888888888888").unwrap(),
    /// };
    /// let root = binding.serialize_into(&store).expect("serialize");
    /// let back = Binding::deserialize(&root, &store).expect("deserialize");
    /// assert_eq!(back, binding);
    /// ```
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] when the underlying `facet-git-tree` write fails
    /// (a backend error from `store`).
    pub fn serialize_into<W>(&self, store: &W) -> Result<ObjectId>
    where
        W: Write + ?Sized,
    {
        match self {
            Self::Commit { commit } => {
                let payload = CommitPayload {
                    commit: oid_bytes(*commit),
                };
                Ok(facet_git_tree::serialize_into(&payload, store)?)
            }
            Self::Tree {
                tree,
                path,
                witness,
            } => {
                let payload = TreePayload {
                    tree: oid_bytes(*tree),
                    path: path.clone(),
                    witness: oid_bytes(*witness),
                };
                Ok(facet_git_tree::serialize_into(&payload, store)?)
            }
            Self::Delta {
                base_tree,
                head_tree,
                path,
                base_witness,
                head_witness,
            } => {
                let payload = DeltaPayload {
                    base_tree: oid_bytes(*base_tree),
                    head_tree: oid_bytes(*head_tree),
                    path: path.clone(),
                    base_witness: oid_bytes(*base_witness),
                    head_witness: oid_bytes(*head_witness),
                };
                Ok(facet_git_tree::serialize_into(&payload, store)?)
            }
            Self::Position(anchor) => Ok(facet_git_tree::serialize_into(anchor, store)?),
            Self::Hybrid { commit, tree } => {
                let payload = HybridPayload {
                    commit: oid_bytes(*commit),
                    tree: oid_bytes(*tree),
                };
                Ok(facet_git_tree::serialize_into(&payload, store)?)
            }
        }
    }

    /// Read the [`Binding`] stored bare (no variant tag) at `id` in
    /// `store`, inferring the variant from which entry names the tree
    /// holds: `blob`+`content` → [`Binding::Position`] (decoded as
    /// [`Anchor`]); `base_tree` → [`Binding::Delta`]; `witness` (with
    /// `tree`) → [`Binding::Tree`]; exactly `{commit, tree}` →
    /// [`Binding::Hybrid`]; exactly `{commit}` → [`Binding::Commit`].
    ///
    /// `store` takes the same bound `facet_git_tree::deserialize` does:
    /// any `gix` object-read source.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] when the recognized shape fails to decode;
    /// [`Error::UnknownBindingShape`] when the entry names match none of
    /// the five shapes; [`Error::Object`] when `id` cannot be read as a
    /// tree at all.
    pub fn deserialize<F>(id: &ObjectId, store: &F) -> Result<Self>
    where
        F: Find + ?Sized,
    {
        let entries = tree_entries(id, store)?;
        let names: std::collections::BTreeSet<&str> =
            entries.iter().map(|(name, _)| name.as_str()).collect();

        if names.contains("blob") && names.contains("content") {
            let anchor: Anchor = facet_git_tree::deserialize(id, store)?;
            return Ok(Self::Position(anchor));
        }
        if names.contains("base_tree") {
            let payload: DeltaPayload = facet_git_tree::deserialize(id, store)?;
            return Ok(Self::Delta {
                base_tree: ObjectId::from_bytes_or_panic(&payload.base_tree),
                head_tree: ObjectId::from_bytes_or_panic(&payload.head_tree),
                path: payload.path,
                base_witness: ObjectId::from_bytes_or_panic(&payload.base_witness),
                head_witness: ObjectId::from_bytes_or_panic(&payload.head_witness),
            });
        }
        if names.contains("witness") && names.contains("tree") {
            let payload: TreePayload = facet_git_tree::deserialize(id, store)?;
            return Ok(Self::Tree {
                tree: ObjectId::from_bytes_or_panic(&payload.tree),
                path: payload.path,
                witness: ObjectId::from_bytes_or_panic(&payload.witness),
            });
        }
        if names.len() == 2 && names.contains("commit") && names.contains("tree") {
            let payload: HybridPayload = facet_git_tree::deserialize(id, store)?;
            return Ok(Self::Hybrid {
                commit: ObjectId::from_bytes_or_panic(&payload.commit),
                tree: ObjectId::from_bytes_or_panic(&payload.tree),
            });
        }
        if names.len() == 1 && names.contains("commit") {
            let payload: CommitPayload = facet_git_tree::deserialize(id, store)?;
            return Ok(Self::Commit {
                commit: ObjectId::from_bytes_or_panic(&payload.commit),
            });
        }

        Err(Error::UnknownBindingShape {
            id: *id,
            entries: entries.into_iter().map(|(name, _)| name).collect(),
        })
    }
}

/// The name and object id of every entry directly under the tree at `id` —
/// this crate's own copy of `facet-git-tree`'s private `find_tree_entries`,
/// needed because [`Binding::deserialize`] must inspect entry names *before*
/// it knows which `Facet` type to hand `facet_git_tree::deserialize`.
fn tree_entries<F>(id: &ObjectId, store: &F) -> Result<Vec<(String, ObjectId)>>
where
    F: Find + ?Sized,
{
    let mut buf = Vec::new();
    let data = store
        .try_find(id, &mut buf)
        .map_err(|error| Error::Object(error.to_string()))?
        .ok_or_else(|| Error::Object(format!("object {id} not found")))?;
    if data.kind != Kind::Tree {
        return Err(Error::Object(format!("object {id} is not a tree")));
    }
    let tree_ref = TreeRef::from_bytes(data.data, data.object_hash)
        .map_err(|error| Error::Object(error.to_string()))?;
    let mut out = Vec::with_capacity(tree_ref.entries.len());
    for entry in &tree_ref.entries {
        let name = std::str::from_utf8(entry.filename)
            .map_err(|_error| Error::Object("tree entry name is not valid UTF-8".to_owned()))?;
        out.push((name.to_owned(), entry.oid.to_owned()));
    }
    Ok(out)
}

/// How up to date a [`Binding`] is as of the revision [`EvalState`]
/// describes, as computed by [`revalidate`].
///
/// # Examples
///
/// ```
/// use ents_anchor::Validity;
///
/// assert_ne!(Validity::Valid, Validity::Stale);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    /// The binding's target is still present as of the state under
    /// evaluation.
    Valid,
    /// The binding's target no longer holds as of the state under
    /// evaluation, though the check itself completed.
    Stale,
    /// Whether the binding still holds could not be determined — an
    /// unresolvable revision, a missing object, or (for a
    /// [`Binding::Delta`]) no delta pair supplied to check against.
    Unknown,
}

/// The minimum state [`revalidate`] needs beyond the [`Binding`] itself: the
/// revision every variant but [`Binding::Delta`] is checked against, plus
/// the tree pair a [`Binding::Delta`] is checked against.
///
/// # Examples
///
/// ```
/// use ents_anchor::EvalState;
///
/// let state = EvalState { at: "HEAD", delta: None };
/// assert_eq!(state.at, "HEAD");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct EvalState<'a> {
    /// The revision (hex id, ref name, or revspec) the binding is being
    /// evaluated against.
    pub at: &'a str,
    /// The `(base_tree, head_tree)` pair a [`Binding::Delta`] is being
    /// evaluated against — irrelevant to every other variant.
    pub delta: Option<(ObjectId, ObjectId)>,
}

/// Check `binding`'s [`Validity`] against `state`.
///
/// Per-variant semantics: [`Binding::Commit`] is
/// [`Validity::Valid`] iff the commit is `state.at` itself or one of its
/// ancestors; [`Binding::Tree`] is [`Validity::Valid`] iff the tree appears
/// — at the recorded path, checked first as a fast path, or anywhere else
/// in `state.at`'s tree; [`Binding::Delta`] is [`Validity::Valid`] iff
/// `state.delta` is exactly the `(base_tree, head_tree)` pair, and
/// [`Validity::Unknown`] whenever `state.delta` is `None` (the pair being
/// evaluated is caller context this crate has no other way to learn);
/// [`Binding::Position`] relocates [`crate::project`]'s existing four-outcome
/// taxonomy onto three (`Current`/`Relocated` → `Valid`,
/// `Outdated`/`Deleted` → `Stale`); [`Binding::Hybrid`] — not one of the
/// four listed above — is given the natural composition: `Valid` iff both
/// its commit (checked as [`Binding::Commit`] would be) and its tree
/// (checked as [`Binding::Tree`] would be, with no recorded path so only
/// the anywhere-in-the-tree search applies) are `Valid`, `Unknown` if
/// either check is, `Stale` otherwise.
///
/// # Errors
///
/// Propagates a [`crate::project`] error other than an unresolvable
/// revision (which becomes [`Validity::Unknown`] instead, since it means
/// the state under evaluation could not be evaluated at all, not that the
/// binding itself is broken), and any I/O or decode error surfaced while
/// walking `state.at`'s tree for a [`Binding::Tree`] or [`Binding::Hybrid`]
/// check.
///
/// # Examples
///
/// ```
/// use ents_anchor::{Binding, EvalState, Validity};
///
/// # let dir = tempfile::tempdir().expect("tempdir");
/// # std::process::Command::new("git").arg("init").arg("-q").arg(dir.path()).status().unwrap();
/// # std::fs::write(dir.path().join("file.txt"), "a\n").unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path()).args(["add", "-A"]).status().unwrap();
/// # std::process::Command::new("git").arg("-C").arg(dir.path())
/// #     .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "one"])
/// #     .status().unwrap();
/// let repo = gix::open(dir.path()).expect("open");
/// let commit = repo.head_id().expect("head").detach();
/// let binding = Binding::Commit { commit };
/// let state = EvalState { at: "HEAD", delta: None };
/// assert_eq!(ents_anchor::revalidate(&repo, &binding, &state).unwrap(), Validity::Valid);
/// ```
pub fn revalidate(
    repo: &gix::Repository,
    binding: &Binding,
    state: &EvalState<'_>,
) -> Result<Validity> {
    match binding {
        Binding::Commit { commit } => Ok(commit_validity(repo, *commit, state.at)),
        Binding::Tree { tree, path, .. } => tree_validity(repo, *tree, path, state.at),
        Binding::Delta {
            base_tree,
            head_tree,
            ..
        } => Ok(delta_validity(*base_tree, *head_tree, state.delta)),
        Binding::Position(anchor) => position_validity(repo, anchor, state.at),
        Binding::Hybrid { commit, tree } => {
            let commit_v = commit_validity(repo, *commit, state.at);
            let tree_v = tree_reachable(repo, *tree, state.at)?;
            Ok(combine(commit_v, tree_v))
        }
    }
}

/// [`Validity::Unknown`] if either input is; [`Validity::Valid`] iff both
/// are; [`Validity::Stale`] otherwise — [`Binding::Hybrid`]'s composition of
/// its commit check and its tree check.
fn combine(a: Validity, b: Validity) -> Validity {
    if a == Validity::Unknown || b == Validity::Unknown {
        Validity::Unknown
    } else if a == Validity::Valid && b == Validity::Valid {
        Validity::Valid
    } else {
        Validity::Stale
    }
}

/// [`Binding::Commit`]'s (and [`Binding::Hybrid`]'s commit half's)
/// [`Validity`]: [`Validity::Unknown`] when `commit` is absent from the odb
/// or `at` cannot be resolved, else [`Validity::Valid`] iff `commit` is `at`
/// itself or one of its ancestors (via the repository's own merge-base
/// machinery, the same idiom `ents_forge` uses for review-target ancestry),
/// else [`Validity::Stale`].
fn commit_validity(repo: &gix::Repository, commit: ObjectId, at: &str) -> Validity {
    if !repo.has_object(commit) {
        return Validity::Unknown;
    }
    let Ok(target) = resolve_commit(repo, at) else {
        return Validity::Unknown;
    };
    let target_id = target.id().detach();
    if commit == target_id
        || repo
            .merge_base(commit, target_id)
            .is_ok_and(|base| base.detach() == commit)
    {
        Validity::Valid
    } else {
        Validity::Stale
    }
}

/// [`Binding::Tree`]'s [`Validity`]: the fast path (`tree` at `path` in
/// `at`'s own tree) first, falling back to [`tree_reachable`]'s recursive
/// anywhere-in-the-tree search.
fn tree_validity(repo: &gix::Repository, tree: ObjectId, path: &str, at: &str) -> Result<Validity> {
    let Ok(commit) = resolve_commit(repo, at) else {
        return Ok(Validity::Unknown);
    };
    let root = commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    if let Ok(Some(entry)) = root.lookup_entry_by_path(path)
        && entry.mode().is_tree()
        && entry.object_id() == tree
    {
        return Ok(Validity::Valid);
    }
    if tree_contains(&root, tree)? {
        Ok(Validity::Valid)
    } else {
        Ok(Validity::Stale)
    }
}

/// [`Binding::Hybrid`]'s tree-half [`Validity`]: [`tree_validity`] without a
/// recorded path to try as a fast path first — [`Binding::Hybrid`] carries
/// none.
fn tree_reachable(repo: &gix::Repository, tree: ObjectId, at: &str) -> Result<Validity> {
    let Ok(commit) = resolve_commit(repo, at) else {
        return Ok(Validity::Unknown);
    };
    let root = commit
        .tree()
        .map_err(|error| Error::Object(error.to_string()))?;
    if tree_contains(&root, tree)? {
        Ok(Validity::Valid)
    } else {
        Ok(Validity::Stale)
    }
}

/// Whether `target` is `tree` itself or the id of any subtree reachable
/// from it, at any depth — git trees form a DAG with no cycles (an entry
/// cannot name its own not-yet-written parent by content-addressed id), so
/// this recursion terminates on any well-formed tree with no explicit depth
/// guard needed.
fn tree_contains(tree: &gix::Tree<'_>, target: ObjectId) -> Result<bool> {
    if tree.id() == target {
        return Ok(true);
    }
    for entry in tree.iter() {
        let entry = entry.map_err(|error| Error::Object(error.to_string()))?;
        if !entry.mode().is_tree() {
            continue;
        }
        if entry.object_id() == target {
            return Ok(true);
        }
        let subtree = entry
            .object()
            .map_err(|error| Error::Object(error.to_string()))?
            .try_into_tree()
            .map_err(|error| Error::Object(error.to_string()))?;
        if tree_contains(&subtree, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// [`Binding::Delta`]'s [`Validity`]: identity comparison against
/// `state.delta` only, per `revalidate`'s spec — no repository access at
/// all, since a `Delta`'s evidence (the tree pair under evaluation) is
/// caller context, not something derivable from a single revision.
fn delta_validity(
    base_tree: ObjectId,
    head_tree: ObjectId,
    delta: Option<(ObjectId, ObjectId)>,
) -> Validity {
    match delta {
        Some(pair) if pair == (base_tree, head_tree) => Validity::Valid,
        Some(_) => Validity::Stale,
        None => Validity::Unknown,
    }
}

/// [`Binding::Position`]'s [`Validity`]: [`crate::project`]'s four outcomes
/// collapsed to three, with an unresolvable `at` reported as
/// [`Validity::Unknown`] rather than propagated — every other
/// [`crate::project`] error is a clearer sign of a broken anchor than of an
/// unevaluable state, so those propagate.
fn position_validity(repo: &gix::Repository, anchor: &Anchor, at: &str) -> Result<Validity> {
    match project(repo, anchor, at) {
        Ok(Projection::Current | Projection::Relocated { .. }) => Ok(Validity::Valid),
        Ok(Projection::Outdated { .. } | Projection::Deleted) => Ok(Validity::Stale),
        Err(Error::Resolve(_)) => Ok(Validity::Unknown),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "unit test"
    )]

    use facet_git_tree::ObjectStore;
    use rstest::rstest;

    use super::*;
    use crate::LineRange;
    use crate::anchor::capture;
    use crate::fixture::{commit_all, numbered, repo};

    fn hex(byte: u8) -> ObjectId {
        let hex_digit = format!("{byte:x}");
        let full = hex_digit.repeat(40);
        ObjectId::from_hex(full.as_bytes()).unwrap()
    }

    fn sample_tree() -> Binding {
        Binding::Tree {
            tree: hex(1),
            path: "src/lib.rs".to_owned(),
            witness: hex(2),
        }
    }

    fn sample_delta() -> Binding {
        Binding::Delta {
            base_tree: hex(3),
            head_tree: hex(4),
            path: "src/lib.rs".to_owned(),
            base_witness: hex(5),
            head_witness: hex(6),
        }
    }

    fn sample_commit() -> Binding {
        Binding::Commit { commit: hex(7) }
    }

    fn sample_hybrid() -> Binding {
        Binding::Hybrid {
            commit: hex(8),
            tree: hex(9),
        }
    }

    fn sample_position() -> Binding {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=5)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();
        Binding::Position(anchor)
    }

    #[rstest]
    #[case::commit(sample_commit())]
    #[case::tree(sample_tree())]
    #[case::delta(sample_delta())]
    #[case::position(sample_position())]
    #[case::hybrid(sample_hybrid())]
    fn every_variant_round_trips_through_serialize_and_deserialize(#[case] binding: Binding) {
        let store = ObjectStore::default();
        let root = binding.serialize_into(&store).expect("serialize");
        let back = Binding::deserialize(&root, &store).expect("deserialize");
        assert_eq!(back, binding);
    }

    // @relation is intentionally absent: `binding.*` has no spec id yet.
    #[test]
    fn sniffing_rejects_an_unknown_entry_set() {
        let store = ObjectStore::default();
        let root = gix_object::Write::write(&store, &gix_object::Tree { entries: vec![] }).unwrap();
        let error = Binding::deserialize(&root, &store).unwrap_err();
        assert!(matches!(error, Error::UnknownBindingShape { .. }));
    }

    #[rstest]
    #[case::commit(sample_commit(), vec![hex(7)])]
    #[case::tree(sample_tree(), vec![hex(2)])]
    #[case::delta(sample_delta(), vec![hex(5), hex(6)])]
    #[case::hybrid(sample_hybrid(), vec![hex(8)])]
    fn witnesses_are_never_empty_and_match_the_spec(
        #[case] binding: Binding,
        #[case] expected: Vec<ObjectId>,
    ) {
        assert_eq!(binding.witnesses(), expected);
        assert!(!binding.witnesses().is_empty());
    }

    #[test]
    fn witnesses_of_a_position_is_the_anchors_own_commit() {
        let binding = sample_position();
        let Binding::Position(anchor) = &binding else {
            panic!("sample_position must build a Position");
        };
        assert_eq!(binding.witnesses(), vec![anchor.commit()]);
    }

    #[test]
    fn same_target_ignores_path_and_witness_for_tree() {
        let a = Binding::Tree {
            tree: hex(1),
            path: "a.rs".to_owned(),
            witness: hex(2),
        };
        let b = Binding::Tree {
            tree: hex(1),
            path: "b.rs".to_owned(),
            witness: hex(9),
        };
        assert_ne!(a, b);
        assert!(a.same_target(&b));
    }

    #[test]
    fn same_target_ignores_path_and_witnesses_for_delta() {
        let a = Binding::Delta {
            base_tree: hex(3),
            head_tree: hex(4),
            path: "a.rs".to_owned(),
            base_witness: hex(5),
            head_witness: hex(6),
        };
        let b = Binding::Delta {
            base_tree: hex(3),
            head_tree: hex(4),
            path: "b.rs".to_owned(),
            base_witness: hex(1),
            head_witness: hex(2),
        };
        assert_ne!(a, b);
        assert!(a.same_target(&b));
    }

    #[test]
    fn same_target_distinguishes_different_variants_naming_overlapping_objects() {
        let commit = Binding::Commit { commit: hex(1) };
        let hybrid = Binding::Hybrid {
            commit: hex(1),
            tree: hex(2),
        };
        assert!(!commit.same_target(&hybrid));
    }

    #[test]
    fn same_target_of_position_compares_blob_lines_and_commit() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(
            &git_repo,
            "HEAD",
            "file.txt",
            Some(LineRange { start: 3, end: 4 }),
        )
        .unwrap();
        assert!(
            Binding::Position(anchor.clone()).same_target(&Binding::Position(anchor.clone())),
            "an anchor is always the same target as an identical copy of itself"
        );

        std::fs::write(dir.path().join("file.txt"), numbered(1..=12)).unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();
        let other = capture(
            &git_repo,
            "HEAD",
            "file.txt",
            Some(LineRange { start: 3, end: 4 }),
        )
        .unwrap();
        assert!(!Binding::Position(anchor).same_target(&Binding::Position(other)));
    }

    #[test]
    fn revalidate_commit_is_valid_for_self_and_ancestor_and_stale_otherwise() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let first = git_repo.head_id().unwrap().detach();

        std::fs::write(dir.path().join("file.txt"), "two\n").unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();
        let second = git_repo.head_id().unwrap().detach();

        let state = EvalState {
            at: "HEAD",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &Binding::Commit { commit: second }, &state).unwrap(),
            Validity::Valid
        );
        assert_eq!(
            revalidate(&git_repo, &Binding::Commit { commit: first }, &state).unwrap(),
            Validity::Valid,
            "an ancestor of the revision under evaluation is still valid"
        );

        // A commit that only exists on an unrelated, unmerged branch is
        // neither `second` nor an ancestor of it — evaluated against
        // `second` explicitly, since `HEAD` itself is about to move to the
        // unrelated branch.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["checkout", "-q", "--orphan", "other"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("other.txt"), "other\n").unwrap();
        commit_all(dir.path(), "unrelated");
        let git_repo = gix::open(dir.path()).unwrap();
        let unrelated = git_repo.head_id().unwrap().detach();

        let second_hex = second.to_string();
        let state_at_second = EvalState {
            at: &second_hex,
            delta: None,
        };
        assert_eq!(
            revalidate(
                &git_repo,
                &Binding::Commit { commit: unrelated },
                &state_at_second
            )
            .unwrap(),
            Validity::Stale
        );
    }

    #[test]
    fn revalidate_commit_is_unknown_when_absent_or_revision_unresolvable() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();

        let missing = Binding::Commit {
            commit: gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap(),
        };
        let state = EvalState {
            at: "HEAD",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &missing, &state).unwrap(),
            Validity::Unknown
        );

        let head = Binding::Commit {
            commit: git_repo.head_id().unwrap().detach(),
        };
        let unresolvable = EvalState {
            at: "not-a-revision",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &head, &unresolvable).unwrap(),
            Validity::Unknown
        );
    }

    #[test]
    fn revalidate_tree_checks_the_recorded_path_then_falls_back_to_any_path() {
        let dir = repo();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let commit = git_repo.head_id().unwrap().detach();
        let root = git_repo.find_commit(commit).unwrap().tree().unwrap();
        let sub_tree = root
            .lookup_entry_by_path("sub")
            .unwrap()
            .unwrap()
            .object_id();

        let state = EvalState {
            at: "HEAD",
            delta: None,
        };

        // Fast path: recorded at its real path.
        let at_path = Binding::Tree {
            tree: sub_tree,
            path: "sub".to_owned(),
            witness: commit,
        };
        assert_eq!(
            revalidate(&git_repo, &at_path, &state).unwrap(),
            Validity::Valid
        );

        // Anywhere fallback: recorded at a wrong path, but the same tree
        // still sits somewhere in the target's tree.
        let wrong_path = Binding::Tree {
            tree: sub_tree,
            path: "not/the/real/path".to_owned(),
            witness: commit,
        };
        assert_eq!(
            revalidate(&git_repo, &wrong_path, &state).unwrap(),
            Validity::Valid
        );

        let missing = Binding::Tree {
            tree: gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap(),
            path: "sub".to_owned(),
            witness: commit,
        };
        assert_eq!(
            revalidate(&git_repo, &missing, &state).unwrap(),
            Validity::Stale
        );
    }

    #[test]
    fn revalidate_tree_is_unknown_when_the_revision_is_unresolvable() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let binding = sample_tree();
        let state = EvalState {
            at: "not-a-revision",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &binding, &state).unwrap(),
            Validity::Unknown
        );
    }

    #[rstest]
    #[case::matching_pair(Some((hex(3), hex(4))), Validity::Valid)]
    #[case::different_pair(Some((hex(1), hex(2))), Validity::Stale)]
    #[case::no_pair(None, Validity::Unknown)]
    fn revalidate_delta_compares_identity_against_state_delta_only(
        #[case] delta: Option<(ObjectId, ObjectId)>,
        #[case] expected: Validity,
    ) {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();

        let binding = sample_delta();
        let state = EvalState { at: "HEAD", delta };
        assert_eq!(revalidate(&git_repo, &binding, &state).unwrap(), expected);
    }

    #[test]
    fn revalidate_position_maps_current_and_relocated_to_valid() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();
        let binding = Binding::Position(anchor);
        let state = EvalState {
            at: "HEAD",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &binding, &state).unwrap(),
            Validity::Valid
        );
    }

    #[test]
    fn revalidate_position_maps_deleted_to_stale() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();

        std::fs::remove_file(dir.path().join("file.txt")).unwrap();
        std::fs::write(dir.path().join("other.txt"), "x\n").unwrap();
        commit_all(dir.path(), "two");
        let git_repo = gix::open(dir.path()).unwrap();

        let binding = Binding::Position(anchor);
        let state = EvalState {
            at: "HEAD",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &binding, &state).unwrap(),
            Validity::Stale
        );
    }

    #[test]
    fn revalidate_position_is_unknown_when_the_revision_is_unresolvable() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), numbered(1..=10)).unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let anchor = capture(&git_repo, "HEAD", "file.txt", None).unwrap();

        let binding = Binding::Position(anchor);
        let state = EvalState {
            at: "not-a-revision",
            delta: None,
        };
        assert_eq!(
            revalidate(&git_repo, &binding, &state).unwrap(),
            Validity::Unknown
        );
    }

    #[test]
    fn revalidate_hybrid_is_valid_iff_both_commit_and_tree_check_out() {
        let dir = repo();
        std::fs::write(dir.path().join("file.txt"), "one\n").unwrap();
        commit_all(dir.path(), "one");
        let git_repo = gix::open(dir.path()).unwrap();
        let commit = git_repo.head_id().unwrap().detach();
        let tree = git_repo
            .find_commit(commit)
            .unwrap()
            .tree()
            .unwrap()
            .id()
            .detach();

        let state = EvalState {
            at: "HEAD",
            delta: None,
        };
        let valid = Binding::Hybrid { commit, tree };
        assert_eq!(
            revalidate(&git_repo, &valid, &state).unwrap(),
            Validity::Valid
        );

        let stale_tree = Binding::Hybrid {
            commit,
            tree: gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap(),
        };
        assert_eq!(
            revalidate(&git_repo, &stale_tree, &state).unwrap(),
            Validity::Stale
        );

        let unknown_commit = Binding::Hybrid {
            commit: gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap(),
            tree,
        };
        assert_eq!(
            revalidate(&git_repo, &unknown_commit, &state).unwrap(),
            Validity::Unknown
        );
    }
}
