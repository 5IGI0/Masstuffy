# Masstuffy

Single-machine object storage built on WARC records, written in Rust. This is a from-scratch rewrite.

- **Source of truth: [`spec/`](spec/)** (written in French). Read [`spec/README.md`](spec/README.md) first, then the chapter(s) relevant to your task. Sections tagged **[RECO]** are the intended design. Sections tagged **[LEGACY]** describe the old implementation and its bugs: they explain intent, but they are **not** something to reproduce.
- Do not port or imitate the legacy implementation. The spec and the rules below decide.
- Out of scope unless explicitly requested: HTTP API, CLI, authentication.

## Workspace layout

The project is a Cargo workspace made of small, **independently reusable crates** under `crates/`.

- Each crate has one clear responsibility and a minimal public API, and is usable outside Masstuffy (e.g. the WARC parser must not know about collections, segments or zstd).
- Dependencies point one way only: low-level format crates → storage crates → the Masstuffy engine. No cycles, and no low-level crate depends on a higher-level one.
- A crate only pulls the dependencies it needs. Optional integrations (tokio, zstd, serde…) go behind cargo features when a crate is useful without them.
- Shared dependency versions live in `[workspace.dependencies]`. Shared lints live in `[workspace.lints]`.
- The crate map and the dependency graph are documented in [`docs/architecture.md`](docs/architecture.md). Keep it up to date when a crate is added or its role changes.

## Workflow

### Small tasks: do them directly

A task is small if **all** of the following hold:

- it touches at most ~30 lines, in one module;
- it does not change the on-disk format, a public API, locking/concurrency, or durability (fsync/rename ordering);
- it does not add a dependency.

Examples: typo, doc comment, rename a local, fix an obvious off-by-one with a test, clippy fix, bump a patch version.

For a small task, make the change yourself, then run the checks in *Definition of done*.

### Features and everything else: planner → coder → reviewer (mandatory)

Any feature, new module, format change, concurrency change, refactor across modules, or anything you are unsure is "small" **must** go through the three agents, in this order:

1. **`planner`** (Opus): produces a written plan. It does not edit code.
2. **`coder`** (Sonnet): implements the plan exactly. Pass it the **full plan verbatim**, not a summary.
3. **`reviewer`** (Opus): reviews the diff against the plan, the spec and the rules below. It does not edit code. Pass it the full plan and the list of changed files.

Loop:

- Reviewer verdict `APPROVE`: done. Report to the user.
- Reviewer verdict `CHANGES_REQUIRED`: send the plan **and** the reviewer's blocking findings verbatim back to `coder`, then review again.
- At most **3** coder/reviewer rounds. After that, stop and report the remaining findings to the user.
- If the coder or the reviewer finds the plan itself is wrong (contradicts the spec, can't work), go back to `planner` with that finding. Do not let the coder improvise a different design.

Never skip the reviewer for a feature, even if the change looks trivial once written.

## Rust rules (apply to everyone, agents included)

### Safety

- Every crate root has `#![forbid(unsafe_code)]`. No exceptions without the user's explicit approval.
- No `unwrap()` / `expect()` / `panic!` / indexing that can panic in non-test code, except on an invariant proven locally, with a comment saying why it holds. Input from disk or from a client is never an invariant.
- Errors: typed errors (`thiserror`) in library code. `anyhow` only in binaries and tests.
- Sizes and offsets are `u64`. Use checked arithmetic (`checked_add`, …) on anything derived from disk or client input. No lossy `as` casts: use `try_from`.
- Parsing (WARC, CDX, manifests, dictionaries) must reject malformed input with an error, never panic and never allocate an attacker-controlled size up front (cap it, or read incrementally).

### Concurrency

- Async runtime: `tokio`. Never block the runtime: blocking file I/O, `fsync`, and CPU-heavy work (zstd on more than a few KB, dictionary training, hashing large buffers) go to `spawn_blocking` or a dedicated thread pool.
- Never hold a `std::sync` lock across `.await`. Hold a `tokio` lock across `.await` only when that is the point (for example, serialising appends), and say so in a comment.
- Keep critical sections short: compress **outside** the lock, then lock only to append.
- Reads must not serialise on a per-file mutex: use positioned reads (`FileExt::read_at` / `pread`) on a shared file handle.
- One writer per collection (append path). Readers are lock-free with respect to writers, thanks to the append-only segments plus "data before index" ordering (spec ch. 5).
- If a function takes more than one lock, document the lock order at the type that owns them. Never take locks in a different order.
- Channels and queues are bounded. Every background task has a defined shutdown path.
- Futures that can be cancelled (e.g. inside `select!`) must not leave on-disk state half-written in a way that the spec's recovery rules don't cover.
- Shared immutable data (dictionaries, manifests snapshots) is shared with `Arc`, not cloned. Swap whole snapshots (`Arc` swap) instead of mutating in place when readers are hot.

### Resources (no wasted RAM or CPU)

- Stream, don't slurp: never read a whole segment or index into memory. Records are read with one positioned read of `raw_size` bytes.
- No needless `clone()` / `to_string()` / `to_vec()`: borrow (`&str`, `&[u8]`), use `Cow` or `Bytes` where ownership is shared.
- Pre-size buffers (`Vec::with_capacity`) when the size is known. Reuse buffers in loops.
- Prepare zstd dictionaries once (`EncoderDictionary` / `DecoderDictionary`), share them via `Arc`, and reuse compression contexts. Never reparse a dictionary per record.
- Caches (open file handles, dictionaries) are bounded and evictable.
- No polling or busy loops. No sleeping to "wait for" a condition.
- Batch syscalls and `fsync` (group commit), not one per record.
- Don't add a dependency for something a few lines of std can do. Every new dependency must be justified in the plan.

### Durability

Follow spec ch. 5: data → fsync → index → fsync → commit point. Metadata is replaced via write-temp + fsync + rename + fsync(dir), never rewritten in place.

### Style

- `cargo fmt`. Public items have doc comments stating invariants and error cases.
- Tests live next to the code (`#[cfg(test)]`). Tests that need files use temporary directories, never the working directory.

## Definition of done

All of these must pass (the reviewer runs them too):

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

New behaviour comes with tests. Concurrent code comes with at least one test that runs the operation from several tasks at once and checks the invariants afterwards.
