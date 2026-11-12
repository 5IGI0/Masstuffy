//! Shared test fixtures.

use crate::RecordRef;

pub(crate) const MIN: &str = "WARC-Type: resource\r\nWARC-Record-ID: <urn:uuid:00000000-0000-4000-8000-000000000000>\r\nWARC-Date: 2026-11-12T10:00:00Z\r\nContent-Length: 5\r\n";
pub(crate) const MIN_BODY: &[u8] = b"hello";

/// `"WARC/1.1\r\n" + headers + "\r\n" + body + "\r\n\r\n"`.
pub(crate) fn rec(headers: &str, body: &[u8]) -> Vec<u8> {
    let mut v = b"WARC/1.1\r\n".to_vec();
    v.extend_from_slice(headers.as_bytes());
    v.extend_from_slice(b"\r\n");
    v.extend_from_slice(body);
    v.extend_from_slice(b"\r\n\r\n");
    v
}

/// Asserts that two views expose the same bytes and accessors.
pub(crate) fn same_view(a: &RecordRef<'_>, b: &RecordRef<'_>) {
    assert_eq!(a.as_bytes(), b.as_bytes());
    assert_eq!(a.body(), b.body());
    assert_eq!(a.warc_type(), b.warc_type());
    assert_eq!(a.record_id(), b.record_id());
    assert_eq!(a.date(), b.date());
    assert_eq!(a.target_uri(), b.target_uri());
    assert_eq!(a.content_length(), b.content_length());
    assert_eq!(a.header_count(), b.header_count());
    assert!(a.headers().eq(b.headers()));
}
