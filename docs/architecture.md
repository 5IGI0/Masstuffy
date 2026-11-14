# Masstuffy architecture

Crate map, dependency graph, build order and cross-crate rules. The spec in [`spec/`](../spec/) is the
source of truth for behaviour. This file decides **where** that behaviour lives. Keep it up to date when
a crate is added or its role changes.

## 1. Crate map

All crates live in `crates/<dir>/`, use the `masstuffy-` package prefix, have `#![forbid(unsafe_code)]`,
and take versions from `[workspace.dependencies]`. Everything except `masstuffy` is **synchronous**
(blocking `std::io`); tokio enters only at the top.

### Format layer (no Masstuffy dependency, no filesystem policy)

**`crates/warc` — `masstuffy-warc`**: parse and serialize WARC/1.0 and WARC/1.1 records (spec 3.1, 1.0 accepted per §5).
- Zero-copy `RecordRef<'a>` (`parse` for one whole record, `parse_prefix` for the first record of a
  buffer): strict, capped by `Limits`, ordered headers kept verbatim, typed accessors `record_id()`
  (without `<>`), `warc_type()`, `date()`, `target_uri()`, `body()`, `as_bytes()` (byte-exact).
- Owned `Record` (validated serialized bytes, `view()` back to a `RecordRef`) and `RecordBuilder`
  (insertion-ordered headers; `Content-Length` is always computed and written last).
- `WarcReader<R: BufRead>`: lending sequential reader (fsck, import) with one reusable buffer, caps on
  header bytes, header count and body length; poisoned after the first error.
- `WarcDate`: strict W3C-ISO8601 parse/format, std only (no date crate); `unix_nanos() -> i128` (absolute instant used by the secondary index for nearest-date and ordering).
- `Version` (`V1_0`, `V1_1`): `version()` on `RecordRef` and `Record`, `RecordBuilder::version` (default 1.1).
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
- `inspect_frame(&[u8]) -> Result<FrameInfo { dict_id, content_size, window_size, single_segment,
  has_checksum, header_len }>`: pure header parse, validates magic (skippable and legacy frames rejected).
  `inspect_single_frame` also walks the block headers and rejects trailing bytes, so raw import can prove a
  delegated payload is exactly one frame (and check its `dict_id`).
- `DictHeader::parse(&[u8])`, `set_dict_id(&mut [u8], u32)`: pure, checks magic `0xEC30A437`;
  `is_allocatable_dict_id` (`[32768, 2^31)`).
- `DecoderDict` (prepared `DecoderDictionary`, level-independent) and `EncoderDict` (prepared
  `EncoderDictionary` for one level), each built from the dictionary bytes, validating the header id, shared
  via `Arc`, with `mem_size()`; reusable `Compressor`/`Decompressor` contexts (no dictionary held between
  calls, so one pool serves every dictionary); output-size cap. Frames carry `dict_id` (0 = none) and the
  content size, with no frame checksum.
- `SampleSet`/`TrainOptions` (bounded in-memory samples) and `train(&SampleSet) -> Vec<u8>`.
- External deps: `thiserror`; `zstd` 0.14.4 (default features off; builds the zstd C sources via `zstd-sys`)
  behind features. Features: `codec` (default: `DecoderDict`, `EncoderDict`, `Compressor`, `Decompressor`),
  `train` (implies `codec`). With no features the crate is a dependency-free frame/dict inspector usable by
  thin clients.

**`crates/cdx` — `masstuffy-cdx`**: the versioned collection index format (replaces spec 3.4, per spec 6).
- `Entry<'a> { kind: Add | Tombstone, segment: u32, offset: u64, raw_size: u64, digest: u64, date:
  WarcDate, warc_type, record_id, url: Option<_> }` (text fields `Cow<'a, str>`: borrowed when read,
  `into_owned()` to keep; `ImportMeta::into_owned` too); `digest` = xxh3-64 of the stored bytes (`cdx::digest`). No urlkey (the index
  computes it from `url`), no `dict_id` (in the frame header and the generation).
- Text, header line `#masstuffy-cdx/1 gen=<gen>`, then one line per entry:
  `kind segment offset raw_size digest date warc_type record_id url check`. Only `url` is escaped
  (`%XX` for bytes 0x00–0x20, `%`, 0x7F; `-` = absent, `%2D` = a literal `-`); `check` = low 32 bits of
  xxh3-64 of the line, so a torn or zero-filled tail is detected, not misparsed. A tombstone repeats
  the full fields of the entry it deletes (matched by `(segment, offset)`). Lines capped (256 KiB).
- `CdxReader<R: BufRead>` (lending, `next_step() -> Step::{Entry{offset, entry}, End(Tail)}`): invalid
  bytes with no valid line after them are `Tail::Torn{at}` (truncate there); a bad line followed by a
  valid one, or a checksum-valid but malformed line, is `ReadError::Corrupt`. `resume` for checkpoint
  replay. `CdxWriter<W: Write>`: one buffer, one `write_all` per batch, no fsync (caller decides).
- `legacy::parse_line` (spec 3.4, read-only; URLs with spaces recovered) and `EnvelopeReader<R>` for
  the raw-import envelope (optional `#masstuffy-cdx/1` header selects V1 lines, else legacy lines;
  each followed by `raw_size` bytes), with a per-record size cap and incremental payload reads.
- External deps: `thiserror`, `xxhash-rust` (`xxh3`). Internal: `masstuffy-warc` (for `WarcDate`
  only, default features off). Features: none.

### Storage layer (filesystem, still WARC-agnostic where possible)

**`crates/durable` — `masstuffy-durable`**: crash-safe filesystem primitives (spec 5.3).
- All functions are blocking `std::fs` I/O (fsync can take tens of ms): async callers use `spawn_blocking`.
- `atomic_write(dir, name, bytes)`: unique `.tmp-<pid>-<seq>` file (`create_new`) + fsync + rename +
  fsync(dir); the temp file is removed on any failure before the rename. `sync_dir(path)`.
- `DirLock::try_acquire(dir)`: exclusive `flock` on `<dir>/.lock` (std `File::try_lock`, per open file
  description), never waits, released on drop; `.lock` is never unlinked.
