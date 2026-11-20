---
name: changelog
description: Write or update CHANGELOG.md for Masstuffy. Use to add entries for recent work under [Unreleased], or to cut a release (move [Unreleased] to a new version, bump crates/http/Cargo.toml). Triggers on "changelog", "release notes", "prepare a release", "bump the version".
---

# Masstuffy changelog

`CHANGELOG.md` (repo root, English, [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
format) is the source of the GitHub release notes: on a push to `release`,
`.github/workflows/release-image.yml` reads the version from `crates/http/Cargo.toml`, extracts
the `## [<version>]` section with `scripts/changelog-section.sh` and **fails before building** if
that section is missing or empty. It then publishes the image, tags the commit `v<version>` and
creates the GitHub release.

## Mode 1: update `[Unreleased]` (default)

1. Find what is not documented yet:
   - last released version: `git ls-remote --tags origin 'v*'` (or `git tag -l 'v*'`), highest semver;
   - `git log --no-merges --format='%h %s' v<last>..HEAD` (no tag yet: the whole history),
     and `git diff --stat v<last>..HEAD` / `git show <sha>` when a subject is unclear;
   - skip anything already described in `CHANGELOG.md`.
2. Write entries under `## [Unreleased]`, grouped in this order, omitting empty groups:
   `### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`, `### Security`.
3. Entry rules:
   - written for **users** of the server, the HTTP API, the Python client and the Docker image,
     not for contributors: describe the visible effect, not the code change;
   - one bullet per change, stating the new behaviour ("`POST /v1/grep` streams NDJSON"),
     wrap at 100 columns;
   - mark breaking changes with a leading **Breaking:** (HTTP API, on-disk format, env vars,
     Python client API) and say what users must do;
   - leave out pure refactors, tests, CI, docs-only and agent/CLAUDE.md changes unless they affect
     users (e.g. a new published image tag);
   - merge several commits about one feature into one bullet.

## Mode 2: cut a release (when asked to prepare / cut a release)

1. Do Mode 1 first so `[Unreleased]` is complete.
2. Pick the version (semver, pre-1.0 rules): breaking change → bump minor; otherwise new feature →
   bump minor or patch as the user prefers (ask if unclear); fixes only → bump patch. It must be
   higher than the last tag.
3. In `CHANGELOG.md`:
   - rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` (today) and add a fresh empty
     `## [Unreleased]` above it;
   - update the link references at the bottom:
     `[Unreleased]: https://github.com/5IGI0/Masstuffy/compare/vX.Y.Z...HEAD` and add
     `[X.Y.Z]: https://github.com/5IGI0/Masstuffy/releases/tag/vX.Y.Z`.
4. Set `version = "X.Y.Z"` in the `[package]` section of `crates/http/Cargo.toml`, then run
   `cargo check -p masstuffy-http` so `Cargo.lock` follows.
5. Check: `scripts/changelog-section.sh X.Y.Z` prints the notes and exits 0.
6. Commit on `master` (`Release vX.Y.Z`). Do **not** push or merge into `release` unless the user
   asks: that merge is what publishes the image, the tag and the GitHub release.
