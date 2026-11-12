//! Header lines and the validators shared by the parser and the builder.

use crate::error::ParseError;

/// Canonical spelling of `WARC-Type`.
pub const WARC_TYPE: &str = "WARC-Type";
/// Canonical spelling of `WARC-Record-ID`.
pub const WARC_RECORD_ID: &str = "WARC-Record-ID";
/// Canonical spelling of `WARC-Date`.
pub const WARC_DATE: &str = "WARC-Date";
/// Canonical spelling of `WARC-Target-URI`.
pub const WARC_TARGET_URI: &str = "WARC-Target-URI";
/// Canonical spelling of `Content-Length`.
pub const CONTENT_LENGTH: &str = "Content-Length";

/// One header line, borrowed. `value` is verbatim (no trimming).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    /// Header name, as written in the input.
    pub name: &'a str,
    /// Header value, everything after the first `": "`.
    pub value: &'a str,
}

/// Iterator over a validated header block, in input order. Allocation-free.
#[derive(Clone, Debug)]
pub struct Headers<'a> {
    lines: std::str::SplitTerminator<'a, &'static str>,
}

impl<'a> Headers<'a> {
    /// `block` must be a validated header block (each line ending in CRLF).
    pub(crate) fn new(block: &'a str) -> Self {
        Self {
            lines: block.split_terminator("\r\n"),
        }
    }
}

impl<'a> Iterator for Headers<'a> {
    type Item = Header<'a>;

    fn next(&mut self) -> Option<Header<'a>> {
        self.lines.by_ref().find_map(|l| {
            l.split_once(": ")
                .map(|(name, value)| Header { name, value })
        })
    }
}

/// RFC 9110 `tchar`.
pub(crate) fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Non-empty token. Used for header names and `WARC-Type` values.
pub(crate) fn validate_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|&b| is_tchar(b))
}

/// Index of the first byte that is a control byte other than HT, or DEL.
pub(crate) fn first_invalid_value_byte(value: &[u8]) -> Option<usize> {
    value
        .iter()
        .position(|&b| !((b >= 0x20 || b == b'\t') && b != 0x7F))
}

/// No control byte except HT, no DEL.
pub(crate) fn validate_value(value: &[u8]) -> bool {
    first_invalid_value_byte(value).is_none()
}

/// `<inner>` with a non-empty inner containing `:`, no byte `<= 0x20`, no `<`, `>`, DEL.
/// Returns the inner part.
pub(crate) fn parse_record_id(value: &str) -> Option<&str> {
    let inner = value.strip_prefix('<')?.strip_suffix('>')?;
    validate_record_id_inner(inner).then_some(inner)
}

/// Checks the part of a record id between the brackets.
pub(crate) fn validate_record_id_inner(inner: &str) -> bool {
    !inner.is_empty()
        && inner.as_bytes().contains(&b':')
        && inner
            .as_bytes()
            .iter()
            .all(|&b| b > 0x20 && b != 0x7F && b != b'<' && b != b'>')
}

/// Canonical decimal `Content-Length`: 1 to 20 digits, no leading zero except `0`, fits in a `u64`.
/// Does not check any limit.
pub(crate) fn parse_content_length(value: &[u8]) -> Result<u64, ParseError> {
    if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
        return Err(ParseError::InvalidContentLength);
    }
    if value.len() > 1 && value.first() == Some(&b'0') {
        return Err(ParseError::InvalidContentLength);
    }
    let mut n: u64 = 0;
    for &b in value {
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(b - b'0')))
            .ok_or(ParseError::InvalidContentLength)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tchar_table() {
        let specials = b"!#$%&'*+-.^_`|~";
        for b in 0..=255u8 {
            let expected = b.is_ascii_digit()
                || b.is_ascii_uppercase()
                || b.is_ascii_lowercase()
                || specials.contains(&b);
            assert_eq!(is_tchar(b), expected, "byte {b}");
        }
    }

    #[test]
    fn content_length_canonical() {
        assert_eq!(parse_content_length(b"0"), Ok(0));
        assert_eq!(parse_content_length(b"18446744073709551615"), Ok(u64::MAX));
        for bad in [
            &b""[..],
            b"00",
            b"01",
            b"+1",
            b" 1",
            b"1 ",
            b"18446744073709551616",
            b"1a",
        ] {
            assert_eq!(
                parse_content_length(bad),
                Err(ParseError::InvalidContentLength),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn record_id_rules() {
        assert_eq!(parse_record_id("<urn:x:1>"), Some("urn:x:1"));
        for bad in ["urn:x", "<>", "<a b:c>", "<noscheme>", "<a:<b>", "<a:b"] {
            assert_eq!(parse_record_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn name_and_value_rules() {
        assert!(validate_name(b"WARC-Type"));
        assert!(!validate_name(b""));
        assert!(!validate_name(b"a b"));
        assert!(validate_value(b"a\tb: c"));
        assert!(!validate_value(b"a\x01"));
        assert!(!validate_value(b"a\x7f"));
    }

    #[test]
    fn headers_iterator() {
        let h: Vec<_> = Headers::new("A: 1\r\nB: x: y \r\nC: \r\n").collect();
        assert_eq!(h.len(), 3);
        assert_eq!(h[1].value, "x: y ");
        assert_eq!(h[2].value, "");
    }
}
