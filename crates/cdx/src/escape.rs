//! Escaping of the `url` field.

use std::borrow::Cow;

use crate::error::{EntryError, LineError};

/// Bytes written as `%XX`: controls, space, `%` and DEL.
fn needs_escape(b: u8) -> bool {
    b <= 0x20 || b == b'%' || b == 0x7F
}

fn hex_upper(n: u8) -> u8 {
    match n & 0xF {
        d @ 0..=9 => b'0' + d,
        d => b'A' + (d - 10),
    }
}

fn unhex_upper(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Appends the escaped form of a present url (the value `-` becomes `%2D`).
pub(crate) fn escape_url(s: &str, out: &mut Vec<u8>) {
    if s == "-" {
        out.extend_from_slice(b"%2D");
        return;
    }
    out.reserve(s.len());
    for &b in s.as_bytes() {
        if needs_escape(b) {
            out.extend_from_slice(&[b'%', hex_upper(b >> 4), hex_upper(b)]);
        } else {
            out.push(b);
        }
    }
}

/// Strict inverse of [`escape_url`]; the field `-` is `None`.
pub(crate) fn unescape_url(field: &str) -> Result<Option<Cow<'_, str>>, LineError> {
    if field == "-" {
        return Ok(None);
    }
    if field == "%2D" {
        return Ok(Some(Cow::Borrowed("-")));
    }
    if field.is_empty() {
        return Err(EntryError::Empty("url").into());
    }
    if !field.contains('%') {
        // Raw bytes that must be escaped are not accepted: one encoding per value.
        if field.bytes().any(needs_escape) {
            return Err(LineError::InvalidEscape);
        }
        return Ok(Some(Cow::Borrowed(field)));
    }
    let mut out = Vec::with_capacity(field.len());
    let mut it = field.bytes();
    while let Some(b) = it.next() {
        if b != b'%' {
            if needs_escape(b) {
                return Err(LineError::InvalidEscape);
            }
            out.push(b);
            continue;
        }
        let hi = it.next().and_then(unhex_upper);
        let lo = it.next().and_then(unhex_upper);
        let (Some(hi), Some(lo)) = (hi, lo) else {
            return Err(LineError::InvalidEscape);
        };
        let v = (hi << 4) | lo;
        if !needs_escape(v) {
            return Err(LineError::InvalidEscape);
        }
        out.push(v);
    }
    // Only ASCII bytes were substituted into valid UTF-8, so this cannot fail.
    let s = String::from_utf8(out).map_err(|_| LineError::InvalidEscape)?;
    Ok(Some(Cow::Owned(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        escape_url(s, &mut v);
        v
    }

    #[test]
    fn escape_roundtrip_specials() {
        for s in [
            "a b", "a\nb", "a\r\nb", "a\tb", "100%", "%20", "\u{7f}", "\0", "ü/ä b", "-", "a-b",
            "--",
        ] {
            let e = esc(s);
            assert!(e.iter().all(|&b| b > 0x20 && b != 0x7F), "{s:?}");
            let text = String::from_utf8(e.clone()).unwrap();
            // every `%` starts an escape
            let pct = e.iter().filter(|&&b| b == b'%').count();
            let expected = s.bytes().filter(|&b| needs_escape(b)).count() + usize::from(s == "-");
            assert_eq!(pct, expected, "{s:?}");
            assert_eq!(unescape_url(&text).unwrap().unwrap(), s);
        }
    }

    #[test]
    fn escape_dash() {
        assert_eq!(esc("-"), b"%2D");
        assert_eq!(unescape_url("-").unwrap(), None);
        assert_eq!(unescape_url("%2D").unwrap().unwrap(), "-");
    }

    #[test]
    fn unescape_rejects() {
        for f in [
            "%2d", "%41", "%", "%4", "%G0", "a%2Db", "%2D%2D", "a\rb", "a\x01%20", "a\x7f", "%20\n",
        ] {
            assert_eq!(unescape_url(f), Err(LineError::InvalidEscape), "{f}");
        }
    }

    #[test]
    fn unescape_borrows_when_plain() {
        assert!(matches!(
            unescape_url("http://a/b").unwrap(),
            Some(Cow::Borrowed(_))
        ));
        assert!(matches!(
            unescape_url("a%20b").unwrap(),
            Some(Cow::Owned(_))
        ));
    }
}
