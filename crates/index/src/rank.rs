//! Shared query semantics: every backend uses these helpers so that they cannot diverge.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::types::{CollectionKey, Position};

/// Lookup key of a URL. Entries are indexed under `url_key(url)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlKey<'a> {
    /// The massaged key (http and https URLs).
    Massaged(String),
    /// The raw URL, verbatim (URLs `massage` rejects: `urn:`, `dns:`, `ftp:`, ...).
    Raw(&'a str),
}

/// `massage(url)` if it is `Some`, else the raw URL verbatim. A `Massaged` query only matches
/// `Massaged` keys, a `Raw` query only byte-equal `Raw` keys.
#[must_use]
pub fn url_key(url: &str) -> UrlKey<'_> {
    match masstuffy_urlkey::massage(url) {
        Some(k) => UrlKey::Massaged(k),
        None => UrlKey::Raw(url),
    }
}

/// `warc_type == "request"` (exact, case-sensitive). Requests are excluded from URL resolution.
#[must_use]
pub fn is_request(warc_type: &str) -> bool {
    warc_type == "request"
}

/// Compares two candidates of a nearest-date lookup. `a` and `b` are `(date nanos, position)`.
/// Returns `Greater` iff `a` is the better candidate.
///
/// With `Some(t)`: smaller `|n - t|` wins; ties go to the greater `n`, then the greater position.
/// With `None`: the greatest `(n, position)` wins. Total order (distinct positions break all ties).
#[must_use]
pub fn nearest_cmp(target: Option<i128>, a: (i128, Position), b: (i128, Position)) -> Ordering {
    let by_date = match target {
        // |values| < 2^69, so the subtraction cannot overflow.
        Some(t) => (b.0 - t).abs().cmp(&(a.0 - t).abs()),
        None => Ordering::Equal,
    };
    by_date.then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
}

/// A visible copy of a record id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdCandidate {
    /// Collection of the copy.
    pub collection: CollectionKey,
    /// `WarcDate::unix_nanos()` of the copy.
    pub date_nanos: i128,
    /// Locator of the copy.
    pub position: Position,
}

/// Picks the "most recently committed active copy": within each collection the greatest position,
/// then across collections the greatest `(date_nanos, collection)`.
pub fn pick_by_id<I: IntoIterator<Item = IdCandidate>>(candidates: I) -> Option<IdCandidate> {
    let mut per_collection: BTreeMap<CollectionKey, IdCandidate> = BTreeMap::new();
    for c in candidates {
        per_collection
            .entry(c.collection)
            .and_modify(|best| {
                if c.position > best.position {
                    *best = c;
                }
            })
            .or_insert(c);
    }
    per_collection
        .into_values()
        .max_by_key(|c| (c.date_nanos, c.collection))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(segment: u32, offset: u64) -> Position {
        Position { segment, offset }
    }

    #[test]
    fn url_key_classes() {
        let a = url_key("http://Example.com:80/a?b=2&a=1");
        let b = url_key("https://example.com/a?a=1&b=2");
        assert!(matches!(a, UrlKey::Massaged(_)));
        assert_eq!(a, b);
        for u in ["urn:uuid:x", "dns:example.com", "ftp://h/p", "not a url"] {
            assert_eq!(url_key(u), UrlKey::Raw(u));
        }
    }

    #[test]
    fn is_request_exact() {
        assert!(is_request("request"));
        for t in ["Request", "response", "requests"] {
            assert!(!is_request(t));
        }
    }

    #[test]
    fn nearest_cmp_cases() {
        use Ordering::{Equal, Greater, Less};
        let t = Some(100);
        assert_eq!(nearest_cmp(t, (90, p(1, 0)), (120, p(1, 1))), Greater);
        assert_eq!(nearest_cmp(t, (120, p(1, 1)), (90, p(1, 0))), Less);
        assert_eq!(nearest_cmp(t, (90, p(1, 0)), (110, p(1, 1))), Less);
        assert_eq!(nearest_cmp(t, (110, p(1, 0)), (90, p(1, 1))), Greater);
        assert_eq!(nearest_cmp(t, (90, p(1, 1)), (90, p(1, 0))), Greater);
        assert_eq!(nearest_cmp(None, (90, p(9, 9)), (80, p(0, 0))), Greater);
        assert_eq!(nearest_cmp(None, (90, p(1, 1)), (90, p(1, 0))), Greater);
        assert_eq!(nearest_cmp(None, (90, p(1, 1)), (90, p(1, 1))), Equal);
        assert_eq!(nearest_cmp(t, (90, p(1, 1)), (90, p(1, 1))), Equal);
    }

    #[test]
    fn pick_by_id_two_level() {
        let c = |k: u128, n: i128, pos: Position| IdCandidate {
            collection: CollectionKey::new(k),
            date_nanos: n,
            position: pos,
        };
        let a1 = c(1, 2020, p(1, 0));
        let a2 = c(1, 2010, p(1, 10));
        let b1 = c(2, 2015, p(1, 0));
        assert_eq!(pick_by_id([a1, a2, b1]), Some(b1));
        let b2 = c(2, 2010, p(1, 0));
        assert_eq!(pick_by_id([a2, b2]), Some(b2));
        assert_eq!(pick_by_id([a1, a2]), Some(a2));
        assert_eq!(pick_by_id([]), None);
    }
}
