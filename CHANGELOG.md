# Changelog

All notable changes to Masstuffy are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

The version is the `masstuffy-http` crate version (`crates/http/Cargo.toml`), which is also what
`masstuffy-server --version` prints, the Docker image tag and the git tag (`v<version>`).
Merging into `release` publishes the image, tags the commit and creates a GitHub release whose
notes are the matching section below: that section must exist before the merge.

## [Unreleased]

## [0.2.0] - 2026-11-20

### Added

- `POST /v1/grep`: grep one or more collections (literal or regex queries) and stream the hits,
  unreadable records and progress as NDJSON, ending with per-collection status and counters.
  Concurrent greps over the same collection share a single circular scan. Limits are listed
  under `limits.grep` in `GET /v1`.
- Python client: `grep`.
- Every collection object, the list included, has a `size` object: live and total records,
  tombstones, stored and tombstoned bytes, segment and CDX bytes of the active generation. Read
  in O(1). The Python client parses it as `CollectionSize`.
- Each release is tagged `vX.Y.Z` in git and published as a GitHub release with these notes.

### Changed

- The collection scan checkpoint format is now version 2. The first open of a collection with an
  older checkpoint does one full CDX scan; nothing to do by hand.

## [0.1.0] - 2026-11-19

First release of the rewrite.

### Added

- Storage engine: append-only WARC segments with optional zstd compression and trained
  dictionaries, crash recovery, tombstones and a secondary index backed by redb.
- Collections: create, rename, delete; record import, lookup, search, scan and deletion;
  dictionary training and index rebuild.
- `masstuffy-server`: HTTP API (`docs/http-api.md`). Every option can be set via `MASSTUFFY_*`
  environment variables.
- Python client (`clients/python`), built on pywarc.
- Docker image published to `ghcr.io/5igi0/masstuffy`.

[Unreleased]: https://github.com/5IGI0/Masstuffy/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/5IGI0/Masstuffy/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/5IGI0/Masstuffy/releases/tag/v0.1.0
