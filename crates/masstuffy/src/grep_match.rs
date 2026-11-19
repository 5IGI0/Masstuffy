//! Pattern building and matching for grep: a byte-mode `regex-automata` meta regex that tells
//! which of up to 64 queries match a haystack, in linear time.

use std::fmt::Write as _;

use masstuffy_warc::RecordRef;
use regex_automata::meta::{self, Cache};
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, PatternSet};

use crate::error::Error;
use crate::grep::{GrepQuery, GrepScope, QuerySet};

/// NFA size limit of one compiled grep.
const NFA_SIZE_LIMIT: usize = 4 << 20;
/// Lazy-DFA cache capacity of one (grep, collection) cache.
const HYBRID_CACHE_CAPACITY: usize = 512 << 10;

/// The compiled queries of one grep, shared by its subscriptions (each with its own `Cache`).
pub(crate) struct Matcher {
    re: meta::Regex,
    n: usize,
}

impl Matcher {
    /// Compiles every query into one multi-pattern regex (`MatchKind::All`, Unicode off, no
    /// UTF-8 requirement). Blocking (CPU): call from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// `InvalidPattern` with the index of the offending query when known (syntax error, non-ASCII
    /// character in a regex, size limit).
    pub(crate) fn compile(queries: &[GrepQuery]) -> Result<Matcher, Error> {
        let mut srcs: Vec<String> = Vec::with_capacity(queries.len());
        for (i, q) in queries.iter().enumerate() {
            let mut src =
                String::with_capacity(q.pattern.len().saturating_mul(2).saturating_add(4));
            if q.case_insensitive {
                src.push_str("(?i)");
            }
            if q.regex {
                // regex-syntax alone accepts a bare non-ASCII literal (as its UTF-8 bytes) in byte
                // mode but rejects one inside a class; reject both for one simple rule.
                if !q.pattern.is_ascii() {
                    return Err(Error::InvalidPattern {
                        index: Some(i),
                        message: "non-ASCII character in a regex (write \\xHH)".to_owned(),
                    });
                }
                src.push_str(&q.pattern);
            } else {
                for &b in q.pattern.as_bytes() {
                    if b.is_ascii_alphanumeric() {
                        src.push(char::from(b));
                    } else {
                        // Writing to a `String` cannot fail.
                        let _ = write!(src, "\\x{b:02X}");
                    }
                }
            }
            srcs.push(src);
        }
        let re = meta::Builder::new()
            .configure(
                meta::Config::new()
                    .match_kind(MatchKind::All)
                    .utf8_empty(false)
                    .nfa_size_limit(Some(NFA_SIZE_LIMIT))
                    .hybrid_cache_capacity(HYBRID_CACHE_CAPACITY),
            )
            .syntax(syntax::Config::new().unicode(false).utf8(false))
            .build_many(&srcs)
            .map_err(|e| Error::InvalidPattern {
                index: e.pattern().map(|p| p.as_usize()),
                message: e.to_string(),
            })?;
        Ok(Matcher {
            re,
            n: queries.len(),
        })
    }

    /// A fresh search cache (one per subscription).
    pub(crate) fn create_cache(&self) -> Cache {
        self.re.create_cache()
    }

    /// A fresh pattern set sized for this matcher.
    pub(crate) fn pattern_set(&self) -> PatternSet {
        PatternSet::new(self.n)
    }

    /// Union of the queries matching any of `parts`; allocation-free.
    pub(crate) fn matches(
        &self,
        parts: &[&[u8]; 2],
        cache: &mut Cache,
        set: &mut PatternSet,
    ) -> QuerySet {
        set.clear();
        for part in parts {
            if !part.is_empty() {
                self.re
                    .which_overlapping_matches_with(cache, &Input::new(part), set);
            }
        }
        let mut bits = 0u64;
        for pid in set.iter() {
            if let Some(bit) = 1u64.checked_shl(u32::try_from(pid.as_usize()).unwrap_or(u32::MAX)) {
                bits |= bit;
            }
        }
        QuerySet::from_bits(bits)
    }
}

/// The searchable parts of one record, borrowed from its decoded buffer. A match never spans two
/// parts.
pub(crate) struct Parts<'a> {
    whole: &'a [u8],
    warc_head: &'a [u8],
    http_head: &'a [u8],
    payload: &'a [u8],
}

/// True when the WARC `Content-Type` (parameters cut, ASCII case ignored) is `application/http`.
fn is_http_message(rec: &RecordRef<'_>) -> bool {
    rec.header("Content-Type").is_some_and(|v| {
        let essence = v.split_once(';').map_or(v, |(a, _)| a);
        essence
            .trim_ascii()
            .eq_ignore_ascii_case("application/http")
    })
}

