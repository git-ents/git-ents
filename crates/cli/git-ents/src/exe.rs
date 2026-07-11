//! `git ents`'s dispatch: the only place a [`crate::cli::Top`] variant is
//! interpreted. Every branch is a thin call into [`crate::commands`]; no
//! business logic lives here.
#![expect(
    clippy::let_underscore_must_use,
    reason = "porcelain output to a writer (stdout in practice) is best-effort; a broken pipe \
              here is not actionable and every write is one-shot, not chained"
)]

use crate::cli::{
    AccountAction, Cli, CommentAction, EffectAction, HookAction, InboxAction, MembersAction,
    RedactAction, ToolchainAction, Top,
};
use crate::commands;
use crate::error::Result;
use crate::root::{HostedRoot, LocalRoot};

/// Run `cli` against the repository discovered from the current
/// directory, writing porcelain output to `out`.
///
/// # Errors
///
/// Any [`crate::Error`] the dispatched command reports.
pub fn run(cli: Cli, out: &mut impl std::io::Write) -> Result<()> {
    match cli.command {
        Top::Setup {
            key,
            hosted: true,
            path,
        } => {
            let target = path.unwrap_or_else(|| ".".into());
            let key_path = commands::setup::run_hosted(&target, key)?;
            let _ = writeln!(out, "signing key: {}", key_path.display());
            let _ = writeln!(out, "hooks installed in {}/hooks", target.display());
            Ok(())
        }
        Top::Setup {
            key,
            hosted: false,
            path: _,
        } => {
            let root = LocalRoot::discover(".")?;
            let key_path = commands::setup::run(&root, key)?;
            let _ = writeln!(out, "signing key: {}", key_path.display());
            Ok(())
        }
        Top::Members { action } => run_members(action, out),
        Top::Account { action } => run_account(action, out),
        Top::Effect { action } => run_effect(action, out),
        Top::Toolchain { action } => run_toolchain(action, out),
        Top::Comment { action } => run_comment(action, out),
        Top::Inbox { action } => run_inbox(action, out),
        Top::Redact { action } => run_redact(action, out),
        Top::Hook { action } => run_hook(action, out),
    }
}

fn run_members(action: MembersAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        MembersAction::List => {
            for (username, member) in commands::members::list(&root)? {
                let _ = writeln!(
                    out,
                    "{username}\t{:?}\t{:?}",
                    member.state, member.provenance
                );
            }
        }
        MembersAction::Add {
            username,
            pubkey,
            key,
        } => {
            commands::members::add(&root, &username, pubkey, key)?;
            let _ = writeln!(out, "enrolled {username}");
        }
        MembersAction::Remove { username, key } => {
            commands::members::remove(&root, &username, key)?;
            let _ = writeln!(out, "removed {username}");
        }
        MembersAction::Revoke { username, key } => {
            commands::members::set_revoked(&root, &username, true, key)?;
            let _ = writeln!(out, "revoked {username}");
        }
        MembersAction::Unrevoke { username, key } => {
            commands::members::set_revoked(&root, &username, false, key)?;
            let _ = writeln!(out, "unrevoked {username}");
        }
        MembersAction::Check { key } => match commands::members::check(&root, key)? {
            Some((username, state)) => {
                let _ = writeln!(out, "{username}\t{state:?}");
            }
            None => {
                let _ = writeln!(out, "not a member");
            }
        },
    }
    Ok(())
}

fn run_account(action: AccountAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        AccountAction::Show => {
            let account = commands::account::show(&root)?;
            let _ = writeln!(out, "member: {}", account.member);
            let _ = writeln!(out, "login: {}", account.login);
        }
        AccountAction::Create { member, login, key } => {
            commands::account::create(&root, member, login, key)?;
            let _ = writeln!(out, "account created");
        }
    }
    Ok(())
}

fn run_effect(action: EffectAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        EffectAction::List => {
            for (name, effect) in commands::effect::list(&root)? {
                let _ = writeln!(out, "{name}\t{}", effect.trigger);
            }
        }
        EffectAction::Show { name, at } => {
            let (effect, status) = commands::effect::show(&root, &name, at)?;
            let _ = writeln!(out, "trigger: {}", effect.trigger);
            let _ = writeln!(out, "run: {}", effect.run);
            let _ = writeln!(out, "result: {status:?}");
        }
        EffectAction::Add {
            name,
            on,
            run,
            toolchain,
            key,
        } => {
            commands::effect::add(&root, &name, on, run, toolchain, key)?;
            let _ = writeln!(out, "defined {name}");
        }
        EffectAction::Run { name, at, key } => {
            let outcomes = commands::effect::run(&root, &name, at, key, root.executor.as_ref())?;
            for (oid, outcome) in outcomes {
                let _ = writeln!(out, "{oid}\t{:?}", outcome.result);
            }
        }
        EffectAction::Log { name } => {
            for (oid, status) in commands::effect::log(&root, &name)? {
                let _ = writeln!(out, "{oid}\t{status:?}");
            }
        }
    }
    Ok(())
}

