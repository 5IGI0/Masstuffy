# Masstuffy architecture

Crate map, dependency graph, build order and cross-crate rules. The spec in [`spec/`](../spec/) is the
source of truth for behaviour. This file decides **where** that behaviour lives. Keep it up to date when
a crate is added or its role changes.

## 1. Crate map

All crates live in `crates/<dir>/`, use the `masstuffy-` package prefix, have `#![forbid(unsafe_code)]`,
and take versions from `[workspace.dependencies]`. Everything except `masstuffy` is **synchronous**
(blocking `std::io`); tokio enters only at the top.

### Format layer (no Masstuffy dependency, no filesystem policy)

**`crates/warc` — `masstuffy-warc`**: parse and serialize WARC/1.1 records (spec 3.1).
- Zero-copy `RecordRef<'a>` (`parse` for one whole record, `parse_prefix` for the first record of a
  buffer): strict, capped by `Limits`, ordered headers kept verbatim, typed accessors `record_id()`
  (without `<>`), `warc_type()`, `date()`, `target_uri()`, `body()`, `as_bytes()` (byte-exact).
- Owned `Record` (validated serialized bytes, `view()` back to a `RecordRef`) and `RecordBuilder`
  (insertion-ordered headers; `Content-Length` is always computed and written last).
- `WarcReader<R: BufRead>`: lending sequential reader (fsck, import) with one reusable buffer, caps on
  header bytes, header count and body length; poisoned after the first error.
- `WarcDate`: strict W3C-ISO8601 parse/format, std only (no date crate).
- `Limits`: caps on header bytes, header count and body length.
- Errors: `ParseError`, `BuildError`, `ReadError` (`thiserror`).
- Feature `generate` (default on): `RecordBuilder::fill_missing` generates `WARC-Record-ID`
  (`urn:uuid` v4) and `WARC-Date` when absent; it is the only user of `uuid`. The policy of *when* to
  fill them stays in the engine.
- External deps: `thiserror`, optional `uuid`.

**`crates/urlkey` — `masstuffy-urlkey`**: massaged-URL canonicalisation and URL pattern queries (spec 1, 4.3).
- `massage(&str) -> Option<String>`: rules 1–6 of spec 1 plus IDNA and IP handling; `None` (never `""`) for
  unparseable input, schemes other than http/https, or hosts outside `[a-z0-9_-]` labels. IPv6 keys are
  `[addr]`; the query is canonicalised (sorted pairs, form-decoding).
- `UrlPattern::new(host, HostMatch, PortFilter, PathFilter) -> Result<UrlPattern, PatternError>`:
  `HostMatch::{Exact, WithSubdomains}`, `PortFilter::{Any, Default, Explicit(u16)}`,
  `PathFilter::{Any, Exact(&str), Prefix(&str)}`; every component validated and normalised. The path
  filter applies to path+query (`Exact` = exact canonical key tail, point range; `Prefix` = begins-with,
  query prefix canonicalised piece-wise, pywb `matchType` semantics).
- `UrlPattern::key_ranges() -> &[Range<String>]` (sorted, disjoint, built once in `new`) and
  `matches(&str) -> bool` (exact filter to apply to scan results). No regex.
- External deps: `url` 2.5 (`std` only; WHATWG parsing, IDNA), `thiserror`. Features: none.

**`crates/zframe` — `masstuffy-zframe`**: one-record zstd frames and zstd dictionaries (spec 3.2, 3.3).
- `inspect_frame(&[u8]) -> Result<FrameInfo { dict_id, content_size }>`: pure header parse, validates magic
  (used to check delegated compression on raw import).
- `DictHeader::parse(&[u8])`, `set_dict_id(&mut [u8], u32)`: pure, checks magic `0xEC30A437`.
- `Dictionary` (prepared `EncoderDictionary` + `DecoderDictionary`, shared via `Arc`) and
  `FrameCodec` with reusable `Compressor`/`Decompressor` contexts and an output-size cap.
- `train(samples, max_size) -> Vec<u8>`.
- External deps: `thiserror`; `zstd` behind features. Features: `codec` (default, `Dictionary`,
  `FrameCodec`), `train` (implies `codec`). With no features the crate is a dependency-free frame/dict
  inspector usable by thin clients.

**`crates/cdx` — `masstuffy-cdx`**: the versioned collection index format (replaces spec 3.4, per spec 6).
- `Entry { kind: Add | Tombstone, url, warc_type, record_id, date: WarcDate, segment: u32, offset: u64,
  raw_size: u64, digest: u64 }`; `digest` = xxh3-64 of the stored bytes.
