//! `git ents`'s dispatch: the only place a [`crate::cli::Top`] variant is
//! interpreted. Every branch is a thin call into [`crate::commands`]; no
//! business logic lives here.
#![expect(
    clippy::let_underscore_must_use,
    reason = "porcelain output to a writer (stdout in practice) is best-effort; a broken pipe \
              here is not actionable and every write is one-shot, not chained"
)]

use crate::cli::{
    AccountAction, AgentAction, Cli, CommentAction, EffectAction, HookAction, InboxAction,
    IssueAction, MembersAction, RedactAction, ReviewAction, ToolchainAction, Top,
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
            commands::setup::configure_global_signing_defaults()?;
            let _ = writeln!(out, "signing key: {}", key_path.display());
            let _ = writeln!(
                out,
                "global git config: commit.gpgsign=true, tag.gpgsign=true, push.gpgsign=if-asked"
            );
            Ok(())
        }
        Top::Bootstrap {
            username,
            server_pubkey,
            server_name,
            remote,
            key,
        } => {
            let root = LocalRoot::discover(".")?;
            commands::bootstrap::run(
                &root,
                &username,
                server_pubkey,
                server_name.as_deref().unwrap_or("forge"),
                remote.as_deref().unwrap_or("origin"),
                key,
                out,
            )
        }
        Top::Members { action } => run_members(action, out),
        Top::Account { action } => run_account(action, out),
        Top::Effect { action } => run_effect(action, out),
        Top::Toolchain { action } => run_toolchain(action, out),
        Top::Comment { action } => run_comment(action, out),
        Top::Issue { action } => run_issue(action, out),
        Top::Agent { action } => run_agent(action, out),
        Top::Review { action } => run_review(action, out),
        Top::Inbox { action } => run_inbox(action, out),
        Top::Redact { action } => run_redact(action, out),
        Top::Hook { action } => run_hook(action, out),
        Top::Login { url, code, key } => commands::login::run(&url, &code, key, out),
        Top::Serve {
            port,
            key,
            hosted: false,
            ..
        } => {
            let root = LocalRoot::discover(".")?;
            commands::serve::run(root, port, key, out)
        }
        // Root choice off the flag belongs exactly here, in the
        // composition root's dispatch (`arch.no-hosted-branch` bans it in
        // library code, not in `exe`).
        Top::Serve {
            port,
            key,
            hosted: true,
            public_host,
            path,
        } => {
            let root = HostedRoot::open(path.unwrap_or_else(|| ".".into()))?;
            commands::serve::run_hosted(root, port, key, public_host, out)
        }
        Top::Lsp { key } => {
            // The lens speaks LSP over stdin/stdout, so nothing may be
            // written to `out` (the process's stdout) here — that stream is
            // the protocol channel. It reuses the exact same local root
            // `serve` does (`lens.serve`), adding only the LSP frontend.
            let root = LocalRoot::discover(".")?;
            commands::lsp::run(root, key)
        }
    }
}

fn run_members(action: MembersAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        MembersAction::List => {
            for (username, member) in commands::members::list(&root.refs, &root.objects)? {
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
        CommentAction::List {
            worktree,
            state,
            open,
            context,
            porcelain,
        } => {
            let state = match (state, open) {
                (Some(state), false) => Some(state),
                (None, true) => Some("open".to_owned()),
                (None, false) => None,
                (Some(_), true) => {
                    return Err(crate::Error::InvalidArgument(
                        "--open is shorthand for --state open; give one or the other".into(),
                    ));
                }
            };
            let filter = ents_forge::comment::ListFilter { state, context };
            let (rows, unreadable) = commands::comment::list_projected(&root, worktree, &filter)?;
            if porcelain {
                // Porcelain stays rows-only for format stability; a tool
                // that wants the unreadable refs takes them from
                // `comment::list_projected` itself.
                let _ = write!(out, "{}", commands::comment::porcelain(&rows));
            } else {
                for row in rows {
                    let _ = writeln!(
                        out,
                        "{}\t{}\t{}",
                        ents_forge::abbreviate_id(&row.id),
                        row.comment.state,
                        row.comment.body
                    );
                }
                for entry in unreadable {
                    let _ = writeln!(out, "! {}\tunreadable: {}", entry.refname, entry.error);
                }
            }
        }
        CommentAction::Add {
            path,
            body,
            lines,
            rev,
            worktree,
            context,
            parent,
            key,
        } => {
            let new = ents_forge::comment::NewComment {
                body,
                path,
                lines,
                rev,
                worktree,
                context,
                parent,
            };
            let id = commands::comment::add(&root, new, key)?;
            let _ = writeln!(out, "commented {id}");
        }
        CommentAction::Reply { id, body, key } => {
            let reply_id = commands::comment::reply(&root, &id, body, key)?;
            let _ = writeln!(out, "replied {reply_id}");
        }
        CommentAction::Resolve { id, key } => {
            commands::comment::set_state(&root, &id, true, key)?;
            let _ = writeln!(out, "resolved {id}");
        }
        CommentAction::Reopen { id, key } => {
            commands::comment::set_state(&root, &id, false, key)?;
            let _ = writeln!(out, "reopened {id}");
        }
        CommentAction::Show { id, rev, worktree } => {
            let (comment, projected) = commands::comment::show(&root, &id, &rev, worktree)?;
            let _ = writeln!(out, "state: {}", comment.state);
            if let Some(context) = &comment.context {
                let _ = writeln!(out, "context: {context}");
            }
            if let Some(parent) = &comment.parent {
                let _ = writeln!(out, "parent: {parent}");
            }
            if let Some((anchor, projection)) = projected {
                let _ = writeln!(out, "path: {}", anchor.path);
                let target = if worktree { "worktree" } else { rev.as_str() };
                let _ = writeln!(out, "projection at {target}: {projection:?}");
            }
            let _ = writeln!(out, "body: {}", comment.body);
        }
    }
    Ok(())
}

fn run_issue(action: IssueAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        IssueAction::List => {
            for (id, issue) in commands::issue::list(&root)? {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}",
                    ents_forge::abbreviate_id(&id),
                    issue.state,
                    issue.title
                );
            }
        }
        IssueAction::Show { id } => {
            let issue = commands::issue::show(&root, &id)?;
            let _ = writeln!(out, "title: {}", issue.title);
            let _ = writeln!(out, "state: {}", issue.state);
            if !issue.assignees.is_empty() {
                let names: Vec<_> = issue.assignees.iter().map(ToString::to_string).collect();
                let _ = writeln!(out, "assignees: {}", names.join(", "));
            }
            if !issue.labels.is_empty() {
                let _ = writeln!(out, "labels: {}", issue.labels.join(", "));
            }
            let _ = writeln!(out, "body: {}", issue.body);
        }
        IssueAction::New {
            title,
            body,
            state,
            label,
            assignee,
            key,
        } => {
            let id = commands::issue::new(&root, title, body, state, label, assignee, key)?;
            let _ = writeln!(out, "opened {id}");
        }
        IssueAction::Edit {
            id,
            state,
            label,
            assignee,
            key,
        } => {
            commands::issue::edit(&root, &id, state, label, assignee, key)?;
            let _ = writeln!(out, "edited {id}");
        }
    }
    Ok(())
}