- `trash_dir(path)`: atomic rename to `.trash-<name>-<pid>-<seq>` + fsync(parent) (commit point), then
  removal. `sweep_trash(dir)` and `sweep_tmp(dir)` remove leftover `.trash-*` / `.tmp-*` entries; they
  are for startup only, under the repository `DirLock`, before any writer runs (or, for one collection
  dir, by `Collection::open` under the engine's exclusivity guarantee: no `atomic_write` into that dir in flight). `.tmp-*` files are
  created next to their target, so `sweep_tmp` must run on **every** directory `atomic_write` writes into
  (repository root, each collection dir, the dictionary store), not only on `repository/`.
- Reserved names `.lock`, `.tmp-*`, `.trash-*` (`is_reserved`): directory scanners in higher crates skip them.
- Errors: `thiserror` enum `Error { Io { op, path, source }, Locked, InvalidName }` (every error names
  the failing path; `Locked` is matchable). External deps: `thiserror`. Features: none.

**`crates/seglog` — `masstuffy-seglog`**: an append-only, segmented byte log with positioned reads.
- Files `<stem>.<N>.<ext>` in one directory, `N` from 1 without gaps; the caller picks `stem`/`ext`
  (the engine uses `seg.<gen>` and `warc` / `warc.zst`). Knows nothing about WARC or zstd.
- `Locator { segment: u32, offset: u64, len: u64 }`.
- `Appender` (`&mut self`, single writer): `append_batch<B: AsRef<[u8]>>(&[B]) -> Vec<Locator>` writes one
  batch contiguously in one segment (I9). It rolls over when `size > 0 && size + batch > threshold` (spec 02/04
  say `>=`; `>` deliberately makes the threshold an inclusive max). An empty current segment takes any batch,
  so an oversize batch is written alone without an extra empty file. Empty items are rejected (`EmptyItem`).
  Any I/O error poisons the Appender (reopen continues at the file length). `sync()` fsyncs every segment
  touched since the last sync, and the directory after a rollover.
- `Reader` (`Clone`, `Send + Sync`, `ReaderOptions { max_open_files, max_read_len }`): `read_at(&Locator,
  &mut Vec<u8>)` via `FileExt::read_exact_at` on a shared handle, `len_of(segment)`, `forget(segment)` /
  `forget_all()`. Hand-written bounded LRU of open files; its lock is never held during open or read.
- `remove_all(dir, &Naming)` is a free function for discarding an uncommitted or retired generation
  (descending order, then a directory fsync; Readers must call `forget_all`).
- Internal: `masstuffy-durable`. External: `thiserror`. Features: none.

**`crates/dictstore` — `masstuffy-dictstore`**: the repository-wide dictionary store (spec 2, 3.3, 4.4).
- `DictStore::open(dir, Options { max_file_size, cache_bytes }) -> (DictStore, ScanReport)`: runs
  `sweep_tmp`, then scans `<label>.<dict_id>.zstdict` (label `[A-Za-z0-9_-]{1,128}`, canonical decimal
  non-zero u32 id, exactly 3 parts). It reads **headers only**. Name/header mismatch, bad header,
  non-regular file, oversize file and duplicate id (I6) are fatal; non-matching names are ignored and
  reported (capped). Dictionaries are prepared lazily on first use.
- `install(label, bytes) -> Installed { dict_id, created }`: formatted dictionaries only (magic
  required), validated by zstd, size-capped. It keeps the trained id if it is allocatable and free,
  else uses linear probing in `[32768, 2^31)` from xxh3 of the content (id field zeroed) and rewrites
  the header id. It is idempotent on identical content (returns the existing id). Installs are
  serialised by an install-only mutex. It writes with `atomic_write`, never overwrites or deletes (I5),
  and the id is visible only once durable.
- `decoder(dict_id)`/`encoder(dict_id, level)`: one LRU cache of prepared dictionaries bounded by the
  sum of zstd `sizeof` (`cache_bytes`). Concurrent misses on a key prepare it once (`OnceLock` slot).
  An entry larger than the budget is returned uncached. Locks are never held during I/O or
  preparation. Lock order: `install` -> `files`; `cache` is standalone.
- Spec 4.4 split: id allocation and the write are in `dictstore`; sampling/training (in-memory
  `SampleSet`), label generation (`<slug>_<ts>` sanitised with `is_valid_label`) and rebuild are in the
  engine (M20).
- `DictResolver` stays in `collection` and is implemented by the engine on a newtype over
  `Arc<DictStore>` (orphan rule; `collection` does not depend on `dictstore`).
- Internal: `masstuffy-zframe` (`codec`), `masstuffy-durable`. External: `thiserror`, `xxhash-rust`.
  Features: none.

**`crates/collection` — `masstuffy-collection`**: one collection on disk: manifest, generations,
append, read, rebuild, recovery (spec 1, 2, 4.1–4.6, 5). Synchronous, no secondary index.
- `Manifest` (JSON, schema `version` 1, `deny_unknown_fields`, 64 KiB cap, `CollectionId` canonical lowercase
  hyphenated): `uuid`, `slug`, `split_threshold`, `next_gen_id`, `active: Generation { id: u32, codec:
  None | Zstd { dict_id, level } }`. The manifest is the **single commit point**; it is written only through
  `atomic_write`. Any format extension bumps `version`.
- Files: `manifest.json`, `seg.<gen>.<N>.warc[.zst]`, `index.<gen>.cdx` (one CDX per generation),
  `scan.<gen>.json` (derived checkpoint: line-aligned fsynced CDX offset, entry count, per-segment max end;
  written with `atomic_write`; deleting it is always safe).
- `Collection::create(dir, CreateSpec { uuid, slug, codec, split_threshold }, &Options, &dyn DictResolver)
  -> (Collection, warcinfo Entry)` creates `dir` itself and writes the `warcinfo` first record (entry 0,
  holding the manifest); `Collection::open(dir, &Options, &RecoveryOptions { repair: RepairPolicy::
  {Refuse (default), TruncateAtCorruption}, verify_last (default 8), full_scan }, &dyn DictResolver) ->
  (Collection, RecoveryReport)`: no lock (the caller guarantees exclusive access: repository `DirLock` +
  per-collection admin mutex). Phase A (read-only, plus a dir fsync making the manifest durable): manifest,
  dictionaries, directory classification, `Appender::open` (gap/missing segment = error), CDX header always
  checked (I4); CDX scanned from the checkpoint when it is valid (generation, offset <= CDX length and
  line-aligned, segments present and not shorter than recorded ends: stat only), else in full (reason
  reported); `RecoveryOptions::full_scan` forces it. Cheap I2 on every scanned line (segment exists, end <=
  length; the prefix is covered by the checkpoint's segment ends), deep I2 (pread + digest) on the last
  `verify_last` scanned Add entries; bit rot in the prefix is caught on read or by a full scan. Phase B:
  truncate the torn tail (or, with repair, at the first corrupt/I2-violating line) + fsync; fsync the CDX
  whenever lines were scanned (they may come from a commit that died before its CDX fsync, and a later
  checkpoint must only cover durable lines); remove a
  checkpoint made stale by a repair cut, or rewrite it after a full scan or >= 1 MiB of newly scanned CDX;
  `sweep_tmp`; unlink every `seg.<g>.*`/`index.<g>.cdx`/`scan.<g>.json` with `g != active` + dir fsync.
  Unknown files are kept and reported. A failed open changes nothing. `RecoveryReport { generation,
  segments, entries, cdx_len, cdx_torn, repair, verified, tmp_removed, removed, unknown, unknown_count,
  scan: ScanMode::{Partial{from}, Full{reason}}, checkpoint_written }`. `Collection::cdx_path()` for CDX
  re-reads (`CdxReader::resume`). `Writer::checkpoint()` / `Collection::checkpoint()` (engine: on clean
  shutdown and periodically from the writer task; never inside `commit`).
  Engine duties: `Repository::open` runs `Collection::open(.., &RecoveryOptions::default(), ..)` per
  collection dir; `ManifestMissing` -> `trash_dir` (aborted create); any other error -> collection skipped
  and reported, startup continues; repair is an explicit operator action; after a poisoned `Writer`, the
  writer task drops it and runs `Collection::open` again (no admin mutex, see Engine) and re-commits the unacknowledged batch once.
  `Snapshot::cdx_lines(from, end) -> CdxLines` (M18) streams every line (`A` and `T`, no filtering) of a fsynced CDX
  range as `CdxLine { at, next, entry }` (`next_line()`, `position()`): the engine's indexer tails the CDX with it. M18
  resumes it at the index checkpoint and re-indexes a collection when the checkpoint is beyond the CDX length the writer
  task started with (the floor rule: it holds even if new puts grew the CDX past the stale checkpoint), is not at a line
  start, or the generation or index state does not match. An offline repair tool must delete `data/index.redb`: a repair
  followed by offline appends past the old checkpoint cannot be detected.
  `Snapshot::entries_from(from, end)` (M19) is `entries(end)` resuming at a line-start cursor: the tombstones of the whole
  prefix are applied, entries are yielded from `from`; `Entries::position()` is the resume point (used by the engine
  scan). `GenContext::max_stored_bytes()` exposes the stored-size cap.
  `Options { max_record_bytes, max_open_files }`.
- `Preparer` (one per worker: `Compressor` + `Arc<GenContext>`), run outside any lock:
  `prepare(Record) -> Prepared` (serialize + compress, records over 1 MiB without dictionary) and
  `prepare_raw(&ImportMeta, Vec<u8>, RawCheck) -> Prepared` (single frame, dict-id rule, declared-size cap,
  digest; `Full` also decompresses, parses and compares the envelope metadata). These replace the earlier
  "free functions". With `HeaderOnly`, records whose envelope metadata disagrees with the payload, or whose
  payload is not a WARC, can only be read with `read_raw`; `read` returns `EntryMismatch`/`Record`
  (handled: `get`/`get_by_*` return `NotDecodable`, scan reports it per item).
- `Writer` (`&mut self`): `commit(batch: &[Prepared]) -> Vec<Entry<'static>>` = data append → data fsync →
  CDX append → CDX fsync. `WrongGeneration` for an item prepared for another collection or generation; any
  I/O error after the append starts poisons the writer.
- `Snapshot` (`Arc`, immutable: manifest + `seglog::Reader` + `GenContext`): `read(&Entry, &mut ReadCtx,
  &mut Vec<u8>) -> RecordRef` (one pread, digest verified on every read, zstd decode, strict parse),
  `read_raw -> RawEncoding`. `Snapshot::entries(end)` / `Collection::entries()`: lending iterator over the live
  `Add` entries of the CDX prefix `[header, end)` (`end` = `Writer::cdx_len()`, the fsynced length), tombstones
  applied by a two-pass scan (16 B of RAM per distinct tombstone), CDX order = locator order.
- Tombstones: `Snapshot::prepare_tombstone(&Entry) -> PreparedTombstone` (kind Add, valid, line fits, not the
  generation's warcinfo at (1,0), within the segment's length: no data read, no index lookup) then
  `Writer::tombstone(&[PreparedTombstone])` = check that each target is covered by a committed (fsynced) line, so a
  `T` line never points into orphan bytes a crash could drop, then CDX append + fsync (same poisoning and `WrongGeneration` contract
  as `commit`); duplicates harmless. No physical zeroing (I1 holds without exception).
- `set_slug(&Snapshot, slug) -> Snapshot` (M21): manifest slug rewrite via `atomic_write` (commit point), same
  generation, reader and context shared (no file reopened); `Collection::set_slug` offline. The warcinfo records are
  not touched (the next rebuild writes one with the new slug).
- `Rebuilder` (M14): `Rebuilder::start(Arc<Snapshot>, Codec, &Options, RebuildOptions { recompress,
  on_unreadable: Fail|Skip, workers, max_inflight_bytes }, &dyn DictResolver)` (deletes non-active generation
  leftovers, creates generation `next_gen_id`, writes its warcinfo with the target manifest),
  `copy_until(end)` (concurrent with the old writer; raw copy when same dictionary unless `recompress`, else
  decode + re-encode; tombstoned records dropped; `T` lines after the first range are an error),
  `finish(&mut Writer) -> Rebuilt { writer, snapshot, report, old }` (final delta, fsync, checkpoint, manifest
  `atomic_write` = commit, old writer retired), `OldGeneration::remove()`, `abort()`; `Collection::rebuild` is
  the offline convenience. Every generation's first record is its own warcinfo; older warcinfos are copied as
  ordinary records. `Writer::cdx_len()`. M20 additions: `Rebuilder::set_cancel(Arc<AtomicBool>)` (the copy stops
  at the next CDX line with `Error::RebuildCancelled`) and `Rebuilder::cdx_start()` (offset of the new CDX's
  warcinfo line: the index's staged resume point); `Preparer::reprepare(Prepared)` (decode with the record's codec,
  re-encode for this generation; `PrepareError::ForeignContext`); `Snapshot::read_plain` is public (decode
  without WARC parse: training, rebuild); `Writer::remove_stale_generations()` (cleanup after a failed,
  uncommitted `finish`).
- `DictResolver` has **two** methods (`decoder`, `encoder`), implemented by the engine on top of `dictstore`
  (keeps this crate usable with any dictionary source).
- Internal: `warc` (feature `generate`), `zframe` (`codec`), `cdx`, `seglog`, `durable`. External: `thiserror`,
  `serde`, `serde_json` (+ `uuid` via `warc`). Features: none.

### Index layer

**`crates/index` — `masstuffy-index`**: the secondary-index contract (spec 1 "Index secondaire", 3.5, 4.3, 4.7, I8).
- `trait SecondaryIndex: Send + Sync`, object-safe, sync (engine: `spawn_blocking`), linearizable with
  snapshot isolation (a query never sees part of a batch). `CollectionKey(u128)` (the collection uuid
  value; `index` does not depend on `collection`), generations are `u32`, `Position { segment, offset }`.
- `apply(&[Op])`: one atomic, ordered batch of `Insert{collection, generation, entry}` (identity =
  (collection, generation, position); duplicate identity = no-op), `Tombstone{collection, generation,
  position}`, `DiscardStaged{collection, generation}` (clears an abandoned staged generation; `NotStaged` if not staged), `ReplaceGeneration{collection, generation}` (O(1) switch; older generations retired, newer stay
  staged and invisible; lower than active = `StaleGeneration`), `DropCollection` (O(1); a recreated
  collection never sees old rows), `Checkpoint{collection, (generation, cdx_offset)}` (an op, so it commits
  with the others; `cdx_offset` = fsynced byte offset after the last applied CDX line); two checkpoint slots (active, staged): a checkpoint of a generation above the active one goes to the staged slot, and `ReplaceGeneration` carries it over.
- Visible (ACTIVE) = generation == active generation and not tombstoned/dropped. Retired/dropped rows are
  reclaimed by `purge(max_entries)` in bounded steps (engine background loop), `stored_entries()` for checks.
- Queries return owned `Hit { collection, generation, entry: cdx::Entry<'static> }`:
  `by_id(Option<collection>, record_id)` (most recently committed copy = greatest position in a collection;
  across collections latest WARC-Date then collection key); `by_url_nearest(collection, url, Option<date>)`
  (URL key = massaged key for http(s), else raw URL exact; `request` excluded; min |Δt|, ties -> later date
  then later position; no date -> latest); `by_pattern(collection, &UrlPattern, PatternQuery{limit ≤ 10 000,
  after: Cursor})` -> `Page{hits, next}` in (massaged key, date, position) order, all types, exact `next`,
  text cursor tokens; `list(collection, after, limit)` in position order (I8 dumps); `state`, `collections`.
  Shared semantics in `index::rank` (both backends use it). `rank` also holds the op rules every
  backend applies: `validate_insert`, `check_op` (StaleGeneration/NotStaged), `step_state` (state transition
  incl. checkpoint routing), and `Op::collection()`.
- `MemIndex`: `RwLock<State>` over `BTreeMap`s, eager deletes, linear queries (reference + test double).
- `conformance` module (feature `conformance`, also under `cfg(test)`) + `conformance_tests!(factory)` macro:
  the suite every implementation runs against itself (incl. drop/recreate, staging, pagination, I8
  rebuilt == incremental, concurrent readers).
- Internal: `warc` (`WarcDate`), `cdx`, `urlkey`. External: `thiserror`. Features: `conformance`.

**`crates/index-redb` — `masstuffy-index-redb`**: default embedded `SecondaryIndex` on `redb`.
- `RedbIndex::open(path, &Options)` (create-or-open, schema check), `flush`, `compact`, `close`;
  `Options { cache_bytes = 64 MiB, sync_every = 1, quick_repair = true, verify_on_open = false }`;
  `OpenError::{AlreadyOpen, SchemaMismatch, Corrupt, Unsupported, Io, Backend}` with `needs_rebuild()`.
- Tables (decided in M16), big-endian, sign-flipped 16-byte dates, pattern bounds via `enc_bound`:
  - `meta` (schema_version, next_slot, stored_entries);
  - `collections: u128 → (active gen, checkpoint, staged_checkpoint, live (gen, slot) list)`;
  - `slots: slot → (collection, gen)`;
  - `dead_slots`;
  - `entries: slot‖pos → entry`;
  - `by_id: record_id‖0‖slot‖pos → date`;
  - `urls: slot‖(0‖massaged‖0 | 1‖len‖raw)‖date‖pos → flags(request)`.
- Slots: each (collection incarnation, generation, staging attempt) gets a fresh `slot: u64` from a global
  counter (new slot on first use after `DropCollection` or `DiscardStaged`). Every row key starts with the
  slot. Visible = slot is the collection's active slot, so pattern, nearest and list scans touch only active
  rows (O(log n + page) even during a rebuild). Dead slots (retired, dropped, discarded) go to `dead_slots`
  that `purge` deletes in bounded transactions. `DropCollection`, `DiscardStaged` and `ReplaceGeneration`
  stay O(1), and old rows can never come back. The massaged key is not stored: it is recomputed on
  `Tombstone` and `purge` (rows of the purged entries), so a change of `urlkey::massage` output bumps
  `SCHEMA_VERSION`.
- Durability: every commit fsynced by default; `sync_every > 1` makes intermediate commits non-durable
  (crash = consistent older prefix, replayed by catch-up); batches with ReplaceGeneration/DiscardStaged/
  DropCollection are always durable so the engine may delete files after they return.
- Purge: `purge(n)`: oldest dead slot first, <= n entries (+ their by_id/url rows) per write txn.
- Write gate: a `Mutex<()>` serialises `apply`/`purge`/`flush` from `begin_write` until after the `unsynced`
  counter update (redb releases its writer lock inside `commit`). Lock order: gate, then redb's writer lock;
  never held across anything else. `by_id` returns `None` for a query id containing NUL.
- Locking: redb ExclusiveWriter whole-file lock on the index file (second guard behind DirLock); reopen
  requires dropping the previous handle.
- Why redb: pure Rust, ACID with fsync on commit, ordered range scans, single file in the workdir; keeps
  the "standalone" goal (spec 6). PostgreSQL would add a server, a network round-trip per lookup and an
  async driver for what is a rebuildable cache; SQLite/LMDB pull C code. A PostgreSQL backend can still be
  written against the trait later.
- Internal: `index`, `cdx` (`WarcDate`/`UrlPattern` via `index` re-exports). External: `redb` 4.6 (`std`
  only; its only dependency is `libc` on Linux/macOS), `thiserror`.

### Engine

**`crates/masstuffy` — `masstuffy`**: the repository engine; the only async crate.
- Repository layout: `.lock`, `config.json`, `data/{buffer,dict/zstd,repository/<uuid>}` (collection directories
  are named after the uuid; the slug lives in the manifest only).
- `config.json` (schema `version` 1, strict: unknown fields rejected, missing fields take their default, 64 KiB cap,
  every range validated): `max_record_bytes`, `max_open_files`, `dict_cache_bytes`, `dict_max_file_size`,
  `default_split_threshold`, `writer_queue` (64), `max_group_requests` (128), `max_group_bytes` (64 MiB),
  `max_inflight_put_bytes` (256 MiB), `preparers_per_collection` (4), `max_concurrent_prepares` (null = cores),
  `max_concurrent_reads` (64), `open_concurrency` (4), `checkpoint_every_bytes` (64 MiB), and (M18)
  `index_sync_every` (1, `1..=65536`), `index_cache_bytes` (64 MiB, `1 MiB..=1 TiB`), `index_batch_ops` (8192,
  `64..=65536`), (M20) `rebuild_workers` (null = `RebuildOptions::suggested_workers()`, `Some(1..=256)`),
  `rebuild_inflight_bytes` (256 MiB, `1 MiB..=1 TiB`), `max_concurrent_rebuilds` (1, `1..=64`), `max_train_bytes`
  (512 MiB, `1 MiB..=1 TiB`). `init` writes all fields. The schema stays version 1: an older binary rejects a config written by
  this one (`deny_unknown_fields`), acceptable before 1.0.
- `Repository::init(dir)`: the directory must be empty, or hold only the leftovers of an interrupted init (`.lock`,
  `.tmp-*`, the empty layout directories); dirs are created and fsynced, then `config.json` by `atomic_write` is
  the commit point; then the open sequence runs with the lock already held.
- `Repository::open(dir)`: `DirLock`; config; layout check; `sweep_tmp` (root) and `sweep_trash`
  (`data/repository`); `DictStore::open` (fatal on error); scan of `data/repository` (non-uuid or non-directory
  entries are kept and reported as unknown); parallel bounded `Collection::open`. `ManifestMissing` in a
  uuid-named directory -> trashed (aborted create); any other error, a manifest uuid that differs from the
  directory name, or a duplicate slug (all duplicates are skipped) -> collection skipped and reported. The slugs of
  skipped collections are reserved so a new collection cannot take them.
- `create_collection(slug, codec)`: slug `[a-z0-9][a-z0-9_-]{0,127}` for new collections, exact (case-sensitive)
  lookup, slugs of loaded manifests accepted as they are; one global create lock.
- `Located { collection, generation, entry }` names a committed record. `put(records)` and
  `put_raw(Vec<RawItem>)` (M17) return one `Located` per record, contiguous (I9); `get(&Located)` returns the
  serialized WARC bytes (`NotDecodable` for a `HeaderOnly` raw import that disagrees with its payload: use
  `get_raw`); `get_raw` returns the stored bytes and their encoding. A locator of another generation is
  `StaleLocator`. M19 adds `CollectionHandle::put_raw_envelope`, `scan`, `search`, `find_by_url`/`get_by_url` and
  `Repository::find_by_id`/`get_by_id` (see "Queries and bulk import (M19)" below).
- **Admin operations (M21)**, `admin.rs`: `Repository::delete_collection(impl Into<CollectionRef>) ->
  DeletedCollection`, `rename_collection(which, new_slug) -> CollectionHandle`, `CollectionHandle::delete(Vec<Located>)`
  and `delete_by_id`. `CollectionRef` is `Slug(&str)` or `Id(CollectionId)`. Each runs its mutation in a spawned unit
  and none takes `Core::gate`: each takes the collection's `admin` mutex and then checks `is_closing` (shutdown holds
  every `admin` it took until it ends, so a flow that got `admin` first finishes with the writer task alive).
  - delete_collection: (1) `Shared::admission` write guard only to set `Shared::deleted` (puts hold `admission` read
    from admission until queued, so every earlier put has queued its request; later ones and every other handle
    operation get `UnknownCollection`); (2) set the rebuild cancel flag (after `deleted`: `start_job` resets it, then
    reads `deleted`); (3) take `admin`; (4) stop the writer task (`Msg::Shutdown`: commits what was queued, final
    checkpoint); (5) `durable::trash_dir` = commit point (rename to `.trash-*`, parent fsync, removal); (6) registry:
    id and slug removed (the slug stays reserved if the parent fsync failed); (7) `IndexCmd::Forget`. Crash table:
    before the rename = loaded normally; rename durable, removal partial = `sweep_trash` at open and the index rows
    dropped by the orphan GC of the first pass; removal done before `Forget` = orphan GC; after `Forget` = nothing.
    A failed (not committed) trash rename leaves the collection registered but closed (Q24): call again or reopen.
  - rename_collection: guards `admin` then `create_lock` taken in the caller (cancellable, nothing mutated), then a unit
    sends `Msg::SetSlug` to the writer task, which runs `collection::set_slug` (manifest `atomic_write` = commit
    point; the writer task is the only place that reopens, and `Collection::open` sweeps tmp files, so the two must not
    overlap) and publishes the new snapshot (same generation: the retired one is kept; the published manifest must
    equal the disk, or the next rebuild fails with `StaleSource`). The registry and `Shared::slug` follow after; an
    ambiguous failure reserves the slug a crash could bring back (`SlugApplied`). The index is keyed by uuid and not
    touched. I7 holds because `create_lock` covers creates and renames.
  - Record deletion: `delete` takes `admin` (owned) in the caller, then the unit runs `prepare_tombstone` (blocking
    pool) for every distinct target, queues `Msg::Tombstone`, and holds `admin` until the writer replied (one CDX
    append + fsync per call), so no rebuild can start between the queuing and the fsync. A target of another
    generation than the current one is `StaleLocator`. The indexer follows through the CDX tail (`T` lines).
  - Errors: `Tombstone { index, source }` (kind `Invalid`); `Commit` with `TombstoneNotCommitted` is also `Invalid`.
- `Repository::sync_index()` and `rebuild_index()` exist since M18 (see the secondary-index paragraph below).
- `train_dictionary(TrainOptions) -> TrainReport` (M20, `train.rs`): bottom-k deterministic sample of the live
  records (key `xxh3(uuid || segment || offset)`, warcinfo and stored size > 1 MiB excluded, heap of
  `max_samples` entries), read in locator order with `Snapshot::read_plain`, `zframe::train`, then
  `DictStore::install` with the label `<slug>_<YYYYmmddHHMMSS>` (sanitised). Same CDX prefix + options = same
  dictionary and id; `install` is atomic and idempotent on content. Read-only for the collection: no admin mutex
  (it delays neither shutdown nor a rebuild; a rebuild removing the sampled generation gives `StaleLocator`), one
  training at a time per repository (`train_sem`). `data/buffer/` stays in the layout, unused ([LEGACY] deviation
  from spec 4.4 steps 1/3/6 and 5.2: the directory lock replaces the cross-process lock, samples are in memory under
  `max_total_bytes`). No combined "train then rebuild" option: callers compose the two calls.
- Rebuild protocol (M20, `rebuild.rs`): `CollectionHandle::rebuild(Codec, RebuildOptions) -> RebuildReport` registers
  under the gate read guard (`RebuildSlot::active`, one per collection: `RebuildInProgress`), then runs as a spawned
  unit: `rebuild_sem` permit, the collection's admin mutex (held to the end; the writer task never takes it),
  **start** job (`Rebuilder::start` with `RebuildSlot::rb` locked; the same mutex is held by the writer task during
  every reopen, so `Collection::open` can never delete the files of a live `Rebuilder`), `IndexCmd::Stage`
  (`DiscardStaged` + staged `Checkpoint{new gen, Rebuilder::cdx_start()}`, best effort), copy rounds
  (`copy_until(published cdx_len)` until the delta < 1 MiB or 8 rounds), `Msg::Finish` (the writer task stops
  draining, takes the `Rebuilder`, runs `finish`: final delta, fsyncs, checkpoint, **manifest swap = commit point**,
  then `Published::switch(snapshot, cdx_len)` and wakes the indexer), `IndexCmd::AwaitActive` (the indexer resumes
  at the staged checkpoint after the switch, indexes the new CDX as staged rows and `ReplaceGeneration`s in the
  last batch), `Published::clear_retired`, `OldGeneration::remove`. Failure before the commit: abort the
  `Rebuilder` (files deleted) + `IndexCmd::Unstage`, the old generation stays active; a `finish` failure that
  poisoned the writer is resolved by a reopen (`Error::Rebuild { committed }`). A writer reopen during the rebuild
  sets the slot's cancel flag, drops the `Rebuilder` and the unit ends with `RebuildAborted`. Requests prepared for
  the old generation that reach the writer after the switch get `Rejected::Stale` (one generation comparison per
  item) and their caller prepares them again with `Preparer::reprepare` (at most 3 times, then `StaleGeneration`).
  Crash table: before the swap, `open` deletes the `N+1` files and the first index resolution discards the staged
  rows; after the swap, `open` deletes generation `N` and the indexer resumes at the staged checkpoint (or
  re-indexes if `Stage` never applied); crash before `old.remove`: `open` deletes the old files.
- Retired snapshot (M20): `Published` holds `{ current, retired }` under one `RwLock`; `retired` is the snapshot of
  the generation the last committed rebuild replaced, kept until the index has switched, so `get`/`get_raw`/`get_by_*`
  of old-generation locators keep working through the switch window (then `StaleLocator`; a retired read whose
  files were removed also gives `StaleLocator`). `scan` pages still give `StaleLocator` after a switch. If the index wait
  after a rebuild fails (`RebuildReport::index_switched == false`: index failure, shutdown), the old files are removed
  anyway and index hits of the old generation return `StaleLocator` until the index catches up.
- Shutdown is the cancel for rebuild and training: it sends `closing` (a `watch`) and sets every slot's cancel flag
  before taking any admin mutex; the units check them at each phase and race their waits (index replies, test
  pauses) with the signal. After `Msg::Finish` was sent the final step is bounded (what 8 rounds left) and the
  cancel flag may still fail it (its final `copy_until` checks it): nothing is committed, the writer task removes the
  new files, the index is unstaged and the unit returns `ShuttingDown`. `Stage` is skipped (`Ok(false)`) while a
  staged checkpoint of another generation is pending (an earlier rebuild whose index switch failed): that one is kept
  and the new generation re-indexes after its own switch. There is no user cancel API.
  Index staging contract: `IndexCmd::{Stage, Unstage, AwaitActive}` are described in `indexer.rs`.
- Per-collection writer task with a bounded queue and group commit; readers use `Arc<Snapshot>`. The writer
  task is a tokio task that runs each commit in one `spawn_blocking` (not a dedicated thread: a commit is
  two fsyncs, and a thread per collection would idle). Callers prepare in the blocking pool, then queue a
  request; the task drains the queued requests without waiting (at most `max_group_requests` and
  `min(max_group_bytes, split_threshold)` stored bytes, a single larger request goes alone) into one
  `Writer::commit`, then answers each request. A non-poisoning commit error (nothing was written) falls back to
  committing each request alone; a poisoned writer is reopened **by the writer task itself** (no admin mutex: the
  rebuild holds it while waiting for the writer task, so waiting for it here would deadlock; the reopen cancels and
  drops an in-progress `Rebuilder` under the slot mutex first) and the group is retried once; a lost writer is reopened at the next group. The
  task publishes counters and the fsynced CDX length (`Published::cdx_len`, read together with the snapshot by
  `Published::tail`), then wakes the indexer if the length changed. `on_committed` is only a debug log: the index
  tails the CDX.
- Admission: a repository-wide byte budget (`max_inflight_put_bytes`, from admission until the writer answered),
  a prepare semaphore (CPU) and a read semaphore bound the work in flight; a request larger than the budget runs
  alone.
- Shutdown: puts and creates hold a read guard of a gate from admission until their request is queued;
  `shutdown` flips the gate under the write guard (new puts get `ShuttingDown`), sends a `Shutdown` message
  behind the queued requests to every writer task (each commits what is queued, writes a final checkpoint and
  exits), joins them, then releases the `DirLock`. Every writer task also holds an `Arc<DirLock>`, so the
  repository stays locked until the last task exited. Dropping everything without `shutdown` drains the same
  way (channels close).
- Cancellation rule (M18, M20, M21 must follow it): **a flow that mutates the repository must survive
  cancellation of its caller, and every blocking job that mutates repository files owns an `Arc<DirLock>`
  clone** (so the directory stays locked while an orphaned job runs). `create_collection` therefore runs
  create + writer spawn + registration as one spawned unit holding owned gate and create-lock guards; `open`'s
  scan and per-collection jobs and `create`'s blocking job carry a lock clone; `shutdown` runs in a spawned task
  (a second or later call waits for it, the error goes to the first caller that observes completion). M21:
  `delete_collection` runs entirely in a spawned unit; `rename_collection` and record `delete` take their guards in
  the caller (cancellable waits, nothing mutated) and run the mutation in a spawned unit owning them.
- tokio features: `rt` and `sync` in the library; `rt-multi-thread`, `macros` and `time` in dev-dependencies
  only (the embedding application picks the runtime).
- Reads `MASSTUFFY_RAW_IMPORT_CHECK` once at `Repository::open` (→ `RawCheck`) and passes it to every
  `prepare_raw` of `put_raw` and `put_raw_envelope`.
- Internal: all crates above. External: `tokio` (`rt`, `sync`), `uuid` (v4), `tracing`,
  `thiserror`. Features: `redb` (default) selects `index-redb`.
- **Queries and bulk import (M19)**, `import.rs` / `scan.rs` / `query.rs` / `read.rs`:
  - (a) Import: `put_raw_envelope(R: Read + Send + 'static, ImportOptions)` reads on one blocking producer thread
    (async sources go through `SyncIoBridge`), a capacity-1 channel, batches of at most `batch_records` records and
    `min(max_group_bytes, split_threshold)` bytes, each batch an ordinary writer request (`put_stage`, shared with
    `put`), one batch in flight so the committed set is a stream prefix. `OnInvalid::{Abort (default), Skip}`; framing
    errors are always fatal. `ImportReport.committed_offset` is resumable (V1: resend the header line). Cancellation
    is at-least-once. The gate is held per batch only; the producer holds no lock or permit, so shutdown is never
    delayed. Peak memory is about three batches.
  - (b) Scan: `CollectionHandle::scan` is CDX-driven (spec 4.3: orphan or invalidated zones are skipped), pull pages
    (`Scan::next_page`), one blocking job and one read permit per page, `ScanCursor` = (generation, CDX line offset),
    a dropped page is replayed, strongly consistent with the CDX prefix captured at (re)open, a generation change
    gives `StaleLocator`. `Fetch::{None, Decoded, Raw}`; a record that cannot be read is an error on its item.
  - (c) `search`, `find_by_*`, `get_by_*` are index-backed and eventually consistent (Q14, `sync_index`).
    `UrlPattern` is re-exported as the query type; limits `1..=10 000` (default 100); index-cursor tokens; a hit of a
    non-active generation gives `StaleLocator` (Q16: the generation a rebuild just replaced stays readable until the
    index has switched); a hit in a collection that is not loaded gives
    `UnknownCollection` on `get_by_id`; `HeaderOnly` mismatches give `NotDecodable` (raw path: `find_*` + `get_raw`).
    `get_by_*` hold one read permit across the lookup and the read.
  - (d) Errors: `InvalidQuery`, `IndexQuery`, `Envelope`, `Import { report, source }`.
- **Secondary index (M18)**, `indexer.rs` / `index.rs`:
  - Backend (D1): cargo feature `redb` (default) opens `RedbIndex` at `<workdir>/data/index.redb` in `spawn_blocking`
    with a `DirLock` clone (options from `index_cache_bytes` / `index_sync_every`); without the feature a `MemIndex`
    rebuilt at every open. `OpenError::needs_rebuild()` -> unlink, fsync `data/`, recreate empty (every collection is
    re-indexed; `OpenReport::index_recreated`); any other error, `AlreadyOpen` included -> `Error::IndexOpen` and the
    open fails (the directory lock is released). The index is never needed for correctness.
  - One **indexer task** per repository **tails the fsynced CDX** (D2) instead of receiving ops: a writer task only
    publishes "the CDX of this collection is durable up to N" and wakes the indexer (`IndexLink::wake`: under the
    `Hub` mutex, test-and-set of the collection in the `queued` set plus push on the queue, then a `try_send` of
    `IndexCmd::Wake`, never blocking; the clear happens on pop under the same mutex, a leaf `std` lock with no I/O and
    no `.await` inside). Ordering rule: the writer stores `cdx_len` before it takes the hub mutex, the indexer reads
    `Published::tail` after popping under it, so no fence is needed.
    Wake invariant: whenever the hub is non-empty an `IndexCmd` is in the channel or the indexer is running and looks
    at the hub before its next `recv`. A full channel or a
    dropped wake never loses work, memory is bounded by the number of collections plus one batch. One pass reads at most
    `index_batch_ops` lines (shared by the collections of the pass) and applies **one** `apply` with a
    `Checkpoint{generation, cdx_offset}` op per collection in the same atomic batch. `T` lines become `Op::Tombstone`.
    If a multi-collection apply fails, each job is applied alone (one bad collection does not block the others).
  - `put` replies after the CDX fsync and **never waits for the index** (D3); queries are eventually consistent (D4,
    Q14). `Repository::sync_index()` is a barrier: it returns once the index covers every record acknowledged before
    the call and every collection is active (`IndexLagging` per the retry rule below, `IndexUnavailable`
    if the indexer is gone).
  - Barrier retry rule: each track has a `failed` flag (last attempt failed, no success since; reset on a successful
    apply). A barrier or re-index requeues a failed track and marks that target `retried`; a barrier target not yet
    retried is retried once on its first failure; a second failure fails the barrier with `IndexLagging` listing the
    collection. A collection that failed earlier but succeeded since gets its retry again.
  - Resolution (catch-up at open, after a failed apply, after a repair: one mechanism). From the index state of the
    collection: unknown -> re-index; forced -> re-index; active generation above the snapshot's -> re-index; checkpoint
    of another generation or missing -> re-index; checkpoint beyond the CDX length (or beyond the length at writer
    start, first resolution only) -> re-index; otherwise resume at the checkpoint (`ReplaceGeneration` when caught up if
    the generation is not active yet, which also completes a generation switch interrupted by a crash). A stale staged
    generation is discarded (`DiscardStaged`) at the first resolution after open. A resume point that is not a line
    start falls back to re-index.
  - Re-index (D6): `DropCollection`, then `Insert`/`Tombstone` for every line from the CDX header with no active
    generation (rows invisible), chunk by chunk with checkpoints, then `ReplaceGeneration` in the batch of the last
    chunk: readers see the collection empty meanwhile, never half built. A crash resumes at the checkpoint (no active
    generation + checkpoint of the target generation) and then activates.
  - `Repository::rebuild_index()` (D7) forces a re-index of every loaded collection through the indexer while puts
    continue (the tail picks up new lines), drops orphan index collections (neither loaded nor skipped at open: covers a
    crash between deleting a collection directory and its `DropCollection`), and is exclusive with itself
    (`index_rebuild` mutex) and with admin operations (every admin mutex, ascending id). It runs as a spawned unit.
  - Purge (D8) runs inside the indexer as lowest-priority work after any applied structural batch (`purge(4096)` in
    `spawn_blocking` while no command or dirty collection waits, until it returns 0). No separate task or timer.
  - A failed apply marks the collection failed and drops its in-memory cursor; it is retried from the durable
    checkpoint at the next commit of that collection or at the next `sync_index` / `rebuild_index`. No timer. An
    indexer panic is not restarted: `sync_index` returns `IndexUnavailable` and the next open catches up.
  - Shutdown order (D10): gate flip, writers drain and exit (joined), `IndexCmd::Shutdown` (the indexer drains every
    dirty collection fully, skips purge, `flush()`es the index, acknowledges), indexer joined, index handle dropped,
    `closed`, `DirLock` released. Every blocking job of the indexer holds an `Arc<DirLock>` clone. An in-flight query
    holding the index handle (M19) keeps the file locked until it ends. Dropping everything without `shutdown` drains
    the index too (the channel closes when the last writer task and `Core` are gone).
  - Errors: `IndexOpen` (kind `Io`), `IndexLagging`, `IndexUnavailable` (kind `Unavailable`).
  - Lock order (M21): `Core::gate`, `index_rebuild`, `rebuild_sem` (a permit, rebuild units only),
    per-collection `admin` (ascending uuid), per-collection `admission`, `create_lock`, `registry`, then the leaves
    `Shared::slug` (taken only under the registry write lock), `RebuildSlot::rb` (held during blocking I/O by design,
    nothing else is taken meanwhile), `Published::snapshot`, pool mutexes, `Hub::dirty`, `Core::index`,
    `Inner::index_task`. **`create_lock` moved after `admin`** (it used to follow the gate): rename holds both, and
    `create_collection` takes only gate then `create_lock`, never an `admin`. `train_sem` is taken with no other lock
    held. The writer task never takes the admin mutex or the gate; the indexer takes neither. `delete_collection`
    releases `admission` before it waits for `admin`.
  - Rebuild commands (M20): `IndexCmd::Stage { key, generation, from, reply }` (if the active generation is below:
    `DiscardStaged` + staged `Checkpoint{generation, from}`; `Ok(false)` otherwise), `Unstage` (`DiscardStaged` under
    the same condition) and `AwaitActive` (a one-collection barrier for the new generation). `Stage`/`Unstage` run
    between passes from `admin_ops` and set the purge flag; the in-memory `Track` is not touched: after the snapshot
    switch the next resolution finds `active < target` + the staged checkpoint and resumes there. The writer task
    stores the new snapshot and its `cdx_len` together (`Published::switch`). M21: `IndexCmd::Forget { key, reply }`
    (`delete_collection`) runs between passes: it removes the hub entry and the track **before** its blocking job, then
    applies `DropCollection` (durable); on failure `gc_pending` is set and the next pass's orphan GC drops the rows. The
    caller has joined the collection's writer task first, so nothing can wake the feed again.
  - Deviation from the earlier text of this section ("a bounded queue of op batches", a purge task with a timer):
    tailing the CDX removes the queue, the loss on overflow and a second code path for catch-up.

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
| index | warc, cdx, urlkey |
| index-redb | index, cdx |
| masstuffy | collection, dictstore, index, index-redb (feature), warc, urlkey, zframe (`codec`, `train`), cdx, durable |

`index` also depends on `warc` (for `WarcDate`); `index-redb` uses `WarcDate`/`UrlPattern` through `index`'s re-exports.

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
| M12 | `collection`: manifest, `create` (warcinfo), `Writer::commit`, `Snapshot::read` (codec None and Zstd) | warcinfo is entry 0; read back equals input; locators contiguous; raw import validation; concurrent readers while committing; manifest rejection |
| M13 | `collection`: `open` + recovery | crash states after each commit step (segment truncated at many offsets, CDX truncated at every byte, zero-filled tail) → `open` yields a clean CDX satisfying I2/I4, committed entries readable; corrupt mid-CDX → error, repair → cut + report; leftovers removed, unknown kept; aborted create → `ManifestMissing`; open idempotent; reopen after poison; partial scan from checkpoint; fallback reasons; stale checkpoint removed after repair |
| M14 | `collection`: tombstones, `entries()`, `Rebuilder` (bounded worker pool) | rebuild None→Zstd, Zstd(D1)→Zstd(D1) (raw copy), Zstd(D1)→Zstd(D2), Zstd→None: live records byte-exact, tombstoned records not copied, new warcinfo first; crash before/after the manifest rename; writes during the rebuild carried over; concurrent readers; tombstone durability/idempotence; `entries()` with interleaved tombstones; pool: output identical for 1 and 4 workers, order preserved, error/panic stops and joins, in-flight memory bound |
| M15 | `index`: trait, `MemIndex`, conformance suite | conformance suite on `MemIndex`: nearest-date semantics and ties, `request` excluded, ACTIVE only (tombstones, staged/retired generations, drop/recreate), pattern queries and cursor pagination, checkpoint atomic with ops, idempotent replay, I8 rebuilt == incremental, concurrent readers |
| M16 | `index-redb` | conformance suite; reopen after drop (incl. lazy sync); crash in a child process (abort) -> consistent prefix, structural batches durable; concurrent readers during a large `apply`; purge bounded progress; differential vs `MemIndex`; schema mismatch / foreign file / already open |
| M17 | `masstuffy`: init/open, registry, writer task with group commit, `put`/`put_raw`/`get`/`get_raw` by locator, shutdown | many tasks `put` to one collection → all entries present once, no interleaving inside a batch |
| M18 | `masstuffy`: secondary-index wiring, catch-up from checkpoint, `rebuild_index` | kill between CDX fsync and index apply → catch-up; rebuilt index equals incremental one (I8) |
| M19 | `masstuffy`: `put_raw` from an envelope stream, `scan`, `search`, `get_by_id`, `get_by_url` | delegated frames with wrong dict_id rejected (both RawCheck levels); scan skips orphan bytes |
| M20 | `masstuffy`: `train_dictionary`, `rebuild` orchestration | deterministic sample; reads keep working during a rebuild (concurrent test) |
| M21 | `masstuffy`: `delete_collection`, `rename_collection`, record deletion (`delete`, `delete_by_id`) | crash mid-delete leaves `.trash-*` swept at open; slug uniqueness under concurrent creates and renames; deletes wait for rebuilds; I8 after deletes |

M3 and M4 are delivered together (one plan → code → review cycle).

M6 and M7 are delivered together (one plan → code → review cycle).

Milestones M0, M1 and M2 are delivered together by the first milestone.

## 4. Key cross-crate decisions

1. **Errors**: one `thiserror` enum per crate, no shared error crate. Higher crates wrap lower errors
   with `#[from]` and add context (path, collection uuid). The engine error exposes a coarse
   `kind()` (`NotFound`, `Conflict`, `Corrupt`, `Invalid`, `Io`, `Unavailable` for shutdown, a stopped writer or a failed
   background task) for future API layers. No `anyhow` in libs.
2. **Sync/async boundary**: all crates except `masstuffy` are sync and runtime-agnostic. The engine runs
   every blocking call (file I/O, fsync, zstd, redb) in `spawn_blocking`; tokio types never appear in
   lower public APIs. The `SecondaryIndex` trait is sync for the same reason.
3. **Single writer per collection**: the engine owns one writer task per collection holding the
   `collection::Writer` (`&mut`, no lock). Requests arrive on a bounded `mpsc`; the task drains what is
   queued and does one `commit` (one data fsync + one CDX fsync for the group), then replies via
   `oneshot`. Serialization and compression (`prepare`) happen in the caller's `spawn_blocking`, before
   queuing, so the writer only appends. The engine keeps a bounded per-worker `Preparer` pool for this.
4. **Lock-free readers**: readers clone an `Arc<Snapshot>` (published under a `std::sync::RwLock<Arc<_>>`
   held only for the clone) and use `pread` on shared file handles. Appends never invalidate a snapshot;
   a rebuild publishes a new one and keeps the previous one as the *retired* snapshot until the secondary index has
   switched. A read through a snapshot that finds a segment gone (`SegmentMissing`) is retried once with the current
   snapshot of the same generation; if the generation changed meanwhile it is `StaleLocator`.
5. **Durability lives in `collection` (per-collection data) and `durable` (primitives)**: data → fsync →
   CDX → fsync → (index apply). Manifests, dictionaries and config only via `atomic_write`. The secondary
   index is never needed for correctness: its checkpoint `(gen, cdx_offset)` lets the engine replay the CDX
   tail at startup.
6. **Generations are explicit** (spec 6): `gen_id: u32` increasing, in the manifest, in file names
   (`seg.<gen>.<N>.warc[.zst]`, `index.<gen>.cdx`) and in the index. Any file of a non-active
   generation (including `scan.<g>.json`) found at `open` is deleted, after the active generation was
   verified and after a dir fsync that makes the manifest durable. Rebuild to the same dictionary is just a new gen.
   A rebuild allocates `next_gen_id`, bumps it only at its commit, and deletes leftover generations at `start`
   as `open` does.
7. **One process per repository**: `DirLock` on `<workdir>/.lock` at `open`. Within the process, a
   `tokio::Mutex` per collection serializes admin operations (rebuild, delete, rename, record deletion);
   dictionary training is read-only and bounded by a repository-wide semaphore; samples are held in memory under a
   byte cap, not written as files (the buffer-directory lock is replaced by the directory lock).
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
- **Segment rollover** (`seglog`): the rule is `size > 0 && size + batch > threshold` (strict `>`, the spec said
  `>=`), so `split_threshold` is an inclusive maximum for multi-batch segments; an oversize batch goes alone
  into an empty segment. Zero-length items are rejected. `Reader` caps a read at 1 GiB by default.
- **Raw import check**: `collection::RawCheck::Full` (default) decompresses each delegated record, parses
  the WARC and rejects envelope metadata that does not match; `HeaderOnly` keeps only single frame, dict-id
  rule, declared-size cap and digest, and indexes the envelope metadata. Consequence: records whose envelope
  metadata disagrees with the payload, or whose payload is not a WARC, can only be read with `read_raw`;
  `read` returns `EntryMismatch`/`Record`, and M17/M19 must handle this on purpose. The engine selects it with the
  env var `MASSTUFFY_RAW_IMPORT_CHECK` = `full` (default) | `header`; any other value is a startup error.
  Library crates never read env vars.
- **Batch atomicity**: not guaranteed in the CDX. A crash during the CDX write may leave a prefix of an
  unacknowledged batch indexed after recovery (every such line points to fsynced data). Delivery is
  at-least-once; duplicates are allowed (see duplicate `record_id`).
- **Dict-less frames**: `prepare` compresses records > 1 MiB without dictionary; raw dict-0 frames must declare
  a content size > 1 MiB.
- **Aborted create**: a collection dir without `manifest.json` is trashed by the engine at open; `create`
  never deletes on error.
- **Record deletion**: logical only (CDX tombstones); physical zeroing is not implemented (I1 holds without
  exception); data and space are reclaimed by the next rebuild.
- **Writes during a rebuild**: allowed — the `Rebuilder` catches up by CDX ranges and commits in a short final
  step under writer exclusivity. Record deletions are serialised with rebuilds by the per-collection admin
  mutex (a tombstone after the first copied range fails the rebuild).
- **Rebuild parallelism**: transcoding runs on a bounded pool of `RebuildOptions::workers` scoped threads
  (crate default 1; the engine passes `suggested_workers()` = available_parallelism/2, >= 1); reads, ordering
  and appends stay on one thread, output is byte-identical for any worker count; in-flight work is capped by
  `workers * 4` records and `max_inflight_bytes` (default 256 MiB).

- **Secondary-index lookups (M15, 2026-11-14)**: URL lookup uses the massaged key for http(s) URLs and exact
  raw equality for the others; URL and pattern queries are per collection, `by_id` is global with an optional
  collection filter; `by_id` returns the most recently committed copy within a collection (greatest position),
  and across collections the copy with the latest WARC-Date, ties broken by the greater collection key.
- **WARC/1.0 accepted (2026-11-14)**: parsing accepts `WARC/1.0\r\n` and `WARC/1.1\r\n` (spec 3.1 said 1.1 only);
  other versions give `InvalidVersion`. The version is preserved byte-exactly (stored bytes are never
  re-serialized). `RecordBuilder` and the warcinfo write 1.1; `from_record_ref` keeps the source version and
  `fill_missing` uses whole seconds for 1.0. `WARC-Date` fractional seconds are accepted in both versions on
  read. For 1.0, `target_uri()` (and therefore the indexed URL) drops one enclosing `<…>`.
- **Conformance lint allow (M15, 2026-11-14)**: a module-level clippy allow (unwrap/expect/panic/indexing) is
  accepted on the `conformance` module only (test-support code behind the `conformance` feature).

## 6. Open questions (recommended answers)

1. **Several active generations per collection** (spec 6, Q1)? *Recommend no for v1*: exactly one active
   generation keeps I4 and recovery simple; the manifest format is versioned so a list can come later.
2. **`record_id` uniqueness** (spec 6)? *Recommend not enforced* (re-importing a WARC must not fail);
   `by_id` returns the **most recently committed** active copy, which is deterministic.
3. **Per-record compression without dictionary** (spec 6, Q4)? Decided in section 5: `dict_id = 0` frames
   are allowed for records over 1 MiB; `zframe`/`cdx` already carry the information, cost is one branch in `collection`.
4. **Pending-dictionary buffer** (spec 6, Q2)? *Recommend*: a normal uncompressed generation followed by an
   automatic rebuild, no special structure (M20 makes this a policy, not new storage).
5. **CDX rotation**: *Recommend none for v1*: one `index.<gen>.cdx` per generation, compacted by rebuild.
6. **Deduplication by digest / `revisit`**: *Recommend out of scope*; the digest field makes it possible later.
7. **CDX torn tail vs corruption**: *Recommend* a bad region at the end with no valid line
   after it is a torn tail (truncated at open); a valid line after a bad one is a hard error (fail-safe),
   with an explicit operator repair in M13 if ever needed. **Decided (M13)**: torn tail truncated at open;
   `Corrupt` and I2 violations are errors unless `RepairPolicy::TruncateAtCorruption`; open scans from a
   checkpoint.
8. **URL lookup by massaged key** (**Decided (user, 2026-11-14)**, see §5): spec 4.3 says `uri = url exactement`.
   *Recommend* massaged-key equality for http(s) (Wayback-like), exact raw equality for URLs `massage` rejects.
9. **`by_id` across collections** (**Decided (user, 2026-11-14)**, see §5): most recently committed copy within a
   collection; across collections the latest WARC-Date wins, ties by collection key (no reproducible
   cross-collection commit order). *Recommend* as is.
10. **Nearest ties / no date** (M15, pending user confirmation): equal distance -> later capture, then latest
    commit; `date = None` -> latest date (not "now"). *Recommend* as is.
11. **Secondary-index durability** (M16): *Recommend* durable commits by default (`sync_every = 1`,
    quick-repair on), structural batches always durable, engine free to raise `sync_every` after
    measurement. Adopted in M16.
    **Decided (M18): `index_sync_every = 1`.** The indexer already batches across commits and collections (while one
    apply runs the next batch accumulates), and `put` never waits for the index, so index fsyncs are amortised.
    Measured with the ignored `bench_index_sync_every` (release, 13th Gen Intel i7-1360P, 16 threads, ext4 on NVMe,
    one collection, 8 tasks x 250 puts of a 2 KiB record, `Codec::None`; two runs, ack time of all 2000 puts / lag of
    `sync_index` after the last ack): `sync_every = 1`: 742 ms / 0.84 ms and 720 ms / 0.99 ms; `16`: 645 ms / 0.22 ms
    and 615 ms / 0.26 ms; `64`: 634 ms / 0.50 ms and 1724 ms (outlier, noisy disk) / 0.69 ms. Raising `sync_every`
    improves the ack time by about 13% (below the 25% bar) and the index lags the last ack by under 1 ms in every
    case: the default stays 1 (durable index commits, nothing to replay after a crash); an operator who needs a
    little more put throughput can set 16.
12. **Index file location** (M16): *Recommend* `<workdir>/data/index.redb`. **Decided (M18).**
13. **Record deletion API** (M18; **Decided (M21)**): `CollectionHandle::delete(Vec<Located>)` and `delete_by_id`,
    run under the collection admin mutex (so they wait for a running rebuild): `prepare_tombstone` off the writer, then
    `Msg::Tombstone` to the writer task, reply after the CDX fsync. The index follows through the CDX tail; `T` lines
    written by other means (offline `Collection::tombstone`) are indexed the same way. Deletion is logical (Q22).
14. **Query freshness** (M18): the index is eventually consistent: queries may lag the CDX and, while a collection is
    re-indexed, answer empty; `Repository::sync_index()` is the read-your-writes barrier. *Recommend* as is.
    M19: scan reads the CDX and is not affected; index-backed queries follow Q14.
15. **Envelope import failure semantics** (M19): `OnInvalid::Abort` by default, `Skip` optional, framing errors always
    fatal, the committed set is a stream prefix reported by `committed_offset`. *Recommend* as is.
16. **Index hit of a non-active generation** (M19): `StaleLocator`, no silent retry. **Revisited (M20)**: the
    generation a rebuild just replaced stays readable through the retired snapshot until the index has switched;
    `StaleLocator` afterwards (and for `scan` pages after a switch).
17. **Index staging timing** (M20): the new generation is staged *after* the manifest commit, through the tail and
    resolution rule 4, from a staged checkpoint placed by `IndexCmd::Stage` at rebuild start; no reader ever touches
    an uncommitted CDX and the retired snapshot covers the window. *Adopted.*
18. **"Train then rebuild" option** (spec 4.4 step 8, M20): not provided; callers compose `train_dictionary` and
    `rebuild`. *Adopted.*
19. **Rebuild cancel API** (M20): none; shutdown cancels a rebuild before its final step and a dropped caller future
    does not. A `cancel_rebuild` can be added if operators need it. *Adopted.*
20. **Training sample** (M20): bottom-k by `xxh3(uuid || segment || offset)` over the live, non-warcinfo entries with
    stored size <= 1 MiB, read in locator order: deterministic for a CDX prefix, independent of CDX order, `O(k)`
    memory. *Adopted.*
21. **`data/buffer/`** (M20): unused ([LEGACY] deviation from spec 4.4/5.2); training takes no admin mutex and runs one
    at a time per repository through a semaphore. *Adopted.*
22. **`get` by locator and tombstones** (M21): `get`/`get_raw` by explicit locator are not tombstone-aware: a deleted
    record stays readable through its locator until the next rebuild (then `StaleLocator`); scans and index-backed
    queries hide it. A tombstone set per collection would need a full CDX scan at open. *Adopted.*
23. **Record deletes and rebuilds** (M21): `delete` waits for a running rebuild (admin mutex) and is serialised per
    collection, one fsync per call: callers batch locators. *Adopted* (revisit if delete throughput matters).
24. **Failed trash rename** (M21): the collection stays registered but closed (`UnknownCollection` on every
    operation, slug still taken); a second `delete_collection` retries, a reopen loads it normally. *Adopted.*
25. **Skipped collections** (M21): cannot be deleted or renamed through the API (not loaded: `UnknownCollection`);
    repairing them is an operator action and their slug stays reserved. *Adopted.*
