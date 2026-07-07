//! The queued work order: one effect plus its materialized inputs, encoded
//! as the text `git_ents_effect_queue.payload` carries. A hand-rolled line
//! format (like `git-effect`'s job files) rather than a serialization
//! dependency, per the dependency policy.
//!
//! One `key value` pair per line; `command` — the only field that can
//! legitimately contain newlines — is escaped ([`escape`]/[`unescape`]).
//! A payload [`decode`] cannot read is a poison row: the dispatcher
//! completes it without running anything (mirroring how
//! `git_effect::engine` drops a malformed job file), rather than retrying
//! it forever.

use std::collections::BTreeMap;

use git_backend::{EffectDef, MaterializedInputs};
use gix_hash::ObjectId;

/// One queue row's decoded work order.
#[derive(Debug, Clone)]
pub struct Job {
    /// What to run.
    pub effect: EffectDef,
    /// The materialized inputs to run it against.
    pub inputs: MaterializedInputs,
}

/// Encode `job` as the queue payload text [`decode`] reads.
#[must_use]
pub fn encode(job: &Job) -> String {
    let mut out = String::new();
    out.push_str(&format!("name {}\n", job.effect.name));
    if let Some(command) = &job.effect.command {
        out.push_str(&format!("command {}\n", escape(command)));
    }
    if let Some(image) = &job.effect.image {
        out.push_str(&format!("image {image}\n"));
    }
    out.push_str(&format!("tree {}\n", job.inputs.tree));
    for (name, path) in &job.inputs.toolchain_paths {
        out.push_str(&format!("toolchain {name} {path}\n"));
    }
    if let Some(cache) = &job.inputs.cache {
        out.push_str(&format!("cache {cache}\n"));
    }
    out
}

/// Decode a queue payload, or `None` when it is malformed (an unknown key,
/// a missing `name`/`tree`, an invalid tree oid).
#[must_use]
pub fn decode(payload: &str) -> Option<Job> {
    let mut name = None;
    let mut command = None;
    let mut image = None;
    let mut tree = None;
    let mut toolchain_paths = BTreeMap::new();
    let mut cache = None;
    for line in payload.lines() {
        if line.is_empty() {
            continue;
        }
        let (key, rest) = line.split_once(' ')?;
        match key {
            "name" => name = Some(rest.to_owned()),
            "command" => command = Some(unescape(rest)),
            "image" => image = Some(rest.to_owned()),
            "tree" => tree = Some(ObjectId::from_hex(rest.as_bytes()).ok()?),
            "toolchain" => {
                let (toolchain, path) = rest.split_once(' ')?;
                toolchain_paths.insert(toolchain.to_owned(), path.to_owned());
            }
            "cache" => cache = Some(rest.to_owned()),
            _ => return None,
        }
    }
    Some(Job {
        effect: EffectDef {
            name: name?,
            command,
            image,
        },
        inputs: MaterializedInputs {
            tree: tree?,
            toolchain_paths,
            cache,
        },
    })
}

/// Escape backslashes and newlines so a multi-line command survives the
/// one-pair-per-line format.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Invert [`escape`].
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "unit test")]

    use super::*;

    fn tree() -> ObjectId {
        ObjectId::from_hex(b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap()
    }

    #[test]
    fn a_job_round_trips_through_the_payload_format() {
        let mut toolchain_paths = BTreeMap::new();
        toolchain_paths.insert("rust".to_owned(), "/toolchains/aaa/bin".to_owned());
        let job = Job {
            effect: EffectDef {
                name: "test".to_owned(),
                command: Some("cargo fmt --check\ncargo test".to_owned()),
                image: Some("debian:stable-slim".to_owned()),
            },
            inputs: MaterializedInputs {
                tree: tree(),
                toolchain_paths,
                cache: Some("sccache".to_owned()),
            },
        };
        let decoded = decode(&encode(&job)).unwrap();
        assert_eq!(decoded.effect, job.effect);
        assert_eq!(decoded.inputs.tree, job.inputs.tree);
        assert_eq!(decoded.inputs.toolchain_paths, job.inputs.toolchain_paths);
        assert_eq!(decoded.inputs.cache, job.inputs.cache);
    }

    #[test]
    fn a_minimal_job_round_trips() {
        let job = Job {
            effect: EffectDef {
                name: "test".to_owned(),
                command: None,
                image: None,
            },
            inputs: MaterializedInputs {
                tree: tree(),
                toolchain_paths: BTreeMap::new(),
                cache: None,
            },
        };
        let decoded = decode(&encode(&job)).unwrap();
        assert_eq!(decoded.effect, job.effect);
        assert_eq!(decoded.inputs.cache, None);
    }

    #[test]
    fn malformed_payloads_decode_to_none() {
        assert!(decode("").is_none());
        assert!(decode("name only-a-name\n").is_none()); // no tree
        assert!(decode("tree bbbb\nname x\n").is_none()); // bad oid
        assert!(decode("unknown key\n").is_none());
    }
}
