//! Collection identifier.

use std::fmt;
use std::str::FromStr;

use crate::error::ManifestError;

/// Canonical UUID of a collection (lowercase, hyphenated, 36 chars). Any version is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CollectionId(u128);

impl CollectionId {
    /// From the 128-bit value.
    #[must_use]
    pub const fn from_u128(v: u128) -> CollectionId {
        CollectionId(v)
    }

    /// The 128-bit value.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl FromStr for CollectionId {
    type Err = ManifestError;

    /// Exactly 36 bytes, `-` at 8, 13, 18, 23, lowercase hex elsewhere.
    fn from_str(s: &str) -> Result<CollectionId, ManifestError> {
        let b = s.as_bytes();
        if b.len() != 36 {
            return Err(ManifestError::InvalidUuid);
        }
        let mut v: u128 = 0;
        for (i, &c) in b.iter().enumerate() {
            if matches!(i, 8 | 13 | 18 | 23) {
                if c != b'-' {
                    return Err(ManifestError::InvalidUuid);
                }
                continue;
            }
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => return Err(ManifestError::InvalidUuid),
            };
            v = (v << 4) | u128::from(d);
        }
        Ok(CollectionId(v))
    }
}

impl fmt::Display for CollectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            v >> 96,
            (v >> 80) & 0xffff,
            (v >> 64) & 0xffff,
            (v >> 48) & 0xffff,
            v & 0xffff_ffff_ffff
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_round_trip() {
        let s = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";
        let id: CollectionId = s.parse().unwrap();
        assert_eq!(id.to_string(), s);
        for x in [0u128, u128::MAX, 0x0123_4567_89ab_cdef_0011_2233_4455_6677] {
            let id = CollectionId::from_u128(x);
            assert_eq!(id.to_string().parse::<CollectionId>().unwrap(), id);
            assert_eq!(id.as_u128(), x);
        }
    }

    #[test]
    fn uuid_rejects() {
        for s in [
            "1B4E28BA-2FA1-41D2-883F-0016D3CCA427",
            "1b4e28ba-2fa1-41d2-883f-0016d3cca42",
            "1b4e28ba-2fa1-41d2-883f-0016d3cca4277",
            "1b4e28ba2-fa1-41d2-883f-0016d3cca427",
            "1b4e28ba-2fa1-41d2-883f0-016d3cca427",
            "1b4e28ba2fa141d2883f0016d3cca427----",
            "1b4e28ba-2fa1-41d2-883f-0016d3cca42g",
            "{1b4e28ba-2fa1-41d2-883f-0016d3cca427}",
            "urn:uuid:1b4e28ba-2fa1-41d2-883f-0016d3cca427",
            "",
            "1b4e28ba-2fa1-41d2-883f-0016d3cca4é",
            "é1b4e28ba-2fa1-41d2-883f-0016d3cca42",
        ] {
            assert!(
                matches!(s.parse::<CollectionId>(), Err(ManifestError::InvalidUuid)),
                "{s}"
            );
        }
    }
}
