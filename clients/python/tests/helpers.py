"""Shared test helpers: record builders and local dictionary training."""

import io
import random

import pywarc

try:
    import zstandard
except ImportError:  # pragma: no cover
    zstandard = None


def make_record(i, body=None, version=b"WARC/1.1", url=None, rtype=b"resource"):
    body = body if body is not None else (b"body %d " % i) * 20
    url = url if url is not None else ("http://h%d.example/p" % i)
    head = (version + b"\r\nWARC-Type: " + rtype + b"\r\nWARC-Record-ID: <urn:uuid:00000000-0000-0000-0000-" + b"%012d" % i + b">\r\n"
            b"WARC-Date: 2020-01-02T03:04:05Z\r\nWARC-Target-URI: " + url.encode() +
            b"\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n")
    return head + body + b"\r\n\r\n"


def make_warc(n, **kw):
    return b"".join(make_record(i, **kw) for i in range(n))


def split_records(data):
    """Parse concatenated records with pywarc -> list of (head, body) raw pieces."""
    out = []
    r = pywarc.WarcReader(io.BytesIO(data), compressed=False)
    while True:
        b = r.get_next_block()
        if b is None:
            return out
        out.append((b.raw_head, b.read()))


def train_local_dict(size=16384, n=300):
    """Train a zstd dictionary on synthetic HTML-ish records. Returns (dict_id, bytes)."""
    rnd = random.Random(7)
    samples = []
    for i in range(n):
        words = " ".join("w%d" % rnd.randrange(400) for _ in range(40))
        samples.append(("<html><head><title>page %d</title></head><body><div class='c%d'>%s</div>"
                        "<a href='http://example.com/%d'>link</a></body></html>" % (i, i % 9, words, i)).encode() * 3)
    d = zstandard.train_dictionary(size, samples)
    return d.dict_id(), d.as_bytes()


def parse_v1(body):
    """Parse a V1 envelope: returns a list of (fields, payload). Verifies the header and line checks."""
    import xxhash
    assert body.startswith(b"#masstuffy-cdx/1 gen=0\n")
    pos = len(b"#masstuffy-cdx/1 gen=0\n")
    out = []
    while pos < len(body):
        nl = body.index(b"\n", pos)
        line = body[pos:nl]
        prefix, check = line.rsplit(b" ", 1)
        assert check == b"%08x" % (xxhash.xxh3_64_intdigest(prefix) & 0xFFFFFFFF), "bad line check"
        f = prefix.decode().split(" ")
        size = int(f[3])
        payload = body[nl + 1:nl + 1 + size]
        assert len(payload) == size
        assert f[4] == "%016x" % xxhash.xxh3_64_intdigest(payload), "bad digest"
        out.append((f, payload))
        pos = nl + 1 + size
    return out
