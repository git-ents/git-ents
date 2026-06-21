//! Browser-facing HTML: a small, hand-styled web UI rendered server-side with
//! Maud. The look mirrors <https://jdc.pub>: DM Sans / Lora / IBM Plex Mono on a
//! warm-gold palette that follows the system light/dark preference. The git
//! smart-HTTP gateway in [`crate::http`] delegates plain browser GETs here.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use arborium::{Config, Highlighter, HtmlFormat};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use tokio::process::Command;

use crate::AppState;
use crate::http::{is_bare_repo, valid_segment};

/// Greatest repository nesting depth served: `repo`, `org/repo`, `org/team/repo`.
const MAX_DEPTH: usize = 3;

/// Web fonts, matching the typography of <https://jdc.pub>.
const FONTS: &str = "https://fonts.googleapis.com/css2?family=DM+Sans:wght@400;500;600;700&family=IBM+Plex+Mono:wght@400;500;600&family=Lora:wght@500;600;700&display=swap";

/// Hand-written stylesheet (no external CSS framework) so the look stays stable
/// and self-contained. Colors, type, and radii track <https://jdc.pub>, with a
/// `prefers-color-scheme` block for automatic dark mode.
const STYLE: &str = r#"
:root {
  --font-sans: "DM Sans", system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  --font-serif: "Lora", Georgia, "Times New Roman", serif;
  --font-mono: "IBM Plex Mono", ui-monospace, "Cascadia Code", "Source Code Pro", Menlo, monospace;
  --max-width: 78rem;
  --color-bg: #faf8f4;
  --color-surface: #fff;
  --color-text: #2a2518;
  --color-text-muted: #8a7e6a;
  --color-link: #b07d10;
  --color-link-hover: #96690a;
  --color-border: #ede9de;
  --color-code-bg: #f5f3eb;
  --color-accent: #b07d10;
  --color-accent-subtle: #b07d100f;
  --shadow-sm: 0 1px 3px #0000000d;
  --shadow-md: 0 4px 16px #0000000f;
  --radius-sm: 10px;
  --radius-pill: 100px;
  --s-comment: #9c8f74;
  --s-keyword: #9d0006;
  --s-func: #427b58;
  --s-type: #b57614;
  --s-string: #79740e;
  --s-const: #8f3f71;
  --s-op: #7c6f57;
  --s-prop: #076678;
  --diff-add: #4e9a0622;
  --diff-del: #cc241d22;
}
@media (prefers-color-scheme: dark) {
  :root {
    --color-bg: #171510;
    --color-surface: #211f17;
    --color-text: #ede8d8;
    --color-text-muted: #a89e88;
    --color-link: #d4a030;
    --color-link-hover: #e4b850;
    --color-border: #383324;
    --color-code-bg: #211f17;
    --color-accent: #d4a030;
    --color-accent-subtle: #d4a03012;
    --shadow-sm: 0 1px 3px #00000040;
    --shadow-md: 0 4px 16px #0000004d;
    --s-comment: #928374;
    --s-keyword: #fb4934;
    --s-func: #8ec07c;
    --s-type: #fabd2f;
    --s-string: #b8bb26;
    --s-const: #d3869b;
    --s-op: #a89984;
    --s-prop: #83a598;
    --diff-add: #b8bb2620;
    --diff-del: #fb493420;
  }
}
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html { font-size: 17px; -webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale; }
body {
  font-family: var(--font-sans);
  background: var(--color-bg);
  color: var(--color-text);
  line-height: 1.7;
  min-height: 100vh;
  display: flex;
  flex-direction: column;
  background-image: radial-gradient(58rem 30rem at 50% -10rem, var(--color-accent-subtle), transparent 72%);
  background-attachment: fixed;
}
a { color: var(--color-link); text-decoration: underline; text-decoration-color: color-mix(in srgb, var(--color-link) 25%, transparent); text-underline-offset: 2px; transition: color .15s, text-decoration-color .15s; }
a:hover { color: var(--color-link-hover); text-decoration-color: currentColor; }
.icon { flex-shrink: 0; fill: currentColor; vertical-align: -0.125em; }

.site-nav { position: sticky; top: 0; z-index: 100; background: color-mix(in srgb, var(--color-bg) 82%, transparent); backdrop-filter: blur(10px); border-bottom: 1px solid var(--color-border); }
.nav-inner { max-width: var(--max-width); margin: 0 auto; height: 58px; padding: 0 1.5rem; display: flex; align-items: center; gap: 1.25rem; }
.nav-logo { display: inline-flex; align-items: center; gap: .5rem; font-family: var(--font-mono); font-weight: 700; font-size: 1.02rem; color: var(--color-text); letter-spacing: -.01em; text-decoration: none; white-space: nowrap; transition: color .15s; }
.nav-logo .icon { color: var(--color-accent); width: 18px; height: 18px; }
.nav-logo:hover { color: var(--color-accent); }
.nav-search { flex: 1; max-width: 24rem; margin: 0 auto; position: relative; display: flex; align-items: center; }
.nav-search .icon { position: absolute; left: .65rem; color: var(--color-text-muted); pointer-events: none; }
.nav-search input { width: 100%; font-family: var(--font-sans); font-size: .82rem; color: var(--color-text); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: .42rem .7rem .42rem 2rem; transition: border-color .15s; }
.nav-search input:focus { outline: none; border-color: var(--color-accent); }
.nav-links { display: flex; align-items: center; gap: 1.1rem; white-space: nowrap; }
.nav-link { font-size: .85rem; color: var(--color-text-muted); text-decoration: none; transition: color .15s; }
.nav-link:hover { color: var(--color-accent); }
.nav-avatar { width: 30px; height: 30px; border-radius: 50%; display: inline-flex; align-items: center; justify-content: center; font-family: var(--font-mono); font-size: .72rem; font-weight: 600; color: var(--color-accent); background: var(--color-accent-subtle); text-decoration: none; }

.content { max-width: var(--max-width); width: 100%; margin: 0 auto; padding: 2.25rem 1.5rem 3rem; flex: 1; }

.page-header { margin-bottom: 1.75rem; padding-bottom: 1.25rem; border-bottom: 1px solid var(--color-border); position: relative; display: flex; align-items: center; gap: .75rem; flex-wrap: wrap; }
.page-header::after { content: ""; position: absolute; bottom: -1px; left: 0; width: 3rem; height: 2px; background: var(--color-accent); border-radius: 1px; }
.page-title { font-family: var(--font-serif); font-size: 1.5rem; font-weight: 700; letter-spacing: -.01em; line-height: 1.3; display: inline-flex; align-items: center; gap: .55rem; }
.page-title .icon { color: var(--color-accent); width: 20px; height: 20px; }
.page-title a { color: inherit; text-decoration: none; }
.page-title a:hover { color: var(--color-accent); }
.count { margin-left: auto; font-family: var(--font-mono); font-size: .8rem; color: var(--color-text-muted); }

.branch { font-family: var(--font-mono); font-size: .72rem; font-weight: 600; color: var(--color-accent); background: var(--color-accent-subtle); border: 1px solid color-mix(in srgb, var(--color-accent) 30%, transparent); border-radius: var(--radius-pill); padding: .1rem .6rem; display: inline-flex; align-items: center; gap: .3rem; }
.branch .icon { width: 13px; height: 13px; }

.repo-header { display: flex; align-items: flex-start; gap: 1.5rem; flex-wrap: wrap; margin-bottom: 1.25rem; }
.repo-headline { flex: 1; min-width: 0; }
.repo-path { font-family: var(--font-mono); font-size: 1.18rem; display: flex; flex-wrap: wrap; align-items: center; gap: .4rem; word-break: break-all; }
.repo-path .icon { color: var(--color-accent); }
.repo-path a { color: var(--color-text-muted); text-decoration: none; }
.repo-path a:hover { color: var(--color-accent); }
.repo-path .here { color: var(--color-accent); font-weight: 600; }
.repo-path .sep { color: var(--color-text-muted); opacity: .55; }
.pill-public { font-family: var(--font-mono); font-size: .7rem; font-weight: 500; color: var(--color-text-muted); background: var(--color-code-bg); border-radius: var(--radius-pill); padding: .1rem .55rem; }
.repo-desc { font-size: .98rem; color: var(--color-text); max-width: 40rem; margin-top: .65rem; }
.topics { display: flex; flex-wrap: wrap; gap: .4rem; margin-top: .75rem; }
.topic { font-family: var(--font-mono); font-size: .72rem; color: var(--color-link); background: var(--color-accent-subtle); border-radius: var(--radius-pill); padding: .12rem .6rem; text-decoration: none; }
.topic:hover { color: var(--color-link-hover); }
.repo-actions { display: flex; gap: .6rem; flex-shrink: 0; }
.action-btn { display: inline-flex; align-items: center; gap: .4rem; font-family: var(--font-sans); font-size: .82rem; font-weight: 500; color: var(--color-text); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); padding: .4rem .8rem; cursor: pointer; transition: border-color .15s; }
.action-btn:hover { border-color: var(--color-accent); }
.action-btn .icon { color: var(--color-text-muted); }

