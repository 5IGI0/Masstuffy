//! base64url (RFC 4648 section 5) without padding, canonical form only.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encodes `data` (no `=` padding).
pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    let sym = |v: u32| ALPHABET.get((v & 63) as usize).copied().unwrap_or(b'A');
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(v >> 18));
        out.push(sym(v >> 12));
        if chunk.len() > 1 {
            out.push(sym(v >> 6));
        }
        if chunk.len() > 2 {
            out.push(sym(v));
        }
    }
    // ALPHABET is ASCII, so the bytes are valid UTF-8; the lossy form never alters them.
    String::from_utf8_lossy(&out).into_owned()
}

fn value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Decodes strictly: alphabet only, no padding, length not 1 mod 4, zero unused trailing bits.
pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4 * 3 + 3);
    for chunk in b.chunks(4) {
        let mut v = 0u32;
        for &c in chunk {
            v = (v << 6) | value(c)?;
        }
        let n = chunk.len();
        // Left-align the sextets of a short final group in 24 bits.
        v <<= 6 * u32::try_from(4 - n).ok()?;
        let [_, x, y, z] = v.to_be_bytes();
        match n {
            4 => out.extend_from_slice(&[x, y, z]),
            3 => {
                if z != 0 {
                    return None;
                }
                out.extend_from_slice(&[x, y]);
            }
            2 => {
                if y != 0 || z != 0 {
                    return None;
                }
                out.push(x);
            }
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_rfc4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, enc) in cases {
            assert_eq!(encode(plain.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn b64_roundtrip_all_lengths_0_to_64() {
        for n in 0..=64u8 {
            let data: Vec<u8> = (0..n).map(|i| i.wrapping_mul(37).wrapping_add(n)).collect();
            let e = encode(&data);
            assert!(e.bytes().all(|c| value(c).is_some()));
            assert_eq!(decode(&e).unwrap(), data);
        }
    }

    #[test]
    fn b64_rejects() {
        for bad in ["Zg==", "Zm9+", "Zm9/", "Zm 9", "Z", "Zm9vY", "Zh", "Zm-"] {
            assert!(decode(bad).is_none(), "{bad}");
        }
        assert!(decode("Zg").is_some());
        assert!(decode("Zm8").is_some());
        assert!(decode("Zm9").is_none());
    }
}