- Text, one line per entry, header line `#masstuffy-cdx/1 gen=<gen_id>`; fields percent-escaped
  (space, `%`, control bytes), `-` = absent; each line ends with an xxh3 line checksum, so a torn or
  zero-filled tail is detected, not misparsed.
- `CdxReader<R: Read>` yields `(byte_offset, Result<Entry>)` and reports a torn tail as `Tail::Torn{at}`
  instead of failing; `CdxWriter<W: Write>` (no fsync, caller decides).
- `legacy::parse_line` and `EnvelopeReader<R>` for the raw-import envelope (CDX line + `raw_size` bytes),
  with a per-record size cap.
- External deps: `thiserror`, `xxhash-rust` (xxh3). Internal: `masstuffy-warc` (for `WarcDate` only).
  Features: `serde` (derive on `Entry`).

### Storage layer (filesystem, still WARC-agnostic where possible)

**`crates/durable` — `masstuffy-durable`**: crash-safe filesystem primitives (spec 5.3).
- All functions are blocking `std::fs` I/O (fsync can take tens of ms): async callers use `spawn_blocking`.
- `atomic_write(dir, name, bytes)`: unique `.tmp-<pid>-<seq>` file (`create_new`) + fsync + rename +
  fsync(dir); the temp file is removed on any failure before the rename. `sync_dir(path)`.
- `DirLock::try_acquire(dir)`: exclusive `flock` on `<dir>/.lock` (std `File::try_lock`, per open file
  description), never waits, released on drop; `.lock` is never unlinked.
- `trash_dir(path)`: atomic rename to `.trash-<name>-<pid>-<seq>` + fsync(parent) (commit point), then
  removal. `sweep_trash(dir)` and `sweep_tmp(dir)` remove leftover `.trash-*` / `.tmp-*` entries; they
  are for startup only, under the repository `DirLock`, before any writer runs. `.tmp-*` files are
  created next to their target, so `sweep_tmp` must run on **every** directory `atomic_write` writes into
  (repository root, each collection dir, the dictionary store), not only on `repository/`.
- Reserved names `.lock`, `.tmp-*`, `.trash-*` (`is_reserved`): directory scanners in higher crates skip them.
- Errors: `thiserror` enum `Error { Io { op, path, source }, Locked, InvalidName }` (every error names
  the failing path; `Locked` is matchable). External deps: `thiserror`. Features: none.

**`crates/seglog` — `masstuffy-seglog`**: an append-only, segmented byte log with positioned reads.
- Files `<stem>.<N>.<ext>` in one directory, `N` from 1 without gaps; the caller picks `stem`/`ext`
  (the engine uses `seg.<gen>` and `warc` / `warc.zst`). Knows nothing about WARC or zstd.
- `Locator { segment: u32, offset: u64, len: u64 }`.
- `Appender` (`&mut self`, single writer): `append_batch(&[&[u8]]) -> Vec<Locator>` writes one batch
  contiguously in one segment (I9), rolls over when `size + batch > threshold`, writes an oversize batch
  alone in a fresh segment; `sync()` fsyncs every segment touched since the last sync.
- `Reader` (`Clone`, `Send + Sync`): `read_at(&Locator, &mut Vec<u8>)` via `FileExt::read_exact_at`
  on a shared handle; bounded LRU of open files; `len_of(segment)`.
- `remove_all()` for discarding an uncommitted or retired generation.
- Internal: `masstuffy-durable`. External: `thiserror`. Features: none.

**`crates/dictstore` — `masstuffy-dictstore`**: the repository-wide dictionary store (spec 2, 3.3, 4.4).
- Scan `dict/zstd/<label>.<dict_id>.zstdict`; validate name/header agreement; duplicate id = error (I6).
- `install(label, bytes) -> dict_id`: keeps the trained id if free, else probes ids in `[32768, 2^31)`
  derived deterministically from xxh3 of the content; writes with `atomic_write`.
- `get(dict_id) -> Arc<Dictionary>`: lazy load, bounded cache; never deletes a file (I5).
- Internal: `masstuffy-zframe` (`codec`), `masstuffy-durable`. External: `thiserror`, `xxhash-rust`.

**`crates/collection` — `masstuffy-collection`**: one collection on disk: manifest, generations,
append, read, rebuild, recovery (spec 1, 2, 4.1–4.6, 5). Synchronous, no secondary index.
- `Manifest` (JSON, versioned): `uuid`, `slug`, `split_threshold`, `next_gen_id`, `active:
  Generation { id: u32, codec: None | Zstd { dict_id, level } }`. The manifest is the **single commit
  point**; it is written only through `atomic_write`.
