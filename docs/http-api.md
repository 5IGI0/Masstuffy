# Masstuffy HTTP API (v1)

Normative wire contract of `masstuffy-http` / `masstuffy-server`. Clients (the Python client in `clients/python`) are
written against this file.

## 1. Overview

- Base URL `http://127.0.0.1:8470`, API prefix `/v1` (`GET /healthz` is outside it).
- HTTP/1.1 only, **no TLS, no authentication**. Bind to loopback. Exposing the server requires a reverse proxy that
  does TLS and authentication; the body limits stay enforced by the server. Repository-wide routes (lookup by id without
  `collection`, jobs, index, dictionaries) are admin scope for a future auth layer.
- All JSON is UTF-8 with `Content-Type: application/json`. Unknown JSON fields in requests are rejected. Unknown or
  duplicate query parameters are rejected (`invalid_param`).
- u64 values are JSON numbers (exact in Python; JavaScript clients beware of values above 2^53). Digests are 16
  lowercase hex characters (xxh3-64).
- `HEAD` is accepted on every `GET` route (same work, no body).
- WARC/1.0 and WARC/1.1 records are accepted (whatever the engine accepts); the engine writes WARC/1.1 for records it
  builds.

## 2. Concepts

- **Collection**: a uuid plus a mutable slug matching `[a-z0-9][a-z0-9_-]{0,127}`. Renaming changes only the slug.
- **Record**: one WARC record (1.0 or 1.1).
- **Locator token**: opaque, `L1…`, URL-safe (`[A-Za-z0-9_-]`), up to 360 448 characters (typically 150-400). It
  carries the collection uuid, generation, position, digest and identity of one record, so it is stable for a record
  within one generation and survives renames. A rebuild invalidates it (`409 stale_locator`; find the record again by
  id or URL). It is integrity-checked (checksum) but **not authenticated**: anyone can forge a token, a forged token
  only reaches real positions (reads verify the digest; a delete writes a tombstone matched by position). Never treat a
  token as a capability. Record routes take the slug and the token; the token's collection must be the slug's
  (`locator_collection_mismatch`).
- **Generation**: each rebuild creates a new generation of a collection; locators carry theirs.
- **Eventual consistency**: lookups (by id, by URL) and search answer from a secondary index that may lag acknowledged
  puts. Use `sync=true` on the query or `POST /v1/index/sync` for read-your-writes. Scan and export read the collection
  index itself and are consistent with every acknowledged put.
- **Logical deletion**: a deleted record disappears from scan, export, search and lookups, but stays readable through
  its explicit token until the next rebuild.
- **Client-side compression.** `GET /v1/collections/{slug}` (and the list) report the `codec` (`{"algo":"none"}` or
  `{"algo":"zstd","dict_id":N,"level":L}`) and `raw_rules`, the rule the server enforces on delegated payloads:
  `{"payload":"warc"|"zstd_frame","single_frame":true,"dict_id":<active id or null>,"dictless_min_content_size":1048577}`.
  A `none` collection takes one WARC record per payload; a `zstd` collection takes exactly one zstd frame whose
  dictionary id equals `dict_id`, or dictionary id 0 only when the frame declares a content size above 1 MiB
  (`> 1 048 576`). Dictionaries are immutable and identified by `dict_id`: fetch `GET /v1/dictionaries/{dict_id}` once and
  cache it forever (strong `ETag`, `immutable`). To decompress a `/raw` body: read `Masstuffy-Dict-Id` (also in the frame
  header; `0` means no dictionary) and decompress the single frame with that dictionary. To compress for import: compress
  the WARC record as one frame with the collection's dictionary and level (frame carrying the content size, no
  checksum), put the frame in the envelope payload and its xxh3-64 in the digest field.

## 3. Errors

Body: `{"error": {"kind", "code", "message", "details"?}}` with `Content-Type: application/json`. A client should branch
on `code` and fall back on `kind`. `Retry-After` (seconds) is set on `index_lagging` (1), `overloaded` (1), `timeout`
(1) and `too_many_jobs` (5).

`kind`: `not_found` 404, `conflict` 409, `invalid` 400, `corrupt` 500, `io` 500, `unavailable` 503, `internal` 500.
Overrides: `not_decodable` is 422 (kind `corrupt`: a `header`-checked raw import whose payload disagrees with its
metadata; use `/raw`); `index_lagging` and `shutting_down` are 503; `import_failed` takes the status and kind of its cause.

