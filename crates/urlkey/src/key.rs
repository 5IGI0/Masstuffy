//! The `massage` function and the helpers shared with the pattern code.

use std::fmt::Write;

use url::{Host, Url};

/// Returns the canonical massaged-URL key of `url`, or `None` when it has no key.
///
/// `None` means: the input is not a valid absolute URL, its scheme is not `http` or `https`, or its host
/// is missing or has a label outside `[a-z0-9_-]` (after IDNA and lowercasing). The result is never
/// `Some("")`. See the crate documentation for the key grammar.
#[must_use]
pub fn massage(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let mut out = String::with_capacity(url.len().saturating_add(8));
    if !push_host_key(&mut out, u.host()?) {
        return None;
    }
    if let Some(p) = u.port() {
        out.push(':');
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{p}");
    }
    out.push(')');
    push_path_and_query(&mut out, u.path(), u.query())?;
    Some(out)
}

/// Appends the canonical path, then the canonical query (if any pair remains) to `out`.
pub(crate) fn push_path_and_query(out: &mut String, path: &str, query: Option<&str>) -> Option<()> {
    normalize_path_into(out, path)?;
    if let Some(q) = query {
        push_canonical_query(out, q);
    }
    Some(())
}

/// Appends the host key of `host` to `out`. On failure returns `false` and leaves `out` unchanged.
pub(crate) fn push_host_key(out: &mut String, host: Host<&str>) -> bool {
    let entry_len = out.len();
    match host {
        Host::Domain(d) => {
            let d = d.trim_matches('.');
            let valid = !d.is_empty()
                && d.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
                });
            if !valid {
                return false;
            }
            for (i, label) in d.rsplit('.').enumerate() {
                if label.is_empty() {
                    out.truncate(entry_len);
                    return false;
                }
                if i > 0 {
                    out.push(',');
                }
                out.push_str(label);
            }
        }
        Host::Ipv4(a) => {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "{a}");
        }
        Host::Ipv6(a) => {
            out.push('[');
            // Writing to a `String` cannot fail.
            let _ = write!(out, "{a}");
            out.push(']');
        }
    }
    true
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_upper(n: u8) -> char {
    // `n & 0xF` is always a valid hex digit, the fallback is unreachable.
    char::from_digit(u32::from(n & 0xF), 16).map_or('0', |c| c.to_ascii_uppercase())
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn push_escaped(out: &mut Vec<u8>, b: u8) {
    out.push(b'%');
    // Hex digits are ASCII, so the cast of a `char` to `u8` is lossless.
    out.push(u8::try_from(hex_upper(b >> 4)).unwrap_or(b'0'));
    out.push(u8::try_from(hex_upper(b)).unwrap_or(b'0'));
}

/// Appends `path` to `out` with percent-escapes normalised: escapes of unreserved bytes are decoded, the
/// other valid escapes get uppercase hex digits, a `%` not followed by two hex digits is kept literally.
pub(crate) fn normalize_path_into(out: &mut String, path: &str) -> Option<()> {
    let bytes = path.as_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%'
            && let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2))
            && let (Some(h), Some(l)) = (hex_val(h), hex_val(l))
        {
            let v = (h << 4) | l;
            if is_unreserved(v) {
                buf.push(v);
            } else {
                push_escaped(&mut buf, v);
            }
            i += 3;
            continue;
        }
        buf.push(b);
        i += 1;
    }
    out.push_str(&String::from_utf8(buf).ok()?);
    Some(())
}

/// Decodes `s` as application/x-www-form-urlencoded into bytes, then re-encodes it canonically.
pub(crate) fn canonical_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b'%' => {
                let hl = (bytes.get(i + 1), bytes.get(i + 2));
                if let (Some(&h), Some(&l)) = hl
                    && let (Some(h), Some(l)) = (hex_val(h), hex_val(l))
                {
                    decoded.push((h << 4) | l);
                    i += 3;
                } else {
                    decoded.push(b'%');
                    i += 1;
                }
            }
            _ => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    let mut out = String::with_capacity(decoded.len());
    for b in decoded {
        if is_unreserved(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b));
        }
    }
    out
}

