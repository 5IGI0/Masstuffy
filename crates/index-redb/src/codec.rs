//! Every byte encoding of the index file: keys and values, pure functions without redb.
//!
//! All integers are big-endian, so byte order equals numeric order for unsigned integers. Slice
//! indexing is denied by lint: reads go through [`Reader`], `first_chunk`, `last_chunk` and
//! `split_at_checked`, and every failure is `Error::Corrupt`.

use std::borrow::Cow;
use std::ops::Bound;

use masstuffy_cdx::{Entry, Kind};
use masstuffy_index::rank::UrlKey;
use masstuffy_index::{Checkpoint, CollectionState, Cursor, Error, Position, WarcDate};

use crate::error::{FieldTooLong, corrupt};

/// Bytes of the fixed tail of an URL row: date (16) + position (12).
pub(crate) const TAIL: usize = 28;

/// Smallest position.
pub(crate) const MIN_POS: Position = Position {
    segment: 0,
    offset: 0,
};
/// Largest position.
pub(crate) const MAX_POS: Position = Position {
    segment: u32::MAX,
    offset: u64::MAX,
};

/// `ENTRIES` value format byte.
const ENTRY_FORMAT: u8 = 1;
/// `COLLECTIONS` value format byte.
const COLL_FORMAT: u8 = 1;

/// Concatenates `parts` into a fixed-size array (extra bytes are dropped, missing ones stay 0;
/// callers pass parts whose sizes add up to `N`).
fn concat<const N: usize>(parts: &[&[u8]]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut slots = out.iter_mut();
    for part in parts {
        for b in *part {
            if let Some(o) = slots.next() {
                *o = *b;
            }
        }
    }
    out
}