- `Collection::create(dir, uuid, slug, codec, &dyn DictResolver)` (writes the `warcinfo` first record),
  `Collection::open(dir, &dyn DictResolver) -> (Collection, RecoveryReport)` (fsck: drop torn CDX tail,
  delete files of non-active generations, check I2 on the last entries).
- `Writer` (`&mut self`): `commit(batch: &[Prepared]) -> Vec<Entry>` = data append → data fsync →
  CDX append → CDX fsync. `prepare(Record) -> Prepared` (serialize + compress, no lock needed) and
  `prepare_raw(entry, bytes)` (frame/dict-id check) are free functions callers run in parallel.
- `Snapshot` (`Arc`, immutable: manifest + `seglog::Reader` + codec): `read(&Entry)`, `read_raw(&Entry)`,
  `entries()` (CDX iterator, tombstones applied).
- `Rebuilder`: writes generation `next_gen_id` beside the active one, then commits by manifest swap.
- Internal: `warc`, `zframe` (`codec`), `cdx`, `seglog`, `durable`. External: `thiserror`, `serde`,
  `serde_json`. Features: none. `DictResolver` is a one-method trait here, implemented by the engine on
  top of `dictstore` (keeps this crate usable with any dictionary source).

### Index layer

**`crates/index` — `masstuffy-index`**: the secondary-index contract (spec 1 "Index secondaire", 3.5, 4.3, 4.7).
- `trait SecondaryIndex: Send + Sync` (sync methods): `apply(&[Op])` atomically, where `Op` is
  `Insert{collection, gen, entry}`, `Tombstone`, `ReplaceGeneration{collection, gen}`, `DropCollection`;
  `by_id`, `by_url_nearest(url, date)` (excludes `request`), `by_pattern(&UrlPattern, limit)`;
  `checkpoint(collection) -> Option<(gen, cdx_offset)>` stored in the same transaction as the ops.
- `MemIndex`: `BTreeMap` implementation (reference + test double).
- `conformance` module (feature `conformance`): a test suite any implementation runs against itself.
- Internal: `cdx`, `urlkey`. External: `thiserror`.

**`crates/index-redb` — `masstuffy-index-redb`**: default embedded `SecondaryIndex` on `redb`.
- Tables: entries by `(collection, gen, seq)`, `record_id → keys`, `(uri, date) → key`,
  `massaged_url → key` (ordered: `UrlPattern::key_ranges` become range scans), checkpoints.
- Why redb: pure Rust, ACID with fsync on commit, ordered range scans, single file in the workdir; keeps
  the "standalone" goal (spec 6). PostgreSQL would add a server, a network round-trip per lookup and an
  async driver for what is a rebuildable cache; SQLite/LMDB pull C code. A PostgreSQL backend can still be
  written against the trait later.
- Internal: `index`, `cdx`, `urlkey`. External: `redb`, `thiserror`.

### Engine

**`crates/masstuffy` — `masstuffy`**: the repository engine; the only async crate.
- `Repository::init(dir)` / `open(dir)`: `DirLock`, config, dictstore scan, collection loading (an invalid
  collection is skipped and reported), trash sweep, secondary-index catch-up.
- `create_collection`, `delete_collection` (trash rename), `rename` (manifest only), `put(records)`,
  `put_raw(envelope stream)`, `get_by_id`, `get_by_url`, `search`, `scan`, `read_raw`.
- `train_dictionary(collection, opts)` and `rebuild(collection, target_codec)`; `rebuild_index()`.
- Per-collection writer task with a bounded queue and group commit; readers use `Arc<Snapshot>`.
- Internal: all crates above. External: `tokio` (`rt-multi-thread`, `sync`), `uuid` (v4), `tracing`,
  `thiserror`. Features: `redb` (default) selects `index-redb`.

## 2. Dependency graph

Strictly layered: an arrow means "depends on"; a crate only depends on crates in lower rows.

```
 L5  engine        masstuffy  (tokio)
                   ├──────────────┬──────────────────┬───────────────┐
                   ▼              ▼                  ▼               ▼
 L4              index-redb     collection        dictstore        (also L0/L1 directly)
                   │            │  │   │  │         │    │
                   ▼            │  │   │  │         │    │
 L3              index          │  │   │  ▼         │    │
                   │  │         │  │   │ seglog     │    │
                   │  │         │  │   │  │         │    │
 L2                │  │         │  │   ▼  ▼         ▼    │
                   │  │         │  │  durable ◄─────┘    │
 L1                │  └──► cdx ◄┘  │                     │
                   │        │      │                     │
 L0                ▼        ▼      ▼                     ▼
                 urlkey    warc   zframe ◄───────────────┘
```

