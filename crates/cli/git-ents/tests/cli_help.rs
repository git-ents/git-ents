//! A snapshot-style check of `git-ents --help`'s content — every
//! porcelain subcommand family named, with the crate's own doc as the
//! description.
//!
//! Not a byte-for-byte `trycmd` snapshot: `figue`'s help renderer emits
//! ANSI color codes unconditionally (confirmed by inspecting raw output
//! bytes even when redirected to a file, not a terminal), which would
//! make a literal snapshot fragile against `figue` version bumps rather
//! than against this crate's own `--help` content. Substring assertions
//! on the captured (ANSI-code-interspersed but still substring-intact)
//! output check the content that matters — every subcommand family is
//! named — without pinning exact styling.

#![allow(clippy::expect_used, reason = "integration test")]

mod common;

use std::process::Command;

fn help_output() -> String {
    let output = Command::new(common::bin_path())
        .arg("--help")
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).expect("utf8")
}

/// Every top-level porcelain subcommand family this phase lands
/// (`docs/development-plan.adoc`'s phase-6 row) is named in `--help`.
// @relation(roots.local, scope=function, role=Verifies)
#[test]
fn help_names_every_subcommand_family() {
    let help = help_output();
    for name in [
        "setup",
        "members",
        "account",
        "effect",
        "toolchain",
        "comment",
        "issue",
        "review",
        "inbox",
        "redact",
        "hook",
        "serve",
        "lsp",
    ] {
        assert!(help.contains(name), "--help must mention {name:?}:\n{help}");
    }
}

/// `git ents lsp --help` documents the stdio-only, no-socket,
/// no-git-transport contract `lens.serve` requires, not just a bare flag
/// list.
#[test]
// @relation(lens.serve, scope=function, role=Verifies)
fn lsp_help_documents_the_stdio_only_contract() {
    let output = Command::new(common::bin_path())
        .args(["lsp", "--help"])
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("stdio") || text.contains("stdin"), "{text}");
    assert!(text.contains("socket"), "{text}");
}

/// `--help` carries this crate's own one-line responsibility, not a
/// generic placeholder.
#[test]
fn help_carries_the_crate_doc() {
    let help = help_output();
    assert!(help.contains("Local root wiring"), "{help}");
}

/// A subcommand's own `--help` (e.g. `members --help`) also renders,
/// confirming the nested subcommand grammar is reachable, not just the
/// top level.
#[test]
fn nested_help_reaches_a_subcommand() {
    let output = Command::new(common::bin_path())
        .args(["members", "--help"])
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("list"), "{text}");
    assert!(text.contains("revoke"), "{text}");
}

/// `git ents serve --help` documents the loopback-only, no-git-transport
/// contract `roots.local` requires, not just a bare flag list.
#[test]
// @relation(roots.local, scope=function, role=Verifies)
fn serve_help_documents_the_loopback_only_contract() {
    let output = Command::new(common::bin_path())
        .args(["serve", "--help"])
        .output()
        .expect("runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("loopback"), "{text}");
    assert!(text.contains("port"), "{text}");
}