/// Bounds-checked cursor over a byte slice. Every method fails with `Corrupt`.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf }
    }

    fn short() -> Error {
        corrupt("truncated stored value")
    }

    fn chunk<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (head, rest) = self.buf.split_first_chunk::<N>().ok_or_else(Self::short)?;
        self.buf = rest;
        Ok(*head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(u8::from_be_bytes(self.chunk::<1>()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.chunk::<2>()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.chunk::<4>()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(self.chunk::<8>()?))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, Error> {
        Ok(u128::from_be_bytes(self.chunk::<16>()?))
    }

    /// The next `n` bytes; `n` comes from disk and is checked against the remaining length.
    pub(crate) fn bytes(&mut self, n: u32) -> Result<&'a [u8], Error> {
        let n = usize::try_from(n).map_err(|_| Self::short())?;
        let (head, rest) = self.buf.split_at_checked(n).ok_or_else(Self::short)?;
        self.buf = rest;
        Ok(head)
    }

    /// A `u32` length followed by that many UTF-8 bytes.
    fn str(&mut self) -> Result<&'a str, Error> {
        let n = self.u32()?;
        let b = self.bytes(n)?;
        std::str::from_utf8(b).map_err(|_| corrupt("stored string is not UTF-8"))
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Fails if bytes remain.
    pub(crate) fn finish(self) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(corrupt("trailing bytes in stored value"))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Positions and dates
// ---------------------------------------------------------------------------------------------

/// 12 bytes, `segment ‖ offset`. Byte order = `Position` order.
pub(crate) fn position_bytes(p: Position) -> [u8; 12] {
    concat(&[&p.segment.to_be_bytes(), &p.offset.to_be_bytes()])
}

/// Inverse of [`position_bytes`].
pub(crate) fn position_from(b: &[u8; 12]) -> Position {
    let mut r = Reader::new(b);
    // 12 bytes are always enough for a u32 and a u64.
    Position {
        segment: r.u32().unwrap_or(0),
        offset: r.u64().unwrap_or(0),
    }
}

/// 16 bytes whose byte order is the numeric order of `n` over the whole `i128` domain (the sign
/// bit is flipped).
pub(crate) fn enc_date(n: i128) -> [u8; 16] {
    (n.cast_unsigned() ^ (1u128 << 127)).to_be_bytes()
}

/// Inverse of [`enc_date`].
pub(crate) fn dec_date(b: [u8; 16]) -> i128 {
    (u128::from_be_bytes(b) ^ (1u128 << 127)).cast_signed()
}

// ---------------------------------------------------------------------------------------------
// ENTRIES / BY_ID keys
// ---------------------------------------------------------------------------------------------

/// `ENTRIES` key: `slot ‖ segment ‖ offset`.
pub(crate) fn entry_key(slot: u64, pos: Position) -> [u8; 20] {
    concat(&[&slot.to_be_bytes(), &position_bytes(pos)])
}

/// Position stored in an `ENTRIES` key.
pub(crate) fn entry_key_position(key: &[u8]) -> Result<Position, Error> {
    let tail = key
        .last_chunk::<12>()
        .filter(|_| key.len() == 20)
        .ok_or_else(|| corrupt("malformed entries key"))?;
    Ok(position_from(tail))
}

/// `BY_ID` key: `record_id ‖ 0x00 ‖ slot ‖ position`.
pub(crate) fn by_id_key(record_id: &str, slot: u64, pos: Position) -> Vec<u8> {
    let mut k = Vec::with_capacity(record_id.len().saturating_add(21));
    k.extend_from_slice(record_id.as_bytes());
    k.push(0);
    k.extend_from_slice(&entry_key(slot, pos));
    k
}

/// Scan bounds `[rid ‖ 0x00, rid ‖ 0x01)` of every `BY_ID` row of `record_id`. Record ids are CDX
/// atoms (no 0x00), so `rid ‖ 0x00` is an unambiguous prefix.
pub(crate) fn by_id_bounds(record_id: &str) -> (Vec<u8>, Vec<u8>) {
    let mut lo = Vec::with_capacity(record_id.len().saturating_add(1));
    lo.extend_from_slice(record_id.as_bytes());
    let mut hi = lo.clone();
    lo.push(0);
    hi.push(1);
    (lo, hi)
}

/// `(slot, position)` of a `BY_ID` key.
pub(crate) fn parse_by_id_key(key: &[u8]) -> Result<(u64, Position), Error> {
    let bad = || corrupt("malformed by_id key");
    let sep = key.len().checked_sub(21).ok_or_else(bad)?;
    if key.get(sep) != Some(&0) {
        return Err(bad());
    }
    let tail = key.last_chunk::<20>().ok_or_else(bad)?;
    let mut r = Reader::new(tail);
    let slot = r.u64()?;
    let mut pos = [0u8; 12];
    for (o, b) in pos.iter_mut().zip(tail.iter().skip(8)) {
        *o = *b;
    }
    Ok((slot, position_from(&pos)))
}

// ---------------------------------------------------------------------------------------------
// URLS keys
// ---------------------------------------------------------------------------------------------

/// URL part of a `URLS` key (class byte included), prefix-free among all URLs.
///
/// # Errors
/// `Corrupt` if a massaged key violates the urlkey charset (`0x21..=0x7E`), which would break
/// the ordering proofs; `Backend(FieldTooLong)` for a raw URL longer than `u32::MAX`.
pub(crate) fn url_part(key: &UrlKey<'_>) -> Result<Vec<u8>, Error> {
    match key {
        UrlKey::Massaged(k) => {
            if !k.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
                return Err(corrupt("massaged key violates the urlkey charset"));
            }
            let mut v = Vec::with_capacity(k.len().saturating_add(2));
            v.push(0);
            v.extend_from_slice(k.as_bytes());
            v.push(0);
            Ok(v)
        }
        UrlKey::Raw(r) => {
            let len = u32::try_from(r.len())
                .map_err(|_| Error::Backend(Box::new(FieldTooLong(r.len()))))?;
            let mut v = Vec::with_capacity(r.len().saturating_add(5));
            v.push(1);
            v.extend_from_slice(&len.to_be_bytes());
            v.extend_from_slice(r.as_bytes());
            Ok(v)
        }
    }
}

/// `URLS` key prefix selecting one URL of one slot: `slot ‖ url part`.
pub(crate) fn url_prefix(slot: u64, part: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(part.len().saturating_add(8));
    v.extend_from_slice(&slot.to_be_bytes());
    v.extend_from_slice(part);
    v
}

/// Appends `date ‖ position` to a prefix: a full `URLS` key.
pub(crate) fn url_row(prefix: &[u8], date: [u8; 16], pos: Position) -> Vec<u8> {
    let mut v = Vec::with_capacity(prefix.len().saturating_add(TAIL));
    v.extend_from_slice(prefix);
    v.extend_from_slice(&date);
    v.extend_from_slice(&position_bytes(pos));
    v
}

/// `(date nanos, position)` of the last 28 bytes of a `URLS` key.
pub(crate) fn row_tail(key: &[u8]) -> Result<(i128, Position), Error> {
    // slot (8) + class (1) + terminator or length (1) + tail.
    if key.len() < TAIL.saturating_add(10) {
        return Err(corrupt("urls key too short"));
    }
    let tail = key
        .last_chunk::<TAIL>()
        .ok_or_else(|| corrupt("urls key too short"))?;
    let mut r = Reader::new(tail);
    let date = r.u128()?;
    let mut pos = [0u8; 12];
    for (o, b) in pos.iter_mut().zip(tail.iter().skip(16)) {
        *o = *b;
    }
    Ok((dec_date(date.to_be_bytes()), position_from(&pos)))
}