fn run_toolchain(action: ToolchainAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        ToolchainAction::List => {
            for name in commands::toolchain::list(&root)? {
                let _ = writeln!(out, "{name}");
            }
        }
        ToolchainAction::Import { name, bin, key } => {
            commands::toolchain::import(&root, &name, &bin, key)?;
            let _ = writeln!(out, "imported {name}");
        }
        ToolchainAction::View { name } => {
            let (toolchain, recipe) = commands::toolchain::view(&root, &name)?;
            let _ = writeln!(out, "name: {}", toolchain.name);
            let _ = writeln!(out, "recipe: {recipe:?}");
        }
        ToolchainAction::Log { name } => {
            for oid in commands::toolchain::log(&root, &name)? {
                let _ = writeln!(out, "{oid}");
            }
        }
    }
    Ok(())
}

fn run_comment(action: CommentAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        CommentAction::List => {
            for (id, comment) in commands::comment::list(&root)? {
                let _ = writeln!(out, "{id}\t{}", comment.body);
            }
        }
        CommentAction::Add {
            path,
            body,
            lines,
            rev,
            key,
        } => {
            let id = commands::comment::add(&root, &path, body, lines, &rev, key)?;
            let _ = writeln!(out, "commented {id}");
        }
        CommentAction::Show { id, rev } => {
            let (comment, anchor, projection) = commands::comment::show(&root, &id, &rev)?;
            let _ = writeln!(out, "path: {}", anchor.path);
            let _ = writeln!(out, "projection: {projection:?}");
            let _ = writeln!(out, "body: {}", comment.body);
        }
    }
    Ok(())
}

fn run_inbox(action: InboxAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        InboxAction::List => {
            for entry in commands::inbox::list(&root)? {
                let _ = writeln!(out, "{entry}");
            }
        }
        InboxAction::Adopt { entry, key } => {
            commands::inbox::adopt(&root, &entry, key)?;
            let _ = writeln!(out, "adopted {entry}");
        }
    }
    Ok(())
}

fn run_redact(action: RedactAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        RedactAction::List => {
            for (id, redaction) in commands::redact::list(&root)? {
                let _ = writeln!(out, "{id}\t{}", redaction.reason);
            }
        }
        RedactAction::Add { oid, reason, key } => {
            commands::redact::add(&root, &oid, reason, key)?;
            let _ = writeln!(out, "redacted {oid}");
        }
    }
    Ok(())
}

fn run_hook(action: HookAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = HostedRoot::open(".")?;
    match action {
        HookAction::PreReceive => {
            let stdin = std::io::stdin();
            crate::hook::pre_receive(&root, stdin.lock(), out)
        }
        HookAction::PostReceive => {
            // Nothing to do: skip resolving a worker signing key and
            // executor entirely rather than fail a repository that has
            // not configured a hosted worker identity yet but also has no
            // effects defined (the common case for a brand-new
            // repository's very first pushes).
            if root.events.pending().is_empty() {
                let _ = writeln!(out, "ran 0 effect(s)");
                return Ok(());
            }
            let scratch = tempfile::tempdir().map_err(|source| crate::Error::Io {
                path: root.path.clone(),
                source,
            })?;
            let cache = tempfile::tempdir().map_err(|source| crate::Error::Io {
                path: root.path.clone(),
                source,
            })?;
            let repo = gix::open(&root.path)?;
            let key_path = crate::sign::resolve_key_path(&repo, None)?;
            let signer = crate::sign::Signer::load(&key_path)?;
            let ran = crate::hook::post_receive(
                &root,
                root.executor.as_ref(),
                scratch.path(),
                cache.path(),
                &signer,
            )?;
            let _ = writeln!(out, "ran {ran} effect(s)");
            Ok(())
        }
        HookAction::Reconcile => {
            let _ = writeln!(out, "reconciled: {} pending", root.events.pending().len());
            Ok(())
        }
    }
}