/// Appends `?` and the canonical, sorted query pairs, or nothing when no pair remains.
fn push_canonical_query(out: &mut String, q: &str) {
    let mut pairs: Vec<(String, Option<String>)> = Vec::new();
    for piece in q.split('&').filter(|p| !p.is_empty()) {
        match piece.split_once('=') {
            Some((k, v)) => pairs.push((canonical_component(k), Some(canonical_component(v)))),
            None => pairs.push((canonical_component(piece), None)),
        }
    }
    if pairs.is_empty() {
        return;
    }
    pairs.sort_unstable();
    out.push('?');
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        if let Some(v) = v {
            out.push('=');
            out.push_str(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::massage;

    fn k(s: &str) -> Option<String> {
        massage(s)
    }

    fn eq(url: &str, key: &str) {
        let got = k(url);
        assert_eq!(got.as_deref(), Some(key), "url: {url}");
        check_alphabet(key);
    }

    fn check_alphabet(key: &str) {
        assert!(
            key.bytes().all(|b| (0x21..=0x7E).contains(&b)),
            "key: {key}"
        );
        assert!(key.contains(')'), "key: {key}");
    }

    #[test]
    fn spec_example() {
        eq(
            "https://www.Example.com:8443/a/b?z=2&a=1#frag",
            "com,example,www:8443)/a/b?a=1&z=2",
        );
    }

    #[test]
    fn default_ports_omitted() {
        eq("http://x.com:80/", "com,x)/");
        eq("https://x.com:443/", "com,x)/");
        eq("http://x.com:080/", "com,x)/");
        eq("https://x.com:80/", "com,x:80)/");
        eq("http://x.com:443/", "com,x:443)/");
        eq("http://x.com:0/", "com,x:0)/");
    }

    #[test]
    fn scheme_and_case() {
        eq("HTTP://WWW.X.COM/A", "com,x,www)/A");
        eq("http:x.com", "com,x)/");
    }

    #[test]
    fn scheme_rejected() {
        for u in [
            "ftp://x.com/",
            "ws://x.com/",
            "mailto:a@b",
            "file:///etc/passwd",
            "urn:x:y",
            "dns:x.com",
        ] {
            assert_eq!(k(u), None, "url: {u}");
        }
    }

    #[test]
    fn unparseable_none() {
        for u in [
            "",
            "   ",
            "x.com/a",
            "http://",
            "http://[::1/",
            "http://x.com:99999/",
            "http://xn--zz.com/",
            "http://foo.123/",
        ] {
            assert_eq!(k(u), None, "url: {u}");
        }
    }

    #[test]
    fn host_dots() {
        eq("http://example.com./", "com,example)/");
        eq("http://example.com/", "com,example)/");
        eq("http://.example.com/", "com,example)/");
        for u in ["http://./", "http://../", "http://a..b.com/"] {
            assert_eq!(k(u), None, "url: {u}");
        }
    }

    #[test]
    fn host_alphabet() {
        for u in [
            "http://a,b.com/",
            "http://ex)ample.com/",
            "http://a*b.com/",
            "http://a!b.com/",
            "http://a~b.com/",
        ] {
            assert_eq!(k(u), None, "url: {u}");
        }
        eq("http://a_b.ex-1.com/", "com,ex-1,a_b)/");
        eq("http://ex%41mple.com/", "com,example)/");
    }

    #[test]
    fn ipv4() {
        eq("http://127.0.0.1:8080/x", "127.0.0.1:8080)/x");
        eq("http://0x7f.1/", "127.0.0.1)/");
        eq("http://123/", "0.0.0.123)/");
    }

    #[test]
    fn ipv6() {
        eq("http://[2001:DB8:0:0:0:0:0:1]/", "[2001:db8::1])/");
        eq("http://[::1]:8080/a", "[::1]:8080)/a");
        eq("https://[::1]:443/", "[::1])/");
        eq("http://[::ffff:1.2.3.4]/", "[::ffff:1.2.3.4])/");
    }

    #[test]
    fn idna() {
        eq("http://Bücher.DE/", "de,xn--bcher-kva)/");
        eq("http://xn--bcher-kva.de/", "de,xn--bcher-kva)/");
    }

    #[test]
    fn path_normalisation() {
        eq("http://x.com", "com,x)/");
        eq("http://x.com/", "com,x)/");
        eq("http://x.com/a/../b/./c", "com,x)/b/c");
        eq("http://x.com/a b/ü", "com,x)/a%20b/%C3%BC");
        eq("http://x.com/%7e%41%2f%2F", "com,x)/~A%2F%2F");
        eq("http://x.com/%zz%a", "com,x)/%zz%a");
        eq("http://x.com/a)b,c:d", "com,x)/a)b,c:d");
        eq("http://x.com/a#f", "com,x)/a");
    }

    #[test]
    fn userinfo_dropped() {
        eq("http://u:p@x.com/", "com,x)/");
    }

    #[test]
    fn query_sorting() {
        eq("http://x.com/?b=2&a=2&a=1", "com,x)/?a=1&a=2&b=2");
        eq("http://x.com/?a=1&a=1", "com,x)/?a=1&a=1");
        eq("http://x.com/?a&a=&a=0", "com,x)/?a&a=&a=0");
        eq("http://x.com/?a-=0&a=1", "com,x)/?a=1&a-=0");
        eq("http://x.com/?=v", "com,x)/?=v");
    }

    #[test]
    fn query_encoding() {
        eq("http://x.com/?q=a b", "com,x)/?q=a%20b");
        eq("http://x.com/?q=a+b", "com,x)/?q=a%20b");
        eq("http://x.com/?q=a%20b", "com,x)/?q=a%20b");
        eq("http://x.com/?q=a%2bb", "com,x)/?q=a%2Bb");
        eq("http://x.com/?q=%7e", "com,x)/?q=~");
        eq("http://x.com/?q=ü", "com,x)/?q=%C3%BC");
        eq("http://x.com/?q=%zz", "com,x)/?q=%25zz");
        eq("http://x.com/?q=%FF", "com,x)/?q=%FF");
        eq("http://x.com/?k%3Dx=1", "com,x)/?k%3Dx=1");
    }

    #[test]
    fn query_empty() {
        eq("http://x.com/?", "com,x)/");
        eq("http://x.com/?&", "com,x)/");
        eq("http://x.com/?&&", "com,x)/");
        eq("http://x.com/?&a=1&", "com,x)/?a=1");
    }

    #[test]
    fn no_panic_on_garbage() {
        let many = vec!["a="; 10_000].join("&");
        let many = format!("http://x.com/?{many}");
        let long_path = format!("http://x.com/{}", "a".repeat(64 * 1024));
        let inputs = [
            "http://%",
            "http://[",
            "http://a b/",
            "http://x.com/\u{1}\u{7f}\r\n\t",
            "http://x.com/\0\0",
            "http://\0/",
            "http://x.com/%",
            "http://x.com/?%",
            "http://x.com/?%a",
            many.as_str(),
            long_path.as_str(),
        ];
        for u in inputs {
            let _ = massage(u);
        }
    }
}
