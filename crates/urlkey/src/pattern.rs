//! `UrlPattern`: pattern search over massaged-URL keys, compiled to ordered key ranges.

use std::ops::Range;

use url::{Host, Url};

use crate::error::PatternError;
use crate::key::{canonical_component, normalize_path_into, push_host_key, push_path_and_query};

/// How the pattern host is compared with the key host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostMatch {
    /// The key host must equal the pattern host.
    Exact,
    /// The key host is the pattern host or any of its subdomains (domain hosts only).
    WithSubdomains,
}

/// Port criterion. Keys omit default ports and drop the scheme, so "no port in the key" means
/// "80 for http or 443 for https".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortFilter {
    /// Any port.
    Any,
    /// Only keys without a port (the scheme default).
    Default,
    /// Only keys carrying exactly this port.
    Explicit(u16),
}

/// Path criterion, applied to the key's path and query (everything after the first `)`), like the
/// Wayback CDX server's `matchType`. Raw user input, normalised by [`UrlPattern::new`]; `#` is refused.
///
/// `Exact` is the whole canonical tail (the query, if given, is canonicalised and sorted like stored
/// keys). `Prefix` is "begins with": the tail may end inside the query, whose complete pairs must already
/// be in canonical order, and a partial escape at its end is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFilter<'a> {
    /// Any path.
    Any,
    /// The path and query must equal this one.
    Exact(&'a str),
    /// The path and query must start with this one.
    Prefix(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PortSpec {
    Any,
    Default,
    Explicit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSpec {
    Any,
    Exact(String),
    Prefix(String),
}

/// A validated URL pattern, compiled once to sorted ordered-key ranges and an exact predicate.
///
/// Scan every range of [`key_ranges`](Self::key_ranges) in an ordered index, then filter the keys found
/// with [`matches`](Self::matches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlPattern {
    host: String,
    subdomains: bool,
    port: PortSpec,
    path: PathSpec,
    ranges: Vec<Range<String>>,
}

impl UrlPattern {
    /// Builds a pattern from user input; every component is validated and normalised like stored URLs.
    ///
    /// The host is percent-decoded, IDNA-encoded and lowercased; a bare IPv6 address (`::1`) is accepted
    /// without brackets. Paths must start with `/` and contain no `#`; a `?` starts a query
    /// (Wayback CDX semantics, see [`PathFilter`]). Tabs and newlines are removed, paths are escaped and
    /// complete dot-segments are resolved like in [`massage`](crate::massage); the last segment of a
    /// `Prefix` is literal (`/.` matches `/.git`).
    ///
    /// # Errors
    ///
    /// [`PatternError::InvalidHost`] for an empty or malformed host (or one that can never have a key),
    /// [`PatternError::SubdomainsOfIp`] for `WithSubdomains` on an IP, [`PatternError::InvalidPath`] for a
    /// bad path (including a prefix ending in an incomplete percent-escape), [`PatternError::InvalidQuery`]
    /// for a bad query prefix.
    pub fn new(
        host: &str,
        host_match: HostMatch,
        port: PortFilter,
        path: PathFilter<'_>,
    ) -> Result<UrlPattern, PatternError> {
        let parsed = if host.contains(':') && !host.starts_with('[') {
            Host::parse(&format!("[{host}]"))
        } else {
            Host::parse(host)
        }
        .map_err(|_| PatternError::InvalidHost)?;
        let (ref_host, is_ip) = match &parsed {
            Host::Domain(d) => (Host::Domain(d.as_str()), false),
            Host::Ipv4(a) => (Host::Ipv4(*a), true),
            Host::Ipv6(a) => (Host::Ipv6(*a), true),
        };
        let mut host_key = String::with_capacity(host.len().saturating_add(4));
        if !push_host_key(&mut host_key, ref_host) {
            return Err(PatternError::InvalidHost);
        }
        let subdomains = matches!(host_match, HostMatch::WithSubdomains);
        if subdomains && is_ip {
            return Err(PatternError::SubdomainsOfIp);
        }
        let port = match port {
            PortFilter::Any => PortSpec::Any,
            PortFilter::Default => PortSpec::Default,
            PortFilter::Explicit(n) => PortSpec::Explicit(n.to_string()),
        };
        let path = match path {
            PathFilter::Any => PathSpec::Any,
            PathFilter::Exact(p) => PathSpec::Exact(canonical_exact(p)?),
            PathFilter::Prefix(p) => PathSpec::Prefix(canonical_prefix(p)?),
        };
        let ranges = build_ranges(&host_key, subdomains, &port, &path);
        Ok(UrlPattern {
            host: host_key,
            subdomains,
            port,
            path,
            ranges,
        })
    }

    /// Sorted, pairwise disjoint, half-open key ranges (at most 4, built once).
    ///
    /// Every key accepted by [`matches`](Self::matches) lies in one of them. The ranges may also contain
    /// keys that do not match, so filter the scan results with `matches`.
    #[must_use]
    pub fn key_ranges(&self) -> &[Range<String>] {
        &self.ranges
    }

    /// Exact predicate on a key produced by [`massage`](crate::massage). Allocation-free; returns `false`
    /// (never panics) on input that is not a well-formed key.
    #[must_use]
    pub fn matches(&self, key: &str) -> bool {
        let Some((auth, rest)) = key.split_once(')') else {
            return false;
        };
        let Some((h, port)) = split_authority(auth) else {
            return false;
        };
        let host_ok = if self.subdomains {
            h.strip_prefix(self.host.as_str())
                .is_some_and(|r| r.is_empty() || r.starts_with(','))
        } else {
            h == self.host
        };
        if !host_ok {
            return false;
        }
        let port_ok = match &self.port {
            PortSpec::Any => true,
            PortSpec::Default => port.is_none(),
            PortSpec::Explicit(s) => port == Some(s.as_str()),
        };
        if !port_ok {
            return false;
        }
        match &self.path {
            PathSpec::Any => true,
            PathSpec::Exact(p) => rest == p,
            PathSpec::Prefix(p) => rest.starts_with(p.as_str()),
        }
    }
}

/// Splits an authority into host and optional port. `None` when malformed.
fn split_authority(auth: &str) -> Option<(&str, Option<&str>)> {
    if auth.starts_with('[') {
        let close = auth.find(']')?;
        let (h, rest) = auth.split_at_checked(close.checked_add(1)?)?;
        if rest.is_empty() {
            Some((h, None))
        } else {
            let p = rest.strip_prefix(':')?;
            if p.is_empty() {
                None
            } else {
                Some((h, Some(p)))
            }
        }
    } else {
        match auth.split_once(':') {
            Some((_, "")) => None,
            Some((h, p)) => Some((h, Some(p))),
            None => Some((auth, None)),
        }
    }
}

/// Canonical path of a user path (no query), resolved like a stored URL path.
fn canonical_path(path: &str) -> Result<String, PatternError> {
    let mut u = Url::parse("http://h.invalid/").map_err(|_| PatternError::InvalidPath)?;
    u.set_path(path);
    let mut out = String::with_capacity(path.len());
    normalize_path_into(&mut out, u.path()).ok_or(PatternError::InvalidPath)?;
    Ok(out)
}

/// Removes `\t`, `\n` and `\r`, which WHATWG parsing strips from any URL input (as `massage` does).
fn strip_ctl(p: &str) -> String {
    p.chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

/// Canonical path of a prefix whose last segment is unfinished: a sentinel `x` keeps that segment from
/// being treated as a dot segment (`/.` stays `/.`, matching `/.git`), then is removed again.
fn canonical_path_prefix(p: &str) -> Result<String, PatternError> {
    let mut u = Url::parse("http://h.invalid/").map_err(|_| PatternError::InvalidPath)?;
    u.set_path(&format!("{p}x"));
    let mut out = String::with_capacity(p.len().saturating_add(1));
    normalize_path_into(&mut out, u.path()).ok_or(PatternError::InvalidPath)?;
    // `x` is not percent-encoded, is the last byte, and cannot join an escape (a trailing `%`/`%H` is
    // refused by the caller), so it is still the last byte here.
    out.strip_suffix('x')
        .map(str::to_owned)
        .ok_or(PatternError::InvalidPath)
}

fn canonical_exact(p: &str) -> Result<String, PatternError> {
    let p = strip_ctl(p);
    let p = p.as_str();
    if !p.starts_with('/') || p.contains('#') {
        return Err(PatternError::InvalidPath);
    }
    let (path, query) = match p.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (p, None),
    };
    let mut u = Url::parse("http://h.invalid/").map_err(|_| PatternError::InvalidPath)?;
    u.set_path(path);
    u.set_query(query);
    let mut out = String::with_capacity(p.len());
    push_path_and_query(&mut out, u.path(), u.query()).ok_or(PatternError::InvalidPath)?;
    Ok(out)
}

/// True when `s` ends with `%` or `%H` (H a hex digit): an escape cut in the middle.
fn ends_with_partial_escape(s: &str) -> bool {
    let mut r = s.bytes().rev();
    match (r.next(), r.next()) {
        (Some(b'%'), _) => true,
        (Some(h), Some(b'%')) => h.is_ascii_hexdigit(),
        _ => false,
    }
}

fn canonical_pair(piece: &str) -> (String, Option<String>) {
    match piece.split_once('=') {
        Some((k, v)) => (canonical_component(k), Some(canonical_component(v))),
        None => (canonical_component(piece), None),
    }
}

fn canonical_prefix(p: &str) -> Result<String, PatternError> {
    let p = strip_ctl(p);
    let p = p.as_str();
    if !p.starts_with('/') || p.contains('#') {
        return Err(PatternError::InvalidPath);
    }
    let Some((path, q)) = p.split_once('?') else {
        if ends_with_partial_escape(p) {
            return Err(PatternError::InvalidPath);
        }
        return canonical_path_prefix(p);
    };
    if ends_with_partial_escape(q) {
        return Err(PatternError::InvalidQuery);
    }
    let mut out = canonical_path(path)?;
    out.push('?');
    let mut pieces = q.split('&');
    // `split` always yields at least one piece; the last one is the partial pair.
    let last = pieces.next_back().unwrap_or("");
    let mut prev: Option<(String, Option<String>)> = None;
    for piece in pieces.filter(|s| !s.is_empty()) {
        let pair = canonical_pair(piece);
        if prev.as_ref().is_some_and(|p| *p > pair) {
            return Err(PatternError::InvalidQuery);
        }
        out.push_str(&pair.0);
        if let Some(v) = &pair.1 {
            out.push('=');
            out.push_str(v);
        }
        out.push('&');
        prev = Some(pair);
    }
    let (k, v) = canonical_pair(last);
    out.push_str(&k);
    if let Some(v) = v {
        out.push('=');
        out.push_str(&v);
    }
    Ok(out)
}

/// `[s, succ(s))` where `succ` increments the last char. Every key byte is in `0x21..=0x7E` and `s` is
/// never empty, so the increment stays ASCII.
fn prefix_range(s: &str) -> Range<String> {
    let mut end = String::with_capacity(s.len());
    let mut chars = s.chars();
    let last = chars.next_back();
    end.push_str(chars.as_str());
    match last {
        Some(c) => {
            let next = u8::try_from(c)
                .ok()
                .and_then(|b| b.checked_add(1))
                .filter(u8::is_ascii);
            // The fallback is unreachable for keys; `char::MAX` is still an upper bound.
            end.push(next.map_or(char::MAX, char::from));
        }
        None => end.push(char::MAX),
    }
    s.to_owned()..end
}

/// `[s, s + "\0")`: contains exactly `s` (keys never contain NUL).
fn point_range(s: &str) -> Range<String> {
    let mut end = String::with_capacity(s.len().saturating_add(1));
    end.push_str(s);
    end.push('\0');
    s.to_owned()..end
}

fn build_ranges(
    host: &str,
    subdomains: bool,
    port: &PortSpec,
    path: &PathSpec,
) -> Vec<Range<String>> {
    let mut head = String::with_capacity(host.len().saturating_add(8));
    head.push_str(host);
    if let PortSpec::Explicit(n) = port {
        head.push(':');
        head.push_str(n);
    }
    head.push(')');
    let mut ranges = Vec::with_capacity(4);
    match path {
        PathSpec::Any => ranges.push(prefix_range(&head)),
        PathSpec::Prefix(p) => ranges.push(prefix_range(&format!("{head}{p}"))),
        PathSpec::Exact(p) => {
            let full = format!("{head}{p}");
            ranges.push(point_range(&full));
        }
    }
    if matches!(port, PortSpec::Any) {
        ranges.push(prefix_range(&format!("{host}:")));
    }
    if subdomains {
        ranges.push(prefix_range(&format!("{host},")));
    }
    ranges.sort_by(|a, b| a.start.cmp(&b.start));
    ranges
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::massage;

    fn pat(host: &str, hm: HostMatch, port: PortFilter, path: PathFilter<'_>) -> UrlPattern {
        UrlPattern::new(host, hm, port, path).unwrap()
    }

    fn key(u: &str) -> String {
        massage(u).unwrap()
    }

    fn rs(p: &UrlPattern) -> Vec<(&str, &str)> {
        p.key_ranges()
            .iter()
            .map(|r| (r.start.as_str(), r.end.as_str()))
            .collect()
    }

    #[test]
    fn new_rejects() {
        for h in [
            "",
            "a/b",
            "a:b:c",
            "a,b",
            "a)b",
            "a..b",
            ".",
            "example.com:80",
        ] {
            let e = UrlPattern::new(h, HostMatch::Exact, PortFilter::Any, PathFilter::Any);
            assert_eq!(e.unwrap_err(), PatternError::InvalidHost, "host: {h:?}");
        }
        for h in ["127.0.0.1", "::1"] {
            let e = UrlPattern::new(
                h,
                HostMatch::WithSubdomains,
                PortFilter::Any,
                PathFilter::Any,
            );
            assert_eq!(e.unwrap_err(), PatternError::SubdomainsOfIp, "host: {h:?}");
        }
        for p in ["a", "", "/x#y"] {
            for f in [PathFilter::Exact(p), PathFilter::Prefix(p)] {
                let e = UrlPattern::new("x.com", HostMatch::Exact, PortFilter::Any, f);
                assert_eq!(e.unwrap_err(), PatternError::InvalidPath, "path: {p:?}");
            }
        }
    }

    #[test]
    fn new_normalises() {
        let a = pat(
            "Bücher.DE.",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Any,
        );
        let b = pat(
            "xn--bcher-kva.de",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Any,
        );
        assert_eq!(a, b);
        let a = pat("::1", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
        let b = pat("[::1]", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
        assert_eq!(a, b);
        let p = pat(
            "x",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Exact("/a b/%7e"),
        );
        assert!(p.matches(&key("http://x/a%20b/~")));
        let pre = |s: &str| {
            pat(
                "x",
                HostMatch::Exact,
                PortFilter::Any,
                PathFilter::Prefix(s),
            )
        };
        assert_eq!(pre("/a/../b"), pre("/b"));
        assert_eq!(pre("/a/.%2E"), pre("/a/.."));
    }

    #[test]
    fn ranges_examples() {
        let p = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Any,
        );
        assert_eq!(
            rs(&p),
            [
                ("com,example)", "com,example*"),
                ("com,example,", "com,example-"),
                ("com,example:", "com,example;"),
            ]
        );
        let p = pat(
            "example.com",
            HostMatch::Exact,
            PortFilter::Explicit(80),
            PathFilter::Prefix("/a/"),
        );
        assert_eq!(rs(&p), [("com,example:80)/a/", "com,example:80)/a0")]);
        let one = |f: PathFilter<'_>| pat("example.com", HostMatch::Exact, PortFilter::Default, f);
        assert_eq!(
            rs(&one(PathFilter::Exact("/a"))),
            [("com,example)/a", "com,example)/a\0")]
        );
        assert_eq!(
            rs(&one(PathFilter::Exact("/a?z=2&a=1"))),
            [("com,example)/a?a=1&z=2", "com,example)/a?a=1&z=2\0")]
        );
        assert_eq!(
            rs(&one(PathFilter::Prefix("/a"))),
            [("com,example)/a", "com,example)/b")]
        );
        assert_eq!(
            rs(&one(PathFilter::Prefix("/a?"))),
            [("com,example)/a?", "com,example)/a@")]
        );
        assert_eq!(
            rs(&one(PathFilter::Prefix("/a?b=1&c"))),
            [("com,example)/a?b=1&c", "com,example)/a?b=1&d")]
        );
        let p = pat(
            "127.0.0.1",
            HostMatch::Exact,
            PortFilter::Default,
            PathFilter::Any,
        );
        assert_eq!(rs(&p), [("127.0.0.1)", "127.0.0.1*")]);
        let p = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Any,
        );
        let in_range = |k: &str| p.key_ranges().iter().any(|r| r.contains(&k.to_owned()));
        assert!(!in_range("com,examples)/"));
        assert!(!in_range("com,example-foo)/"));
        assert!(in_range("com,example,www:8443)/a/b?a=1&z=2"));
        assert!(in_range("com,example:8080)/"));
    }

    #[test]
    fn subdomain_vs_exact() {
        let sub = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Any,
        );
        let exact = pat(
            "example.com",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Any,
        );
        for (u, in_sub, in_exact) in [
            ("http://example.com/", true, true),
            ("http://www.example.com/", true, false),
            ("http://a.b.example.com/", true, false),
            ("http://examples.com/", false, false),
            ("http://example-foo.com/", false, false),
            ("http://xexample.com/", false, false),
            ("http://example.com.au/", false, false),
            ("http://example.org/", false, false),
        ] {
            assert_eq!(sub.matches(&key(u)), in_sub, "{u}");
            assert_eq!(exact.matches(&key(u)), in_exact, "{u}");
        }
    }

    #[test]
    fn port_filters() {
        let any = pat("x.com", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
        let def = pat(
            "x.com",
            HostMatch::Exact,
            PortFilter::Default,
            PathFilter::Any,
        );
        let p80 = pat(
            "x.com",
            HostMatch::Exact,
            PortFilter::Explicit(80),
            PathFilter::Any,
        );
        let p8080 = pat(
            "x.com",
            HostMatch::Exact,
            PortFilter::Explicit(8080),
            PathFilter::Any,
        );
        assert!(def.matches(&key("http://x.com/")));
        assert!(def.matches(&key("https://x.com/")));
        assert!(!def.matches(&key("https://x.com:80/")));
        assert!(p80.matches(&key("https://x.com:80/")));
        assert!(!p80.matches(&key("http://x.com/")));
        assert!(!p80.matches(&key("https://x.com:8080/")));
        assert!(p8080.matches(&key("http://x.com:8080/")));
        assert!(!p8080.matches(&key("http://x.com:808/")));
        assert!(!p8080.matches(&key("http://x.com:8008/")));
        for u in ["http://x.com/", "http://x.com:81/", "https://x.com:8080/"] {
            assert!(any.matches(&key(u)));
        }
    }

    #[test]
    fn path_filters() {
        let m = |f: PathFilter<'_>| pat("x.com", HostMatch::Exact, PortFilter::Any, f);
        let k = |p: &str| key(&format!("http://x.com{p}"));
        let pre = m(PathFilter::Prefix("/a"));
        for p in ["/a", "/a/b", "/ab", "/a?x=1"] {
            assert!(pre.matches(&k(p)), "{p}");
        }
        let pre = m(PathFilter::Prefix("/a/"));
        assert!(!pre.matches(&k("/a")));
        assert!(pre.matches(&k("/a/")));
        let pre = m(PathFilter::Prefix("/a?"));
        assert!(pre.matches(&k("/a?x=1")));
        assert!(!pre.matches(&k("/a")));
        let ex = m(PathFilter::Exact("/a"));
        assert!(ex.matches(&k("/a")));
        assert!(!ex.matches(&k("/a?x=1")));
        assert!(!ex.matches(&k("/a/")));
        assert!(!ex.matches(&k("/ab")));
        let ex = m(PathFilter::Exact("/a)b"));
        assert!(ex.matches(&k("/a)b")));
        assert!(!ex.matches(&k("/a")));
        assert!(!ex.matches(&k("/a)bc")));
        let pre = m(PathFilter::Prefix("/a,"));
        assert!(pre.matches(&k("/a,b")));
    }

    #[test]
    fn exact_with_query() {
        let m = |f: PathFilter<'_>| pat("x.com", HostMatch::Exact, PortFilter::Any, f);
        let k = |p: &str| key(&format!("http://x.com{p}"));
        let ex = m(PathFilter::Exact("/a?z=2&a=1"));
        assert!(ex.matches(&k("/a?a=1&z=2")));
        assert!(ex.matches(&k("/a?z=2&a=1")));
        assert!(!ex.matches(&k("/a?a=1")));
        assert_eq!(m(PathFilter::Exact("/a?")), m(PathFilter::Exact("/a")));
        assert!(m(PathFilter::Exact("/a?q=a b")).matches(&k("/a?q=a+b")));
    }

    #[test]
    fn prefix_query() {
        let m = |f: &str| {
            pat(
                "x.com",
                HostMatch::Exact,
                PortFilter::Any,
                PathFilter::Prefix(f),
            )
        };
        let k = |p: &str| key(&format!("http://x.com{p}"));
        assert!(m("/a?q=a b").matches(&k("/a?q=a%20bc")));
        assert!(m("/a?b=1&").matches(&k("/a?b=1&c=2")));
        assert!(!m("/a?b=1&").matches(&k("/a?b=1")));
        assert_eq!(m("/a?b=1&&c"), m("/a?b=1&c"));
        assert!(m("/a?q=ü").matches(&k("/a?q=%C3%BC%C3%A9")));
        let any_q = m("/?");
        assert!(any_q.matches(&k("/?x=1")));
        assert!(!any_q.matches(&k("/a?x=1")));
        assert!(!any_q.matches(&k("/a")));
    }

    #[test]
    fn prefix_rejects() {
        let n = |f: PathFilter<'_>| {
            UrlPattern::new("x.com", HostMatch::Exact, PortFilter::Any, f).unwrap_err()
        };
        for p in ["/a?b=1&a=1&c", "/a?x=%2", "/a?x=%"] {
            assert_eq!(n(PathFilter::Prefix(p)), PatternError::InvalidQuery, "{p}");
        }
        for p in ["/a%2", "/a%", "/a#x"] {
            assert_eq!(n(PathFilter::Prefix(p)), PatternError::InvalidPath, "{p}");
        }
        assert_eq!(n(PathFilter::Exact("/a#x")), PatternError::InvalidPath);
        assert!(
            UrlPattern::new(
                "x.com",
                HostMatch::Exact,
                PortFilter::Any,
                PathFilter::Exact("/a?x=%2")
            )
            .is_ok()
        );
    }

    #[test]
    fn prefix_dot_segments() {
        let m = |f: &str| {
            pat(
                "x.com",
                HostMatch::Exact,
                PortFilter::Default,
                PathFilter::Prefix(f),
            )
        };
        let keys: Vec<String> = ["/.git", "/a/..foo", "/a/.hidden", "/zzz", "/"]
            .iter()
            .map(|s| key(&format!("http://x.com{s}")))
            .collect();
        let hits = |p: &UrlPattern| -> Vec<&str> {
            keys.iter()
                .filter(|k| p.matches(k))
                .map(|k| k.as_str())
                .collect()
        };
        assert_eq!(hits(&m("/.")), ["com,x)/.git"]);
        assert_eq!(hits(&m("/a/..")), ["com,x)/a/..foo"]);
        assert_eq!(hits(&m("/a/.%2E")), ["com,x)/a/..foo"]);
        assert_eq!(hits(&m("/a/.")), ["com,x)/a/..foo", "com,x)/a/.hidden"]);
        assert_eq!(rs(&m("/.")), [("com,x)/.", "com,x)//")]);
        assert_eq!(rs(&m("/a/..")), [("com,x)/a/..", "com,x)/a/./")]);
        assert_eq!(rs(&m("/a/.")), [("com,x)/a/.", "com,x)/a//")]);
    }

    #[test]
    fn prefix_tab_newline() {
        let m = |f: PathFilter<'_>| pat("x.com", HostMatch::Exact, PortFilter::Any, f);
        let k = |p: &str| key(&format!("http://x.com{p}"));
        let pre = m(PathFilter::Prefix("/a?b\tc"));
        assert!(pre.matches(&k("/a?b\tc")));
        assert!(pre.matches(&k("/a?bcd")));
        assert_eq!(
            m(PathFilter::Prefix("/a\n/b")),
            m(PathFilter::Prefix("/a/b"))
        );
        assert!(m(PathFilter::Exact("/a?b\tc")).matches(&k("/a?bc")));
        let e = UrlPattern::new(
            "x.com",
            HostMatch::Exact,
            PortFilter::Any,
            PathFilter::Prefix("/a%\t"),
        );
        assert_eq!(e.unwrap_err(), PatternError::InvalidPath);
    }

    #[test]
    fn spec_example_pattern() {
        let k = key("https://www.Example.com:8443/a/b?z=2&a=1#frag");
        let p = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Explicit(8443),
            PathFilter::Prefix("/a/"),
        );
        assert!(p.matches(&k));
        let p = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Explicit(443),
            PathFilter::Prefix("/a/"),
        );
        assert!(!p.matches(&k));
    }

    #[test]
    fn matches_garbage() {
        let p = pat(
            "example.com",
            HostMatch::WithSubdomains,
            PortFilter::Any,
            PathFilter::Any,
        );
        for k in [
            "",
            ")",
            "[",
            "[::1",
            "[::1]x)/",
            "com,example:)",
            "com,example",
            "é)/",
        ] {
            assert!(!p.matches(k), "{k:?}");
        }
        let v6 = pat("::1", HostMatch::Exact, PortFilter::Any, PathFilter::Any);
        for k in ["[", "[::1", "[::1]x)/", "[é)/"] {
            assert!(!v6.matches(k), "{k:?}");
        }
    }

    fn oracle(host: &str, subs: bool, port: PortFilter, path: PathFilter<'_>, url: &str) -> bool {
        let u = Url::parse(url).unwrap();
        let ph = Url::parse(&format!("http://{host}/")).unwrap();
        let ph = ph.host_str().unwrap().to_owned();
        let uh = u.host_str().unwrap();
        let host_ok = uh == ph || (subs && uh.ends_with(&format!(".{ph}")));
        let port_ok = match port {
            PortFilter::Any => true,
            PortFilter::Default => u.port().is_none(),
            PortFilter::Explicit(n) => u.port() == Some(n),
        };
        let mut pq = u.path().to_owned();
        if let Some(q) = u.query().filter(|q| !q.is_empty()) {
            pq.push('?');
            pq.push_str(q);
        }
        let path_ok = match path {
            PathFilter::Any => true,
            PathFilter::Exact(p) => pq == p,
            PathFilter::Prefix(p) => pq.starts_with(p),
        };
        host_ok && port_ok && path_ok
    }

    fn is_tight(p: &UrlPattern) -> bool {
        let path_any = p.path == PathSpec::Any;
        let port_any = p.port == PortSpec::Any;
        (port_any && path_any) || (!port_any && !p.subdomains)
    }

    #[test]
    fn ranges_invariants() {
        let hosts = [
            "example.com",
            "www.example.com",
            "a.b.example.com",
            "examples.com",
            "example-foo.com",
            "xexample.com",
            "example.com.au",
            "com",
            "example.org",
            "127.0.0.1",
            "127.0.0.10",
            "[::1]",
            "[::2]",
            "bücher.de",
        ];
        let ports = ["", ":80", ":443", ":8080", ":808", ":0"];
        let paths = [
            "/",
            "/a",
            "/a/",
            "/a/b",
            "/ab",
            "/a)b",
            "/a,b",
            "/b",
            "/.git",
            "/a/.hidden",
            "/a/..foo",
        ];
        let queries = ["", "?x=1", "?a=&b"];
        let mut corpus: Vec<(String, String)> = Vec::new();
        for s in ["http", "https"] {
            for h in hosts {
                for p in ports {
                    for pa in paths {
                        for q in queries {
                            let url = format!("{s}://{h}{p}{pa}{q}");
                            let k = massage(&url).unwrap();
                            corpus.push((url, k));
                        }
                    }
                }
            }
        }
        let all: BTreeSet<String> = corpus.iter().map(|(_, k)| k.clone()).collect();
        let phosts = [
            "example.com",
            "www.example.com",
            "com",
            "127.0.0.1",
            "[::1]",
            "bücher.de",
        ];
        let pports = [
            PortFilter::Any,
            PortFilter::Default,
            PortFilter::Explicit(8080),
            PortFilter::Explicit(80),
        ];
        let ppaths = [
            PathFilter::Any,
            PathFilter::Exact("/a"),
            PathFilter::Exact("/"),
            PathFilter::Prefix("/a"),
            PathFilter::Prefix("/a/"),
            PathFilter::Prefix("/a)"),
            PathFilter::Exact("/a?x=1"),
            PathFilter::Prefix("/a?"),
            PathFilter::Prefix("/a?a="),
            PathFilter::Prefix("/."),
            PathFilter::Prefix("/a/."),
        ];
        let mut npat = 0usize;
        for ph in phosts {
            let is_ip = ph == "127.0.0.1" || ph == "[::1]";
            for hm in [HostMatch::Exact, HostMatch::WithSubdomains] {
                if is_ip && hm == HostMatch::WithSubdomains {
                    continue;
                }
                for port in pports {
                    for path in ppaths {
                        npat += 1;
                        let p = pat(ph, hm, port, path);
                        let subs = hm == HostMatch::WithSubdomains;
                        let r = p.key_ranges();
                        for w in r.windows(2) {
                            assert!(w[0].end <= w[1].start, "{r:?}");
                        }
                        for x in r {
                            assert!(x.start < x.end, "{r:?}");
                        }
                        let tight = is_tight(&p);
                        let mut expected = BTreeSet::new();
                        for (url, k) in &corpus {
                            let m = p.matches(k);
                            assert_eq!(m, oracle(ph, subs, port, path, url), "{ph} {url} {k}");
                            let inr = r.iter().any(|x| {
                                x.start.as_str() <= k.as_str() && k.as_str() < x.end.as_str()
                            });
                            if m {
                                assert!(inr, "{ph} {k} {r:?}");
                                expected.insert(k.clone());
                            }
                            if tight && inr {
                                assert!(m, "tight {ph} {k} {r:?}");
                            }
                        }
                        let mut scanned = BTreeSet::new();
                        for x in r {
                            for k in all.range(x.start.clone()..x.end.clone()) {
                                if p.matches(k) {
                                    scanned.insert(k.clone());
                                }
                            }
                        }
                        assert_eq!(scanned, expected, "{ph}");
                    }
                }
            }
        }
        assert!(npat > 100);
    }
}