/// `(massaged key, date nanos, position)` of a class-0 `URLS` key.
pub(crate) fn parse_massaged_row(key: &[u8]) -> Result<(&str, i128, Position), Error> {
    let bad = || corrupt("malformed massaged urls key");
    let (date, pos) = row_tail(key)?;
    let head_len = key.len().checked_sub(TAIL).ok_or_else(bad)?;
    let (head, _) = key.split_at_checked(head_len).ok_or_else(bad)?;
    if head.get(8) != Some(&0) {
        return Err(bad());
    }
    let body = head.get(9..).ok_or_else(bad)?;
    let (last, k) = body.split_last().ok_or_else(bad)?;
    if *last != 0 {
        return Err(bad());
    }
    let k = std::str::from_utf8(k).map_err(|_| bad())?;
    Ok((k, date, pos))
}

/// Maps a string bound `b` of a massaged-key range to a byte bound on `key ‖ 0x00 ‖ suffix`.
///
/// If `b` contains a 0x00 byte at index `i` the result is `b[..i] ‖ 0x01`, otherwise `b ‖ 0x00`.
///
/// Property (tested): for every key `k` whose bytes are all `>= 0x02` and every suffix `s`,
/// `k < b` (byte order of strings) iff `k ‖ 0x00 ‖ s < enc_bound(b)`.
///
/// Proof sketch. Case A, `b` has no 0x00: if `k == b`, the left side has `b ‖ 0x00` as a prefix, so
/// it is not less; if `k` is a proper prefix of `b`, then `0x00 < b[|k|]`, so it is less; if `b` is
/// a proper prefix of `k`, then `k[|b|] >= 0x02 > 0x00`, so it is greater; otherwise the first
/// differing byte decides, as for the strings. Case B, `b = p ‖ 0x00 ‖ r`: since `k` has no 0x00,
/// `k < b` iff `k <= p`; `k == p`: `p ‖ 0x00 < p ‖ 0x01`; `k` a proper prefix of `p`: `0x00 <
/// p[|k|]`; `p` a proper prefix of `k`: `k[|p|] >= 0x02 > 0x01`; otherwise the first differing byte
/// decides.
///
/// Hence the URL rows of a slot whose massaged key lies in `[start, end)` are exactly those in
/// `[slot ‖ 0x00 ‖ enc_bound(start), slot ‖ 0x00 ‖ enc_bound(end))`. This is needed because
/// `urlkey::point_range` ends with `'\0'` and `prefix_range` may end with `char::MAX`.
pub(crate) fn enc_bound(b: &str) -> Vec<u8> {
    let bytes = b.as_bytes();
    match bytes.iter().position(|x| *x == 0) {
        Some(i) => {
            let mut v = Vec::with_capacity(i.saturating_add(1));
            v.extend_from_slice(bytes.get(..i).unwrap_or_default());
            v.push(1);
            v
        }
        None => {
            let mut v = Vec::with_capacity(bytes.len().saturating_add(1));
            v.extend_from_slice(bytes);
            v.push(0);
            v
        }
    }
}

/// Full class-0 bound `slot ‖ 0x00 ‖ enc_bound(b)`.
pub(crate) fn class0_bound(slot: u64, b: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(b.len().saturating_add(10));
    v.extend_from_slice(&slot.to_be_bytes());
    v.push(0);
    v.extend_from_slice(&enc_bound(b));
    v
}

/// Seek bound of "strictly after `c`" in the class-0 URL rows of `slot`.
///
/// For a key in the stored charset it is `Excluded` on the exact row prefix. Otherwise no stored
/// key can equal `c.key`, so "after the cursor" means "key > c.key": `Included(enc_bound(key))`.
/// This keeps the answer identical to `MemIndex` for any `Cursor` value.
pub(crate) fn cursor_bound(slot: u64, c: &Cursor) -> Bound<Vec<u8>> {
    if c.key.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        let mut v = Vec::with_capacity(c.key.len().saturating_add(38));
        v.extend_from_slice(&slot.to_be_bytes());
        v.push(0);
        v.extend_from_slice(c.key.as_bytes());
        v.push(0);
        v.extend_from_slice(&enc_date(c.date_nanos));
        v.extend_from_slice(&position_bytes(c.position));
        Bound::Excluded(v)
    } else {
        Bound::Included(class0_bound(slot, &c.key))
    }
}

/// The raw key of a bound.
pub(crate) fn bound_key(b: &Bound<Vec<u8>>) -> &[u8] {
    match b {
        Bound::Included(k) | Bound::Excluded(k) => k,
        Bound::Unbounded => &[],
    }
}

// ---------------------------------------------------------------------------------------------
// Entry value
// ---------------------------------------------------------------------------------------------

