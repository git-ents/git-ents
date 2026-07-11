//! Writing a run's outcome back to the repository (`effect.results-writeback`,
//! `effect.identity`): an ordinary [`ents_receive::receive`] client, never a
//! privileged write outside the gate.
//!
//! [`write_result`] builds the [`ents_model::Status`] typed tree, seals it
//! into a signed commit exactly the way [`ents_sync::resolve::merge_heads`]
//! seals a merge tip (`sign` is a caller-injected closure, so this crate
//! never holds key material — the composition root injects the worker's own
//! member key, `effect.identity`: "its result commit MUST be signed with
//! its own member key"), and hands the result to `receive` like any other
//! frontend.

use ents_model::Status;
use ents_model::trailer::Trailers;
use ents_receive::{EventSink, Mode, Outcome, Proposal, RefTransition};
use gix::refs::FullName;
use gix_object::{Commit, Find, Kind, Write, WriteTo as _};
use gix_ref_store::RefStore;

use crate::error::{Error, Result};

/// Build a signed commit recording `status` on `results_ref`, and push it
/// through [`ents_receive::receive`] — the sole path an effect's outcome
/// may re-enter the repository (`effect.results-writeback`).
///
/// `results_ref` is the caller's choice: the canonical
/// `refs/meta/results/<effect>/<short-oid>` for a designated worker, or the
/// self-run `refs/meta/self/<member>/<effect>/<short-oid>` for any other
/// member running the same effect on their own account
/// (`effect.self-run`). Which one is "official" is a refname authorization
/// rule the gate enforces (`effect.official`), not a decision this
/// function makes.
///
/// # Errors
///
/// [`Error::Facet`] if `status` cannot be serialized; [`Error::Refs`] if
/// reading `results_ref`'s current tip fails; [`Error::Receive`] if
/// `receive` itself could not reach an outcome.
///
/// # Examples
///
/// ```
/// use ents_effect::write_result;
/// use ents_model::Status;
/// use ents_receive::{Mode, NullEventSink};
/// use ents_testutil::{Keypair, MemRefStore, ObjectStore};
///
/// let refs = MemRefStore::default();
/// let objects = ObjectStore::default();
/// let key = Keypair::from_seed(1);
/// let author = gix::actor::Signature {
///     name: "worker".into(),
///     email: "worker@ents.test".into(),
///     time: gix::date::Time { seconds: 1_000, offset: 0 },
/// };
///
/// let name: gix::refs::FullName =
///     "refs/meta/results/unit/abc123456789".try_into().expect("valid");
/// let outcome = write_result(
///     &refs, &objects, &NullEventSink, name, Status::Pass, &author,
///     |payload| key.sign(payload), Mode::Advisory,
/// ).expect("evaluates");
/// assert_eq!(outcome.result, ents_receive::TxResult::Applied);
/// ```
// @relation(effect.results-writeback, effect.identity, effect.result-taxonomy, effect.self-run, scope=function)
#[expect(
    clippy::too_many_arguments,
    reason = "one input per commit-building step, mirrors ents_sync::resolve::merge_heads's shape"
)]
pub fn write_result(
    refs: &dyn RefStore,
    objects: &(impl Find + Write),
    events: &dyn EventSink,
    results_ref: FullName,
    status: Status,
    author: &gix::actor::Signature,
    sign: impl FnOnce(&[u8]) -> String,
    mode: Mode,
) -> Result<Outcome> {
    let tree = facet_git_tree::serialize_into(&status, objects)?;
    let old = refs.get(results_ref.as_ref())?;
    let parents: Vec<_> = old.into_iter().collect();

    let trailers = Trailers {
        ents_ref: Some(results_ref.clone()),
        schema_version: None,
    };
    let summary = match status {
        Status::Pass => "Record pass",
        Status::Fail => "Record fail",
        Status::Error => "Record error",
    };
    let message = format!("{summary}\n\n{}", trailers.render());
    let mut commit = Commit {
        tree,
        parents: parents.clone().into(),
        author: author.clone(),
        committer: author.clone(),
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };

    let mut payload = Vec::new();
    commit.write_to(&mut payload).map_err(|e| Error::Decode {
        oid: tree,
        detail: format!("serializing result commit failed: {e}"),
    })?;
    let pem = sign(&payload);
    commit
        .extra_headers
        .push(("gpgsig".into(), pem.trim_end().into()));

    let mut raw = Vec::new();
    commit.write_to(&mut raw).map_err(|e| Error::Decode {
        oid: tree,
        detail: format!("serializing signed result commit failed: {e}"),
    })?;
    let tip = objects
        .write_buf(Kind::Commit, &raw)
        .map_err(|e| Error::Decode {
            oid: tree,
            detail: e.to_string(),
        })?;

    let proposal = Proposal {
        transitions: vec![RefTransition {
            name: results_ref,
            old,
            new: Some(tip),
        }],
        objects: vec![tip],
        auth: None,
    };
    Ok(ents_receive::receive(
        refs, objects, events, &proposal, mode,
    )?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, reason = "unit test")]

    use ents_model::{Provenance, namespace};
    use ents_receive::{NullEventSink, TxResult};
    use ents_testutil::{Keypair, MemRefStore, ObjectStore, enroll_member};
    use gix_ref_store::RefStoreRead as _;
    use rstest::rstest;

    use super::*;

    fn author() -> gix::actor::Signature {
        gix::actor::Signature {
            name: "worker".into(),
            email: "worker@ents.test".into(),
            time: gix::date::Time {
                seconds: 1_000,
                offset: 0,
            },
        }
    }

    #[rstest]
    #[case::pass(Status::Pass)]
    #[case::fail(Status::Fail)]
    #[case::error(Status::Error)]
    // @relation(effect.results-writeback, effect.identity, effect.result-taxonomy, scope=function, role=Verifies)
    fn write_result_lands_a_signed_commit_on_the_canonical_results_ref(#[case] status: Status) {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let worker = Keypair::from_seed(1);
        enroll_member(
            &refs,
            &objects,
            "worker",
            &worker,
            Provenance::AdminRegistered,
            100,
        );

        let name = namespace::result_ref("unit", "deadbeefcafe").expect("valid");
        let outcome = write_result(
            &refs,
            &objects,
            &NullEventSink,
            name.clone(),
            status,
            &author(),
            |payload| worker.sign(payload),
            Mode::Advisory,
        )
        .expect("evaluates");
        assert_eq!(outcome.result, TxResult::Applied);
        // The gate validated the commit's signature against the worker's
        // own member key — `effect.identity`: "its result commit MUST be
        // signed with its own member key".
        let (_, verdict) = outcome.verdicts.first().expect("one transition proposed");
        assert!(verdict.is_pass());

        let tip = refs.get(name.as_ref()).expect("readable").expect("landed");
        let mut buf = Vec::new();
        let data = gix_object::Find::try_find(&objects, &tip, &mut buf)
            .expect("readable")
            .expect("present");
        let commit = gix_object::CommitRef::from_bytes(data.data, tip.kind()).expect("decodes");
        let landed: Status =
            facet_git_tree::deserialize(&commit.tree(), &objects).expect("deserializes");
        assert_eq!(landed, status);
    }

    #[rstest]
    // @relation(effect.self-run, effect.results-writeback, scope=function, role=Verifies)
    fn write_result_can_target_the_self_run_namespace() {
        let refs = MemRefStore::default();
        let objects = ObjectStore::default();
        let member = Keypair::from_seed(2);
        enroll_member(
            &refs,
            &objects,
            "bob",
            &member,
            Provenance::AdminRegistered,
            100,
        );

        let name = namespace::self_result_ref(&ents_model::MemberId::new("bob"), "unit", "abc")
            .expect("valid");
        let outcome = write_result(
            &refs,
            &objects,
            &NullEventSink,
            name.clone(),
            Status::Fail,
            &author(),
            |payload| member.sign(payload),
            Mode::Advisory,
        )
        .expect("evaluates");
        assert_eq!(outcome.result, TxResult::Applied);
        assert!(refs.get(name.as_ref()).expect("readable").is_some());
    }
}