fn run_agent(action: AgentAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        AgentAction::New {
            prompt,
            model,
            toolchain,
            base,
            review_policy,
            retry_of,
            key,
        } => {
            let id = commands::agent::new(
                &root,
                prompt,
                model,
                toolchain,
                base,
                review_policy,
                retry_of,
                key,
            )?;
            let _ = writeln!(out, "started {id}");
        }
        AgentAction::Plan { id, text, key } => {
            commands::agent::plan(&root, &id, text, key)?;
            let _ = writeln!(out, "drafted plan for {id}");
        }
        AgentAction::Confirm {
            id,
            review_policy,
            key,
        } => {
            commands::agent::confirm(&root, &id, review_policy, key)?;
            let _ = writeln!(out, "confirmed {id}");
        }
        AgentAction::List => {
            for (id, session) in commands::agent::list(&root)? {
                let _ = writeln!(
                    out,
                    "{}\t{:?}\t{}",
                    ents_forge::abbreviate_id(&id),
                    session.meta.status,
                    session.meta.member
                );
            }
        }
        AgentAction::Show { id } => {
            let session = commands::agent::show(&root, &id)?;
            let _ = writeln!(out, "member: {}", session.meta.member);
            let _ = writeln!(out, "status: {:?}", session.meta.status);
            let _ = writeln!(out, "base: {}", session.meta.base_ref);
            let _ = writeln!(out, "queued: {}", session.queued());
            let _ = writeln!(
                out,
                "awaiting confirmation: {}",
                session.awaiting_confirmation()
            );
            if let Some(plan) = &session.plan {
                let _ = writeln!(out, "plan: {plan}");
            }
            if let Some(branch) = &session.meta.result_branch {
                let _ = writeln!(out, "result branch: {branch}");
            }
        }
    }
    Ok(())
}

fn run_review(action: ReviewAction, out: &mut impl std::io::Write) -> Result<()> {
    let root = LocalRoot::discover(".")?;
    match action {
        ReviewAction::New {
            target,
            verdict,
            body,
            key,
        } => {
            let new = ents_forge::review::NewReview {
                target,
                verdict: verdict.parse()?,
                body,
            };
            let target = commands::review::new(&root, new, key)?;
            let _ = writeln!(out, "reviewed {}", ents_forge::abbreviate_id(&target));
        }
        ReviewAction::List { target } => {
            for ((review_target, member), review) in commands::review::list(&root, target)? {
                let _ = writeln!(
                    out,
                    "{}\t{member}\t{}\t{}",
                    ents_forge::abbreviate_id(&review_target),
                    review.target(),
                    review.verdict
                );
            }
        }
        ReviewAction::Show { target, member } => {
            let (review, thread) = commands::review::show(&root, &target, &member)?;
            let _ = writeln!(out, "target: {}", review.target());
            let _ = writeln!(out, "verdict: {}", review.verdict);
            let _ = writeln!(out, "body: {}", review.body);
            for (comment_id, comment) in thread {
                let _ = writeln!(
                    out,
                    "comment {}\t{}\t{}",
                    ents_forge::abbreviate_id(&comment_id),
                    comment.state,
                    comment.body
                );
            }
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