/// Encodes the value of an `ENTRIES` row (version 1) into `out`.
///
/// # Errors
/// `Backend(FieldTooLong)` for a string longer than `u32::MAX` bytes.
pub(crate) fn encode_entry(entry: &Entry<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
    let url = entry.url.as_deref();
    let lens = entry
        .warc_type
        .len()
        .saturating_add(entry.record_id.len())
        .saturating_add(url.map_or(0, str::len));
    out.reserve(lens.saturating_add(41));
    out.push(ENTRY_FORMAT);
    out.push(u8::from(url.is_some()));
    out.extend_from_slice(&entry.raw_size.to_be_bytes());
    out.extend_from_slice(&entry.digest.to_be_bytes());
    out.extend_from_slice(&entry.date.year().to_be_bytes());
    out.extend_from_slice(&[
        entry.date.month(),
        entry.date.day(),
        entry.date.hour(),
        entry.date.minute(),
        entry.date.second(),
    ]);
    out.extend_from_slice(&entry.date.nanos().to_be_bytes());
    let put = |out: &mut Vec<u8>, s: &str| -> Result<(), Error> {
        let n =
            u32::try_from(s.len()).map_err(|_| Error::Backend(Box::new(FieldTooLong(s.len()))))?;
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(s.as_bytes());
        Ok(())
    };
    put(out, &entry.warc_type)?;
    put(out, &entry.record_id)?;
    if let Some(u) = url {
        put(out, u)?;
    }
    Ok(())
}

/// Decodes an `ENTRIES` value. Strings are borrowed from `buf`. `Entry::validate` is not called.
///
/// # Errors
/// `Corrupt` on any malformed input.
pub(crate) fn decode_entry(pos: Position, buf: &[u8]) -> Result<Entry<'_>, Error> {
    let mut r = Reader::new(buf);
    if r.u8()? != ENTRY_FORMAT {
        return Err(corrupt("unknown entry format"));
    }
    let flags = r.u8()?;
    if flags & !1 != 0 {
        return Err(corrupt("unknown entry flags"));
    }
    let raw_size = r.u64()?;
    let digest = r.u64()?;
    let year = r.u16()?;
    let month = r.u8()?;
    let day = r.u8()?;
    let hour = r.u8()?;
    let minute = r.u8()?;
    let second = r.u8()?;
    let nanos = r.u32()?;
    let date = WarcDate::new(year, month, day, hour, minute, second, nanos)
        .ok_or_else(|| corrupt("invalid stored date"))?;
    let warc_type = r.str()?;
    let record_id = r.str()?;
    let url = if flags & 1 != 0 { Some(r.str()?) } else { None };
    r.finish()?;
    Ok(Entry {
        kind: Kind::Add,
        segment: pos.segment,
        offset: pos.offset,
        raw_size,
        digest,
        date,
        warc_type: Cow::Borrowed(warc_type),
        record_id: Cow::Borrowed(record_id),
        url: url.map(Cow::Borrowed),
    })
}

// ---------------------------------------------------------------------------------------------
// Collection record and slot value
// ---------------------------------------------------------------------------------------------

/// Stored form of a collection: its state plus the live slots of its current incarnation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CollRecord {
    /// Public state.
    pub(crate) state: CollectionState,
    /// Live `(generation, slot)` pairs, sorted by generation, generations unique.
    pub(crate) gens: Vec<(u32, u64)>,
}

impl CollRecord {
    /// Slot of generation `g`.
    pub(crate) fn slot_of(&self, g: u32) -> Option<u64> {
        self.gens
            .binary_search_by_key(&g, |(gen_, _)| *gen_)
            .ok()
            .and_then(|i| self.gens.get(i))
            .map(|(_, s)| *s)
    }

    /// Slot of the active generation.
    pub(crate) fn active_slot(&self) -> Option<u64> {
        self.state.active_generation.and_then(|g| self.slot_of(g))
    }

    /// Adds a live slot, keeping `gens` sorted. The generation must not already have one.
    pub(crate) fn push_slot(&mut self, g: u32, slot: u64) {
        let at = self
            .gens
            .binary_search_by_key(&g, |(gen_, _)| *gen_)
            .unwrap_or_else(|i| i);
        self.gens.insert(at, (g, slot));
    }