.tabs { display: flex; gap: .15rem; border-bottom: 1px solid var(--color-border); margin-bottom: 1.75rem; overflow-x: auto; }
.tab { display: inline-flex; align-items: center; gap: .4rem; padding: 10px 14px; font-size: .88rem; color: var(--color-text-muted); text-decoration: none; white-space: nowrap; position: relative; transition: color .15s; }
.tab:hover { color: var(--color-text); }
.tab.active { color: var(--color-text); font-weight: 600; }
.tab.active::after { content: ""; position: absolute; left: 0; right: 0; bottom: -1px; height: 2px; background: var(--color-accent); }
.tab-count { font-family: var(--font-mono); font-size: .68rem; color: var(--color-text-muted); background: var(--color-code-bg); border: 1px solid var(--color-border); border-radius: var(--radius-pill); padding: 0 .4rem; }
.tab-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--s-func); }

.overview { display: grid; grid-template-columns: 1fr 19rem; gap: 34px; align-items: start; }
.aside { position: sticky; top: 78px; display: flex; flex-direction: column; gap: 18px; min-width: 0; }
.aside .card { margin-bottom: 0; }
.aside .clone code { font-size: .76rem; padding: .6rem .8rem; }
.aside-row { display: flex; align-items: center; gap: .5rem; padding: .55rem 1.1rem; font-size: .82rem; }
.aside-row + .aside-row { border-top: 1px solid var(--color-border); }
.aside-row .icon { color: var(--color-text-muted); flex-shrink: 0; }
.aside-row .muted { color: var(--color-text-muted); }
.aside-row .count { margin-left: 0; font-family: var(--font-mono); font-weight: 600; color: var(--color-accent); }
.aside-row a { text-decoration: none; }
.lang-dot, .swatch { width: 9px; height: 9px; border-radius: 2px; flex-shrink: 0; }
.dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
.lang { padding: .8rem 1.1rem; }
.lang-bar { display: flex; height: 8px; border-radius: var(--radius-pill); overflow: hidden; background: var(--color-code-bg); }
.lang-bar span { display: block; height: 100%; }
.lang-legend { list-style: none; display: flex; flex-direction: column; gap: .35rem; margin-top: .7rem; font-size: .78rem; }
.lang-legend li { display: flex; align-items: center; gap: .45rem; }
.lang-legend .pct { margin-left: auto; font-family: var(--font-mono); color: var(--color-text-muted); }
.latest { font-family: var(--font-mono); font-size: .8rem; }
.tag-pill { color: var(--color-accent); font-weight: 600; }

@media (max-width: 860px) {
  .overview { grid-template-columns: 1fr; }
  .aside { position: static; }
}

.repo-list { list-style: none; }
.repo-list li + li { border-top: 1px solid var(--color-border); }
.repo-row { display: flex; align-items: center; gap: .85rem; padding: .9rem .75rem; border-radius: var(--radius-sm); text-decoration: none; color: inherit; transition: background .18s, transform .18s; }
.repo-row:hover { text-decoration: none; transform: translateX(3px); }
.repo-row:hover .repo-name { color: var(--color-accent); }
.repo-row:hover .repo-arrow { opacity: 1; transform: translateX(0); }
.repo-row .repo-icon { color: var(--color-accent); display: inline-flex; }
.repo-name { font-family: var(--font-mono); font-size: 1rem; font-weight: 600; flex: 1; min-width: 0; transition: color .18s; word-break: break-all; }
.repo-badge { font-family: var(--font-mono); font-size: .68rem; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: var(--color-text-muted); border: 1px solid var(--color-border); border-radius: var(--radius-pill); padding: .08rem .55rem; }
.repo-arrow { color: var(--color-accent); display: inline-flex; opacity: 0; transform: translateX(-6px); transition: opacity .22s, transform .22s; }

.card { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); margin-bottom: 1.5rem; overflow: hidden; }
.card-header { font-family: var(--font-mono); font-size: .72rem; font-weight: 600; text-transform: uppercase; letter-spacing: .06em; color: var(--color-text-muted); background: var(--color-code-bg); padding: .55rem 1.1rem; border-bottom: 1px solid var(--color-border); }
.card-row { display: flex; align-items: center; gap: .65rem; padding: .7rem 1.1rem; font-family: var(--font-mono); font-size: .9rem; }
.card-row + .card-row { border-top: 1px solid var(--color-border); }
.card-row .icon { color: var(--color-text-muted); }
.card-row.is-dir .icon { color: var(--color-accent); }

.commit { padding: .85rem 1.1rem; }
.commit + .commit { border-top: 1px solid var(--color-border); }
.commit-subject { font-weight: 600; line-height: 1.45; }
.commit-meta { font-size: .8rem; color: var(--color-text-muted); margin-top: .15rem; }
.commit-meta .sha { font-family: var(--font-mono); background: var(--color-code-bg); padding: .08rem .4rem; border-radius: 5px; font-size: .76rem; margin-right: .5rem; }

.clone { display: flex; align-items: stretch; }
.clone code { flex: 1; font-family: var(--font-mono); font-size: .82rem; background: var(--color-code-bg); padding: .7rem 1rem; overflow-x: auto; white-space: pre; color: var(--color-text); }
.copy-btn { font-family: var(--font-mono); font-size: .74rem; font-weight: 600; border: none; border-left: 1px solid var(--color-border); background: var(--color-surface); color: var(--color-text-muted); padding: 0 1rem; cursor: pointer; transition: color .15s, background .15s; }
.copy-btn:hover { color: var(--color-accent); background: var(--color-accent-subtle); }

.blankslate { text-align: center; padding: 3rem 1.5rem; background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); }
.blankslate h2 { font-family: var(--font-serif); font-size: 1.3rem; font-weight: 700; margin-bottom: .5rem; }
.blankslate p { color: var(--color-text-muted); }
.blankslate code { font-family: var(--font-mono); background: var(--color-code-bg); padding: .15rem .45rem; border-radius: 5px; font-size: .85rem; }
.btn { display: inline-flex; align-items: center; gap: .4rem; margin-top: 1.25rem; font-size: .88rem; font-weight: 600; color: var(--color-accent); text-decoration: none; padding: .45rem 1rem; border-radius: var(--radius-sm); border: 1px solid var(--color-border); background: var(--color-surface); box-shadow: var(--shadow-sm); transition: border-color .15s, box-shadow .15s; }
.btn:hover { text-decoration: none; border-color: var(--color-accent); box-shadow: var(--shadow-md); }

.card-row a { color: inherit; text-decoration: none; flex: 1; min-width: 0; word-break: break-all; }
.card-row a:hover { color: var(--color-accent); }
.commit-subject a { color: inherit; text-decoration: none; }
.commit-subject a:hover { color: var(--color-accent); }

.crumbs { font-family: var(--font-mono); font-size: .92rem; margin-bottom: 1.25rem; display: flex; flex-wrap: wrap; align-items: center; gap: .3rem; word-break: break-all; }
.crumbs a { text-decoration: none; }
.crumbs .sep { color: var(--color-text-muted); opacity: .55; }
.crumbs .here { color: var(--color-text-muted); }

