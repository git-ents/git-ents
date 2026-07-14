//! Workspace-level pairwise schema-disjointness test (`gate.identity-binding`):
//! "entity structs MUST stay pairwise disjoint under this decode, a
//! property held by test rather than by a stored marker" — so one signed
//! genesis commit can never be admitted under two different meta-ref
//! namespaces. Every genesis-borne entity struct — [`Comment`], [`Issue`],
//! [`Member`], [`Effect`], [`Review`], [`ResultRecord`] — lives here rather
//! than in `ents-testutil`: that crate is kernel-side and must never
//! depend on `ents-forge` (`ents-forge`'s own crate doc), but `Comment` and
//! `Review` are defined in `ents-forge`, so this workspace-wide check can
//! only be written once this crate compiles against every entity it needs.
//!
//! This is *not* the same claim `model.extensibility`'s
//! `every_entity_shape_name_tracks_its_struct_declaration` test makes
//! (each type reflects under its own Rust name): a type could carry a
//! distinct name and still accidentally decode another type's tree if
//! their field shapes lined up. This test instead builds one real tree per
//! entity and asserts every *other* entity's decoder refuses it.
//!
//! Plain `facet_git_tree::deserialize` alone is *not* the mechanism this
//! proves: it fills declared fields by name and leaves an absent one at
//! its default (`None` for an `Option`), silently ignoring any tree entry
//! that names no field of `T` — so it is not by itself strict. The actual
//! disjointness mechanism is `ents_gate::verify`'s private `strict_decode`,
//! which layers "every tree entry must name one of `T`'s fields, an
//! unknown one refusing" on top of that lenient decode before trusting it
//! (`gate.identity-binding`). That helper is kernel-private, so this test
//! mirrors its exact two-step check rather than reimplementing a third,
//! looser one.

#![allow(clippy::expect_used, reason = "integration test")]

use ents_forge::comment::Comment;
use ents_forge::issue::Issue;
use ents_forge::review::Review;
use ents_model::{Effect, Member, MemberId, Provenance, ResultRecord, Status};
use facet::{Type, UserType};
use facet_git_tree::ObjectStore;
use gix_hash::ObjectId;
use gix_object::{Find, Kind, TreeRef};

/// One entity's own tree oid, tagged with the type name it was serialized
/// from — used only to report which pair failed to stay disjoint.
struct Sample {
    name: &'static str,
    tree: ObjectId,
}

/// One tree per entity type this phase's genesis/composite binding covers,
/// all written into the same `store` so every decoder below can read every
/// tree.
fn samples(store: &ObjectStore) -> Vec<Sample> {
    let target = ObjectId::null(gix_hash::Kind::Sha1);
    vec![
        Sample {
            name: "Comment",
            tree: facet_git_tree::serialize_into(
                &Comment {
                    body: "looks off by one".to_owned(),
                    state: "open".to_owned(),
                    anchor: None,
                    context: Some("issues/abc".to_owned()),
                    parent: None,
                },
                store,
            )
            .expect("serialize Comment"),
        },
        Sample {
            name: "Issue",
            tree: facet_git_tree::serialize_into(
                &Issue {
                    title: "gate rejects a valid signature".to_owned(),
                    body: "steps to reproduce".to_owned(),
                    state: "open".to_owned(),
                    assignees: vec![MemberId::new("jdc")],
                    labels: vec!["bug".to_owned()],
                },
                store,
            )
            .expect("serialize Issue"),
        },
        Sample {
            name: "Member",
            tree: facet_git_tree::serialize_into(
                &Member::new(
                    "jdc",
                    "ssh-ed25519 AAAA... jdc",
                    Provenance::AdminRegistered,
                ),
                store,
            )
            .expect("serialize Member"),
        },
        Sample {
            name: "Effect",
            tree: facet_git_tree::serialize_into(
                &Effect {
                    name: "unit".to_owned(),
                    trigger: "rev(refs/heads/main)".to_owned(),
                    toolchains: vec!["rust-stable".to_owned()],
                    run: "cargo nextest run".to_owned(),
                },
                store,
            )
            .expect("serialize Effect"),
        },
        Sample {
            name: "Review",
            tree: facet_git_tree::serialize_into(
                &Review::new(target, ents_forge::review::Verdict::Approve, "looks good"),
                store,
            )
            .expect("serialize Review"),
        },
        Sample {
            name: "ResultRecord",
            tree: facet_git_tree::serialize_into(
                &ResultRecord::new("unit", target, Status::Pass),
                store,
            )
            .expect("serialize ResultRecord"),
        },
    ]
}