    /// Encodes the record (version 1).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let st = &self.state;
        let mut flags = 0u8;
        if st.active_generation.is_some() {
            flags |= 1;
        }
        if st.checkpoint.is_some() {
            flags |= 2;
        }
        if st.staged_checkpoint.is_some() {
            flags |= 4;
        }
        let mut v = Vec::with_capacity(
            self.gens
                .len()
                .saturating_mul(12)
                .saturating_add(2 + 4 + 12 + 12 + 4),
        );
        v.push(COLL_FORMAT);
        v.push(flags);
        if let Some(a) = st.active_generation {
            v.extend_from_slice(&a.to_be_bytes());
        }
        for cp in [st.checkpoint, st.staged_checkpoint].into_iter().flatten() {
            v.extend_from_slice(&cp.generation.to_be_bytes());
            v.extend_from_slice(&cp.cdx_offset.to_be_bytes());
        }
        // A collection never has 2^32 live generations (one slot per generation number).
        let n = u32::try_from(self.gens.len()).unwrap_or(u32::MAX);
        v.extend_from_slice(&n.to_be_bytes());
        for (g, s) in &self.gens {
            v.extend_from_slice(&g.to_be_bytes());
            v.extend_from_slice(&s.to_be_bytes());
        }
        v
    }

    /// Decodes a record.
    ///
    /// # Errors
    /// `Corrupt` on malformed input, unsorted generations, or an active generation without a slot.
    pub(crate) fn decode(buf: &[u8]) -> Result<CollRecord, Error> {
        let mut r = Reader::new(buf);
        if r.u8()? != COLL_FORMAT {
            return Err(corrupt("unknown collection record format"));
        }
        let flags = r.u8()?;
        if flags & !7 != 0 {
            return Err(corrupt("unknown collection record flags"));
        }
        let active_generation = if flags & 1 != 0 { Some(r.u32()?) } else { None };
        let mut cp = |present: bool| -> Result<Option<Checkpoint>, Error> {
            if present {
                Ok(Some(Checkpoint {
                    generation: r.u32()?,
                    cdx_offset: r.u64()?,
                }))
            } else {
                Ok(None)
            }
        };
        let checkpoint = cp(flags & 2 != 0)?;
        let staged_checkpoint = cp(flags & 4 != 0)?;
        let n = r.u32()?;
        let n = usize::try_from(n).map_err(|_| corrupt("collection record too large"))?;
        if n.checked_mul(12) != Some(r.remaining()) {
            return Err(corrupt("collection record length mismatch"));
        }
        let mut gens: Vec<(u32, u64)> = Vec::with_capacity(n);
        for _ in 0..n {
            let g = r.u32()?;
            let s = r.u64()?;
            if gens.last().is_some_and(|(prev, _)| *prev >= g) {
                return Err(corrupt("collection generations not strictly ascending"));
            }
            gens.push((g, s));
        }
        r.finish()?;
        let rec = CollRecord {
            state: CollectionState {
                active_generation,
                checkpoint,
                staged_checkpoint,
            },
            gens,
        };
        if active_generation.is_some() && rec.active_slot().is_none() {
            return Err(corrupt("active generation has no slot"));
        }
        Ok(rec)
    }
}

/// `SLOTS` value: `collection u128 ‖ generation u32`.
pub(crate) fn slot_value(collection: u128, generation: u32) -> [u8; 20] {
    concat(&[&collection.to_be_bytes(), &generation.to_be_bytes()])
}

/// Inverse of [`slot_value`].
///
/// # Errors
/// `Corrupt` if the length is not 20.
pub(crate) fn parse_slot_value(buf: &[u8]) -> Result<(u128, u32), Error> {
    let mut r = Reader::new(buf);
    let c = r.u128()?;
    let g = r.u32()?;
    r.finish()?;
    Ok((c, g))
}