Edge list (authoritative):

| Crate | Depends on |
|---|---|
| warc, urlkey, zframe, durable | — |
| cdx | warc |
| seglog | durable |
| dictstore | zframe, durable |
| collection | warc, zframe, cdx, seglog, durable |
| index | cdx, urlkey |
| index-redb | index, cdx, urlkey |
| masstuffy | collection, dictstore, index, index-redb (feature), warc, urlkey, zframe, cdx, durable |

`collection` and `index` do not know each other; only the engine connects data to the secondary index.

## 3. Milestones

Each milestone is one plan → code → review cycle and leaves the workspace green.

| # | Delivers | Test focus |
|---|---|---|
| M0 | Workspace `Cargo.toml`, `[workspace.lints]`, empty crates, this file | `cargo fmt/clippy/test` pass |
| M1 | `warc`: `RecordRef::parse`, `RecordBuilder::build`, ordered headers, `WarcDate` | byte-exact round trip; every malformed case from 3.1; caps; no panic on truncated input at every offset |
| M2 | `warc`: `WarcReader` streaming | multi-record files, garbage between records, header-size limit |
| M3 | `urlkey`: `massage` | spec example; default ports; IP v4/v6; IDNA; query sort/encoding; unparseable → `None` |
| M4 | `urlkey`: `UrlPattern`, `key_ranges`, `matches` | subdomain vs exact host, port, path prefix/exact, escaping of `,` `)` |
| M5 | `durable`: `atomic_write`, `sync_dir`, `DirLock`, trash | lock held twice fails; tmp leftovers ignored; trash sweep |
| M6 | `zframe` (no features): `inspect_frame`, `DictHeader` | real zstd frames (fixtures), bad magic, dict id 0, truncated headers |
| M7 | `zframe` `codec` + `train` | compress/decompress round trip with dict; frame dict_id matches; size cap on decompression |
| M8 | `seglog`: `Appender`, rollover, oversize, `sync` | I9 contiguity; threshold edge (`=`, `+1`); oversize batch alone; reopen continues the last segment |
| M9 | `seglog`: `Reader` (pread, LRU) | N tasks reading while one appends; eviction; reading past EOF errors |
| M10 | `cdx`: entry, writer, reader, torn tail, legacy + envelope | round trip with spaces/newlines in URLs; torn/zeroed tail at every byte; checksum mismatch; envelope size cap |
| M11 | `dictstore` | name/header mismatch, duplicate id fatal, id probing, atomic install, cache bound |
| M12 | `collection`: manifest, `create` (warcinfo), `Writer::commit`, `Snapshot::read` (codec None and Zstd) | warcinfo is entry 0; read back equals input; locators contiguous |
| M13 | `collection`: `open` + recovery | crash simulated after each commit step (truncate files) → `open` yields I2/I4; orphan generation removed |
| M14 | `collection`: tombstones, `Rebuilder` | rebuild None→Zstd, Zstd(D1)→Zstd(D1); crash before/after manifest rename; tombstoned records not copied |
| M15 | `index`: trait, `MemIndex`, conformance suite | nearest-date semantics, `request` excluded, ACTIVE only, pattern queries, checkpoint atomic with ops |
| M16 | `index-redb` | conformance suite; reopen after drop; concurrent readers during `apply` |
| M17 | `masstuffy`: init/open, registry, writer task, `put`/`get` by locator | many tasks `put` to one collection → all entries present once, no interleaving inside a batch |
| M18 | `masstuffy`: secondary-index wiring, catch-up from checkpoint, `rebuild_index` | kill between CDX fsync and index apply → catch-up; rebuilt index equals incremental one (I8) |
| M19 | `masstuffy`: `put_raw`, `read_raw`, `scan`, `search` | delegated frames with wrong dict_id rejected; scan skips orphan bytes |
| M20 | `masstuffy`: `train_dictionary`, `rebuild` orchestration | deterministic sample; reads keep working during a rebuild (concurrent test) |
| M21 | `masstuffy`: `delete_collection`, `rename` | crash mid-delete leaves `.trash-*` swept at open; slug uniqueness under concurrent creates |

M3 and M4 are delivered together (one plan → code → review cycle).

