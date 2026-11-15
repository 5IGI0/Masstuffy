# Masstuffy

A **single-machine** object store written in Rust, built to hold **millions of small objects** (web pages, documents,
API responses…) compactly, durably, and with fast reads. Every object is a **WARC record**, the web archiving format
(ISO 28500).

## The problem

Storing lots of small objects looks easy, but the usual approaches break down quickly:

| Usual approach | What goes wrong |
|---|---|
| **One file per object** | Millions of inodes, a filesystem that is slow to walk, back up and copy, and wasted space at the end of every small file's last block. |
| **A database (blobs)** | The database grows without bound, compacts poorly and becomes the bottleneck. The data is locked into a proprietary format. |
| **A distributed object store** (S3, Ceph, MinIO…) | A cluster to run and monitor, plus network latency, when one machine is enough for the target volume. |
| **Compressing each object on its own** | An object of a few KB compresses badly: the compressor has no history to work from. |
| **Compressing big archives in one go** (`.tar.gz`, a `.warc.gz` as a single stream) | Good ratio, but reading one object means decompressing everything before it. |

Masstuffy aims for the missing trade-off: **the compression ratio of a big archive, with the direct access of one file
per object, on one machine and with no external service.**

## How Masstuffy solves it

### Few files, many objects

Records are **concatenated** into multi-GB **segments**, append-only. A collection of ten million objects fits in a
handful of files, so copies, backups and `rsync` stay fast.

### One disk access per read

Each record is addressed by a **locator** `(segment, offset, size)`. A read is **a single positioned read** (`pread`):
no scanning, no decompressing of neighbouring data. Reads take no lock and never block writes.

### Compressing small objects well

Each record is an **independent zstd frame**, compressed with a **dictionary trained on its collection**. The dictionary
supplies the shared history a small object lacks on its own. The ratio gets close to end-to-end compression while every
object stays readable by itself.

Objects in the same collection look alike (same site, same kind of document), so the collection is the right unit for a
dictionary. When the data drifts, you **train a new dictionary and recompress the collection without stopping it**:
writes keep flowing during the rebuild, and the new generation only becomes active once it is complete.

### Self-contained

All state lives in **one working directory**: the segments, each collection's index (CDX), the dictionaries and an
embedded search index ([redb](https://github.com/cberner/redb)). No database to install, no cluster: one binary and one
directory.

### Data that outlives the software

Segments are **standard WARC files** (zstd-compressed). Web archiving tools can read them without Masstuffy. Every
collection starts with a `warcinfo` record that describes it. Each object's metadata travels in its WARC headers, and
the original bytes are kept as they were. Both WARC/1.0 and WARC/1.1 are accepted.

### Finding an object

Besides the locator, the search index finds records:

- **by identifier** (`WARC-Record-ID`);
- **by URL and date**, with "Wayback" semantics: the capture closest to the requested date;
- **by URL pattern**: a domain with or without its subdomains, a host, a port, an exact path or a path prefix;
- by a **full scan** of a collection, or an **export** to a WARC file.

This index is only a cache: it can be **rebuilt entirely** from the collection indexes, which remain the source of
truth.

### Crash-safe

The write order is fixed and checked: data, then `fsync`, then index, then `fsync`, then the commit point. After a
crash or a power cut, an object is either fully present or absent, and an acknowledged write is never lost. `fsync`
calls are shared between concurrent writes (group commit), which keeps throughput high without giving up durability.

### Offloading the server

Clients can **compress and decompress on their side**: they fetch the collection's codec settings and its dictionary
(immutable, cacheable), then send frames that are already compressed. The server checks them and stores them without
recompressing. A read can also return the raw frame for the client to decompress.

## What Masstuffy is not

- **Not distributed**: one machine, one process, one directory. Replication and backups happen at the file level.
- **Not built for updating objects**: a record is immutable, and changing it means writing a new one. Deletion is
  logical (the object becomes invisible) and the space is reclaimed at the next rebuild.
- **No authentication** yet: the HTTP API listens on localhost by default and belongs behind a proxy if exposed.

## Quick start

```sh
# Start the server on a new repository (listens on 127.0.0.1:8470)
cargo run --release --bin masstuffy-server -- ./data --init

# Create a collection (uncompressed; see docs/http-api.md for zstd)
curl -X POST localhost:8470/v1/collections -d '{"slug":"web"}'
```

Every server option can also be set through environment variables (`MASSTUFFY_WORKDIR`, `MASSTUFFY_LISTEN`,
`MASSTUFFY_INIT=1`…). `masstuffy-server --help` lists them all.

A Python client is included, see [`clients/python`](clients/python/README.md):

```python
import masstuffy as m

with m.Client("http://127.0.0.1:8470") as client:
    col = client.create_collection("web")
    locs = col.put([m.RecordSpec("resource", b"hello", headers={"WARC-Target-URI": "http://a.example/"})])
    with col.get(locs[0]) as rec:
        print(rec.read())
```

## Repository layout

| Path | Contents |
|---|---|
| [`spec/`](spec/README.md) | Specification of the concept and the on-disk format (in French): the source of truth. |
| [`docs/architecture.md`](docs/architecture.md) | Crate map and dependency graph. |
| [`docs/http-api.md`](docs/http-api.md) | HTTP API v1 contract. |
| `crates/warc`, `urlkey`, `zframe`, `cdx` | Formats: WARC, canonical URLs, zstd frames and dictionaries, CDX index. Usable on their own. |
| `crates/durable`, `seglog`, `dictstore`, `collection` | Storage: durable writes, segments, dictionaries, collections. |
| `crates/index`, `index-redb` | Secondary search index and its redb implementation. |
| `crates/masstuffy` | The engine (async, tokio). |
| `crates/http` | The HTTP API and the `masstuffy-server` binary. |
| [`clients/python`](clients/python/README.md) | Python client, built on [pywarc](https://github.com/5IGI0/pywarc). |

The code is safe Rust (`unsafe` is forbidden in every crate). Each crate has a single responsibility and can be used
outside Masstuffy.

## License

AGPL-3.0-or-later.
