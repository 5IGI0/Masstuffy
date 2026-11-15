# masstuffy (Python client)

Python client for the [Masstuffy HTTP API](../../docs/http-api.md) (the wire contract), built on
[pywarc](https://github.com/5IGI0/pywarc). Synchronous, thread-safe, streaming (archives, exports and large records
are never loaded in memory), with typed exceptions and optional client-side zstd compression.

## Install

pywarc 0.4.1 (needs `WarcBlock.raw_head`) is not on PyPI yet. Until it is, install it from git or a local clone:

```sh
pip install "pywarc @ git+https://github.com/5IGI0/pywarc@masstuffy-client-support"   # or: pip install -e /path/to/pywarc
pip install -e clients/python                # core
pip install -e 'clients/python[zstd]'        # + zstandard and xxhash: client-side compression, digest checks
pip install -e 'clients/python[envelope]'    # + xxhash only (V1 envelopes for `none` collections)
```

Python >= 3.8. Only pywarc (>= 0.4.1, pinned `<0.5`) is a hard dependency. If an older pywarc is forced in anyway,
everything that frames raw records (`put_warc_file`, `import_records` with bytes items, `export`, `iter_export`)
raises `MissingDependencyError`, because `WarcBlock.raw_head` / `iter_raw` are missing. pywarc >= 0.4.1 also reads
`.warc.gz` sequentially (older versions were quadratic).

## Quick start

```python
import masstuffy as m

with m.Client("http://127.0.0.1:8470") as client:
    col = client.create_collection("web")                         # codec none; Codec.zstd(dict_id, level) also works
    locs = col.put([m.RecordSpec("resource", b"hello", headers={"WARC-Target-URI": "http://a.example/"})])
    with col.get(locs[0]) as rec:                                 # always use `with`: it owns a connection
        print(rec.warc_type, rec.target_uri, rec.read())
    loc = col.lookup_by_url("http://a.example/", sync=True)       # lookups are eventually consistent
```

## Calls

- Collections: `client.create_collection / collection / get_collection / collections / rename_collection /
  delete_collection`, or the same on a `Collection` handle (`info`, `rename`, `delete_collection`).
- Write, server-side compression (returns locators): `col.put([...])` (one request, one durable commit; items are
  `RecordSpec` or raw serialized WARC bytes), `col.put_raw(data)`, `col.put_warc_file(path, mode="put")` (batches;
  the exact source bytes are forwarded, WARC/1.0 stays 1.0; plain or gzip, sniffed).
- Write, client-side compression (returns an `ImportReport`, not locators; find the records with
  `lookup_by_id(..., sync=True)` or `scan()`): `col.import_records([...])`, `col.put_warc_file(path, mode="import")`,
  `col.import_envelope(source, resume_offset=...)`. The compression follows the collection's `raw_rules`
  (dictionary fetched once and cached). If the codec changed meanwhile, the client refreshes the info and retries once
  with the remaining records.
- Read: `col.get(locator, decode="server"|"client")`, `col.get_raw(locator, decompress=True)` (decompresses locally;
  the digest and size are verified at EOF when xxhash is installed), `lookup_by_url / get_by_url / lookup_by_id /
  get_by_id`, `client.lookup_by_id(..., collection=None)`.
- Iterate: `col.search(host, subdomains=, port=, path=, path_match=, limit=)` and `col.scan()` (flat iterators; `*_pages`
  variants give `Page` objects with their cursor), `col.export(dest)` and `col.iter_export()`.

```python
col.put_warc_file("crawl.warc.gz", batch_records=500)                     # resume: start_record=report.next_record
rep = col.export("out.warc.gz")                                           # atomic (.part), one gzip member per record
# resume after ExportInterruptedError e: fileobj.truncate(e.report.bytes); fileobj.seek(e.report.bytes);
#   col.export(fileobj, cursor=e.report.cursor)   (report.bytes is an absolute position when dest is seekable)
col.delete([loc]); col.delete_by_id(record_id)                            # logical deletion
job = col.train(max_dict_size=16384).wait(120)                            # then rebuild with the new dictionary
col.rebuild(m.Codec.zstd(job.result["dict_id"], 3)).wait(600)
client.sync_index(); client.rebuild_index().wait(); client.jobs(); client.dictionaries()
```

