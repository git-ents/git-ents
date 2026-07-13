---
name: ents-comments
description: Read, address, and leave git-ents comments — the universal conversational primitive anchored into source. Use when asked to "address the comments", "address all comments in the working tree", act on review feedback, or leave a comment/reply on code. Editor-agnostic: the same `git ents comment` CLI an editor, the web UI, and you all drive.
---

# Addressing and leaving ents comments

A git-ents comment is a body anchored to exact content — a blob, an optional line range, a commit (or the working tree).
Comments live on `refs/meta/comments/*`, one ref each, and carry a `state` (`open` or `resolved`).
Editors, the web UI, and the CLI are three frontends of one mechanism, so a comment left in Zed is the same entity you resolve here.

A comment's (and an issue's) `<id>` is the oid of its own genesis commit — never minted, always derivable from the signed content.
The default (non-`--porcelain`) listing abbreviates it for display exactly as git abbreviates a commit oid; `--porcelain` and refnames always carry the full oid.
Every `<id>` argument below needs the **full** id — copy it from `--porcelain` output (or the id an `add`/`new`/`reply` command just printed), not the abbreviated form a plain listing shows.

Everything below is `git ents comment …`.
Run it from inside the repo.
There is no MCP server and no daemon — just the CLI.

## The loop: "address all comments in the working tree"

This is the primary workflow.
A human (or an agent) leaves open comments; you fix the code they point at and resolve them.

1. **Enumerate open comments, projected onto the working tree.**
   Use the porcelain form — it is stable and designed for you to parse:

   ```text
   git ents comment list --worktree --open --porcelain
   ```

   Records are blank-line-separated.
   Each starts with:

   ```text
   <id> <state> <projection> <location>
   ```

   - `<projection>` is `current`, `relocated`, `outdated`, `deleted`, or

   `-` (the comment has no anchor).
   It is the anchor projected onto the **working tree's current bytes**, so `current` means the comment still points at the exact code you are looking at.

   - `<location>` is `path:start-end`, or `path` for a whole-file anchor,

   or `-` when there is no anchor or the file is gone.

   - Optional `context <c>` and `parent <id>` lines follow, then the body

   with **every body line prefixed by one tab**.

2. **Address each open comment.**
   Read the location, make the change the comment asks for.
   Treat `relocated` as authoritative about where the code moved; treat `outdated`/`deleted` as a signal the comment may no longer apply — read the body and decide, don't blindly resolve.

3. **Reply if there's something to say**, then **resolve**:

   ```text
   git ents comment reply <id> --body "Done — extracted the helper as suggested."
   git ents comment resolve <id>
   ```

   Resolving is an ordinary signed mutation on the comment's ref, never a deletion — the thread stays auditable.
   Reopen with `git ents comment reopen <id>` if you resolved too early.

Resolve a comment only once the code actually satisfies it.
If you can't address one, leave a reply explaining why and leave it `open`.

## Reading one thread

```text
git ents comment show <id>            # projected onto HEAD
git ents comment show <id> --worktree # projected onto the working tree
```

Shows state, context, parent, the anchored snippet, and body.
To read a whole conversation on an entity (an issue or review), filter by context:

```text
git ents comment list --context issues/42 --porcelain
```

## Leaving a comment

A comment must be *about* something: an anchor, a context, a parent, or any combination.
A comment about nothing is refused.

```text
# Anchor to lines of a file at HEAD:
git ents comment add src/gate.rs --lines 40:52 --body "This branch never runs when epoch is unset."

# Anchor to uncommitted work — the exact bytes on disk right now:
git ents comment add src/gate.rs --lines 40:52 --worktree --body "..."

# Whole-file comment (omit --lines); comment on a specific revision with --rev.
# Reply (inherits the parent's aboutness, no anchor needed):
git ents comment reply <id> --body "..."
```

Comments are signed with `user.signingkey` by default; `--key <path>` overrides.
Anchoring `--worktree` captures the current on-disk bytes, so a remark about uncommitted code stays pinned to exactly what you read even after it's committed or amended.

## Issues and reviews are made of comments

Issues and reviews are their own entities, but their *discussion* is ordinary comments carrying the entity as a `context` — so the same loop above works on them.
A context is the entity's ref path below `refs/meta/`, e.g. `issues/42` or `reviews/<target>/<member>`.

### Issues

```text
git ents issue list
git ents issue show <id>
git ents issue new --title "Gate rejects a valid signature" --body "..."
git ents issue new            # omit --title to compose in $GIT_EDITOR/$EDITOR
git ents issue edit <id> --state closed          # also --label, --assignee
git ents comment add --context issues/42 --body "I can repro this."
git ents comment list --context issues/42 --porcelain    # the issue's thread
```

`issue new` with no `--title` opens an editor on a scratch file: first line is the title, the rest is the body, `#` lines are stripped, an empty title aborts.

### Reviews

A review is a verdict plus a body about a commit, keyed by the composite pair `(target, member)` — no minted id anywhere.
It occupies **two refs**: the entity at `refs/meta/reviews/<target>/<member>` and a retention pin at `refs/meta/pins/reviews/<target>/<member>` that keeps the reviewed commit reachable.
`review new` writes both, keyed to the caller's own member id (the enrolled username matching the signing key, or a fingerprint-derived placeholder if unenrolled).

Reviewing a commit an ancestor of which you already reviewed is a **re-review**: it advances the same two refs fast-forward (the composite key stays anchored at the original target) rather than opening a second, unrelated review — so re-reviewing after a branch moves forward is just `review new` again with the new target.

```text
git ents review new --target HEAD --verdict approve --body "LGTM."
git ents review new --target <rev> --verdict request-changes --body "..."
git ents review list [--target <rev>]
git ents review show <target> <member>     # verdict, body, and its comment thread
# Line-level review notes are just anchored comments in the review's context:
git ents comment add src/gate.rs --lines 40:52 --context reviews/<target>/<member> --body "..."
```

Verdicts are free-form strings; `approve` and `request-changes` are conventions, not a fixed set.

## Notes

- Prefer `--porcelain` for anything you parse; the default output is for
  humans and may change.
- `--worktree` is what makes this an *iteration* loop: it projects and
  anchors against your live edits, not just committed history.
- Author and time come from each ref's commit chain, never from stored
  fields — don't expect an author field in the entity.
- One mechanism everywhere: issues, reviews, and line comments are all
  comments-on-a-context or their own small entities on `refs/meta/*`,
  the same ones the editor lens and the web UI read and write.
