//! Pagination cursor of pattern queries.

use crate::error::Error;
use crate::types::{Hit, Position};

/// Longest accepted cursor token, in bytes.
pub const MAX_TOKEN_LEN: usize = 262_144;

/// Position in the pattern order, strictly after which the next page starts.
///
/// Field order = pattern order, so the derived `Ord` is `(key, date_nanos, segment, offset)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    /// Massaged URL key.
    pub key: String,
    /// `WarcDate::unix_nanos()` of the entry.
    pub date_nanos: i128,
    /// Locator of the entry.
    pub position: Position,
}

impl Cursor {
    /// Cursor of `hit`; `key` is the massaged key of the hit (the caller passes it).
    #[must_use]
    pub fn of(key: &str, hit: &Hit) -> Cursor {
        Cursor {
            key: key.to_owned(),
            date_nanos: hit.entry.date.unix_nanos(),
            position: hit.position(),
        }
    }

    /// `"<date_nanos>.<segment>.<offset>.<key>"` (decimal, `-` sign only for negative nanos).
    #[must_use]
    pub fn to_token(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            self.date_nanos, self.position.segment, self.position.offset, self.key
        )
    }

    /// Inverse of [`to_token`](Self::to_token).
    ///
    /// # Errors
    /// `Error::InvalidCursor` for a token longer than [`MAX_TOKEN_LEN`], with fewer than 4 parts,
    /// unparseable numbers, an empty key, a key byte outside `0x21..=0x7E`, or any non-canonical
    /// token (`from_token(t)?.to_token() != t`, e.g. `+1`, `01`).
    pub fn from_token(token: &str) -> Result<Cursor, Error> {
        if token.len() > MAX_TOKEN_LEN {
            return Err(Error::InvalidCursor);
        }
        let mut parts = token.splitn(4, '.');
        let (Some(n), Some(s), Some(o), Some(key)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::InvalidCursor);
        };
        if key.is_empty() || !key.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(Error::InvalidCursor);
        }
        let cursor = Cursor {
            key: key.to_owned(),
            date_nanos: n.parse().map_err(|_| Error::InvalidCursor)?,
            position: Position {
                segment: s.parse().map_err(|_| Error::InvalidCursor)?,
                offset: o.parse().map_err(|_| Error::InvalidCursor)?,
            },
        };
        if cursor.to_token() != token {
            return Err(Error::InvalidCursor);
        }
        Ok(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(key: &str, n: i128, segment: u32, offset: u64) -> Cursor {
        Cursor {
            key: key.to_owned(),
            date_nanos: n,
            position: Position { segment, offset },
        }
    }

    #[test]
    fn token_round_trip() {
        for key in ["com,example)/a?b=1&c=2", "127.0.0.1)/x.y", "[::1]:8080)/"] {
            for n in [
                0i128,
                -5,
                1_700_000_000_000_000_000,
                -62_167_219_200_000_000_000,
            ] {
                let c = cur(key, n, u32::MAX, u64::MAX);
                assert_eq!(Cursor::from_token(&c.to_token()).unwrap(), c);
            }
        }
    }

    #[test]
    fn token_rejects() {
        let long = format!("1.2.3.{}", "k".repeat(MAX_TOKEN_LEN));
        for t in [
            "",
            "1.2.3",
            "1.2.3.",
            "x.2.3.k",
            "1.-2.3.k",
            "+1.2.3.k",
            "01.2.3.k",
            "1.2.3.k y",
            "1.2.3.k\u{e9}",
            "1.2.3.k\u{7f}",
            "-0.2.3.k",
            long.as_str(),
        ] {
            assert!(
                matches!(Cursor::from_token(t), Err(Error::InvalidCursor)),
                "{t:?}"
            );
        }
    }

    #[test]
    fn cursor_order_is_pattern_order() {
        assert!(cur("a", 9, 9, 9) < cur("b", 0, 0, 0));
        assert!(cur("a", 1, 9, 9) < cur("a", 2, 0, 0));
        assert!(cur("a", 1, 1, 9) < cur("a", 1, 2, 0));
        assert!(cur("a", 1, 1, 1) < cur("a", 1, 1, 2));
    }
}
