//! Helpers shared by the unit tests.

use std::borrow::Cow;

use masstuffy_warc::WarcDate;

use crate::entry::{Entry, Kind};
use crate::line::{encode_entry, encode_header};

/// A varied valid entry.
pub(crate) fn sample(i: u32) -> Entry<'static> {
    let url = match i % 4 {
        0 => Some(Cow::Owned(format!("https://example.com/a b/{i}"))),
        1 => None,
        2 => Some(Cow::Owned(format!("https://example.com/%20/{i}?q=ü"))),
        _ => Some(Cow::Owned(format!("https://example.com/{i}"))),
    };
    Entry {
        kind: Kind::Add,
        segment: 1 + i / 10,
        offset: u64::from(i) * 1000,
        raw_size: 100 + u64::from(i),
        digest: u64::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        date: WarcDate::new(
            2025,
            5,
            3,
            11,
            43,
            1,
            if i.is_multiple_of(2) { 0 } else { 1_000 },
        )
        .unwrap(),
        warc_type: Cow::Borrowed(if i.is_multiple_of(3) {
            "response"
        } else {
            "request"
        }),
        record_id: Cow::Owned(format!("urn:uuid:{i:08x}-0000-4000-8000-000000000000")),
        url,
    }
}

/// A whole file; returns the bytes and the start offset of each entry line plus the final end.
pub(crate) fn file(generation: u32, entries: &[Entry<'_>]) -> (Vec<u8>, Vec<u64>) {
    let mut v = Vec::new();
    encode_header(generation, &mut v);
    let mut starts = Vec::new();
    for e in entries {
        starts.push(v.len() as u64);
        encode_entry(e, usize::MAX, &mut v).unwrap();
    }
    starts.push(v.len() as u64);
    (v, starts)
}

/// `content` + ` <check>\n`, computed like the encoder.
pub(crate) fn with_check(content: &str) -> Vec<u8> {
    let h = xxhash_rust::xxh3::xxh3_64(content.as_bytes());
    let [a, b, c, d, ..] = h.to_le_bytes();
    let check = u32::from_le_bytes([a, b, c, d]);
    format!("{content} {check:08x}\n").into_bytes()
}

/// xorshift64.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform-ish in `0..n` (`n > 0`).
    pub(crate) fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next_u64() % u64::try_from(n).unwrap()).unwrap()
    }
}