.files { display: grid; grid-template-columns: 17rem minmax(0, 1fr); min-height: 30rem; background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); overflow: hidden; margin-bottom: 1.5rem; }
.tree-pane { background: var(--color-bg); border-right: 1px solid var(--color-border); overflow: auto; padding-bottom: .5rem; }
.tree-head { font-family: var(--font-mono); font-size: .72rem; font-weight: 600; color: var(--color-text-muted); background: var(--color-code-bg); padding: .55rem .9rem; border-bottom: 1px solid var(--color-border); display: flex; align-items: center; gap: .4rem; }
.tree-head .icon { width: 13px; height: 13px; }
.tree-row { display: flex; align-items: center; gap: .3rem; font-family: var(--font-mono); font-size: .79rem; padding: 4px 8px; text-decoration: none; color: var(--color-text); white-space: nowrap; }
.tree-row:hover { text-decoration: none; background: var(--color-code-bg); }
.tree-row.sel { background: var(--color-accent-subtle); color: var(--color-accent); font-weight: 600; }
.tree-row .chev { width: 12px; height: 12px; flex-shrink: 0; color: var(--color-text-muted); transition: transform .15s; }
.tree-row .chev.open { transform: rotate(90deg); }
.tree-row .ic-folder { display: inline-flex; color: var(--color-accent); }
.tree-row.sel .ic-folder { color: var(--color-accent); }
.tree-row .ic-file { display: inline-flex; color: var(--color-text-muted); }
.tree-row span:last-child { overflow: hidden; text-overflow: ellipsis; }
.blob-pane { display: flex; flex-direction: column; min-width: 0; }
.blob-head { display: flex; align-items: center; gap: .5rem; background: var(--color-code-bg); border-bottom: 1px solid var(--color-border); padding: .5rem 1rem; font-family: var(--font-mono); font-size: .78rem; }
.blob-head .meta { margin-left: auto; display: flex; align-items: center; gap: .8rem; color: var(--color-text-muted); }
.blob-head .copy-btn { border: 1px solid var(--color-border); border-radius: 6px; padding: .1rem .55rem; background: var(--color-surface); }
.blob-pane .blob { border: 0; border-radius: 0; box-shadow: none; margin: 0; overflow: auto; flex: 1; }
.files-empty { margin: auto; padding: 3rem; text-align: center; color: var(--color-text-muted); }
.files-empty .icon { width: 28px; height: 28px; opacity: .5; margin-bottom: .5rem; }

@media (max-width: 700px) {
  .files { grid-template-columns: 1fr; }
  .tree-pane { border-right: 0; border-bottom: 1px solid var(--color-border); max-height: 16rem; }
}

.blob { display: grid; grid-template-columns: auto minmax(0, 1fr); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); overflow: hidden; margin-bottom: 1.5rem; }
.blob pre { font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; margin: 0; padding: 1rem 0; }
.blob-nums { text-align: right; color: var(--color-text-muted); background: var(--color-code-bg); border-right: 1px solid var(--color-border); padding-left: 1rem; padding-right: 1rem; user-select: none; -webkit-user-select: none; }
.blob-code { overflow-x: auto; min-width: 0; }
.blob-code code { display: block; font-family: inherit; padding: 0 1rem; white-space: pre; color: var(--color-text); }
.binary { padding: 2.5rem; text-align: center; font-family: var(--font-mono); font-size: .85rem; color: var(--color-text-muted); }

.code .keyword, .code .macro, .code .tag { color: var(--s-keyword); }
.code .function, .code .constructor { color: var(--s-func); }
.code .type { color: var(--s-type); }
.code .string { color: var(--s-string); }
.code .number, .code .constant, .code .label { color: var(--s-const); }
.code .comment { color: var(--s-comment); font-style: italic; }
.code .operator, .code .punctuation { color: var(--s-op); }
.code .property, .code .attribute { color: var(--s-prop); }
.code .title { color: var(--s-keyword); font-weight: 700; }
.code .strong { font-weight: 700; }
.code .emphasis { font-style: italic; }
.code .link, .code .url, .code .reference { color: var(--s-prop); text-decoration: underline; }
.code .markup { color: var(--s-func); }

.diff { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm); box-shadow: var(--shadow-sm); overflow-x: auto; margin-bottom: 1.5rem; font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; padding: .6rem 0; }
.diff .ln { display: block; padding: 0 1rem; white-space: pre; }
.diff .add { background: var(--diff-add); }
.diff .del { background: var(--diff-del); }
.diff .hunk { color: var(--s-prop); background: var(--color-code-bg); }
.diff .meta { color: var(--color-text-muted); }
.diff .file { color: var(--color-text); font-weight: 600; background: var(--color-code-bg); padding-top: .3rem; padding-bottom: .3rem; }

.commit-msg { font-family: var(--font-mono); font-size: .9rem; white-space: pre-wrap; word-break: break-word; }

.adoc-body { padding: 40px 48px 52px; max-width: 44rem; overflow-wrap: break-word; }
.adoc-body > :first-child { margin-top: 0; }
.adoc-body h1, .adoc-body h2, .adoc-body h3, .adoc-body h4 { font-family: var(--font-serif); font-weight: 700; letter-spacing: -.01em; line-height: 1.25; margin: 1.8rem 0 .9rem; }
.adoc-body h1 { font-size: 2.4rem; letter-spacing: -.02em; }
.adoc-body .doc-subtitle { font-family: var(--font-serif); font-style: italic; font-size: 1.18rem; color: var(--color-text-muted); margin: -.4rem 0 1.2rem; }
.adoc-body h2 { font-size: 1.4rem; font-weight: 600; position: relative; padding-bottom: .55rem; }
.adoc-body h2::after { content: ""; position: absolute; left: 0; bottom: 0; width: 3rem; height: 2px; background: var(--color-accent); border-radius: 1px; }
.adoc-body h3 { font-size: 1.15rem; font-weight: 600; }
.adoc-body p, .adoc-body ul, .adoc-body ol { margin: 0 0 1rem; }
.adoc-body ul, .adoc-body ol { padding-left: 1.4rem; }
.adoc-body li { margin: .25rem 0; }
.adoc-body a { font-weight: 500; }
.adoc-body code, .adoc-body .literal { font-family: var(--font-mono); font-size: .86em; background: var(--color-code-bg); padding: .1rem .35rem; border-radius: 5px; }
.adoc-body pre { font-family: var(--font-mono); font-size: .82rem; line-height: 1.55; background: var(--color-code-bg); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: 1rem 1.2rem; overflow-x: auto; margin: 0 0 1rem; }
.adoc-body pre code { background: none; padding: 0; font-size: inherit; }
.adoc-body blockquote { border-left: 3px solid var(--color-accent); padding: .2rem 0 .2rem 1.1rem; margin: 0 0 1rem; color: var(--color-text-muted); }
.adoc-body table { border-collapse: collapse; margin: 0 0 1rem; font-size: .92rem; }
.adoc-body th, .adoc-body td { border: 1px solid var(--color-border); padding: .4rem .7rem; text-align: left; }
.adoc-body th { background: var(--color-code-bg); font-weight: 600; }
.adoc-body .title { font-weight: 600; color: var(--color-text-muted); font-size: .9rem; margin-bottom: .3rem; }
.adoc-body img { max-width: 100%; height: auto; }
.adoc-body hr { border: none; border-top: 1px solid var(--color-border); margin: 1.8rem 0; }

.timeline { position: relative; padding-left: 30px; }
.timeline::before { content: ""; position: absolute; left: 5px; top: 0; bottom: 0; width: 2px; background: var(--color-border); }
.release { position: relative; margin-bottom: 1.5rem; }
.release::before { content: ""; position: absolute; left: -30px; top: 16px; width: 12px; height: 12px; border-radius: var(--radius-pill); background: var(--color-surface); border: 2px solid var(--color-border); box-shadow: 0 0 0 4px var(--color-bg); }
.release.latest::before { background: var(--color-accent); border-color: var(--color-accent); }
.release-head { display: flex; align-items: center; gap: .55rem; padding: .85rem 1.1rem; border-bottom: 1px solid var(--color-border); flex-wrap: wrap; }
.release-head .icon { color: var(--color-accent); }
.release-tag { font-family: var(--font-mono); font-size: .9rem; font-weight: 600; color: var(--color-accent); }
.release-name { font-family: var(--font-serif); font-size: 1.05rem; font-weight: 600; }
.release-date { margin-left: auto; font-size: .8rem; color: var(--color-text-muted); }
.badge-latest { font-family: var(--font-mono); font-size: .66rem; font-weight: 600; text-transform: uppercase; letter-spacing: .05em; color: var(--s-func); background: var(--color-code-bg); border-radius: var(--radius-pill); padding: .05rem .5rem; }
.release-body { padding: 1rem 1.1rem; }
.release-body p { white-space: pre-wrap; font-size: .9rem; }
.release-body .muted { color: var(--color-text-muted); }
.release-foot { display: flex; align-items: center; padding: .7rem 1.1rem; border-top: 1px solid var(--color-border); font-family: var(--font-mono); font-size: .74rem; }
.release-foot .sha { margin-left: auto; color: var(--color-text-muted); display: inline-flex; align-items: center; gap: .35rem; }