## Errors

| exception | when |
|---|---|
| `InvalidRequestError` (`PayloadTooLargeError` for 413) | kind `invalid` |
| `NotFoundError` | kind `not_found` (lookups return `None` for `record_not_found`) |
| `ConflictError` (`StaleLocatorError`) | kind `conflict` (`stale_locator`: the collection was rebuilt; re-resolve the record) |
| `NotDecodableError` | `not_decodable` (422): read the bytes with `get_raw` or `get(decode="client")` |
| `UnavailableError` | kind `unavailable` (503) |
| `ServerError` | kind `corrupt` / `io` / `internal`, other 5xx |
| `TransportError` | network failure; `outcome_unknown` is True when a mutation may have happened |
| `ProtocolError` | the server broke the contract (bad JSON, missing header, malformed WARC, count mismatch) |
| `PartialPutError`, `ExportInterruptedError` | failure part-way; `.report` says where to resume, `.cause` is the error |
| `JobFailedError`, `JobTimeoutError`, `RecordTooLargeError`, `MissingDependencyError` | see docstrings |

All `ApiError`s carry `status`, `kind`, `code`, `message`, `details`, `retry_after`; import errors expose
`import_report`.

## Delivery semantics

- `put` returns after the server's 201: durable. A `TransportError` with `outcome_unknown=True` or a 5xx means the
  records may be committed; retrying can duplicate them (at-least-once; duplicate record ids are allowed and
  `lookup_by_id` returns the latest). The client never retries a put automatically, except 503 `overloaded` /
  `too_many_jobs` / `index_lagging` (rejected before any work) when the body is replayable. GET requests and
  `sync_index` are retried on transport errors.
- `put_warc_file` batches commit independently. Import commits batch by batch (a prefix); resume with
  `import_envelope(resume_offset=report.committed_offset)` or `put_warc_file(start_record=...)`. If a record is
  committed before a codec-change retry and the error carried no report, it can be sent twice. When an import
  fails the report always includes the records committed by an earlier attempt of the same call
  (`PartialPutError` for `put_warc_file`, and for `import_records` when a retry had already happened). If the failure
  was not an API error (network, client side) the server may have committed batches unseen: `PutFileReport.uncertain`
  is `(next_record, -1)` (records from `next_record` on may exist; for `import_records` such errors are raised as is,
  and the same caveat applies).
- Lookups and search may lag acknowledged puts: pass `sync=True` or call `client.sync_index()`. Scan and export are
  consistent. After a rebuild, old locators and export cursors give `StaleLocatorError`.
- Deleted records disappear from scan, export, search and lookups but stay readable by token until the next rebuild.

## Threads, memory, limits

`Client` is thread-safe (pooled keep-alive connections). `Collection`, `Record`, `RawBody`, `Job` and iterators belong to
one thread. Streaming objects (`Record`, `RawBody`, `iter_export`) hold a connection until read to the end or closed:
use `with`. `Record.read()` without an argument loads the body in memory. Records above 4 MiB are spooled to a temp file
when compressed on the client. Limits come from `client.status().limits`; the server answers 413
(`PayloadTooLargeError`) over its caps, and `put_warc_file` checks record sizes before sending.

## Tests

```sh
cd clients/python
python3 -m unittest discover -s tests -t .                     # unit tests (fake in-process server)
cargo build -p masstuffy-http --features server --bin masstuffy-server
MASSTUFFY_SERVER_BIN=../../target/debug/masstuffy-server python3 -m unittest discover -s tests -t .   # + integration
```

Tests that need `zstandard` / `xxhash` are skipped when the extras are missing; integration tests are skipped when
`MASSTUFFY_SERVER_BIN` is unset.
