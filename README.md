# 🎁 `git-vendor`

*An in-source vendoring alternative to submodules and subtrees.*

<!-- rumdl-disable MD013 -->
[![CI](https://github.com/git-ents/git-vendor/actions/workflows/CI.yml/badge.svg)](https://github.com/git-ents/git-vendor/actions/workflows/CI.yml)
[![CD](https://github.com/git-ents/git-vendor/actions/workflows/CD.yml/badge.svg)](https://github.com/git-ents/git-vendor/actions/workflows/CD.yml)
<!-- rumdl-enable MD013 -->

> [!CAUTION]
> This project is being refactored to improve the API, and use [Gitoxide][gitoxide] for all repository operations.
> The last [release] is fully featured: please try that (alpha) release if you want to try the project out.
> When the refactor is finished, this warning will be removed.

[gitoxide]: https://github.com/GitoxideLabs/gitoxide
[release]: https://github.com/git-ents/git-vendor/releases/tag/git-vendor-v1.0.0-alpha.1

## About

To support a more expansive usage of the Git object database — as is the goal for other projects within the [`git-ents`](https://github.com/git-ents) organization — new tooling is needed.
This project provides a command that allows users to fetch and merge remote content.

You may see the terms *porcelain* and *plumbing* used across this project.
These are [borrowed from Git itself](https://git-scm.com/book/en/v2/Git-Internals-Plumbing-and-Porcelain): porcelain refers to user-facing commands, and plumbing refers to the lower-level libraries and commands they are built on.

## Crates

| Crate | Description | API |
|---|---|----|
| [`git-vendor`](crates/git-vendor/) | An in-source vendoring alternative to submodules and subtrees. | Porcelain |
| [`git-set-attr`](crates/git-set-attr/) | Set Git attributes programmatically. | Plumbing |

## Alternatives

This is not the first `git-vendor` project.
In fact, despite being developed independently, this project was published *after* multiple other `git-vendor` projects.

The [`thejoshwolfe/git-vendor`][thejoshwolfe] project appears to be near identical.
Another project, [`brettlangdon/git-vendor`][brettlangdon], carries the same name but has slightly different goals; it additionally supports contributing changes upstream.
Please check out each of those projects!

[brettlangdon]: https://github.com/brettlangdon/git-vendor
[thejoshwolfe]: https://github.com/thejoshwolfe/git-vendor