#[cfg(test)]
mod tests {
    use super::*;
    use masstuffy_urlkey::{HostMatch, PathFilter, PortFilter, UrlPattern};

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
        }
        fn i128(&mut self) -> i128 {
            let hi = u128::from(self.next());
            let lo = u128::from(self.next());
            ((hi << 64) | lo).cast_signed()
        }
    }

    fn pos(segment: u32, offset: u64) -> Position {
        Position { segment, offset }
    }

    #[test]
    fn date_order_preserved() {
        let mut vals = vec![i128::MIN, -1, 0, 1, i128::MAX];
        let lo = WarcDate::new(0, 1, 1, 0, 0, 0, 0).unwrap();
        let hi = WarcDate::new(9999, 12, 31, 23, 59, 60, 999_999_999).unwrap();
        vals.push(lo.unix_nanos());
        vals.push(hi.unix_nanos());
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..200 {
            vals.push(rng.i128());
        }
        for &a in &vals {
            assert_eq!(dec_date(enc_date(a)), a);
            for &b in &vals {
                assert_eq!(a.cmp(&b), enc_date(a).cmp(&enc_date(b)));
            }
        }
        for _ in 0..10_000 {
            let (a, b) = (rng.i128() >> rng.below(100), rng.i128() >> rng.below(100));
            assert_eq!(a.cmp(&b), enc_date(a).cmp(&enc_date(b)));
            assert_eq!(dec_date(enc_date(a)), a);
        }
    }

    #[test]
    fn position_order_preserved() {
        let mut ps = vec![
            pos(0, 0),
            pos(0, 1),
            pos(1, 0),
            pos(u32::MAX, u64::MAX),
            pos(u32::MAX, 0),
            pos(0, u64::MAX),
        ];
        let mut rng = Rng(12345);
        for _ in 0..300 {
            ps.push(pos(
                u32::try_from(rng.next() >> 40).unwrap(),
                rng.next() >> rng.below(60),
            ));
        }
        for a in &ps {
            assert_eq!(position_from(&position_bytes(*a)), *a);
            for b in &ps {
                assert_eq!(a.cmp(b), position_bytes(*a).cmp(&position_bytes(*b)));
                assert_eq!(a.cmp(b), entry_key(7, *a).cmp(&entry_key(7, *b)));
            }
        }
    }

    fn pattern_bounds() -> Vec<String> {
        let mut out = vec![
            String::new(),
            "a".into(),
            "a\0".into(),
            "a\0b".into(),
            "\0".into(),
            format!("com,a){}", char::MAX),
        ];
        let pats = [
            UrlPattern::new(
                "example.com",
                HostMatch::Exact,
                PortFilter::Any,
                PathFilter::Exact("/a/b"),
            ),
            UrlPattern::new(
                "example.com",
                HostMatch::WithSubdomains,
                PortFilter::Explicit(8080),
                PathFilter::Prefix("/x"),
            ),
            UrlPattern::new(
                "127.0.0.1",
                HostMatch::Exact,
                PortFilter::Any,
                PathFilter::Any,
            ),
            UrlPattern::new(
                "example.com",
                HostMatch::Exact,
                PortFilter::Default,
                PathFilter::Any,
            ),
        ];
        for p in pats.into_iter().map(Result::unwrap) {
            for r in p.key_ranges() {
                out.push(r.start.clone());
                out.push(r.end.clone());
            }
        }
        out
    }

    #[test]
    fn enc_bound_matches_string_order() {
        let alphabet = b"ab,)/:";
        let mut rng = Rng(777);
        let mut bounds = pattern_bounds();
        for _ in 0..300 {
            let n = rng.below(8);
            let s: String = (0..n)
                .map(|_| {
                    if rng.below(10) == 0 {
                        '\0'
                    } else {
                        char::from(*alphabet.get(rng.below(alphabet.len())).unwrap())
                    }
                })
                .collect();
            bounds.push(s);
        }
        let mut keys: Vec<String> = Vec::new();
        for _ in 0..300 {
            let n = 1 + rng.below(12);
            let s: String = (0..n)
                .map(|_| {
                    if rng.below(3) == 0 {
                        char::from(0x21 + u8::try_from(rng.below(94)).unwrap())
                    } else {
                        char::from(*alphabet.get(rng.below(alphabet.len())).unwrap())
                    }
                })
                .collect();
            keys.push(s);
        }
        for b in &bounds {
            keys.push(b.chars().filter(|c| *c >= '\u{2}').collect());
        }
        let eb: Vec<Vec<u8>> = bounds.iter().map(|b| enc_bound(b)).collect();
        for k in &keys {
            if k.is_empty() {
                continue;
            }
            for (b, e) in bounds.iter().zip(&eb) {
                for _ in 0..3 {
                    let n = rng.below(31);
                    let sigma: Vec<u8> = (0..n)
                        .map(|_| match rng.below(4) {
                            0 => 0,
                            1 => 0xFF,
                            _ => u8::try_from(rng.next() & 0xFF).unwrap(),
                        })
                        .collect();
                    let mut row = k.as_bytes().to_vec();
                    row.push(0);
                    row.extend_from_slice(&sigma);
                    assert_eq!(
                        k.as_bytes() < b.as_bytes(),
                        row.as_slice() < e.as_slice(),
                        "k={k:?} b={b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn url_part_prefix_free() {
        let m = url_part(&UrlKey::Massaged("a".into())).unwrap();
        let r = url_part(&UrlKey::Raw("a")).unwrap();
        assert_ne!(m, r);
        assert!(!m.starts_with(&r) && !r.starts_with(&m));
        let mx = url_part(&UrlKey::Massaged("ax".into())).unwrap();
        assert!(!mx.starts_with(&m));
        let urls: Vec<String> = (0..300).map(|n| "u".repeat(n)).collect();
        let parts: Vec<Vec<u8>> = urls
            .iter()
            .map(|u| url_part(&UrlKey::Raw(u)).unwrap())
            .collect();
        for (i, a) in parts.iter().enumerate() {
            for (j, b) in parts.iter().enumerate() {
                if i != j {
                    assert!(!b.starts_with(a));
                }
            }
        }
        assert!(url_part(&UrlKey::Massaged("a b".into())).is_err());
        assert!(url_part(&UrlKey::Massaged("a\u{e9}".into())).is_err());
    }

    #[test]
    fn by_id_key_round_trip_and_rejects() {
        let k = by_id_key("abc", 9, pos(3, 4));
        assert_eq!(parse_by_id_key(&k).unwrap(), (9, pos(3, 4)));
        assert!(parse_by_id_key(&k[..k.len() - 1]).is_err());
        assert!(parse_by_id_key(&k[..20]).is_err());
        let mut bad = k.clone();
        bad[3] = 1;
        assert!(parse_by_id_key(&bad).is_err());
        let (lo, hi) = by_id_bounds("abc");
        let inside = |key: &[u8]| key >= lo.as_slice() && key < hi.as_slice();
        assert!(inside(&k));
        assert!(!inside(&by_id_key("abcd", 0, pos(0, 0))));
        assert!(!inside(&by_id_key("ab", u64::MAX, pos(u32::MAX, u64::MAX))));
    }

    #[test]
    fn massaged_row_round_trip_and_rejects() {
        let part = url_part(&UrlKey::Massaged("com,a)/x".into())).unwrap();
        let prefix = url_prefix(5, &part);
        let row = url_row(&prefix, enc_date(-42), pos(1, 2));
        let (k, d, p) = parse_massaged_row(&row).unwrap();
        assert_eq!((k, d, p), ("com,a)/x", -42, pos(1, 2)));
        assert_eq!(row_tail(&row).unwrap(), (-42, pos(1, 2)));
        for n in 0..row.len() {
            // Every truncation either fails or (when it still looks like a row) never panics.
            let _ = parse_massaged_row(&row[..n]);
        }
        assert!(parse_massaged_row(&row[..30]).is_err());
        let mut wrong_class = row.clone();
        wrong_class[8] = 1;
        assert!(parse_massaged_row(&wrong_class).is_err());
        let mut no_term = row.clone();
        let i = no_term.len() - TAIL - 1;
        no_term[i] = b'x';
        assert!(parse_massaged_row(&no_term).is_err());
        let mut bad_utf8 = row.clone();
        bad_utf8[10] = 0xFF;
        assert!(parse_massaged_row(&bad_utf8).is_err());
    }

    fn sample(url: Option<&str>, date: WarcDate) -> Entry<'static> {
        Entry {
            kind: Kind::Add,
            segment: 7,
            offset: 99,
            raw_size: u64::MAX,
            digest: u64::MAX,
            date,
            warc_type: Cow::Owned("response".into()),
            record_id: Cow::Owned("urn:uuid:1".into()),
            url: url.map(|u| Cow::Owned(u.into())),
        }
    }

    #[test]
    fn entry_value_round_trip() {
        let dates = [
            WarcDate::new(2024, 2, 29, 23, 59, 60, 999_999_999).unwrap(),
            WarcDate::new(0, 1, 1, 0, 0, 0, 0).unwrap(),
        ];
        for d in dates {
            for url in [None, Some("http://h/p"), Some("urn:x\u{e9}\u{4e2d}")] {
                let mut e = sample(url, d);
                for (rs, dg) in [(1, 0), (u64::MAX, u64::MAX)] {
                    e.raw_size = rs;
                    e.digest = dg;
                    let mut buf = Vec::new();
                    encode_entry(&e, &mut buf).unwrap();
                    assert_eq!(decode_entry(pos(7, 99), &buf).unwrap(), e);
                }
            }
        }
    }

    #[test]
    fn entry_value_rejects_malformed() {
        let e = sample(
            Some("http://h/p"),
            WarcDate::new(2020, 5, 6, 7, 8, 9, 10).unwrap(),
        );
        let mut buf = Vec::new();
        encode_entry(&e, &mut buf).unwrap();
        let is_corrupt = |b: &[u8]| matches!(decode_entry(pos(0, 0), b), Err(Error::Corrupt(_)));
        for n in 0..buf.len() {
            assert!(is_corrupt(&buf[..n]), "prefix {n}");
        }
        let mut extra = buf.clone();
        extra.push(0);
        assert!(is_corrupt(&extra));
        let mut m = buf.clone();
        m[0] = 2;
        assert!(is_corrupt(&m));
        let mut m = buf.clone();
        m[1] |= 2;
        assert!(is_corrupt(&m));
        let mut m = buf.clone();
        m[20] = 13; // month
        assert!(is_corrupt(&m));
        let mut m = buf.clone();
        m[21] = 0; // day
        assert!(is_corrupt(&m));
        // warc_type length field starts at 29; its bytes at 33.
        let mut m = buf.clone();
        m[33] = 0xFF;
        assert!(is_corrupt(&m));
        let mut m = buf.clone();
        m[29..33].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(is_corrupt(&m));
    }

    #[test]
    fn coll_record_round_trip() {
        let cp = |g, o| Checkpoint {
            generation: g,
            cdx_offset: o,
        };
        for mask in 0..8u8 {
            for n in [0usize, 1, 5] {
                let gens: Vec<(u32, u64)> = (0..n)
                    .map(|i| {
                        (
                            u32::try_from(i).unwrap() + 1,
                            u64::try_from(i).unwrap() + 10,
                        )
                    })
                    .collect();
                let active = if mask & 1 != 0 && n > 0 {
                    Some(1)
                } else {
                    None
                };
                let rec = CollRecord {
                    state: CollectionState {
                        active_generation: active,
                        checkpoint: (mask & 2 != 0).then(|| cp(1, 5)),
                        staged_checkpoint: (mask & 4 != 0).then(|| cp(2, u64::MAX)),
                    },
                    gens,
                };
                assert_eq!(CollRecord::decode(&rec.encode()).unwrap(), rec);
            }
        }
    }

    #[test]
    fn coll_record_rejects_malformed() {
        let rec = CollRecord {
            state: CollectionState {
                active_generation: Some(2),
                checkpoint: Some(Checkpoint {
                    generation: 2,
                    cdx_offset: 3,
                }),
                staged_checkpoint: None,
            },
            gens: vec![(2, 10), (3, 11)],
        };
        let buf = rec.encode();
        let is_corrupt = |b: &[u8]| matches!(CollRecord::decode(b), Err(Error::Corrupt(_)));
        for n in 0..buf.len() {
            assert!(is_corrupt(&buf[..n]), "prefix {n}");
        }
        let mut t = buf.clone();
        t.push(0);
        assert!(is_corrupt(&t));
        let mut f = buf.clone();
        f[1] |= 8;
        assert!(is_corrupt(&f));
        for gens in [vec![(3, 1), (2, 2)], vec![(2, 1), (2, 2)]] {
            let r = CollRecord {
                state: CollectionState::default(),
                gens,
            };
            assert!(is_corrupt(&r.encode()));
        }
        let no_slot = CollRecord {
            state: CollectionState {
                active_generation: Some(9),
                ..CollectionState::default()
            },
            gens: vec![(2, 1)],
        };
        assert!(is_corrupt(&no_slot.encode()));
        let mut huge = vec![COLL_FORMAT, 0];
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(is_corrupt(&huge));
    }

    #[test]
    fn slot_value_round_trip() {
        let v = slot_value(u128::MAX - 3, 77);
        assert_eq!(parse_slot_value(&v).unwrap(), (u128::MAX - 3, 77));
        assert!(parse_slot_value(&v[..19]).is_err());
        let mut long = v.to_vec();
        long.push(0);
        assert!(parse_slot_value(&long).is_err());
    }

    #[test]
    fn cursor_bound_cases() {
        let slot = 4;
        let part = url_part(&UrlKey::Massaged("com,a)/x".into())).unwrap();
        let prefix = url_prefix(slot, &part);
        let cur = |key: &str, n: i128, p: Position| Cursor {
            key: key.to_owned(),
            date_nanos: n,
            position: p,
        };
        let after = |b: &Bound<Vec<u8>>, row: &[u8]| match b {
            Bound::Excluded(k) => row > k.as_slice(),
            Bound::Included(k) => row >= k.as_slice(),
            Bound::Unbounded => true,
        };
        let b = cursor_bound(slot, &cur("com,a)/x", 10, pos(1, 5)));
        assert!(matches!(b, Bound::Excluded(_)));
        assert!(!after(&b, &url_row(&prefix, enc_date(10), pos(1, 5))));
        assert!(after(&b, &url_row(&prefix, enc_date(10), pos(1, 6))));
        assert!(!after(&b, &url_row(&prefix, enc_date(10), pos(1, 4))));
        assert!(after(&b, &url_row(&prefix, enc_date(11), pos(0, 0))));
        assert!(!after(&b, &url_row(&prefix, enc_date(9), pos(9, 9))));
        // An empty key sorts before every real row.
        let b = cursor_bound(slot, &cur("", i128::MIN, pos(0, 0)));
        assert!(after(&b, &url_row(&prefix, enc_date(i128::MIN), pos(0, 0))));
        // A key outside the charset: rows with a greater key come after, smaller ones do not.
        let b = cursor_bound(slot, &cur("com,a)/x y", 0, pos(0, 0)));
        assert!(matches!(b, Bound::Included(_)));
        let greater = url_part(&UrlKey::Massaged("com,a)/x!".into())).unwrap();
        let smaller = url_part(&UrlKey::Massaged("com,a)/x".into())).unwrap();
        assert!(after(
            &b,
            &url_row(&url_prefix(slot, &greater), enc_date(0), pos(0, 0))
        ));
        assert!(!after(
            &b,
            &url_row(&url_prefix(slot, &smaller), enc_date(0), pos(0, 0))
        ));
    }
}
