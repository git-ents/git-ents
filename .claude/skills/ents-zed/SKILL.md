---
name: ents-zed
description: Use git-ents comments inside the Zed editor via the ents-zed extension and the ents-lens language server. Use when working in Zed and asked to see, leave, or resolve inline comments, or to set up the editor for the ents comment loop.
---

# ents in Zed

The `ents-zed` extension (at `editors/zed/`) registers a language server, `ents-lsp`, that runs `git ents lsp`.
The server projects the repo's comments (`refs/meta/comments/*`) into whatever buffer you have open and lets you leave new ones — no MCP, no network, just an LSP over stdio reusing the local composition root.

The language server is the same mechanism the CLI and web UI use (`lens.parity`), so a comment left in Zed is readable and resolvable from `git ents comment` and the web, and vice versa.
If you are acting on comments programmatically rather than through the editor, use the `ents-comments` skill (the CLI) instead — it is the same entities.

## Setup

1. `git ents lsp` must be on `PATH` (build/install the `git-ents` binary).
   The extension launches `git` with `ents lsp` and speaks LSP over stdio.
2. Install the extension as a dev extension: Zed → command palette →
   `zed: install dev extension` → pick `editors/zed`.
3. **Turn on code lenses.**
   Zed renders LSP code lenses but they are **off by default**.
   Enable them, or the inline comment lenses won't show:
   - setting: `"code_lens": "on"`, or
   - command palette: `editor: toggle code lens`.

## What renders

- **Code lenses** at each open comment's projected line (once enabled):
  the id, a summary, and View / Reply / Resolve actions.
- **Hint diagnostics** for the same comments — these show inline **even without code lenses enabled**, which is why the server publishes them (`lens.diagnostics`).
  They are hints, never warnings or errors.
- **Hover** over a commented range shows the full thread.
- **Code action** on a selection ("leave an ents comment") to compose a
  new comment; see the crate's compose flow for how saving creates it.

## Composing a comment

Trigger the code action on the lines you want to anchor to.
It opens a template file (git-commit style: first content is the body, `#` lines are ignored, an empty body aborts).
Saving a non-empty body creates the comment, anchored to the working-tree bytes you selected.

Because everything is the one mechanism, the iteration loop is: leave comments in Zed, then tell an agent to "address all comments in the working tree" (the `ents-comments` skill) — it reads the same open comments, fixes the code, replies, and resolves them, and your next Zed publish reflects the resolutions.

See `editors/zed/README.adoc` for the authoritative, version-tracked notes on exactly which surfaces the current Zed renders.
