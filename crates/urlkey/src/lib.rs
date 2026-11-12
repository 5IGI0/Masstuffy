//! Massaged-URL canonical keys and URL pattern range queries.
//!
//! [`massage`] turns a URL into a canonical, byte-orderable key (spec 01 "Massaged URL"): the host
//! labels are reversed so that all subdomains of a domain are contiguous in key order. [`UrlPattern`]
//! (spec 04 §4.3) turns user input (domain, port, path) into sorted, disjoint key ranges plus an exact
//! predicate, so an ordered index can answer pattern searches by range scans. There is no regex.
//!
//! # Key grammar
//!
//! ```text
//! key        = host [ ":" port ] ")" path [ "?" query ]
//! host       = domain-key | ipv4 | "[" ipv6 "]"
//! domain-key = label *( "," label )        ; labels of the host in REVERSE order
//! label      = 1*( %x61-7A / DIGIT / "-" / "_" )   ; [a-z0-9_-] only, never empty
//! ipv4       = dotted decimal as printed by std (e.g. 127.0.0.1), NOT reversed
//! ipv6       = RFC 5952 text as printed by std `Ipv6Addr` Display (lowercase, compressed)
//! port       = decimal u16, no leading zero, present only if not the scheme default
//! path       = "/" *pchar                  ; never contains '?' ; never empty
//! query      = pair *( "&" pair )          ; pair = enc-key [ "=" enc-value ]
//! ```
//!
//! Invariants: every key byte is in `0x21..=0x7E` (printable ASCII, no space); the first `)` in a key
//! ends the authority (no host form contains `)`); the first `?` after it starts the query (paths never
//! contain `?`); domain keys contain no `.` or `:`; IPv4 keys contain `.`; IPv6 keys start with `[`, so
//! the three host forms are disjoint.
//!
//! # Decisions
//!
//! * Only `http` and `https` get a key; the scheme is dropped, so other schemes would collide.
//!   Anything else, an unparseable URL or a host outside `[a-z0-9_-]` labels gives `None`.
//! * Parsing is WHATWG (`url` crate): case, dot segments, `\`, IDNA (punycode) and percent-decoded hosts
//!   are handled there. Userinfo and fragment are ignored.
//! * Default ports (80 for http, 443 for https) are omitted; any other port is kept.
//! * Leading and trailing dots of the host are removed; `www` is kept.
//! * The path keeps its case; percent-escapes of unreserved bytes are decoded, others are uppercased.
//! * The query is canonicalised: pairs are split on `&` (empty pieces dropped), keys and values are
//!   decoded as form-urlencoded (`+` is a space) and re-encoded, then pairs are sorted. `a` and `a=` stay
//!   distinct. An empty query leaves no `?`.
//!
//! # Path filters
//!
//! A pattern path filter applies to the key tail (path and query, everything after the first `)`), like
//! the Wayback CDX server's `matchType`: `Exact` is the whole canonical tail (a query in it is
//! canonicalised and sorted like stored keys), `Prefix` is "begins with". A query prefix is
//! canonicalised piece-wise without sorting, so its complete pairs must already be in canonical order,
//! and it must not end inside a percent-escape (a path prefix neither). The last path segment of a prefix
//! is literal (`/.` matches `/.git`); complete segments are resolved. Violations give
//! `PatternError::InvalidQuery` (or `InvalidPath`). `#` is refused; tabs and newlines are removed.
//!
//! Changing the output of [`massage`] is an index format change: it requires rebuilding the index.
//!
//! ```
//! use std::collections::BTreeSet;
//! use masstuffy_urlkey::{massage, HostMatch, PathFilter, PortFilter, UrlPattern};
//!
//! let key = massage("https://www.Example.com:8443/a/b?z=2&a=1#frag");
//! assert_eq!(key.as_deref(), Some("com,example,www:8443)/a/b?a=1&z=2"));
//!
//! let keys: BTreeSet<String> = [
//!     "https://example.com/a/1",
//!     "https://www.example.com/a/2",
//!     "https://examples.com/a/3",
//!     "https://example.com/b",
//! ]
//! .iter()
//! .filter_map(|u| massage(u))
//! .collect();
//!
//! let pattern = UrlPattern::new(
//!     "example.com",
//!     HostMatch::WithSubdomains,
//!     PortFilter::Any,
//!     PathFilter::Prefix("/a/"),
//! )?;
//! let mut found = Vec::new();
//! for r in pattern.key_ranges() {
//!     for k in keys.range(r.start.clone()..r.end.clone()) {
//!         if pattern.matches(k) {
//!             found.push(k.as_str());
//!         }
//!     }
//! }
//! assert_eq!(found, ["com,example)/a/1", "com,example,www)/a/2"]);
//! # Ok::<(), masstuffy_urlkey::PatternError>(())
//! ```

#![forbid(unsafe_code)]

mod error;
mod key;
mod pattern;

pub use error::PatternError;
pub use key::massage;
pub use pattern::{HostMatch, PathFilter, PortFilter, UrlPattern};
