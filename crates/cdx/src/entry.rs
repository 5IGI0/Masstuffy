//! The index entry.

use std::borrow::Cow;

use masstuffy_warc::WarcDate;

use crate::error::EntryError;

/// Kind of an index line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A record was added.
    Add,
    /// The record added at the same `(segment, offset)` earlier in the file is deleted.
    Tombstone,
}

/// One index line.
///
/// Borrowed (`Cow::Borrowed`) when read (zero-copy from the reader's line buffer) or built from a
/// parsed WARC record; use [`Entry::into_owned`] to keep it past the buffer. Fields are public:
/// [`Entry::validate`] checks them, `encode_entry` calls it, and every parsed entry satisfies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Add or tombstone. A tombstone repeats all fields of the Add it deletes.
    pub kind: Kind,
    /// Segment number in the seglog.
    pub segment: u32,
    /// Byte offset in the segment.
    pub offset: u64,
    /// Stored size (frame size if compressed); at least 1.
    pub raw_size: u64,
    /// xxh3-64 of the `raw_size` stored bytes (see [`digest`](crate::digest)).
    pub digest: u64,
    /// `WARC-Date`.
    pub date: WarcDate,
    /// `WARC-Type`: non-empty, no byte `<= 0x20` or `0x7F`.
    pub warc_type: Cow<'a, str>,
    /// `WARC-Record-ID` without `<>`: non-empty, no byte `<= 0x20` or `0x7F`.
    pub record_id: Cow<'a, str>,
    /// `WARC-Target-URI` verbatim, `None` if absent. `Some("")` is invalid: callers map an empty
    /// `WARC-Target-URI` to `None`.
    pub url: Option<Cow<'a, str>>,
}

fn is_atom(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b > 0x20 && b != 0x7F)
}

impl Entry<'_> {
    /// Checks the invariants of the entry.
    ///
    /// # Errors
    /// `Empty` or `InvalidAtom` for `warc_type`/`record_id`, `Empty("url")` for `Some("")`,
    /// `ZeroRawSize` and `LocatorOverflow`.
    pub fn validate(&self) -> Result<(), EntryError> {
        for (name, v) in [
            ("warc_type", &*self.warc_type),
            ("record_id", &*self.record_id),
        ] {
            if v.is_empty() {
                return Err(EntryError::Empty(name));
            }
            if !is_atom(v) {
                return Err(EntryError::InvalidAtom(name));
            }
        }
        if self.url.as_deref() == Some("") {
            return Err(EntryError::Empty("url"));
        }
        if self.raw_size == 0 {
            return Err(EntryError::ZeroRawSize);
        }
        if self.offset.checked_add(self.raw_size).is_none() {
            return Err(EntryError::LocatorOverflow);
        }
        Ok(())
    }

    /// `offset + raw_size` (never `None` on a validated entry).
    #[must_use]
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.raw_size)
    }

    /// Copies the borrowed fields so the entry no longer borrows.
    #[must_use]
    pub fn into_owned(self) -> Entry<'static> {
        Entry {
            kind: self.kind,
            segment: self.segment,
            offset: self.offset,
            raw_size: self.raw_size,
            digest: self.digest,
            date: self.date,
            warc_type: Cow::Owned(self.warc_type.into_owned()),
            record_id: Cow::Owned(self.record_id.into_owned()),
            url: self.url.map(|u| Cow::Owned(u.into_owned())),
        }
    }
}