.shell-note { color: var(--color-text-muted); font-size: .95rem; max-width: 44rem; margin-bottom: 1.5rem; }

.site-footer { border-top: 1px solid var(--color-border); color: var(--color-text-muted); font-size: .8rem; margin-top: auto; }
.footer-inner { max-width: var(--max-width); margin: 0 auto; padding: 2rem 1.5rem; text-align: center; }
.footer-inner a { color: var(--color-text-muted); text-decoration: none; }
.footer-inner a:hover { color: var(--color-accent); }

@media (max-width: 640px) {
  html { font-size: 16px; }
  .content { padding: 1.5rem 1.25rem 2.5rem; }
}
"#;

/// Clipboard handler for the clone-URL copy button.
const COPY_SCRIPT: &str = r#"
document.querySelectorAll('[data-copy]').forEach((btn) => {
  btn.addEventListener('click', () => {
    navigator.clipboard.writeText(btn.dataset.copy).then(() => {
      const label = btn.textContent;
      btn.textContent = 'Copied';
      setTimeout(() => { btn.textContent = label; }, 1200);
    });
  });
});
"#;

/// Render the page for `path`: the repository index at the root, a repository
/// overview, or one of its browse views (`tree`, `blob`, `commit`). `host` is
/// the request's `Host` header, used to build a copy-pasteable clone URL.
pub(crate) async fn render(state: &AppState, path: &str, host: Option<&str>) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return index(state).into_response();
    }

    // The repository is the shortest valid prefix (up to `MAX_DEPTH` segments)
    // that names a bare repo on disk; anything after it selects a browse view.
    // Resolving the boundary this way keeps a repo named `tree`/`blob`/`commit`
    // distinct from the route markers of the same name.
    let depth_limit = segments.len().min(MAX_DEPTH);
    for depth in 1..=depth_limit {
        let Some(repo_segs) = segments.get(..depth) else {
            break;
        };
        if !repo_segs.iter().all(|s| valid_segment(s)) {
            break;
        }
        let relative: PathBuf = repo_segs.iter().collect();
        let repo = state.data_dir.join(&relative);
        if !is_bare_repo(&repo) {
            continue;
        }
        let rel = repo_segs.join("/");
        let rest = segments.get(depth..).unwrap_or_default();
        return route(&repo, &rel, rest, host).await;
    }

    not_found().into_response()
}

/// Dispatch the part of the path that follows the repository to a browse view.
/// Each top-level tab is its own route, since the product is server-rendered
/// with no client JavaScript.
async fn route(repo: &Path, rel: &str, rest: &[&str], host: Option<&str>) -> Response {
    let meta = gather_meta(repo, rel).await;
    match rest.split_first() {
        None => repo_page(repo, &meta, host).await.into_response(),
        Some((&"files", sub)) => files_page(repo, &meta, sub).await,
        Some((&"tree", sub)) => tree_page(repo, &meta, sub).await,
        Some((&"blob", sub)) => blob_page(repo, &meta, sub).await,
        Some((&"commit", &[sha])) => commit_page(repo, &meta, sha).await,
        Some((&"releases", &[])) => releases_page(repo, &meta).await.into_response(),
        _ => not_found().into_response(),
    }
}

/// The top-level tabs of a repository page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Files,
    Releases,
    Hooks,
    Issues,
    Settings,
}

/// Metadata shown in the repository header band and tab bar, gathered once per
/// request and shared by every view.
struct RepoMeta {
    rel: String,
    branch: Option<String>,
    description: Option<String>,
    topics: Vec<String>,
    releases: usize,
    issues: usize,
    has_hooks: bool,
}

