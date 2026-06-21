# Handoff: Git Ents — repository web UI

## Overview

A web presence for a self-hosted Git forge ("git-ents").
It is a single repository page that deliberately centers the things a code editor is *bad* at: a large, editorial rendered README; a wander-able file tree; releases as a changelog; CI defined as Git hooks with run logs; bug reports; and repository configuration.

The visual language is "Warm Ledger" — technical minimalism with warmth: a cream-and-gold paper palette, serif display type (Lora) over a geometric sans (DM Sans), monospace (IBM Plex Mono) for anything code-like, hairline borders, barely-there shadows, and a signature 3rem gold accent-underline under section headers.
Light and dark are equals and switch automatically with the OS setting.

## About the Design Files

The file in this bundle (`Git Ents.dc.html`) is a **design reference created in HTML** — a working prototype showing intended look and behavior, **not production code to copy directly**.
It is authored as a "Design Component" (a streaming single-file format) and will not run standalone in a normal app.

The task is to **recreate this design in your target codebase** using its established framework and patterns (React, Vue, Svelte, server-rendered templates, etc.).
If no front-end environment exists yet, pick the most appropriate one for the project.
Note: the prototype itself is client-rendered, but the product it depicts is explicitly **server-rendered, no client JS** — if you are building the real forge, prefer a server-side HTML approach (the mock's interactivity, e.g. tab switching, would become separate page routes).

## Fidelity

**High-fidelity.**
Final colors, typography, spacing, and interactions.
Recreate the UI faithfully using your codebase's libraries and patterns.
Exact hex values, font sizes, and measurements are documented below and in the source file.

---

## Design Tokens

All styling reads from CSS custom properties.
Light is the base; dark overrides under `@media (prefers-color-scheme: dark)`.
**Never hardcode hex in components — read from these.**

### Color

| Token | Light | Dark | Used for |
|---|---|---|---|
| `--color-bg` | `#faf8f4` | `#171510` | page background |
| `--color-surface` | `#ffffff` | `#211f17` | cards, raised panels |
| `--color-text` | `#2a2518` | `#ede8d8` | body text |
| `--color-text-muted` | `#8a7e6a` | `#a89e88` | secondary text, metadata |
| `--color-link` | `#b07d10` | `#d4a030` | links |
| `--color-link-hover` | `#96690a` | `#e4b850` | link hover |
| `--color-border` | `#ede9de` | `#383324` | hairlines, card borders |
| `--color-code-bg` | `#f5f3eb` | `#211f17` | code, card headers, gutters |
| `--color-accent` | `#b07d10` | `#d4a030` | gold accent: rules, active states |
| `--color-accent-subtle` | `#b07d100f` | `#d4a03012` | accent tint backgrounds |

Shadows: `--shadow-sm` = `0 1px 3px #0000000d` (light) / `0 1px 3px #00000040` (dark); `--shadow-md` = `0 4px 16px #0000000f` / `0 4px 16px #0000004d`.

Page background has a faint top-center radial glow in the accent color: `radial-gradient(58rem 30rem at 50% -10rem, var(--color-accent-subtle), transparent 72%)`, `background-attachment: fixed`.

### Syntax / categorical palette (Gruvbox-warm) — light / dark

| Token | Light | Dark | Role |
|---|---|---|---|
| `--kw` | `#9d0006` | `#fb4934` | keyword / error / danger |
| `--fn` | `#427b58` | `#8ec07c` | function / success / "passed" |
| `--ty` | `#b57614` | `#fabd2f` | type / "running" / Rust lang dot |
| `--str` | `#79740e` | `#b8bb26` | string |
| `--num` | `#8f3f71` | `#d3869b` | number / constant |
| `--cmt` | `#9c8f74` | `#928374` | comment |
| `--prop` | `#076678` | `#83a598` | property / link / TOML section headers |

Diff tints: `--diff-add` = `#4e9a0622` / `#b8bb2620`; `--diff-rem` = `#cc241d22` / `#fb493420`.

### Typography

Loaded from Google Fonts.

- `--font-body`: `"DM Sans", system-ui, sans-serif` — weights 400–700.
  UI + body.
- `--font-display`: `"Lora", Georgia, serif` — weights 500–700, `letter-spacing: -.01em`.
  Headings (the signature move).
- `--font-mono`: `"IBM Plex Mono", ui-monospace, Menlo, monospace` — weights 400–600.
  Code, repo/file names, labels, logos, metadata.

Base: `font-size: 17px`, `line-height: 1.7`, antialiased.
Page section headings ~`1.5rem` Lora 700.
README h1 `2.4rem` Lora 700.
Uppercase mono micro-labels at `.7rem` with `.06em` letter-spacing for card headers and eyebrows.

### Shape & spacing

`--radius-sm: 10px` (cards, inputs, buttons); list rows / small controls use `7–8px`; `--radius-pill: 100px` (badges, branch tags, toggles).
App content max-width `78rem`, side padding `1.5rem`.
README reading column capped ~`44rem`.
Generous vertical rhythm; sections separated by hairlines + the accent underline, not heavy dividers.

---

## Global Chrome

### Sticky nav (height 58px)

- `position: sticky; top: 0; z-index: 50`.
  Background `color-mix(in srgb, var(--color-bg) 82%, transparent)` with `backdrop-filter: blur(10px)`.
  Bottom hairline.
- Left: mono wordmark "git-ents" with a small tree/shield glyph in accent; hover → accent; `white-space: nowrap`.
- Center: search input (`max-width: 24rem`), placeholder "Jump to file or symbol", leading search icon, focus border → accent. (Decorative in mock.)
- Right: "Explore", "Docs" muted links (hover → accent), and a 30px circular avatar chip showing initials ("el") in accent on `--color-accent-subtle`.

### Repo header band

- Path line (mono, `1.18rem`): folder icon · `git-ents` (muted link) · `/` · `git-ents` (accent, weight 600) · **branch pill** (`main`, accent text on `--color-accent-subtle`, thin accent border, pill, with branch icon) · **`Public` pill** (muted on `--color-code-bg`).
- Description paragraph (`.98rem`, max ~40rem).
- Topic chips: `rust`, `git-forge`, `self-hosted`, `html-first` — mono `.72rem`, link color on `--color-accent-subtle`, pill.
- Right side: `Watch` and `Star 128` buttons — surface bg, hairline border, `--shadow-sm`, hover border → accent, leading icons.

### Tab bar

Row of buttons over a `1px` bottom border.
Each tab: `10px 14px`, `.88rem`.
Active tab = `--color-text` + weight 600 + a full-width **2px accent underline** pinned to `bottom: -1px`.
Idle = `--color-text-muted`, hover → `--color-text`.
Tabs: `Overview`, `Files`, `Releases` (count pill "4"), `Hooks` (green status dot), `Issues` (count pill "3"), `Settings`.
Count pills are mono `.68rem`, muted, on `--color-code-bg` with hairline border.

### Footer

Top hairline, centered mono `.72rem` muted: "git-ents · served as paper-grain HTML · no JavaScript required".

---

## Screens / Views

### 1. Overview (default tab)

**Purpose:** read the project at a glance — the README is the star.
**Layout:** CSS grid, `grid-template-columns: 1fr 19rem`, gap `34px`, `align-items: start`.

- **README card** (left): surface, hairline border, `--radius-sm`, `--shadow-sm`, overflow hidden.
  - Header strip: `--color-code-bg`, bottom hairline, mono uppercase `.72rem` muted label "README.md" with a file icon.
  - Body: padding `40px 48px 52px`, `max-width: 44rem`.
    - `h1` "git-ents" — Lora 700, `2.4rem`, `letter-spacing: -.02em`.
    - Italic Lora subtitle in muted (`1.18rem`).
    - Badge row: pills (build passing — with `--fn` dot, `v0.7.0` in `--ty`, `MIT` in `--prop`, `rust 1.78+` muted), mono `.7rem` on `--color-code-bg` with hairline.
    - Body paragraphs; `h2` section headers (Lora 600, `1.4rem`) each followed by a **3rem × 2px accent underline**.
    - Bulleted lists; inline `code` on `--color-code-bg`, `5px` radius.
    - Two code blocks on `--color-code-bg` with hairline + `--radius-sm`: an install block (shell, `$` prompts muted, comments in `--cmt`) and a syntax-highlighted Rust snippet using the syntax palette (`--kw` keywords, `--fn` functions, `--ty` types, `--cmt` comments).
- **Aside** (right, `position: sticky; top: 78px`, column, gap `18px`): four cards, each with a mono uppercase header strip + hairline:
  - **Clone**: readonly mono input (`git.ents.dev/git-ents/git-ents.git`) on `--color-code-bg` + 34px copy button; on copy → green check for 1.4s.
  - **About**: Rust·MIT (with `--ty` dot), stars/forks, "Updated 3 hours ago".
  - **Releases**: count "4" in accent; latest `v0.7.0` row with tag icon + "Latest" pill + "Paper Trail — released 9 days ago".
  - **Languages**: thin stacked bar (Rust 86% `--ty` / HTML 9% `--fn` / CSS 5% `--prop`) + legend with color dots.

### 2. Files

**Purpose:** browse the repo tree and read files.
**Layout:** grid `17rem 1fr` inside one bordered, shadowed, rounded card; min-height `30rem`; overflow hidden.

- **Left tree pane:** `--color-bg`, right hairline.
  Header strip = mono "main" with branch icon.
  - Rows: mono `.79rem`, `padding: 4px 8px`, indent `8 + depth*15` px.
    Folders show a chevron (rotates 90° when open, `.15s`) + accent folder icon; files show a spacer + muted file icon.
    Hover bg → `--color-code-bg`.
    **Selected file**: bg `--color-accent-subtle`, text → accent, weight 600.
  - Tree data: `.gitents/hooks.toml`; `src/{main.rs, forge/{mod,repo,render,blob}.rs, hooks/{mod,runner}.rs}`; `templates/{repo,blob}.html`; `static/paper.css`; `Cargo.toml`; `LICENSE`; `README.md`.
    Default-expanded: `.gitents`, `src`, `src/forge`.
    Default selection: `Cargo.toml`.
- **Right blob pane:** column.
  - Header: `--color-code-bg`, bottom hairline.
    Left = mono current path.
    Right = mono meta ("N lines · SIZE") + a Copy button (→ green "Copied" check for 1.4s).
  - Body: grid `auto 1fr`, scrollable.
    Left = line-number gutter on `--color-code-bg`, right hairline, mono `.78rem`, muted at `opacity .6`, right-aligned.
    Right = code lines, mono `.78rem`, `line-height: 1.55`, `white-space: pre`, horizontal scroll. (Mock renders plain mono per file; real impl should syntax-highlight server-side.)

### 3. Releases

**Purpose:** read the changelog.
**Layout:** Page header "Releases" (Lora `1.5rem`) + 3rem accent underline.
Then a **timeline rail**: container `padding-left: 30px` with a `2px` vertical line at `left: 5px` in `--color-border`.

- Each release: a **dot** absolutely positioned at `left: -30px; top: 16px`, 12px, pill, with a `4px` ring in `--color-bg` (`box-shadow`).
  Latest = filled accent; others = surface fill with border ring.
- Release card (surface, hairline, rounded, shadow):
  - Header row (bottom hairline): tag icon (accent) + mono tag (`v0.7.0`, accent, weight 600) + Lora name ("Paper Trail") + optional "Latest" pill (`--fn`); right-aligned date (muted).
  - Body: note sections grouped **Added / Changed / Fixed**, each with a mono uppercase eyebrow colored `--fn` / `--ty` / `--prop` respectively, then a bullet list (`.88rem`).
    The latest release also shows a 2-line **diff** block (added line on `--diff-add`, removed line on `--diff-rem`, mono `.76rem`).
  - Footer (top hairline): asset download links (mono `.74rem`, link color, download icon, size in muted) + right-aligned mono sha with a commit glyph.
- Four releases: `v0.7.0 Paper Trail` (latest, June 12 2026), `v0.6.1` (May 28), `v0.6.0 Hairline` (May 3), `v0.5.0 First Light` (Apr 10).

### 4. Hooks

**Purpose:** CI defined as Git hooks — view runs and logs.
**Layout:** Page header "Hooks" + underline + intro paragraph (notes CI is plain `pre-receive`/`post-receive`, config in `.gitents/hooks.toml`).
Then grid `20rem 1fr`, gap `20px`, `align-items: start`.

- **Run list** (left card): header "Recent runs".
  Each run is a button row (bottom hairline): a status icon (passed = `--fn` filled check-circle; failed = `--kw` x-circle; running = `--ty` spinner, `@keyframes spin` 1s linear) + subject (truncated) + mono submeta "#id · branch · trigger" + right-aligned age.
  **Selected run**: bg `--color-accent-subtle` + `2px` left accent border.
  Hover → `--color-code-bg`.
- **Run detail** (right column, gap 18px):
  - Detail card: header (status icon + Lora subject + right-aligned mono "sha · duration").
    **Stages** row (bottom hairline): pills on `--color-code-bg` each = a status dot (`--fn`/`--kw`/`--ty`/muted) + mono stage name + mono duration (`lint`, `build`, `test`, `deploy`).
    **Log viewer**: `--color-code-bg`, mono `.76rem`, `line-height: 1.7`, `white-space: pre`, horizontal scroll.
    Each line = muted line number + text colored by level: cmd→`--prop`, ok→`--fn`, err→`--kw`, warn→`--ty`, info→muted.
  - Config card: mono header "`.gitents/hooks.toml`" on `--color-code-bg`; body renders the TOML with section headers (`[pre-receive.lint]` etc.) in `--prop`, comments in `--cmt`, rest in text color.
- Four runs (#142 passed, #141 failed, #140 passed, #139 running).
  Default selection: #142.

### 5. Issues ("Bug reports")

**Purpose:** file and track bugs.
**Layout:** Header row: "Bug reports" (Lora `1.5rem`, `white-space: nowrap`) + underline on the left; "New issue" button on the right (solid accent bg, `--color-bg` text, weight 600, plus icon, hover → `--color-link-hover`).

- **Filter row:** search input (flex-grow, leading icon, `onInput` filters by title, focus border → accent) + **label chips** (`All`, `bug`, `enhancement`, `question`).
  Active chip = solid accent bg + `--color-bg` text; idle = surface + muted + hairline.
  Pill shape, `.76rem`.
- **List card:** header strip on `--color-code-bg` with **Open / Closed** sub-tabs (each = status icon + label + count; active = text + weight 600).
  Each issue row (bottom hairline, hover → `--color-code-bg`): status icon (open = `--fn` open-issue dot-circle; closed = muted check-circle) + title link (hover → accent) + label pills (mono `.66rem`, colored per label: bug→`--kw`, enhancement→`--fn`, question→`--prop`, docs→`--ty`, on `--color-code-bg` + hairline) + mono submeta "#num opened/closed … by author" + right-aligned comment count with speech icon (hidden if 0).
  Empty state: centered muted "No bug reports match these filters."
- Six issues (3 open, 3 closed) covering bug/enhancement/question/docs labels.

### 6. Settings ("Repository settings")

**Purpose:** configure the repo.
**Layout:** `max-width: 46rem`.
Page header + underline.
Four stacked cards, each with a mono uppercase header strip + hairline.

- **General:** labeled fields — repo name (mono input), description (textarea, body font, vertical resize), default branch (`select`: main/develop/trunk).
  Inputs: `--color-bg` bg, hairline, `8px` radius, focus border → accent.
- **Features:** four rows (Bug reports, Releases, Hooks (CI), Wiki), each = label + description + a **toggle switch** (42×24 track, pill; on = accent bg + knob at `left: 21px`; off = `--color-border` + knob at `left: 3px`; 18px white knob, `.18s` transition).
  Defaults: issues/releases/hooks on, wiki off.
- **Visibility:** two radio rows (Public / Private) — each a button with a 16px radio (filled accent dot when selected), title + description; selected row = `--color-accent-subtle` bg + accent border.
  Default: Public.
- **Danger zone:** card with a `--kw` border + `--kw` header.
  Two rows: "Archive this repository" (outline `--kw` button, hover bg `--diff-rem`) and "Delete this repository" (solid `--kw` button, white text).

---

## Interactions & Behavior

- **Tabs:** clicking a tab swaps the content region; active underline + weight change.
  In a server-rendered build these become separate routes/pages.
- **File tree:** folder rows toggle expand/collapse (chevron rotates `.15s`); file rows select and load the blob into the right pane.
- **Run list:** selecting a run swaps the detail card (stages + log + the run's own logs).
  Running runs show an infinite spinner.
- **Issue filters:** Open/Closed sub-tabs, label chips, and the search box compose (AND) to filter the list live; empty result → empty-state message.
- **Copy buttons** (clone URL, blob): write to clipboard, then show a green check / "Copied" for ~1.4s before reverting.
- **Settings toggles & radios:** flip local state with animated knob / radio fill.
- **Hover motion** is small (≤3px) and quick (`.15s–.22s`); hover states lean on the accent and the subtle tint, rarely on shadow.
  List-row hover tints to `--color-code-bg`.
- **Dark mode:** automatic via `prefers-color-scheme`; every token has a dark value — no manual toggle.

## State Management

Local UI state only (no data fetching in the mock):

- `tab` — active top-level tab.
- `exp` — map of expanded folder paths (file tree).
- `file` — selected file path (blob view).
- `run` — selected CI run id.
- `itab` (open|closed), `ilabel` (all|bug|enhancement|question), `iq` (search string) — issue filters.
- `copied` — transient key marking which copy button is showing its confirmation.
- `features` — map of feature toggles; `visibility` — public|private.

In a real forge most of this maps to **server routes + query params** (tab/path/run/ filters in the URL) since the product is server-rendered with no client JS; only the copy-to-clipboard affordance strictly needs a sprinkle of JS.

## Assets

- **Fonts:** DM Sans, Lora, IBM Plex Mono (Google Fonts).
- **Icons:** inline 16px line icons in the Octicon idiom, `fill: currentColor` so they inherit accent/muted from context.
  No emoji in chrome.
  Recreate with your icon set (e.g. Octicons / Lucide) at equivalent weights.
- No raster images; the avatar is an initials chip.

## Files

- `Git Ents.dc.html` — the full hi-fi prototype (all six tabs, all interactions).
  Open it in a browser to inspect exact markup, inline styles, and the logic that computes the lists/states.
  All design tokens are defined in its `:root` block (and the dark `@media` override) near the top of the file.