/// The names of every top-level entry in `tree` — mirrors
/// `ents_gate::object::tree_entry_names` exactly (that helper is
/// `pub(crate)` to the kernel gate crate, so this test cannot call it
/// directly).
fn tree_entry_names(tree: &ObjectId, store: &ObjectStore) -> Vec<String> {
    let mut buf = Vec::new();
    let Ok(Some(data)) = store.try_find(tree, &mut buf) else {
        return Vec::new();
    };
    if data.kind != Kind::Tree {
        return Vec::new();
    }
    let Ok(parsed) = TreeRef::from_bytes(data.data, tree.kind()) else {
        return Vec::new();
    };
    parsed
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(e.filename).into_owned())
        .collect()
}

/// Whether `tree` strictly decodes as `T` — mirrors
/// `ents_gate::verify::strict_decode` exactly: every top-level tree entry
/// must name one of `T`'s declared fields (an unknown one refuses), and
/// only then does the ordinary (lenient) `facet_git_tree::deserialize`
/// get to run. This, not the bare decode alone, is the mechanism
/// `gate.identity-binding` names.
fn decodes_as<T: for<'facet> facet::Facet<'facet>>(tree: &ObjectId, store: &ObjectStore) -> bool {
    let Type::User(UserType::Struct(st)) = T::SHAPE.ty else {
        return false;
    };
    let fields: Vec<&str> = st.fields.iter().map(|f| f.name).collect();
    if tree_entry_names(tree, store)
        .iter()
        .any(|entry| !fields.contains(&entry.as_str()))
    {
        return false;
    }
    facet_git_tree::deserialize::<T>(tree, store).is_ok()
}

/// One entity type's [`decodes_as`] instantiation.
type Decoder = fn(&ObjectId, &ObjectStore) -> bool;

/// One named decoder per entity type — a `fn` pointer table so the test
/// below can loop every (sample, decoder) pair generically instead of one
/// hand-written assertion per combination.
fn decoders() -> Vec<(&'static str, Decoder)> {
    vec![
        ("Comment", decodes_as::<Comment>),
        ("Issue", decodes_as::<Issue>),
        ("Member", decodes_as::<Member>),
        ("Effect", decodes_as::<Effect>),
        ("Review", decodes_as::<Review>),
        ("ResultRecord", decodes_as::<ResultRecord>),
    ]
}

/// `gate.identity-binding`: every sample decodes as its own type and
/// refuses every other type in this list — pairwise disjointness held by
/// this test, not a stored `.schema` marker.
#[test]
// @relation(gate.identity-binding, meta-ref.typed-tree, model.comment, model.issue, model.review, model.result-identity, scope=function, role=Verifies)
fn entity_schemas_stay_pairwise_disjoint_under_strict_decode() {
    let store = ObjectStore::default();
    let samples = samples(&store);
    let decoders = decoders();

    for sample in &samples {
        for (decoder_name, decode) in &decoders {
            let decodes = decode(&sample.tree, &store);
            let should_decode = *decoder_name == sample.name;
            assert_eq!(
                decodes,
                should_decode,
                "a {}'s tree {} decode as {decoder_name} (expected {should_decode}) — entity \
                 schemas must stay pairwise disjoint under strict decode (gate.identity-binding)",
                sample.name,
                if decodes { "does" } else { "does not" },
            );
        }
    }
}