/// Collect the header/tab metadata for the repository at `rel`.
async fn gather_meta(repo: &Path, rel: &str) -> RepoMeta {
    let branch = git_output(repo, &["symbolic-ref", "--short", "HEAD"])
        .await
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let description = std::fs::read_to_string(repo.join("description"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty() && !s.starts_with("Unnamed repository"));
    let topics = git_output_bytes(repo, &["cat-file", "-p", "HEAD:.gitents/topics"])
        .await
        .map(|b| {
            String::from_utf8_lossy(&b)
                .split([',', '\n', ' ', '\t'])
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let releases = git_output(repo, &["tag", "--list"])
        .await
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    let has_hooks = git_output(repo, &["cat-file", "-t", "HEAD:.gitents/hooks.toml"])
        .await
        .as_deref()
        == Some("blob\n");
    RepoMeta {
        rel: rel.to_owned(),
        branch,
        description,
        topics,
        releases,
        issues: 0,
        has_hooks,
    }
}

/// Wrap a repository view in the shared header band and tab bar, then the page
/// shell. `active` highlights the current tab.
fn repo_shell(meta: &RepoMeta, active: Tab, title: &str, body: Markup) -> Markup {
    page(
        title,
        html! { (repo_header(meta)) (tab_bar(meta, active)) (body) },
    )
}

/// The repository header band: path line, branch and visibility pills,
/// description, topic chips, and the Watch/Star actions.
fn repo_header(meta: &RepoMeta) -> Markup {
    let segments: Vec<&str> = meta.rel.split('/').collect();
    let last = segments.len().saturating_sub(1);
    html! {
        div.repo-header {
            div.repo-headline {
                div.repo-path {
                    (icon_folder())
                    @for (i, seg) in segments.iter().enumerate() {
                        @if i > 0 { span.sep { "/" } }
                        @if i == last {
                            span.here { (seg) }
                        } @else {
                            @let href = format!("/{}", segments.get(..=i).unwrap_or_default().join("/"));
                            a href=(href) { (seg) }
                        }
                    }
                    @if let Some(branch) = &meta.branch {
                        span.branch { (icon_branch()) (branch) }
                    }
                    span.pill-public { "Public" }
                }
                @if let Some(desc) = &meta.description {
                    p.repo-desc { (desc) }
                }
                @if !meta.topics.is_empty() {
                    div.topics {
                        @for topic in &meta.topics {
                            span.topic { (topic) }
                        }
                    }
                }
            }
            div.repo-actions {
                button.action-btn type="button" { (icon_eye()) "Watch" }
                button.action-btn type="button" { (icon_star()) "Star" }
            }
        }
    }
}

/// The tab bar with the active tab underlined. Tabs that have no backing data
/// yet still render so the navigation matches the design.
fn tab_bar(meta: &RepoMeta, active: Tab) -> Markup {
    let rel = &meta.rel;
    html! {
        nav.tabs {
            a.tab.active[active == Tab::Overview] href={ "/" (rel) } { "Overview" }
            a.tab.active[active == Tab::Files] href={ "/" (rel) "/files" } { "Files" }
            a.tab.active[active == Tab::Releases] href={ "/" (rel) "/releases" } {
                "Releases"
                @if meta.releases > 0 { span.tab-count { (meta.releases) } }
            }
            a.tab.active[active == Tab::Hooks] href={ "/" (rel) "/hooks" } {
                "Hooks"
                @if meta.has_hooks { span.tab-dot {} }
            }
            a.tab.active[active == Tab::Issues] href={ "/" (rel) "/issues" } {
                "Issues"
                @if meta.issues > 0 { span.tab-count { (meta.issues) } }
            }
            a.tab.active[active == Tab::Settings] href={ "/" (rel) "/settings" } { "Settings" }
        }
    }
}

/// Join the path segments of a browse view, rejecting empty or traversing
/// components. The result is used only as a git tree path (`HEAD:<path>`), never
/// touched on disk, but refusing `..` keeps the rendered links well-formed.
fn browse_path(sub: &[&str]) -> Option<String> {
    if sub.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(sub.join("/"))
}

/// The repository listing shown at `/`.
fn index(state: &AppState) -> Markup {
    let repos = discover_repos(&state.data_dir);
    page(
        "Repositories",
        html! {
            div.page-header {
                h1.page-title { (icon_repo()) "Repositories" }
                @if !repos.is_empty() {
                    span.count { (repos.len()) " repos" }
                }
            }
            @if repos.is_empty() {
                div.blankslate {
                    h2 { "No repositories yet" }
                    p { "Push to this server to create one:" }
                    p { code { "git push <url>/my-repo.git HEAD" } }
                }
            } @else {
                ul.repo-list {
                    @for repo in &repos {
                        li {
                            a.repo-row href={ "/" (repo) } {
                                span.repo-icon { (icon_repo()) }
                                span.repo-name { (repo) }
                                span.repo-badge { "git" }
                                span.repo-arrow { (icon_arrow()) }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// A single repository's overview: the rendered README beside an aside of
/// clone, about, releases, and language cards.
async fn repo_page(repo: &Path, meta: &RepoMeta, host: Option<&str>) -> Markup {
    let rel = &meta.rel;
    let updated = git_output(repo, &["log", "-1", "--format=%ar"])
        .await
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let is_empty = updated.is_none();
    let tree = root_tree(repo, !is_empty).await;
    let readme = readme(repo, &tree).await;
    let clone_url = clone_url(host, rel);
    let langs = languages(repo).await;
    let latest = latest_release(repo).await;
    let name = rel.rsplit('/').next().unwrap_or(rel);

    let main = html! {
        @if is_empty {
            div.blankslate {
                h2 { "This repository is empty" }
                p { "Push a commit to get started." }
            }
        } @else if let Some((file, html)) = &readme {
            div.card {
                div.card-header { (icon_file()) " " (file) }
                article.adoc-body { (PreEscaped(html)) }
            }
        } @else if !tree.is_empty() {
            div.card {
                div.card-header { "Files" }
                @for entry in &tree {
                    div.card-row.is-dir[entry.is_dir] {
                        @if entry.is_dir { (icon_folder()) } @else { (icon_file()) }
                        a href=(entry_href(rel, "", entry)) { (entry.name) }
                    }
                }
            }
        }
    };

    let aside = html! {
        aside.aside {
            div.card {
                div.card-header { "Clone" }
                div.clone {
                    code { (clone_url) }
                    button.copy-btn data-copy={ "git clone " (clone_url) } { "Copy" }
                }
            }
            div.card {
                div.card-header { "About" }
                @if let Some((lang, color, _)) = langs.first() {
                    div.aside-row {
                        span.dot style={ "background:" (color) } {}
                        span { (lang) }
                    }
                }
                @if let Some(updated) = &updated {
                    div.aside-row {
                        (icon_clock())
                        span.muted { "Updated " (updated) }
                    }
                }
            }
            @if let Some(release) = &latest {
                div.card {
                    div.card-header { "Releases" span.count { (meta.releases) } }
                    div.aside-row {
                        (icon_tag())
                        a href={ "/" (rel) "/releases" } { span.tag-pill { (release.tag) } }
                        span.badge-latest { "Latest" }
                    }
                    div.aside-row {
                        span.muted { (release.title) " · " (release.date) }
                    }
                }
            }
            @if !langs.is_empty() {
                div.card {
                    div.card-header { "Languages" }
                    div.lang {
                        div.lang-bar {
                            @for (_, color, pct) in &langs {
                                span style={ "width:" (pct) "%;background:" (color) } {}
                            }
                        }
                        ul.lang-legend {
                            @for (lang, color, pct) in &langs {
                                li {
                                    span.lang-dot style={ "background:" (color) } {}
                                    span { (lang) }
                                    span.pct { (pct) "%" }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    repo_shell(
        meta,
        Tab::Overview,
        name,
        html! { div.overview { div { (main) } (aside) } },
    )
}

/// A single entry in the repository's root tree.
struct TreeEntry {
    name: String,
    is_dir: bool,
}

/// The entries of the root tree at `HEAD`, directories first then by name.
async fn root_tree(repo: &Path, has_head: bool) -> Vec<TreeEntry> {
    if !has_head {
        return Vec::new();
    }
    list_tree(repo, "HEAD").await
}

/// The rendered README for the overview: the first AsciiDoc file in the root
/// tree whose stem is `README`, converted to HTML, paired with its filename.
/// `None` when there is no such file or it fails to render.
async fn readme(repo: &Path, tree: &[TreeEntry]) -> Option<(String, String)> {
    let entry = tree.iter().find(|e| {
        !e.is_dir
            && crate::asciidoc::is_asciidoc(&e.name)
            && e.name
                .rsplit_once('.')
                .is_some_and(|(stem, _)| stem.eq_ignore_ascii_case("readme"))
    })?;
    let spec = format!("HEAD:{}", entry.name);
    let bytes = git_output_bytes(repo, &["cat-file", "-p", &spec]).await?;
    let html = crate::asciidoc::to_html(&String::from_utf8_lossy(&bytes))?;
    Some((entry.name.clone(), html))
}

/// A language's display name, swatch color (a CSS custom property), and the
/// percentage of tracked bytes it accounts for.
type Language = (&'static str, &'static str, u8);

/// Map a filename to a language name and swatch color by its extension, or
/// `None` for files that do not count toward the language breakdown.
fn classify_language(name: &str) -> Option<(&'static str, &'static str)> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => ("Rust", "var(--s-type)"),
        "html" | "htm" => ("HTML", "var(--s-func)"),
        "css" => ("CSS", "var(--s-prop)"),
        "js" | "mjs" | "cjs" => ("JavaScript", "var(--s-const)"),
        "ts" | "tsx" => ("TypeScript", "var(--s-prop)"),
        "py" => ("Python", "var(--s-string)"),
        "go" => ("Go", "var(--s-prop)"),
        "c" | "h" => ("C", "var(--s-const)"),
        "cpp" | "cc" | "hpp" | "cxx" => ("C++", "var(--s-const)"),
        "sh" | "bash" => ("Shell", "var(--s-func)"),
        "toml" => ("TOML", "var(--s-type)"),
        "yaml" | "yml" => ("YAML", "var(--s-prop)"),
        "json" => ("JSON", "var(--s-const)"),
        "md" | "adoc" | "asciidoc" => ("Prose", "var(--s-comment)"),
        _ => return None,
    };
    Some(lang)
}

/// The language breakdown for `HEAD`, by tracked blob size, as the top few
/// languages with integer percentages summing to roughly 100.
async fn languages(repo: &Path) -> Vec<Language> {
    let Some(out) = git_output(repo, &["ls-tree", "-r", "-l", "HEAD"]).await else {
        return Vec::new();
    };
    let mut totals: Vec<(&'static str, &'static str, u64)> = Vec::new();
    let mut grand: u64 = 0;
    for line in out.lines() {
        let Some((meta, name)) = line.split_once('\t') else {
            continue;
        };
        let size: u64 = meta
            .split_whitespace()
            .nth(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let Some((lang, color)) = classify_language(name) else {
            continue;
        };
        grand = grand.saturating_add(size);
        match totals.iter_mut().find(|(l, _, _)| *l == lang) {
            Some(entry) => entry.2 = entry.2.saturating_add(size),
            None => totals.push((lang, color, size)),
        }
    }
    if grand == 0 {
        return Vec::new();
    }
    totals.sort_by_key(|b| std::cmp::Reverse(b.2));
    totals.truncate(4);
    totals
        .into_iter()
        .map(|(lang, color, bytes)| {
            let pct = bytes.saturating_mul(100).checked_div(grand).unwrap_or(0);
            (lang, color, u8::try_from(pct).unwrap_or(100))
        })
        .filter(|(_, _, pct)| *pct > 0)
        .collect()
}

/// A tagged release: its tag, the release name and notes drawn from the tag (or
/// commit) message, the relative date, and the target commit's short hash.
struct Release {
    tag: String,
    title: String,
    body: String,
    date: String,
    short: String,
}

/// All tags as releases, newest first by creation date.
async fn releases(repo: &Path) -> Vec<Release> {
    let Some(list) = git_output(repo, &["tag", "--sort=-creatordate", "--list"]).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tag in list
        .lines()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(40)
    {
        let Some(meta) = git_output(repo, &["log", "-1", "--format=%h%x00%ar", tag]).await else {
            continue;
        };
        let mut parts = meta.trim().split('\u{0}');
        let short = parts.next().unwrap_or_default().to_owned();
        let date = parts.next().unwrap_or_default().to_owned();
        let notes = git_output(
            repo,
            &[
                "tag",
                "--list",
                "--format=%(contents:subject)%00%(contents:body)",
                tag,
            ],
        )
        .await
        .unwrap_or_default();
        let mut np = notes.split('\u{0}');
        let title = np.next().unwrap_or_default().trim().to_owned();
        let body = np.next().unwrap_or_default().trim().to_owned();
        out.push(Release {
            tag: tag.to_owned(),
            title,
            body,
            date,
            short,
        });
    }
    out
}

/// The newest release, if any.
async fn latest_release(repo: &Path) -> Option<Release> {
    releases(repo).await.into_iter().next()
}

/// The entries of the tree named by `spec` (a git tree-ish such as `HEAD` or
/// `HEAD:src`), directories first then by name. Empty if `spec` is not a tree.
async fn list_tree(repo: &Path, spec: &str) -> Vec<TreeEntry> {
    let Some(out) = git_output(repo, &["ls-tree", spec]).await else {
        return Vec::new();
    };
    let mut entries: Vec<TreeEntry> = out
        .lines()
        .filter_map(|line| {
            let (meta, name) = line.split_once('\t')?;
            let kind = meta.split(' ').nth(1).unwrap_or_default();
            Some(TreeEntry {
                name: name.to_owned(),
                is_dir: kind == "tree",
            })
        })
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// The link to a tree entry: a `tree` view for directories, a `blob` view for
/// files. `dir` is the tree's path within the repo (empty at the root).
fn entry_href(rel: &str, dir: &str, entry: &TreeEntry) -> String {
    let view = if entry.is_dir { "tree" } else { "blob" };
    let name = &entry.name;
    if dir.is_empty() {
        format!("/{rel}/{view}/{name}")
    } else {
        format!("/{rel}/{view}/{dir}/{name}")
    }
}

/// Run `git -C <repo> <args>` and return its stdout, or `None` on failure.
async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `git -C <repo> <args>` and return its raw stdout bytes, or `None` on
/// failure. Used for blob contents, which may not be valid UTF-8.
async fn git_output_bytes(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// All bare repositories under `root`, as relative slash paths, sorted.
fn discover_repos(root: &Path) -> Vec<String> {
    let mut repos = Vec::new();
    collect_repos(root, root, MAX_DEPTH, &mut repos);
    repos.sort();
    repos
}

/// Recurse into `dir` (up to `depth` levels) collecting bare repositories.
fn collect_repos(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_bare_repo(&path) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        } else {
            collect_repos(root, &path, depth.saturating_sub(1), out);
        }
    }
}

/// The clone URL for `rel`, using the request host when known.
fn clone_url(host: Option<&str>, rel: &str) -> String {
    match host {
        Some(host) => format!("http://{host}/{rel}"),
        None => format!("/{rel}"),
    }
}

/// One row of the Files tree pane.
struct TreeRow {
    name: String,
    path: String,
    is_dir: bool,
    depth: usize,
    expanded: bool,
    selected: bool,
}

/// Walk the tree under `dir`, emitting a row per entry and recursing into any
/// directory in `expanded`. Boxed because the recursion is `async`.
fn collect_rows<'a>(
    repo: &'a Path,
    dir: &'a str,
    depth: usize,
    expanded: &'a HashSet<String>,
    selected: &'a str,
    out: &'a mut Vec<TreeRow>,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let spec = if dir.is_empty() {
            "HEAD".to_owned()
        } else {
            format!("HEAD:{dir}")
        };
        for entry in list_tree(repo, &spec).await {
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            let is_expanded = entry.is_dir && expanded.contains(&path);
            out.push(TreeRow {
                name: entry.name.clone(),
                path: path.clone(),
                is_dir: entry.is_dir,
                depth,
                expanded: is_expanded,
                selected: !entry.is_dir && path == selected,
            });
            if is_expanded {
                collect_rows(
                    repo,
                    &path,
                    depth.saturating_add(1),
                    expanded,
                    selected,
                    out,
                )
                .await;
            }
        }
    })
}

/// The Files tab: a tree pane beside a blob pane, both inside one card. With no
/// client JavaScript, expanding a folder or opening a file is a link to
/// `/<repo>/files/<path>`; the tree is rendered already expanded along the
/// selected path.
async fn files_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
    let rel = &meta.rel;
    let Some(selected) = browse_path(sub) else {
        return not_found().into_response();
    };

    // Classify the selection so the right pane shows a file and the tree expands
    // the correct ancestors.
    let kind = if selected.is_empty() {
        None
    } else {
        git_output(repo, &["cat-file", "-t", &format!("HEAD:{selected}")])
            .await
            .map(|s| s.trim().to_owned())
    };
    let selected_file = matches!(kind.as_deref(), Some("blob")).then(|| selected.clone());
    let selected_dir = match kind.as_deref() {
        Some("tree") => selected.clone(),
        Some("blob") => selected
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_owned()),
        _ if selected.is_empty() => String::new(),
        _ => return not_found().into_response(),
    };

    // Expand every ancestor directory of the selection (and the selection itself
    // when it is a directory).
    let mut expanded = HashSet::new();
    let mut acc = String::new();
    for part in selected_dir.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        expanded.insert(acc.clone());
    }

    let mut rows = Vec::new();
    collect_rows(
        repo,
        "",
        0,
        &expanded,
        selected_file.as_deref().unwrap_or_default(),
        &mut rows,
    )
    .await;

    let right = match &selected_file {
        Some(path) => blob_pane(repo, path).await,
        None => html! {
            div.files-empty {
                (icon_file())
                p { "Select a file to view its contents." }
            }
        },
    };

    let name = rel.rsplit('/').next().unwrap_or(rel);
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            div.files {
                div.tree-pane {
                    div.tree-head { (icon_branch()) (meta.branch.as_deref().unwrap_or("HEAD")) }
                    @for row in &rows {
                        a.tree-row.sel[row.selected]
                            href={ "/" (rel) "/files/" (row.path) }
                            style={ "padding-left:" (row.depth.saturating_mul(15).saturating_add(8)) "px" }
                        {
                            @if row.is_dir {
                                span.chev.open[row.expanded] { (icon_chevron()) }
                                span.ic-folder { (icon_folder()) }
                            } @else {
                                span.chev {}
                                span.ic-file { (icon_file()) }
                            }
                            span { (row.name) }
                        }
                    }
                }
                div.blob-pane { (right) }
            }
        },
    )
    .into_response()
}

/// The right-hand pane of the Files view: a file's path, line/size meta, and its
/// syntax-highlighted source (or a binary notice).
async fn blob_pane(repo: &Path, path: &str) -> Markup {
    let Some(bytes) = git_output_bytes(repo, &["cat-file", "-p", &format!("HEAD:{path}")]).await
    else {
        return html! { div.files-empty { "File not found." } };
    };
    let name = path.rsplit('/').next().unwrap_or(path);
    html! {
        div.blob-head {
            span { (path) }
            span.meta {
                @if !is_binary(&bytes) {
                    span { (String::from_utf8_lossy(&bytes).lines().count()) " lines" }
                }
                span { (human_size(bytes.len())) }
                button.copy-btn data-copy=(String::from_utf8_lossy(&bytes)) { "Copy" }
            }
        }
        @if is_binary(&bytes) {
            div.binary { "Binary file (" (human_size(bytes.len())) ") not shown." }
        } @else {
            (blob_body(name, &String::from_utf8_lossy(&bytes)))
        }
    }
}

/// A byte count rendered as a compact human-readable size.
fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    #[expect(
        clippy::cast_precision_loss,
        reason = "an approximate display size; exactness past 2^52 bytes is irrelevant"
    )]
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len().saturating_sub(1) {
        size /= 1024.0;
        unit = unit.saturating_add(1);
    }
    let label = UNITS.get(unit).unwrap_or(&"B");
    if unit == 0 {
        format!("{bytes} {label}")
    } else {
        format!("{size:.1} {label}")
    }
}

/// A directory listing at `sub` within the repository.
async fn tree_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
    let rel = &meta.rel;
    let Some(dir) = browse_path(sub) else {
        return not_found().into_response();
    };
    let spec = if dir.is_empty() {
        "HEAD".to_owned()
    } else {
        format!("HEAD:{dir}")
    };
    let entries = list_tree(repo, &spec).await;
    if entries.is_empty() && !dir.is_empty() {
        return not_found().into_response();
    }
    let name = rel.rsplit('/').next().unwrap_or(rel);
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            (crumbs(rel, &dir, false))
            div.card {
                div.card-header { "Files" }
                @if dir.is_empty() && entries.is_empty() {
                    div.card-row { "Empty repository." }
                }
                @for entry in &entries {
                    div.card-row.is-dir[entry.is_dir] {
                        @if entry.is_dir { (icon_folder()) } @else { (icon_file()) }
                        a href=(entry_href(rel, &dir, entry)) { (entry.name) }
                    }
                }
            }
        },
    )
    .into_response()
}

/// A single file's contents at `sub`, syntax-highlighted when the language is
/// recognized and the file is text.
async fn blob_page(repo: &Path, meta: &RepoMeta, sub: &[&str]) -> Response {
    let rel = &meta.rel;
    let Some(path) = browse_path(sub).filter(|p| !p.is_empty()) else {
        return not_found().into_response();
    };
    let spec = format!("HEAD:{path}");
    if git_output(repo, &["cat-file", "-t", &spec])
        .await
        .as_deref()
        != Some("blob\n")
    {
        return not_found().into_response();
    }
    let Some(bytes) = git_output_bytes(repo, &["cat-file", "-p", &spec]).await else {
        return not_found().into_response();
    };
    let name = path.rsplit('/').next().unwrap_or(&path);
    let body = if is_binary(&bytes) {
        html! { div.blob { div.binary { "Binary file (" (bytes.len()) " bytes) not shown." } } }
    } else {
        let text = String::from_utf8_lossy(&bytes);
        match crate::asciidoc::is_asciidoc(name)
            .then(|| crate::asciidoc::to_html(&text))
            .flatten()
        {
            Some(html) => html! { div.card { article.adoc-body { (PreEscaped(html)) } } },
            None => blob_body(name, &text),
        }
    };
    repo_shell(
        meta,
        Tab::Files,
        name,
        html! {
            (crumbs(rel, &path, true))
            (body)
        },
    )
    .into_response()
}

/// Render text file `source` with a line-number gutter, highlighting via
/// `arborium` when the filename maps to a known grammar.
fn blob_body(name: &str, source: &str) -> Markup {
    let lines = source.lines().count().max(1);
    let mut gutter = String::new();
    for n in 1..=lines {
        gutter.push_str(&n.to_string());
        gutter.push('\n');
    }
    let highlighted = highlight(name, source);
    html! {
        div.blob {
            pre.blob-nums { (gutter) }
            pre.blob-code {
                @match highlighted {
                    Some(html) => code.code { (PreEscaped(html)) },
                    None => code { (source) },
                }
            }
        }
    }
}

/// Highlighted HTML for `source`, or `None` when the filename has no grammar
/// (in which case the caller renders escaped plain text). The highlighter is
/// built and used synchronously so its non-`Send` grammar store is never held
/// across an `.await`.
fn highlight(name: &str, source: &str) -> Option<String> {
    let language = arborium::detect_language(name)?;
    let config = Config {
        html_format: HtmlFormat::ClassNames,
        ..Default::default()
    };
    Highlighter::with_config(config)
        .highlight(language, source)
        .ok()
}

/// Whether `bytes` looks like binary content (a NUL byte in the leading chunk,
/// the same heuristic git uses).
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// A single commit: its metadata and a colorized unified diff.
async fn commit_page(repo: &Path, meta: &RepoMeta, sha: &str) -> Response {
    if sha.is_empty() || sha.len() > 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return not_found().into_response();
    }
    let Some(info) = git_output(
        repo,
        &["show", "-s", "--format=%H%x00%an%x00%ar%x00%s%x00%b", sha],
    )
    .await
    else {
        return not_found().into_response();
    };
    let mut parts = info.split('\u{0}');
    let hash = parts.next().unwrap_or_default().trim().to_owned();
    let author = parts.next().unwrap_or_default().to_owned();
    let when = parts.next().unwrap_or_default().to_owned();
    let subject = parts.next().unwrap_or_default().to_owned();
    let body = parts.next().unwrap_or_default().trim_end().to_owned();
    let short = hash.get(..7).unwrap_or(&hash).to_owned();
    let patch = git_output(repo, &["show", "--no-color", "--format=", "--patch", sha])
        .await
        .unwrap_or_default();

    repo_shell(
        meta,
        Tab::Files,
        &subject,
        html! {
            div.card {
                div.card-header { (icon_commit()) " Commit " span.sha { (short) } }
                div.commit {
                    div.commit-subject { (subject) }
                    @if !body.is_empty() {
                        div.commit-msg { (body) }
                    }
                    div.commit-meta { (author) " · " (when) }
                }
            }
            (diff_view(&patch))
        },
    )
    .into_response()
}

/// The Releases tab: tags presented as a changelog timeline, newest first.
async fn releases_page(repo: &Path, meta: &RepoMeta) -> Markup {
    let releases = releases(repo).await;
    repo_shell(
        meta,
        Tab::Releases,
        "Releases",
        html! {
            div.page-header { h1.page-title { "Releases" } }
            @if releases.is_empty() {
                div.blankslate {
                    h2 { "No releases yet" }
                    p { "Push a tag to publish a release: " code { "git push <url> v1.0.0" } }
                }
            } @else {
                div.timeline {
                    @for (i, release) in releases.iter().enumerate() {
                        article.release.latest[i == 0] {
                            div.card {
                                div.release-head {
                                    (icon_tag())
                                    span.release-tag { (release.tag) }
                                    @if !release.title.is_empty() && release.title != release.tag {
                                        span.release-name { (release.title) }
                                    }
                                    @if i == 0 { span.badge-latest { "Latest" } }
                                    span.release-date { (release.date) }
                                }
                                @if !release.body.is_empty() {
                                    div.release-body { p { (release.body) } }
                                }
                                div.release-foot {
                                    span.sha { (icon_commit()) (release.short) }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

/// Render a unified diff, coloring each line by its leading marker.
fn diff_view(patch: &str) -> Markup {
    if patch.trim().is_empty() {
        return html! {};
    }
    html! {
        div.diff {
            @for line in patch.lines() {
                span class={ "ln " (diff_class(line)) } { (line) "\n" }
            }
        }
    }
}

/// The CSS class for a diff line, chosen from its leading marker.
fn diff_class(line: &str) -> &'static str {
    if line.starts_with("@@") {
        "hunk"
    } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        "file"
    } else if line.starts_with("index ")
        || line.starts_with("new file")
        || line.starts_with("deleted file")
        || line.starts_with("old mode")
        || line.starts_with("new mode")
        || line.starts_with("rename ")
        || line.starts_with("similarity ")
        || line.starts_with("Binary files")
    {
        "meta"
    } else if line.starts_with('+') {
        "add"
    } else if line.starts_with('-') {
        "del"
    } else {
        "ctx"
    }
}

/// One segment of a breadcrumb trail: a label and, unless it is the current
/// file, the link to its directory listing.
struct Crumb {
    label: String,
    href: Option<String>,
}

/// Breadcrumb navigation from the repository root down through `path`. When
/// `is_file` is set, the final component is shown as plain text rather than a
/// link, since a file has no listing of its own.
fn crumbs(rel: &str, path: &str, is_file: bool) -> Markup {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut acc = String::new();
    let mut trail: Vec<Crumb> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let is_last = i.saturating_add(1) == parts.len();
        let href = (!(is_last && is_file)).then(|| format!("/{rel}/tree/{acc}"));
        trail.push(Crumb {
            label: (*part).to_owned(),
            href,
        });
    }
    html! {
        nav.crumbs {
            a href={ "/" (rel) } { (rel) }
            @for crumb in &trail {
                span.sep { "/" }
                @match &crumb.href {
                    Some(href) => a href=(href) { (crumb.label) },
                    None => span.here { (crumb.label) },
                }
            }
        }
    }
}

/// A `404` page.
fn not_found() -> (StatusCode, Markup) {
    (
        StatusCode::NOT_FOUND,
        page(
            "Not found",
            html! {
                div.blankslate {
                    h2 { "404" }
                    p { "No such repository." }
                    a.btn href="/" { "Back to repositories" }
                }
            },
        ),
    )
}

/// Wrap page `body` in the shared HTML shell, navigation, and styling.
fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Git Ents" }
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link rel="stylesheet" href=(FONTS);
                style { (PreEscaped(STYLE)) }
            }
            body {
                nav.site-nav {
                    div.nav-inner {
                        a.nav-logo href="/" { (icon_tree()) "git-ents" }
                        div.nav-search {
                            (icon_search())
                            input type="search" placeholder="Jump to file or symbol" aria-label="Search";
                        }
                        div.nav-links {
                            a.nav-link href="/" { "Explore" }
                            a.nav-link href="/" { "Docs" }
                            a.nav-avatar href="/" title="Account" { "el" }
                        }
                    }
                }
                main.content { (body) }
                footer.site-footer {
                    div.footer-inner {
                        "git-ents · served as paper-grain HTML · no JavaScript required"
                    }
                }
                script { (PreEscaped(COPY_SCRIPT)) }
            }
        }
    }
}

/// Inline icons (16×16 Octicons paths), kept local so the UI has no asset deps.
fn svg(path: &str) -> Markup {
    html! {
        svg.icon viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" {
            (PreEscaped(format!("<path d=\"{path}\"/>")))
        }
    }
}

fn icon_repo() -> Markup {
    svg(
        "M2 2.5A2.5 2.5 0 0 1 4.5 0h8.75a.75.75 0 0 1 .75.75v12.5a.75.75 0 0 1-.75.75h-2.5a.75.75 0 0 1 0-1.5h1.75v-2h-8a1 1 0 0 0-.714 1.7.75.75 0 1 1-1.072 1.05A2.495 2.495 0 0 1 2 11.5Zm10.5-1h-8a1 1 0 0 0-1 1v6.708A2.486 2.486 0 0 1 4.5 9h8ZM5 12.25a.25.25 0 0 1 .25-.25h3.5a.25.25 0 0 1 .25.25v3.25a.25.25 0 0 1-.4.2l-1.45-1.087a.249.249 0 0 0-.3 0L5.4 15.7a.25.25 0 0 1-.4-.2Z",
    )
}

fn icon_folder() -> Markup {
    svg(
        "M1.75 1A1.75 1.75 0 0 0 0 2.75v10.5C0 14.216.784 15 1.75 15h12.5A1.75 1.75 0 0 0 16 13.25v-8.5A1.75 1.75 0 0 0 14.25 3H7.5a.25.25 0 0 1-.2-.1l-.9-1.2C6.07 1.26 5.55 1 5 1H1.75Z",
    )
}

fn icon_file() -> Markup {
    svg(
        "M2 1.75C2 .784 2.784 0 3.75 0h6.586c.464 0 .909.184 1.237.513l2.914 2.914c.329.328.513.773.513 1.237v9.586A1.75 1.75 0 0 1 13.25 16h-9.5A1.75 1.75 0 0 1 2 14.25Zm1.75-.25a.25.25 0 0 0-.25.25v12.5c0 .138.112.25.25.25h9.5a.25.25 0 0 0 .25-.25V6h-2.75A1.75 1.75 0 0 1 9 4.25V1.5Zm6.75.062V4.25c0 .138.112.25.25.25h2.688l-.011-.013-2.914-2.914-.013-.011Z",
    )
}

fn icon_chevron() -> Markup {
    svg(
        "M6.22 3.22a.75.75 0 0 1 1.06 0l4.25 4.25a.75.75 0 0 1 0 1.06l-4.25 4.25a.75.75 0 0 1-1.06-1.06L9.94 8 6.22 4.28a.75.75 0 0 1 0-1.06Z",
    )
}

fn icon_branch() -> Markup {
    svg(
        "M9.5 3.25a2.25 2.25 0 1 1 3 2.122V6A2.5 2.5 0 0 1 10 8.5H6a1 1 0 0 0-1 1v1.128a2.251 2.251 0 1 1-1.5 0V5.372a2.25 2.25 0 1 1 1.5 0v1.836A2.493 2.493 0 0 1 6 7h4a1 1 0 0 0 1-1v-.628A2.25 2.25 0 0 1 9.5 3.25Zm-6 0a.75.75 0 1 0 1.5 0 .75.75 0 0 0-1.5 0Zm8.25-.75a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5ZM4.25 12a.75.75 0 1 0 0 1.5.75.75 0 0 0 0-1.5Z",
    )
}

fn icon_eye() -> Markup {
    svg(
        "M8 2c1.981 0 3.671.992 4.933 2.078 1.27 1.091 2.187 2.345 2.637 3.023a1.62 1.62 0 0 1 0 1.798c-.45.678-1.367 1.932-2.637 3.023C11.67 13.008 9.981 14 8 14c-1.981 0-3.671-.992-4.933-2.078C1.797 10.831.88 9.577.43 8.9a1.62 1.62 0 0 1 0-1.798c.45-.678 1.367-1.932 2.637-3.023C4.33 2.992 6.019 2 8 2Zm0 1.5c-1.504 0-2.88.762-3.957 1.69-1.077.926-1.882 2.025-2.281 2.624a.12.12 0 0 0 0 .172c.4.599 1.204 1.698 2.28 2.624C5.121 11.738 6.497 12.5 8 12.5c1.504 0 2.88-.762 3.957-1.69 1.077-.926 1.882-2.025 2.281-2.624a.12.12 0 0 0 0-.172c-.4-.599-1.204-1.698-2.28-2.624C10.879 4.262 9.503 3.5 8 3.5ZM8 6a2 2 0 1 1 0 4 2 2 0 0 1 0-4Z",
    )
}

fn icon_star() -> Markup {
    svg(
        "M8 .25a.75.75 0 0 1 .673.418l1.882 3.815 4.21.612a.75.75 0 0 1 .416 1.279l-3.046 2.97.719 4.192a.751.751 0 0 1-1.088.791L8 12.347l-3.766 1.98a.75.75 0 0 1-1.088-.79l.72-4.194L.818 6.374a.75.75 0 0 1 .416-1.28l4.21-.611L7.327.668A.75.75 0 0 1 8 .25Z",
    )
}

fn icon_tag() -> Markup {
    svg(
        "M1 7.775V2.75C1 1.784 1.784 1 2.75 1h5.025c.464 0 .91.184 1.238.513l6.25 6.25a1.75 1.75 0 0 1 0 2.474l-5.026 5.026a1.75 1.75 0 0 1-2.474 0l-6.25-6.25A1.75 1.75 0 0 1 1 7.775Zm1.5 0c0 .066.026.13.073.177l6.25 6.25a.25.25 0 0 0 .354 0l5.025-5.025a.25.25 0 0 0 0-.354l-6.25-6.25a.25.25 0 0 0-.177-.073H2.75a.25.25 0 0 0-.25.25ZM6 5a1 1 0 1 1 0 2 1 1 0 0 1 0-2Z",
    )
}

fn icon_clock() -> Markup {
    svg(
        "M8 0a8 8 0 1 1 0 16A8 8 0 0 1 8 0ZM1.5 8a6.5 6.5 0 1 0 13 0 6.5 6.5 0 0 0-13 0Zm7-3.25v2.992l2.028.812a.75.75 0 0 1-.557 1.392l-2.5-1A.751.751 0 0 1 7 8.25v-3.5a.75.75 0 0 1 1.5 0Z",
    )
}

fn icon_commit() -> Markup {
    svg(
        "M11.93 8.5a4.002 4.002 0 0 1-7.86 0H.75a.75.75 0 0 1 0-1.5h3.32a4.002 4.002 0 0 1 7.86 0h3.32a.75.75 0 0 1 0 1.5Zm-1.43-.75a2.5 2.5 0 1 0-5 0 2.5 2.5 0 0 0 5 0Z",
    )
}

fn icon_tree() -> Markup {
    svg(
        "M8 0a4 4 0 0 1 .91 7.895A.749.749 0 0 1 8.75 8v2.5h2.75a1.75 1.75 0 0 1 1.75 1.75v1.25h.25a.75.75 0 0 1 0 1.5h-2a.75.75 0 0 1 0-1.5h.25v-1.25a.25.25 0 0 0-.25-.25H4.25a.25.25 0 0 0-.25.25v1.25h.25a.75.75 0 0 1 0 1.5h-2a.75.75 0 0 1 0-1.5H2.5v-1.25c0-.966.784-1.75 1.75-1.75H7V8a.749.749 0 0 1-.16-.105A4 4 0 0 1 8 0Zm0 1.5a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5Z",
    )
}

fn icon_search() -> Markup {
    svg(
        "M10.68 11.74a6 6 0 0 1-7.922-8.982 6 6 0 0 1 8.982 7.922l3.04 3.04a.749.749 0 0 1-.326 1.275.749.749 0 0 1-.734-.215ZM11.5 7a4.499 4.499 0 1 0-8.997 0A4.499 4.499 0 0 0 11.5 7Z",
    )
}

fn icon_arrow() -> Markup {
    svg(
        "M6.22 3.22a.75.75 0 0 1 1.06 0l4.25 4.25a.75.75 0 0 1 0 1.06l-4.25 4.25a.75.75 0 0 1-1.06-1.06L9.94 8 6.22 4.28a.75.75 0 0 1 0-1.06Z",
    )
}