impl<'a> Parts<'a> {
    /// Splits `rec`: the WARC head, and for an HTTP message the HTTP head (through the first
    /// CRLF CRLF, or the whole body when there is none) and the payload after it; for any other
    /// record the payload is the whole body.
    pub(crate) fn of(rec: &RecordRef<'a>) -> Parts<'a> {
        let body = rec.body();
        let (http_head, payload): (&'a [u8], &'a [u8]) = if is_http_message(rec) {
            let split = body
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .and_then(|i| i.checked_add(4))
                .and_then(|n| body.split_at_checked(n));
            split.unwrap_or((body, &[]))
        } else {
            (&[], body)
        };
        Parts {
            whole: rec.as_bytes(),
            warc_head: rec.head(),
            http_head,
            payload,
        }
    }

    /// The slices searched for `scope`: `All` the whole record; `Headers` the WARC head and the
    /// HTTP head; `Payload` the payload.
    pub(crate) fn for_scope(&self, scope: GrepScope) -> [&'a [u8]; 2] {
        match scope {
            GrepScope::All => [self.whole, &[]],
            GrepScope::Headers => [self.warc_head, self.http_head],
            GrepScope::Payload => [self.payload, &[]],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use masstuffy_warc::RecordBuilder;

    fn q(pattern: &str, regex: bool, ci: bool) -> GrepQuery {
        GrepQuery {
            pattern: pattern.to_owned(),
            regex,
            case_insensitive: ci,
        }
    }

    fn run(queries: &[GrepQuery], hay: &[u8]) -> QuerySet {
        let m = Matcher::compile(queries).unwrap();
        let mut cache = m.create_cache();
        let mut set = m.pattern_set();
        m.matches(&[hay, &[]], &mut cache, &mut set)
    }

    fn hit(query: GrepQuery, hay: &[u8]) -> bool {
        !run(&[query], hay).is_empty()
    }

    fn compile_err(queries: &[GrepQuery]) -> Error {
        match Matcher::compile(queries) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        }
    }

    #[test]
    fn literal_matches_bytes() {
        assert!(hit(q("a.b", false, false), b"xa.by"));
        assert!(!hit(q("a.b", false, false), b"axb"));
    }

    #[test]
    fn literal_non_ascii() {
        assert!(hit(q("é", false, false), "caf\u{e9} noir".as_bytes()));
        assert!(!hit(q("é", false, false), b"cafe"));
    }

    #[test]
    fn literal_case_insensitive_ascii() {
        assert!(hit(q("NeEdLe", false, true), b"a needle here"));
        assert!(!hit(q("NeEdLe", false, false), b"a needle here"));
    }

    #[test]
    fn regex_byte_mode() {
        assert!(hit(q("n[e]+dle", true, false), b"a neeedle"));
        assert!(hit(q(r"\xFF\x00", true, false), &[1, 0xFF, 0x00, 2]));
        assert!(hit(q("a.b", true, false), &[b'a', 0x80, b'b']));
        assert!(!hit(q("a.b", true, false), b"a\nb"));
    }

    #[test]
    fn regex_non_ascii_rejected() {
        let e = compile_err(&[q("é", true, false)]);
        assert!(matches!(e, Error::InvalidPattern { index: Some(0), .. }));
    }

    #[test]
    fn regex_syntax_error_index() {
        let e = compile_err(&[
            q("ok", true, false),
            q("(", true, false),
            q("ok", true, false),
        ]);
        assert!(matches!(e, Error::InvalidPattern { index: Some(1), .. }));
    }

    #[test]
    fn size_limit_rejected() {
        let e = compile_err(&[q("(?:a{1000}){1000}", true, false)]);
        assert!(matches!(e, Error::InvalidPattern { .. }));
    }

    #[test]
    fn multi_query_set() {
        let s = run(
            &[
                q("alpha", false, false),
                q("beta", false, false),
                q("gamma", false, false),
            ],
            b"alpha and gamma",
        );
        assert_eq!(s.bits(), 0b101);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(s.len(), 2);
        assert!(s.contains(2) && !s.contains(1) && !s.contains(64));
    }

    fn http_record(content_type: &str, body: &[u8]) -> masstuffy_warc::Record {
        let mut b = RecordBuilder::new("response").unwrap();
        b.record_id("urn:uuid:00000000-0000-4000-8000-000000000001")
            .unwrap();
        b.date(masstuffy_warc::WarcDate::parse("2026-11-13T10:00:00Z").unwrap());
        b.header("Content-Type", content_type).unwrap();
        b.build(body).unwrap()
    }

    #[test]
    fn parts_rules() {
        let body = b"HTTP/1.1 200 OK\r\nX-H: v\r\n\r\npayload\r\n\r\nmore";
        let rec = http_record("Application/HTTP; msgtype=response", body);
        let view = rec.view();
        let p = Parts::of(&view);
        assert_eq!(p.http_head, b"HTTP/1.1 200 OK\r\nX-H: v\r\n\r\n");
        assert_eq!(p.payload, b"payload\r\n\r\nmore");
        assert_eq!(p.for_scope(GrepScope::All), [view.as_bytes(), &[][..]]);
        assert_eq!(p.for_scope(GrepScope::Headers), [view.head(), p.http_head]);
        assert_eq!(p.for_scope(GrepScope::Payload), [p.payload, &[][..]]);

        let rec = http_record("application/http", b"HTTP/1.1 200 OK\r\nX-H: v");
        let view = rec.view();
        let p = Parts::of(&view);
        assert_eq!(p.http_head, b"HTTP/1.1 200 OK\r\nX-H: v");
        assert!(p.payload.is_empty());

        let rec = http_record("text/plain", b"a\r\n\r\nb");
        let view = rec.view();
        let p = Parts::of(&view);
        assert!(p.http_head.is_empty());
        assert_eq!(p.payload, b"a\r\n\r\nb");
    }

    #[test]
    fn match_does_not_span_parts() {
        let rec = http_record("application/http", b"HTTP/1.1 200 OK\r\n\r\n");
        let view = rec.view();
        let p = Parts::of(&view);
        let warc_tail = &view.head()[view.head().len() - 4..];
        assert_eq!(warc_tail, b"\r\n\r\n");
        // The WARC head ends with CRLF CRLF and the HTTP head starts with `HTTP/`.
        let m = Matcher::compile(&[q(r"\r\n\r\nHTTP", true, false)]).unwrap();
        let mut cache = m.create_cache();
        let mut set = m.pattern_set();
        let hit = m.matches(&p.for_scope(GrepScope::Headers), &mut cache, &mut set);
        assert!(hit.is_empty());
        let hit = m.matches(&p.for_scope(GrepScope::All), &mut cache, &mut set);
        assert!(!hit.is_empty());
    }
}
