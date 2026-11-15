//! Locator tokens: a record position of the engine (`Located`) as a URL-safe string.
//!
//! Format v1: `"L1"` followed by the base64url (no padding) of the payload below. Integers are
//! big-endian.
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 16 | collection uuid |
//! | 16 | 4 | generation (at least 1) |
//! | 20 | 4 | segment |
//! | 24 | 8 | offset |
//! | 32 | 8 | raw_size (at least 1) |
//! | 40 | 8 | digest |
//! | 48 | 11 | date: year u16, month, day, hour, minute, second (u8 each), nanos u32 |
//! | 59 | 2 + n | warc_type: u16 length, UTF-8 |
//! | .. | 4 + n | record_id: u32 length, UTF-8 |
//! | .. | 1 [+ 4 + n] | url: flag 0 = absent, 1 = present then u32 length and UTF-8 |
//! | .. | 4 | check: low 32 bits of xxh3-64 of every preceding payload byte |
//!
//! The check detects typos and truncation. It is **not** authentication: anyone can forge a token.

use std::borrow::Cow;

use masstuffy::{CollectionId, Entry, Located};
use masstuffy_cdx::Kind;
use masstuffy_warc::WarcDate;

use crate::b64;

/// Longest accepted token, in characters. Covers a record id or URL of `MAX_QUERY_LEN` bytes.
pub const MAX_LOCATOR_TOKEN_LEN: usize = 360_448;
/// Token prefix of format version 1.
pub const LOCATOR_PREFIX: &str = "L1";

/// Why a locator token was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LocatorError {
    /// More than [`MAX_LOCATOR_TOKEN_LEN`] characters.
    #[error("locator token too long")]
    TooLong,
    /// The token does not start with a known version prefix.
    #[error("unknown locator token version")]
    Prefix,
    /// The text after the prefix is not canonical base64url.
    #[error("locator token is not canonical base64url")]
    Base64,
    /// The payload ends before the last field.
    #[error("locator token is truncated")]
    Truncated,
    /// Bytes follow the last field.
    #[error("locator token has trailing bytes")]
    Trailing,
    /// The check does not match the payload.
    #[error("locator token checksum mismatch")]
    Checksum,
    /// A field has an invalid value.
    #[error("locator token field {0} is invalid")]
    Field(&'static str),
}

fn check_of(bytes: &[u8]) -> u32 {
    let h = xxhash_rust::xxh3::xxh3_64(bytes);
    let [a, b, c, d, ..] = h.to_le_bytes();
    u32::from_le_bytes([a, b, c, d])
}

/// Encodes `at` as a token. Never fails: a `Located` of the engine always fits the field widths
/// (`Entry::validate` caps the text fields far below `u32::MAX`).
#[must_use]
pub fn encode_locator(at: &Located) -> String {
    let e = &at.entry;
    let url = e.url.as_deref();
    let lens = e.warc_type.len() + e.record_id.len() + url.map_or(0, str::len);
    let mut p: Vec<u8> = Vec::with_capacity(63 + lens + 5);
    p.extend_from_slice(&at.collection.as_u128().to_be_bytes());
    p.extend_from_slice(&at.generation.to_be_bytes());
    p.extend_from_slice(&e.segment.to_be_bytes());
    p.extend_from_slice(&e.offset.to_be_bytes());
    p.extend_from_slice(&e.raw_size.to_be_bytes());
    p.extend_from_slice(&e.digest.to_be_bytes());
    p.extend_from_slice(&e.date.year().to_be_bytes());
    p.extend_from_slice(&[
        e.date.month(),
        e.date.day(),
        e.date.hour(),
        e.date.minute(),
        e.date.second(),
    ]);
    p.extend_from_slice(&e.date.nanos().to_be_bytes());
    // The fallbacks (`MAX`) make decoding fail; they cannot happen for an engine-produced entry.
    p.extend_from_slice(
        &u16::try_from(e.warc_type.len())
            .unwrap_or(u16::MAX)
            .to_be_bytes(),
    );
    p.extend_from_slice(e.warc_type.as_bytes());
    p.extend_from_slice(
        &u32::try_from(e.record_id.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    p.extend_from_slice(e.record_id.as_bytes());
    match url {
        None => p.push(0),
        Some(u) => {
            p.push(1);
            p.extend_from_slice(&u32::try_from(u.len()).unwrap_or(u32::MAX).to_be_bytes());
            p.extend_from_slice(u.as_bytes());
        }
    }
    let check = check_of(&p);
    p.extend_from_slice(&check.to_be_bytes());
    let mut token = String::with_capacity(LOCATOR_PREFIX.len() + p.len().div_ceil(3) * 4);
    token.push_str(LOCATOR_PREFIX);
    token.push_str(&b64::encode(&p));
    token
}

struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], LocatorError> {
        let (head, tail) = self
            .rest
            .split_at_checked(n)
            .ok_or(LocatorError::Truncated)?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], LocatorError> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| LocatorError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, LocatorError> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, LocatorError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, LocatorError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, LocatorError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn text(&mut self, len: usize, field: &'static str) -> Result<String, LocatorError> {
        // `take` compares the length with the remaining bytes before slicing.
        let b = self.take(len)?;
        std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| LocatorError::Field(field))
    }
}