Engine codes (code: details):

| code | status | details |
|---|---|---|
| `unknown_collection` | 404 | |
| `not_a_repository` | 404 | |
| `dict_not_found` | 404 | `{"dict_id"}` |
| `locked`, `not_empty`, `already_initialized`, `slug_taken` | 409 | |
| `stale_locator` | 409 | `{"expected","got"}` generations |
| `rebuild_in_progress`, `rebuild_aborted`, `stale_generation` | 409 | |
| `rebuild_failed` | by cause | `{"committed"}` |
| `layout`, `reopen_failed`, `read_failed`, `collection_error`, `dict_store`, `index_query` | 500 | |
| `not_decodable` | 422 | |
| `invalid_envelope`, `invalid_query`, `invalid_slug`, `invalid_options`, `config`, `invalid_env`, `train_failed` | 400 | |
| `invalid_record` | 400 | `{"index"}` |
| `invalid_delete_target` | 400 | `{"index"}` |
| `locator_not_committed` | 400 | |
| `commit_failed`, `io`, `index_open` | 500 | |
| `shutting_down`, `index_unavailable`, `shutdown_failed` | 503 | |
| `collection_unavailable` | 503 | `{"collection","outcome":"unknown"}` (the write may or may not be durable) |
| `index_lagging` | 503 | `{"collections":[uuid…]}` |
| `import_failed` | cause's | `{"report": ImportReport, "cause": {"kind","code","message"}}` |
| `internal` | 500 | |

HTTP-layer codes:

| code | status | kind | when |
|---|---|---|---|
| `no_route` | 404 | not_found | unknown path |
| `method_not_allowed` | 405 | invalid | wrong method |
| `invalid_param` | 400 | invalid | bad, unknown, duplicate or missing query parameter or path segment |
| `invalid_json` | 400 | invalid | body is not the expected JSON |
| `invalid_codec` | 400 | invalid | `dict_id` 0 or zstd level out of range |
| `invalid_locator` | 400 | invalid | bad token (`details.index` for lists) |
| `locator_collection_mismatch` | 400 | invalid | token of another collection (`details.index` for lists) |
| `invalid_warc` | 400 | invalid | WARC parse failure on put (`details: {"record_index","offset"}`) |
| `empty_body` | 400 | invalid | put with zero records |
| `too_many_records` | 413 | invalid | more than 10 000 records in one put |
| `payload_too_large` | 413 | invalid | body over its cap (`Content-Length` pre-check, or counted while streaming) |
| `unsupported_media_type` | 415 | invalid | wrong `Content-Type`, or `Content-Encoding` other than `identity` |
| `body_timeout` | 408 | invalid | no request-body data for the idle timeout |
| `body_read` | 400 | invalid | the request body stream failed (disconnect) |
| `record_not_found` | 404 | not_found | a lookup found nothing |
| `job_not_found` | 404 | not_found | |
| `too_many_jobs` | 503 | unavailable | running-job limit reached |
| `overloaded` | 503 | unavailable | concurrent-request limit reached |
| `timeout` | 503 | unavailable | request timeout; the outcome of a mutation is unknown |
| `unreadable_record` | 500 | corrupt | export hit a bad record (`details: {"locator","cause"}`) |

For a body cap or failure during an import, the import report is attached as `details.report` (413, 408, 400 `body_read`), together with the engine's abort cause as `details.cause` (`{"kind","code","message"}`).

## 4. Limits

`GET /v1/status` reports the values in force. Each limit is settable by a flag or an environment variable, see section 8.

| Limit | Default |
|---|---|
| put body | 64 MiB, 10 000 records |
| per record | min(`max_record_bytes`, put cap) |
| JSON request body | 32 MiB |
| import body | unbounded (streamed) |
| export page | `max_bytes` up to 64 MiB (default 8 MiB) |
| scan / search page | 10 000 (search default 100; scan and export default 1000) |
| delete locators per call | 65 536 |
| query string | 1 MiB |
| id / url | 262 144 bytes |
| request head | 512 KiB |
| request timeout | 60 s (routes without a streamed body) |
| body idle timeout | 30 s between chunks |
| header read timeout | 30 s |
| connections | 256 |
| concurrent requests | 256, at most 384 (held while the response streams; `/healthz` is exempt). Each in-flight upload parks one thread of the runtime's blocking pool (512 threads) for its WARC parse or import producer, so the cap keeps room for the engine's own blocking work |
| jobs | 8 running, 256 retained, `wait` up to 60 s |

