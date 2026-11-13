//! Versioned, checksummed collection index (CDX) format `#masstuffy-cdx/1`.
//!
//! A file is UTF-8 text of `\n`-terminated lines: one header line
//! `#masstuffy-cdx/1 gen=<gen>`, then one line per entry in append order:
//!
//! ```text
//! <kind> <segment> <offset> <raw_size> <digest> <date> <warc_type> <record_id> <url> <check>
//! ```
//!
//! `kind` is `A` (add) or `T` (tombstone, repeating all fields of the add it deletes). Only `url`
//! is escaped (`%XX` uppercase for bytes `0x00..=0x20`, `%` and `0x7F`; `-` is absent, `%2D` a
//! literal `-`). `check` is the low 32 bits of the xxh3-64 of the line before it, in 8 lowercase
//! hex digits, so a torn or zero-filled tail is detected instead of misparsed. Lines are capped
//! at [`DEFAULT_MAX_LINE`]; use the same cap to write and read.
//!
//! [`CdxReader`] reports invalid bytes at the end of the file with no valid line after them as
//! [`Tail::Torn`] (the caller truncates to `at`); a bad line followed by a valid one, or a line
//! with a valid checksum but invalid content, is [`ReadError::Corrupt`]. The crate never opens,
//! syncs or truncates files: that is the caller's job. An empty `WARC-Target-URI` must be mapped to
//! `url: None` by the caller.
//!
//! ```
//! use masstuffy_cdx::{CdxReader, CdxWriter, DEFAULT_MAX_LINE, Entry, Kind, Step, Tail};
//! use masstuffy_warc::WarcDate;
//!
//! let entry = Entry {
//!     kind: Kind::Add,
//!     segment: 1,
//!     offset: 0,
//!     raw_size: 10,
//!     digest: masstuffy_cdx::digest(b"0123456789"),
//!     date: WarcDate::new(2025, 5, 3, 11, 43, 1, 0).unwrap(),
//!     warc_type: "response".into(),
//!     record_id: "urn:uuid:0f3e5a1c-0000-4000-8000-000000000000".into(),
//!     url: Some("https://example.com/a b".into()),
//! };
//! let mut w = CdxWriter::new(Vec::new(), 1, DEFAULT_MAX_LINE);
//! w.push(&entry).unwrap();
//! w.write_pending().unwrap();
//! let bytes = w.into_inner();
//!
//! let mut r = CdxReader::open(&bytes[..], DEFAULT_MAX_LINE).unwrap();
//! assert!(matches!(r.next_step().unwrap(), Step::Entry { entry: e, .. } if e == entry));
//! assert!(matches!(r.next_step().unwrap(), Step::End(Tail::Clean { .. })));
//! ```

#![forbid(unsafe_code)]

mod entry;
mod envelope;
mod error;
mod escape;
pub mod legacy;
mod line;
mod reader;
#[cfg(test)]
mod testutil;
mod writer;

pub use entry::{Entry, Kind};
pub use envelope::{EnvelopeFormat, EnvelopeItem, EnvelopeLimits, EnvelopeReader, ImportMeta};
pub use error::{EntryError, EnvelopeError, HeaderError, LineError, ReadError, WriteError};
pub use line::{encode_entry, encode_header, parse_header, parse_line};
pub use reader::{CdxReader, Step, Tail};
pub use writer::CdxWriter;

/// Default line cap in bytes, `\n` included: a WARC head is capped at 64 KiB, so a URL is at most
/// 192 KiB fully escaped, plus under 200 bytes of other fields.
pub const DEFAULT_MAX_LINE: usize = 256 * 1024;

/// xxh3-64 of stored bytes: the algorithm of [`Entry::digest`]. Hashing is CPU work: async callers
/// hash inside `spawn_blocking`.
#[must_use]
pub fn digest(stored: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(stored)
}

#[cfg(test)]
mod fuzz_tests {
    use crate::testutil::{Rng, file, sample};
    use crate::{
        CdxReader, EnvelopeLimits, EnvelopeReader, Step, legacy, parse_header, parse_line,
    };

    const ALPHABET: &[u8] = b"0123456789abcdefAT-%:.Z \n#\0";

    fn random_bytes(rng: &mut Rng) -> Vec<u8> {
        let len = rng.below(400);
        (0..len)
            .map(|_| match rng.below(10) {
                0 => u8::try_from(rng.below(256)).unwrap(),
                1 => 0xC3,
                _ => ALPHABET[rng.below(ALPHABET.len())],
            })
            .collect()
    }

    fn drain_reader(data: &[u8]) {
        if let Ok(mut r) = CdxReader::open(data, 512) {
            for _ in 0..10_000 {
                match r.next_step() {
                    Ok(Step::Entry { entry, .. }) => assert!(entry.validate().is_ok()),
                    _ => break,
                }
            }
        }
    }

    #[test]
    fn fuzz_no_panic() {
        let mut rng = Rng::new(0x1234_5678_9abc_def1);
        let header = b"#masstuffy-cdx/1 gen=1\n";
        for i in 0..20_000 {
            let data = random_bytes(&mut rng);
            let _ = parse_line(&data);
            let _ = parse_header(&data);
            let _ = legacy::parse_line(&data);
            let mut with = Vec::new();
            if i % 2 == 0 {
                with.extend_from_slice(header);
            }
            with.extend_from_slice(&data);
            drain_reader(&with);
            let limits = EnvelopeLimits {
                max_line: 512,
                max_record: 1 << 16,
            };
            if let Ok(mut r) = EnvelopeReader::new(&with[..], limits) {
                for _ in 0..10_000 {
                    if !matches!(r.next_item(), Ok(Some(_))) {
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn fuzz_mutated_valid_file() {
        let es: Vec<_> = (0..10).map(sample).collect();
        let (base, _) = file(1, &es);
        let mut rng = Rng::new(0xdead_beef_cafe_f00d);
        for _ in 0..2_000 {
            let mut d = base.clone();
            for _ in 0..=rng.below(3) {
                let pos = rng.below(d.len());
                let b = u8::try_from(rng.below(256)).unwrap();
                match rng.below(3) {
                    0 => d[pos] = b,
                    1 => d.insert(pos, b),
                    _ => {
                        d.remove(pos);
                    }
                }
            }
            if let Ok(mut r) = CdxReader::open(&d[..], 4096) {
                loop {
                    match r.next_step() {
                        Ok(Step::Entry { entry, .. }) => assert!(entry.validate().is_ok()),
                        Ok(Step::End(crate::Tail::Torn { at, end })) => {
                            assert!(at <= end);
                            break;
                        }
                        Ok(Step::End(_)) | Err(_) => break,
                    }
                }
            }
        }
    }
}