Milestones M0, M1 and M2 are delivered together by the first milestone.

## 4. Key cross-crate decisions

1. **Errors**: one `thiserror` enum per crate, no shared error crate. Higher crates wrap lower errors
   with `#[from]` and add context (path, collection uuid). The engine error exposes a coarse
   `kind()` (`NotFound`, `Conflict`, `Corrupt`, `Invalid`, `Io`) for future API layers. No `anyhow` in libs.
2. **Sync/async boundary**: all crates except `masstuffy` are sync and runtime-agnostic. The engine runs
   every blocking call (file I/O, fsync, zstd, redb) in `spawn_blocking`; tokio types never appear in
   lower public APIs. The `SecondaryIndex` trait is sync for the same reason.
3. **Single writer per collection**: the engine owns one writer task per collection holding the
   `collection::Writer` (`&mut`, no lock). Requests arrive on a bounded `mpsc`; the task drains what is
   queued and does one `commit` (one data fsync + one CDX fsync for the group), then replies via
   `oneshot`. Serialization and compression (`prepare`) happen in the caller's `spawn_blocking`, before
   queuing, so the writer only appends.
4. **Lock-free readers**: readers clone an `Arc<Snapshot>` (published under a `std::sync::RwLock<Arc<_>>`
   held only for the clone) and use `pread` on shared file handles. Appends never invalidate a snapshot;
   a rebuild publishes a new one. A read through a retired snapshot that finds its file gone returns
   `Stale` and the engine retries once with the current snapshot.
5. **Durability lives in `collection` (per-collection data) and `durable` (primitives)**: data → fsync →
   CDX → fsync → (index apply). Manifests, dictionaries and config only via `atomic_write`. The secondary
   index is never needed for correctness: its checkpoint `(gen, cdx_offset)` lets the engine replay the CDX
   tail at startup.
6. **Generations are explicit** (spec 6): `gen_id: u32` increasing, in the manifest, in file names
   (`seg.<gen>.<N>.warc[.zst]`, `index.<gen>.cdx`) and in the index. Any file of a non-active
   generation found at `open` is deleted. Rebuild to the same dictionary is just a new gen.
7. **One process per repository**: `DirLock` on `<workdir>/.lock` at `open`. Within the process, a
   `tokio::Mutex` per collection serializes admin operations (rebuild, train, delete) — it replaces the
   buffer-directory lock; training samples are held in memory under a byte cap, not written as files.
8. **Bounded resources**: open-file LRU in `seglog::Reader`, dictionary cache in `dictstore`, bounded writer
   queues, caps on header bytes, record size, envelope size and decompressed size, all configurable.
9. **Libraries do not log**: recovery and load problems are returned as data (`RecoveryReport`, skipped
   collections); only the engine logs, with `tracing`.

## 5. Decisions confirmed by the user (2026-11-13)

These override the spec where they differ.

- **Secondary index**: `redb` (embedded) is the default backend, behind the `SecondaryIndex` trait.
- **Duplicate `record_id`**: not rejected. `by_id` returns the **most recently committed** active copy
  (the spec said "first").
- **Frames without dictionary**: allowed (`dict_id = 0`) for large records (over 1 MiB) in a compressed
  collection (the legacy forbade it).
- **Duplicate unique WARC headers** (`WARC-Type`, `WARC-Record-ID`, `WARC-Date`, `Content-Length`) in parsed
  input: rejected with `DuplicateHeader`. The builder keeps "replace" semantics (the spec said "last wins").

## 6. Open questions (recommended answers)

1. **Several active generations per collection** (spec 6, Q1)? *Recommend no for v1*: exactly one active
   generation keeps I4 and recovery simple; the manifest format is versioned so a list can come later.
2. **`record_id` uniqueness** (spec 6)? *Recommend not enforced* (re-importing a WARC must not fail);
   `by_id` returns the **most recently committed** active copy, which is deterministic.
3. **Per-record compression without dictionary** (spec 6, Q4)? *Recommend allow* `dict_id = 0` frames
   for records over 1 MiB; `zframe`/`cdx` already carry the information, cost is one branch in `collection`.
4. **Pending-dictionary buffer** (spec 6, Q2)? *Recommend*: a normal uncompressed generation followed by an
   automatic rebuild, no special structure (M20 makes this a policy, not new storage).
5. **CDX rotation**: *Recommend none for v1*: one `index.<gen>.cdx` per generation, compacted by rebuild.
6. **Deduplication by digest / `revisit`**: *Recommend out of scope*; the digest field makes it possible later.