## 5. Endpoints

Success statuses are in the headings. Every error follows section 3.

### `GET /healthz` 200
`{"status":"ok"}`; 503 `shutting_down` once the repository shuts down. It is exempt from the concurrent-request limit and the request timeout, so it answers under load.

### `GET /v1/status` 200
`{"version","api":1,"repository","raw_check":"full"|"header","max_record_bytes","collections","dictionaries","jobs_running","limits":{max_put_body_bytes,max_put_records,max_import_body_bytes (null if unbounded),max_json_body_bytes,max_export_page_bytes,request_timeout_ms,body_idle_timeout_ms,max_concurrent_requests},"open_report":null|{loaded,skipped,trashed,unknown_count,tmp_removed,trash_removed,index_recreated}}`.

### Collections
Collection object: `{"id","slug","generation","codec","raw_rules"}` (+ `"stats":{"commits","records","cdx_len"}` on single GET, create and rename).

- `GET /v1/collections` 200: `{"collections":[…]}` sorted by slug.
- `POST /v1/collections` 201, `Location: /v1/collections/{slug}`. Body `{"slug", "codec"?}` (default `{"algo":"none"}`). Errors: `slug_taken` 409, `invalid_slug`, `invalid_codec`, `dict_not_found` 404.
- `GET /v1/collections/{slug}` 200.
- `PATCH /v1/collections/{slug}` 200, body `{"slug": new}`; manifest only. `slug_taken` 409.
- `DELETE /v1/collections/{slug}` 200: `{"id","slug","trash_removed","slug_reserved","index_dropped"}`.

```
curl -X POST localhost:8470/v1/collections -d '{"slug":"web","codec":{"algo":"zstd","dict_id":32770,"level":3}}'
```

### Records
- `POST /v1/collections/{slug}/records` 201. `Content-Type: application/warc`, body = concatenated uncompressed WARC records.
  **One engine commit per request**: every record is durable, listed in order and contiguous, or none is acknowledged
  (see section 7). Response `{"records":[Locator…]}`. A missing `WARC-Record-ID` or `WARC-Date` is generated. Errors:
  `invalid_warc` (nothing written), `empty_body`, `too_many_records`, `payload_too_large`, `unsupported_media_type`, `body_timeout`.
- `GET …/records/{locator}` 200: `application/warc`, the decoded record, header `Masstuffy-Record-Id`.
- `GET …/records/{locator}/raw` 200: the stored bytes. Headers: `Masstuffy-Encoding: warc|zstd`; `Masstuffy-Dict-Id` (zstd
  only, `0` = no dictionary); `Masstuffy-Raw-Size` (stored bytes); `Masstuffy-Digest` (xxh3-64 of the body, 16 hex);
  `Masstuffy-Content-Size` (decoded size; zstd only, when the frame declares it). `Content-Type` is `application/warc` or
  `application/zstd` (one zstd frame). **Never** `Content-Encoding`.
- `DELETE …/records/{locator}` 200 `{"deleted":1}`.
- `POST …/records/delete` 200 `{"deleted":n}` (tombstones written after dedup). Body `{"locators":[…]}` (up to 65 536). `invalid_locator` with `details.index`; nothing is deleted on error. `invalid_delete_target` for a warcinfo.
- `POST …/records/delete-by-id` 200 `{"deleted":Locator}`; body `{"record_id"}`; 404 `record_not_found`. Index-backed (use `sync` first).

Locator object: `{"locator","collection","generation","segment","offset","raw_size","digest","date","warc_type","record_id","url"|null}`.

### `POST /v1/collections/{slug}/import`
Raw import, `Content-Type: application/x-masstuffy-envelope` (section 6), streamed. Query: `on_invalid=abort|skip`
(default abort), `batch_records` (1..65 536, default 4096), `batch_bytes` (>=1). 200 with the report:
`{"format":"v1"|"legacy"|null,"records","batches","rejected","rejections":[{"offset","ordinal","record_id","error"}],"committed_offset"}`.
On failure `import_failed` carries the same report in `details.report`; resume as described in section 6.

