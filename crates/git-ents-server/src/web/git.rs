//! The data layer behind the web UI: thin wrappers over `git` plus the parsing
//! that turns its output into the trees, releases, and language breakdowns the
//! views render.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::http::{MAX_REPO_DEPTH, is_bare_repo};

/// Run `git -C <repo> <args>` and return its stdout as lossy UTF-8, or `None` on
/// failure.
pub(super) async fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    git_output_bytes(repo, args)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Run `git -C <repo> <args>` and return its raw stdout bytes, or `None` on
/// failure. Used for blob contents, which may not be valid UTF-8.
pub(super) async fn git_output_bytes(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
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

/// A single entry in a git tree.
pub(super) struct TreeEntry {
    pub(super) name: String,
    pub(super) is_dir: bool,
}

/// The entries of the root tree at `HEAD`, directories first then by name.
pub(super) async fn root_tree(repo: &Path, has_head: bool) -> Vec<TreeEntry> {
    if !has_head {
        return Vec::new();
    }
    list_tree(repo, "HEAD").await
}

/// The entries of the tree named by `spec` (a git tree-ish such as `HEAD` or
/// `HEAD:src`), directories first then by name. Empty if `spec` is not a tree.
pub(super) async fn list_tree(repo: &Path, spec: &str) -> Vec<TreeEntry> {
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

/// Join the path segments of a browse view, rejecting empty or traversing
/// components. The result is used only as a git tree path (`HEAD:<path>`), never
/// touched on disk, but refusing `..` keeps the rendered links well-formed.
pub(super) fn browse_path(sub: &[&str]) -> Option<String> {
    if sub.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(sub.join("/"))
}

/// All bare repositories under `root`, as relative slash paths, sorted.
pub(super) fn discover_repos(root: &Path) -> Vec<String> {
    let mut repos = Vec::new();
    collect_repos(root, root, MAX_REPO_DEPTH, &mut repos);
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

/// A language's display name, swatch color (a CSS custom property), and the
/// percentage of tracked bytes it accounts for.
pub(super) type Language = (&'static str, &'static str, u8);

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
pub(super) async fn languages(repo: &Path) -> Vec<Language> {
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
pub(super) struct Release {
    pub(super) tag: String,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) date: String,
    pub(super) short: String,
}

/// All tags as releases, newest first by creation date.
pub(super) async fn releases(repo: &Path) -> Vec<Release> {
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
pub(super) async fn latest_release(repo: &Path) -> Option<Release> {
    releases(repo).await.into_iter().next()
}
