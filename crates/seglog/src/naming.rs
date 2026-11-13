//! Segment file naming and directory scan.

use std::ffi::OsStr;
use std::fs::{self, FileType};
use std::path::Path;
use std::str::FromStr;

use crate::error::{Error, Op, io};

/// How the segment files of one log are named: `<stem>.<N>.<ext>`.
///
/// Invariants: `stem` and `ext` are non-empty, contain neither `/` nor NUL, and `stem` does not
/// start with `.` (so no segment name can collide with `masstuffy_durable`'s reserved names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Naming {
    stem: String,
    ext: String,
}

/// Result of matching a directory entry name against a [`Naming`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    NotOurs,
    Segment(u32),
    Malformed,
}

impl Naming {
    /// Builds a naming.
    ///
    /// # Errors
    ///
    /// `InvalidNaming` if `stem` or `ext` is empty or contains `/` or NUL, or if `stem` starts
    /// with `.`.
    pub fn new(stem: impl Into<String>, ext: impl Into<String>) -> Result<Naming, Error> {
        let stem = stem.into();
        let ext = ext.into();
        let bad = |s: &str| s.is_empty() || s.contains(['/', '\0']);
        if bad(&stem) || bad(&ext) || stem.starts_with('.') {
            return Err(Error::InvalidNaming { stem, ext });
        }
        Ok(Naming { stem, ext })
    }

    /// The stem.
    #[must_use]
    pub fn stem(&self) -> &str {
        &self.stem
    }

    /// The extension (may contain dots, e.g. `warc.zst`).
    #[must_use]
    pub fn ext(&self) -> &str {
        &self.ext
    }

    /// Parses a directory entry name.
    pub(crate) fn parse(&self, name: &OsStr) -> Parsed {
        if masstuffy_durable::is_reserved(name) {
            return Parsed::NotOurs;
        }
        let b = name.as_encoded_bytes();
        let Some(rest) = b
            .strip_prefix(self.stem.as_bytes())
            .and_then(|r| r.strip_prefix(b"."))
        else {
            return Parsed::NotOurs;
        };
        // Strip the suffix from the remainder, so prefix and suffix never overlap.
        let Some(mid) = rest
            .strip_suffix(self.ext.as_bytes())
            .and_then(|r| r.strip_suffix(b"."))
        else {
            return Parsed::NotOurs;
        };
        if mid.is_empty() || !mid.iter().all(u8::is_ascii_digit) {
            return Parsed::NotOurs;
        }
        if mid.starts_with(b"0") {
            return Parsed::Malformed;
        }
        // Ascii digits only, so valid UTF-8.
        let Ok(s) = std::str::from_utf8(mid) else {
            return Parsed::Malformed;
        };
        match u32::from_str(s) {
            Ok(n) if n >= 1 => Parsed::Segment(n),
            _ => Parsed::Malformed,
        }
    }
}

/// Calls `f(n, file_type, path)` for every entry of `dir` that parses as `Segment(n)`.
///
/// # Errors
///
/// `Io{ReadDir}` on `dir`; `Io{Stat}` on an entry; `BadSegmentName` on the first malformed entry;
/// any error returned by `f`.
pub(crate) fn scan(
    dir: &Path,
    naming: &Naming,
    f: &mut dyn FnMut(u32, FileType, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    let rd = fs::read_dir(dir).map_err(io(Op::ReadDir, dir))?;
    for entry in rd {
        let entry = entry.map_err(io(Op::ReadDir, dir))?;
        match naming.parse(&entry.file_name()) {
            Parsed::NotOurs => {}
            Parsed::Malformed => return Err(Error::BadSegmentName { path: entry.path() }),
            Parsed::Segment(n) => {
                let path = entry.path();
                let ft = entry.file_type().map_err(io(Op::Stat, &path))?;
                f(n, ft, &path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn n() -> Naming {
        Naming::new("seg.1", "warc").unwrap()
    }

    #[test]
    fn naming_rejects_invalid() {
        for (s, e) in [
            ("", "warc"),
            ("seg", ""),
            ("a/b", "warc"),
            ("seg", "w/x"),
            ("a\0", "warc"),
            (".tmp-x", "warc"),
            (".hidden", "warc"),
        ] {
            assert!(matches!(
                Naming::new(s, e),
                Err(Error::InvalidNaming { .. })
            ));
        }
        assert!(Naming::new("seg.1", "warc.zst").is_ok());
    }

    #[test]
    fn parse_table() {
        let nm = n();
        let p = |s: &str| nm.parse(OsStr::new(s));
        assert_eq!(p("seg.1.3.warc"), Parsed::Segment(3));
        assert_eq!(p("seg.1.4294967295.warc"), Parsed::Segment(u32::MAX));
        for s in ["seg.1.03.warc", "seg.1.0.warc", "seg.1.4294967296.warc"] {
            assert_eq!(p(s), Parsed::Malformed, "{s}");
        }
        for s in [
            "seg.1.warc",
            "seg.1..warc",
            "seg.1.3.warc.zst",
            "seg.12.3.warc",
            "seg.1.3.x.warc",
            "index.1.cdx",
            ".lock",
            ".tmp-1-2",
        ] {
            assert_eq!(p(s), Parsed::NotOurs, "{s}");
        }
        assert_eq!(
            nm.parse(OsStr::from_bytes(b"seg.1.\xff.warc")),
            Parsed::NotOurs
        );
    }

    #[test]
    fn parse_multi_dot_ext() {
        let nm = Naming::new("seg.2", "warc.zst").unwrap();
        assert_eq!(nm.parse(OsStr::new("seg.2.7.warc.zst")), Parsed::Segment(7));
        assert_eq!(nm.parse(OsStr::new("seg.2.7.warc")), Parsed::NotOurs);
    }
}