### Lookups
All accept `sync=true|false` (default false; `true` runs `sync_index` first).
- `GET /v1/lookup/by-id?id=&collection=&fetch=record|locator`: `id` required; `collection` (slug) optional. `fetch=locator` returns a Locator object; `fetch=record` (default) returns `application/warc` with `Masstuffy-Locator: <token>` (omitted, with `Masstuffy-Locator-Omitted: too-long`, if over 8192 bytes). Latest committed copy wins. 404 `record_not_found`.
- `GET /v1/collections/{slug}/lookup/by-url?url=&date=&fetch=`: `date` optional: a WARC date (`2024-01-02T03:04:05Z`, fractions allowed) or a Wayback timestamp of 4, 6, 8, 10, 12 or 14 digits (missing parts default to month 01, day 01, 00:00:00). The capture nearest to `date` is returned (latest without `date`); `request` records are excluded.

### `GET /v1/collections/{slug}/search`
Parameters: `host` (required); `subdomains` (default false: exact host vs host and subdomains); `port` = `any` (default) | `default` (scheme default) | `1..65535`; `path` (applied to path+query; absent = any); `path_match` = `prefix` (default) | `exact` (needs `path`; pywb `matchType` semantics); `limit` (1..10 000, default 100); `cursor`; `sync`. Response `{"hits":[Locator…],"next":token|null}`, ordered by (massaged URL key, date, position), every record type. `next` is null when no more hits.

### `GET /v1/collections/{slug}/scan?cursor=&limit=`
`{"items":[Locator…],"next":token}`; `limit` 1..10 000, default 1000. `next` is null **only** when `items` is empty: loop until an empty page (the last page costs one extra request). The first item is the warcinfo. A cursor of another generation gives `409 stale_locator` (restart).

### `GET /v1/collections/{slug}/export?cursor=&max_records=&max_bytes=&on_unreadable=fail|skip`
One page per request: `200 application/warc`, the decoded records in index order with nothing between them (the concatenation of pages is a valid WARC stream). Headers: `Masstuffy-Record-Count`, `Masstuffy-Unreadable-Count` (skip mode), `Masstuffy-Next-Cursor` (when the page was non-empty, even if every item was skipped). A page ends once `max_bytes` (1..64 MiB, default 8 MiB) is reached and holds at least one record. The end is an empty body without `Masstuffy-Next-Cursor` and without `Masstuffy-Record-Count`. `on_unreadable=fail` (default): the first unreadable record gives 500 `unreadable_record`; `skip` drops and counts it.

### Admin and jobs
- `POST …/rebuild` 202 + `Location: /v1/jobs/{id}`: body `{"codec", "recompress"?, "on_unreadable"?: "fail"|"skip", "workers"?, "max_inflight_bytes"?}`. Result: `{rounds,index_switched,old_files_removed,from_generation,generation,codec,copied,raw_copied,dropped_tombstoned,tombstone_lines,skipped,skipped_first,bytes_read,bytes_written}`.
- `POST …/train` 202: optional body `{"max_samples","max_sample_bytes","max_total_bytes","max_dict_size"}` (empty body = defaults). Result `{dict_id,created,label,generation,cdx_len,candidates,samples,sample_bytes,skipped,dict_size}`.
- `POST /v1/index/rebuild` 202 (result `{}`); `POST /v1/index/sync` 204.
- `GET /v1/jobs` 200 `{"jobs":[…]}` (running first, then finished newest first).
- `GET /v1/jobs/{id}?wait=<seconds>` 200: job `{"id","kind":"rebuild"|"train"|"rebuild_index","collection","collection_id","state":"running"|"succeeded"|"failed","started_at","finished_at","result","error"}`. With `wait` (0..60) and a running job, the call waits up to that long then returns the current state. Finished jobs are kept (256, oldest evicted); a failed job's `error` is an error object (section 3).

### Dictionaries
- `GET /v1/dictionaries` 200: `{"dictionaries":[{"dict_id","label","size"}…]}` sorted by id.
- `GET /v1/dictionaries/{dict_id}` 200: `dict_id` canonical decimal u32 (else 400 `invalid_param`; unknown 404 `dict_not_found`). `application/octet-stream`, the `.zstdict` bytes, `Masstuffy-Dict-Id`, strong `ETag: "<16 hex xxh3-64>"`, `Cache-Control: public, max-age=31536000, immutable`. `If-None-Match` equal to the ETag (comma lists compared per item, `*` matches) gives 304 without a body.

## 6. Raw import envelope (`application/x-masstuffy-envelope`)

