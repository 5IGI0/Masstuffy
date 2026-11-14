//! WARC format version.

const LINE_1_0: &[u8] = b"WARC/1.0\r\n";
const LINE_1_1: &[u8] = b"WARC/1.1\r\n";

/// WARC format version of a record, taken from its first line.
///
/// Invariant: the version line of a record is exactly `WARC/1.0\r\n` or `WARC/1.1\r\n` (10 bytes each).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Version {
    /// `WARC/1.0` (ISO 28500:2009).
    V1_0,
    /// `WARC/1.1` (ISO 28500:2017). What Masstuffy writes by default.
    #[default]
    V1_1,
}

impl Version {
    /// Length of every version line (10).
    pub(crate) const LINE_LEN: usize = 10;

    /// `"WARC/1.0"` or `"WARC/1.1"` (without CRLF).
    pub const fn as_str(self) -> &'static str {
        match self {
            Version::V1_0 => "WARC/1.0",
            Version::V1_1 => "WARC/1.1",
        }
    }

    /// The full version line including CRLF: `b"WARC/1.0\r\n"` / `b"WARC/1.1\r\n"`.
    pub(crate) const fn line(self) -> &'static [u8] {
        match self {
            Version::V1_0 => LINE_1_0,
            Version::V1_1 => LINE_1_1,
        }
    }

    /// `Some(v)` iff `line` is exactly one of the two version lines (CRLF included).
    pub(crate) fn from_line(line: &[u8]) -> Option<Version> {
        if line == LINE_1_0 {
            Some(Version::V1_0)
        } else if line == LINE_1_1 {
            Some(Version::V1_1)
        } else {
            None
        }
    }

    /// True iff `prefix` is a proper or full prefix of at least one version line.
    /// Used to tell `Truncated` from `InvalidVersion`. `b""` is a prefix.
    pub(crate) fn is_line_prefix(prefix: &[u8]) -> bool {
        LINE_1_0.starts_with(prefix) || LINE_1_1.starts_with(prefix)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines() {
        for (v, s) in [(Version::V1_0, "WARC/1.0"), (Version::V1_1, "WARC/1.1")] {
            assert_eq!(v.as_str(), s);
            assert_eq!(v.line(), format!("{s}\r\n").as_bytes());
            assert_eq!(Version::LINE_LEN, v.line().len());
            assert_eq!(v.to_string(), s);
        }
        assert_eq!(Version::default(), Version::V1_1);
        assert!(Version::V1_0 < Version::V1_1);
    }

    #[test]
    fn from_line_and_prefix() {
        assert_eq!(Version::from_line(b"WARC/1.0\r\n"), Some(Version::V1_0));
        assert_eq!(Version::from_line(b"WARC/1.1\r\n"), Some(Version::V1_1));
        for bad in [
            &b"WARC/1.0"[..],
            b"WARC/1.0\n",
            b"warc/1.0\r\n",
            b"WARC/1.0\r\nX",
            b"",
        ] {
            assert_eq!(Version::from_line(bad), None, "{bad:?}");
        }
        for ok in [
            &b""[..],
            b"W",
            b"WARC/1.",
            b"WARC/1.0",
            b"WARC/1.1\r",
            b"WARC/1.0\r\n",
            b"WARC/1.1\r\n",
        ] {
            assert!(Version::is_line_prefix(ok), "{ok:?}");
        }
        for bad in [&b"X"[..], b"WARC/1.2", b"WARC/2", b"WARC/1.0\r\n "] {
            assert!(!Version::is_line_prefix(bad), "{bad:?}");
        }
    }

    #[test]
    fn types_are_send_sync() {
        fn check<T: Send + Sync + Copy>() {}
        check::<Version>();
    }
}