fn len_usize(n: u32) -> Result<usize, LocatorError> {
    usize::try_from(n).map_err(|_| LocatorError::Truncated)
}

/// Decodes a token.
///
/// # Errors
///
/// See [`LocatorError`]. Checks run in this order: length, prefix, base64, check, fields,
/// trailing bytes, entry invariants.
pub fn decode_locator(token: &str) -> Result<Located, LocatorError> {
    if token.len() > MAX_LOCATOR_TOKEN_LEN {
        return Err(LocatorError::TooLong);
    }
    let body = token
        .strip_prefix(LOCATOR_PREFIX)
        .ok_or(LocatorError::Prefix)?;
    let payload = b64::decode(body).ok_or(LocatorError::Base64)?;
    let split = payload
        .len()
        .checked_sub(4)
        .ok_or(LocatorError::Truncated)?;
    let (data, tail) = payload.split_at(split);
    let stored = <[u8; 4]>::try_from(tail).map_err(|_| LocatorError::Truncated)?;
    if check_of(data) != u32::from_be_bytes(stored) {
        return Err(LocatorError::Checksum);
    }
    let mut c = Cursor { rest: data };
    let collection = CollectionId::from_u128(u128::from_be_bytes(c.array()?));
    let generation = c.u32()?;
    let segment = c.u32()?;
    let offset = c.u64()?;
    let raw_size = c.u64()?;
    let digest = c.u64()?;
    let year = c.u16()?;
    let [month, day, hour, minute, second] = c.array()?;
    let nanos = c.u32()?;
    let tlen = usize::from(c.u16()?);
    let warc_type = c.text(tlen, "warc_type")?;
    let ilen = len_usize(c.u32()?)?;
    let record_id = c.text(ilen, "record_id")?;
    let url = match c.u8()? {
        0 => None,
        1 => {
            let n = len_usize(c.u32()?)?;
            Some(c.text(n, "url")?)
        }
        _ => return Err(LocatorError::Field("url")),
    };
    if !c.rest.is_empty() {
        return Err(LocatorError::Trailing);
    }
    let date = WarcDate::new(year, month, day, hour, minute, second, nanos)
        .ok_or(LocatorError::Field("date"))?;
    let entry = Entry {
        kind: Kind::Add,
        segment,
        offset,
        raw_size,
        digest,
        date,
        warc_type: Cow::Owned(warc_type),
        record_id: Cow::Owned(record_id),
        url: url.map(Cow::Owned),
    };
    entry.validate().map_err(|_| LocatorError::Field("entry"))?;
    if generation == 0 {
        return Err(LocatorError::Field("generation"));
    }
    if raw_size == 0 {
        return Err(LocatorError::Field("raw_size"));
    }
    Ok(Located {
        collection,
        generation,
        entry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(url: Option<&str>) -> Located {
        Located {
            collection: CollectionId::from_u128(0x0123_4567_89ab_cdef_0011_2233_4455_6677),
            generation: 3,
            entry: Entry {
                kind: Kind::Add,
                segment: 2,
                offset: 12_345,
                raw_size: 678,
                digest: 0xdead_beef,
                date: WarcDate::new(2024, 1, 2, 3, 4, 5, 0).unwrap(),
                warc_type: Cow::Borrowed("response"),
                record_id: Cow::Borrowed("urn:uuid:00000000-0000-4000-8000-000000000001"),
                url: url.map(|u| Cow::Owned(u.to_owned())),
            },
        }
    }

    /// Builds a token from a hand-made payload, with a correct check.
    fn forge(mut payload: Vec<u8>) -> String {
        let c = check_of(&payload);
        payload.extend_from_slice(&c.to_be_bytes());
        format!("{LOCATOR_PREFIX}{}", b64::encode(&payload))
    }

    #[allow(clippy::too_many_arguments)]
    fn payload(
        generation: u32,
        raw_size: u64,
        date: [u8; 5],
        wtype: &[u8],
        rid: &[u8],
        url_part: &[u8],
        len_override: Option<u32>,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&7u128.to_be_bytes());
        p.extend_from_slice(&generation.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&0u64.to_be_bytes());
        p.extend_from_slice(&raw_size.to_be_bytes());
        p.extend_from_slice(&9u64.to_be_bytes());
        p.extend_from_slice(&2024u16.to_be_bytes());
        p.extend_from_slice(&date);
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&u16::try_from(wtype.len()).unwrap().to_be_bytes());
        p.extend_from_slice(wtype);
        let l = len_override.unwrap_or(u32::try_from(rid.len()).unwrap());
        p.extend_from_slice(&l.to_be_bytes());
        p.extend_from_slice(rid);
        p.extend_from_slice(url_part);
        p
    }

    fn good(url_part: &[u8]) -> Vec<u8> {
        payload(1, 5, [1, 2, 3, 4, 5], b"response", b"urn:x", url_part, None)
    }

    #[test]
    fn locator_roundtrip_variants() {
        let mut cases = vec![sample(None), sample(Some("http://example.com/a b/é?x=1"))];
        let mut l = sample(Some("http://e.org/"));
        l.entry.date = WarcDate::new(2024, 1, 2, 3, 4, 5, 123_456_789).unwrap();
        cases.push(l);
        let mut l = sample(None);
        l.generation = u32::MAX;
        l.entry.offset = u64::MAX - 1;
        l.entry.raw_size = 1;
        cases.push(l);
        let mut l = sample(None);
        l.entry.digest = 0;
        cases.push(l);
        let mut l = sample(None);
        l.entry.digest = u64::MAX;
        cases.push(l);
        for x in cases {
            let t = encode_locator(&x);
            assert!(t.starts_with("L1"));
            assert!(
                t.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            );
            assert_eq!(decode_locator(&t).unwrap(), x);
        }
    }

    #[test]
    fn locator_rejects_bad_prefix() {
        assert_eq!(decode_locator(""), Err(LocatorError::Prefix));
        let t = encode_locator(&sample(None));
        let l2 = format!("L2{}", &t[2..]);
        assert_eq!(decode_locator(&l2), Err(LocatorError::Prefix));
    }

    #[test]
    fn locator_rejects_too_long() {
        let t = "L".repeat(MAX_LOCATOR_TOKEN_LEN + 1);
        assert_eq!(decode_locator(&t), Err(LocatorError::TooLong));
    }

    #[test]
    fn locator_every_truncation_fails() {
        let t = encode_locator(&sample(Some("http://example.com/x")));
        for n in 0..t.len() {
            assert!(decode_locator(&t[..n]).is_err(), "prefix {n}");
        }
    }

    #[test]
    fn locator_flipped_char_fails() {
        let t = encode_locator(&sample(Some("http://example.com/x")));
        let bytes = t.as_bytes();
        for i in 2..bytes.len() {
            let mut b = bytes.to_vec();
            b[i] = if b[i] == b'A' { b'B' } else { b'A' };
            let s = String::from_utf8(b).unwrap();
            let e = decode_locator(&s).unwrap_err();
            assert!(
                matches!(e, LocatorError::Checksum | LocatorError::Base64),
                "{i}: {e:?}"
            );
        }
    }

    #[test]
    fn locator_crafted_payloads() {
        // The well-formed baseline decodes.
        assert!(decode_locator(&forge(good(&[0]))).is_ok());
        // Length field larger than what remains.
        let p = payload(
            1,
            5,
            [1, 2, 3, 4, 5],
            b"response",
            b"urn:x",
            &[0],
            Some(1000),
        );
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Truncated));
        let p = payload(
            1,
            5,
            [1, 2, 3, 4, 5],
            b"response",
            b"urn:x",
            &[0],
            Some(u32::MAX),
        );
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Truncated));
        // Trailing byte.
        let mut p = good(&[0]);
        p.push(0);
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Trailing));
        // Month 13.
        let p = payload(1, 5, [13, 2, 3, 4, 5], b"response", b"urn:x", &[0], None);
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Field("date")));
        // Empty record id.
        let p = payload(1, 5, [1, 2, 3, 4, 5], b"response", b"", &[0], None);
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Field("entry")));
        // URL flag 2.
        assert_eq!(
            decode_locator(&forge(good(&[2]))),
            Err(LocatorError::Field("url"))
        );
        // Invalid UTF-8.
        let p = payload(1, 5, [1, 2, 3, 4, 5], b"resp\xffnse", b"urn:x", &[0], None);
        assert_eq!(
            decode_locator(&forge(p)),
            Err(LocatorError::Field("warc_type"))
        );
        // Generation 0, raw_size 0.
        let p = payload(0, 5, [1, 2, 3, 4, 5], b"response", b"urn:x", &[0], None);
        assert_eq!(
            decode_locator(&forge(p)),
            Err(LocatorError::Field("generation"))
        );
        let p = payload(1, 0, [1, 2, 3, 4, 5], b"response", b"urn:x", &[0], None);
        assert_eq!(decode_locator(&forge(p)), Err(LocatorError::Field("entry")));
    }
}