- **V1**: first line `#masstuffy-cdx/1 gen=0\n` (gen is ignored). Then, per record, one line
  `A <segment> <offset> <raw_size> <digest> <date> <warc_type> <record_id> <url> <check>\n` followed by exactly `raw_size`
  payload bytes. Send segment and offset as `0` (ignored). `<digest>` is the xxh3-64 of the payload, 16 lowercase hex.
  `<date>` is the canonical WARC date (no fraction when 0, trailing zeros trimmed). `record_id` is without `<>`.
  `<url>` is `-` when absent, `%2D` for a literal `-`; bytes 0x00-0x20, `%` and 0x7F are written `%XX` (uppercase hex),
  nothing else is escaped. `<check>` is the low 32 bits of xxh3-64 (seed 0) of the line bytes before the space that
  precedes the check, as 8 lowercase hex. Lines are at most 256 KiB.
- **Legacy**: the 7-field line `url type record_id YYYYmmddHHMMSS filename offset raw_size`, no header, no digest.
- Payload: for a `none` collection the WARC record bytes; for a `zstd` collection one zstd frame compressed with the
  collection's dictionary (or dictionary id 0 for records over 1 MiB), see `raw_rules`. Dictionaries come from
  `GET /v1/dictionaries/{dict_id}`.
- Server checks: `raw_check` `full` (default; decodes and compares with the claims) or `header` (cheap checks; set with the
  server environment variable `MASSTUFFY_RAW_IMPORT_CHECK`); see `GET /v1/status`. `on_invalid`, `batch_records` and
  `batch_bytes` as in section 5. Framing errors always stop the import.
- Resume: the import commits batch by batch, so the committed set is a prefix. After a failure, send the header line
  again followed by the bytes from `committed_offset` (from `details.report`).

## 7. Streaming, cancellation and durability

- A 201 on put means durable (fsynced).
- If the client disconnects or the request times out after the body was fully sent, the records may or may not be
  committed (at-least-once). A retry can duplicate records (duplicate `record_id`s are allowed; lookup by id returns the
  most recently committed copy).
- A disconnect during the upload writes nothing for `…/records` (the parse fails before the put). For import, every batch
  acknowledged before the disconnect stays committed (a prefix); find the resume point with a by-id lookup.
- `timeout` on a mutation: the outcome is unknown, check state.
- Export and scan: resume with the cursor; `stale_locator` means restart.
- Jobs survive the client and end at server shutdown (cancelled rebuilds report `rebuild_aborted` or `shutting_down`).
- A client that stops reading a response holds a concurrency permit until the connection drops (no write timeout); put a
  proxy in front.

## 8. Server

```
masstuffy-server [WORKDIR] [OPTIONS]
```

### Configuration

Every option is set by a command-line flag or by an environment variable; precedence is flag > environment
variable > built-in default. The variable name is derived from the flag: `MASSTUFFY_` + the flag without `--`,
upper-cased, `-` becoming `_` (`--max-body-bytes` is `MASSTUFFY_MAX_BODY_BYTES`); `WORKDIR` is
`MASSTUFFY_WORKDIR`. `--flag value` and `--flag=value` are accepted. `masstuffy-server --help` prints the same table.

- SIZE: bytes, or a number with suffix `KiB`, `MiB`, `GiB` or `TiB` (binary, case-sensitive, e.g. `64MiB`).
  `--max-import-body-bytes` also accepts `unlimited`.
- DURATION: seconds, or a number with suffix `ms`, `s`, `m` or `h` (e.g. `500ms`, `30s`, `2m`).
- N: a positive decimal integer. Zero, signs, spaces and other characters are rejected.
- Booleans (`MASSTUFFY_INIT`): `1`, `true`, `yes`, `on`, `0`, `false`, `no`, `off`, case-insensitive. `--init` on the
  command line takes no value and cannot be negated: unset the variable or set it to `0`.
- An empty variable is an error, and so is a non-UTF-8 value (except `MASSTUFFY_WORKDIR`, a path). A malformed
  variable is an error even when a flag overrides it.
- An unknown `MASSTUFFY_*` variable is ignored with a warning in the log (names only, never values).
- An invalid or out-of-range value (flag or variable) is a usage error: the message names the flag or variable, the
  exit code is 2, and the repository is not opened.
- Layers: the environment is applied first, then the command line, each in the order of the table below. A higher
  layer overrides a lower one entirely: `--max-body-bytes` overrides `MASSTUFFY_MAX_PUT_BODY_BYTES`. Within one layer
  the specific cap beats `--max-body-bytes` / `MASSTUFFY_MAX_BODY_BYTES` whatever the order.
- The repository's `config.json` (engine configuration) is not overridable by the environment.

| Flag | Environment variable | Default | Meaning |
|---|---|---|---|
| `WORKDIR` (argument) | `MASSTUFFY_WORKDIR` | required | Repository directory. |
| `--listen ADDR` | `MASSTUFFY_LISTEN` | `127.0.0.1:8470` | Address to bind, IP:PORT (IPv6 as [::1]:PORT). |
| `--init` | `MASSTUFFY_INIT` | `false` | Create the repository if WORKDIR has none. |
| `--log LEVEL` | `MASSTUFFY_LOG` | `info` | Log level: error, warn, info, debug or trace. |
| `--max-body-bytes SIZE` | `MASSTUFFY_MAX_BODY_BYTES` | `unset` | Sets both --max-put-body-bytes and --max-import-body-bytes. |
| `--max-put-body-bytes SIZE` | `MASSTUFFY_MAX_PUT_BODY_BYTES` | `64MiB` | Body cap of POST .../records, also the per-record cap. |
| `--max-import-body-bytes SIZE` | `MASSTUFFY_MAX_IMPORT_BODY_BYTES` | `unlimited` | Body cap of POST .../import, or unlimited (the import is streamed). |
| `--max-put-records N` | `MASSTUFFY_MAX_PUT_RECORDS` | `10000` | Records per POST .../records, at most 1000000. |
| `--max-json-body-bytes SIZE` | `MASSTUFFY_MAX_JSON_BODY_BYTES` | `32MiB` | Cap of JSON request bodies. |
| `--max-export-page-bytes SIZE` | `MASSTUFFY_MAX_EXPORT_PAGE_BYTES` | `64MiB` | Upper bound of max_bytes on export, at most 1GiB. |
| `--request-timeout DURATION` | `MASSTUFFY_REQUEST_TIMEOUT` | `60s` | Timeout of routes without a streamed request body. |
| `--body-idle-timeout DURATION` | `MASSTUFFY_BODY_IDLE_TIMEOUT` | `30s` | Longest gap between two request-body chunks. |
| `--max-concurrent-requests N` | `MASSTUFFY_MAX_CONCURRENT_REQUESTS` | `256` | Requests in flight, response streaming included, at most 384. |
| `--max-running-jobs N` | `MASSTUFFY_MAX_RUNNING_JOBS` | `8` | Jobs running at once, at most 1024. |
| `--max-retained-jobs N` | `MASSTUFFY_MAX_RETAINED_JOBS` | `256` | Finished jobs kept for GET /v1/jobs, at most 100000. |
| `--max-job-wait DURATION` | `MASSTUFFY_MAX_JOB_WAIT` | `60s` | Upper bound of wait on GET /v1/jobs/{id}, at most 1h. |
| `--max-connections N` | `MASSTUFFY_MAX_CONNECTIONS` | `256` | Open connections, at most 1000000. |
| `--header-read-timeout DURATION` | `MASSTUFFY_HEADER_READ_TIMEOUT` | `30s` | Time allowed to receive a request head. |
| `--max-head-bytes SIZE` | `MASSTUFFY_MAX_HEAD_BYTES` | `512KiB` | Largest request head, above 416KiB and at most 16MiB. |
| `--drain-timeout DURATION` | `MASSTUFFY_DRAIN_TIMEOUT` | `30s` | How long a graceful shutdown waits for in-flight requests. |

`--init` creates the repository if `WORKDIR` has none. `--max-body-bytes` sets both the put and the import body caps.
Limits are described in section 4; `--max-concurrent-requests` is at most 384.

- Signals: the first SIGINT/SIGTERM drains (stops accepting, waits for in-flight requests up to `--drain-timeout`, 30 s by default);
  a second one skips the wait; a third exits the process at once (status 130) without waiting for `Repository::shutdown` (the next open repairs whatever was in flight). Otherwise `Repository::shutdown` runs before exit.
- Exit codes: 0 clean shutdown, 1 runtime failure, 2 usage error.
- Environment: `MASSTUFFY_RAW_IMPORT_CHECK=full|header` (read by the engine, no flag).
- Listening on a non-loopback address logs a warning: there is no authentication; put an authenticating reverse proxy in
  front.
