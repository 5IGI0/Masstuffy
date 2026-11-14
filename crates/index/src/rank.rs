//! Shared semantics (queries and op rules): every backend uses these helpers so that they cannot
//! diverge.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use masstuffy_cdx::{Entry, Kind};

use crate::error::Error;
use crate::types::{CollectionKey, CollectionState, Op, Position};

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

/// Checks an `Insert` entry the way every backend does before any state check: `NotAnAdd` if
/// `entry.kind != Kind::Add` (checked first), then `InvalidEntry` if `entry.validate()` fails.
///
/// # Errors
/// `Error::NotAnAdd { op }`, `Error::InvalidEntry { op, source }`.
pub fn validate_insert(op: usize, entry: &Entry<'_>) -> Result<(), Error> {
    if entry.kind != Kind::Add {
        return Err(Error::NotAnAdd { op });
    }
    entry
        .validate()
        .map_err(|source| Error::InvalidEntry { op, source })
}

/// Generation checks of op number `op` against the state of its collection at that point of the
/// batch (`None` = unknown collection). `StaleGeneration` for Insert/Tombstone/ReplaceGeneration
/// (their `generation`) and Checkpoint (`checkpoint.generation`) when `state` has an active
/// generation greater than that generation. `NotStaged` for `DiscardStaged` when `state` has an
/// active generation `>= generation`. `DropCollection` never fails. No check when `state` is `None`
/// or has no active generation.
///
/// # Errors
/// `Error::StaleGeneration`, `Error::NotStaged`.
pub fn check_op(
    op: usize,
    state: Option<&CollectionState>,
    operation: &Op<'_>,
) -> Result<(), Error> {
    let collection = operation.collection();
    let Some(active) = state.and_then(|s| s.active_generation) else {
        return Ok(());
    };
    let checked = match operation {
        Op::Insert { generation, .. }
        | Op::Tombstone { generation, .. }
        | Op::ReplaceGeneration { generation, .. } => Some(*generation),
        Op::Checkpoint { checkpoint, .. } => Some(checkpoint.generation),
        Op::DiscardStaged { .. } | Op::DropCollection { .. } => None,
    };
    if let Some(generation) = checked
        && generation < active
    {
        return Err(Error::StaleGeneration {
            op,
            collection,
            generation,
            active,
        });
    }
    if let Op::DiscardStaged { generation, .. } = operation
        && *generation <= active
    {
        return Err(Error::NotStaged {
            op,
            collection,
            generation: *generation,
            active,
        });
    }
    Ok(())
}

/// State of the op's collection after `operation`, given the state before (`check_op` must have
/// passed):
/// * `DropCollection` -> `None`;
/// * `DiscardStaged { generation }` -> `None` stays `None`; otherwise `staged_checkpoint` is cleared
///   if its generation equals `generation`;
/// * any other op -> `Some(state.unwrap_or_default())`, then `ReplaceGeneration { g }` with
///   `active_generation != Some(g)` activates `g`, and `Checkpoint` routes the checkpoint.
#[must_use]
pub fn step_state(state: Option<CollectionState>, operation: &Op<'_>) -> Option<CollectionState> {
    match operation {
        Op::DropCollection { .. } => None,
        Op::DiscardStaged { generation, .. } => state.map(|mut st| {
            st.staged_checkpoint = st.staged_checkpoint.filter(|c| c.generation != *generation);
            st
        }),
        _ => {
            let mut st = state.unwrap_or_default();
            match operation {
                Op::ReplaceGeneration { generation, .. }
                    if st.active_generation != Some(*generation) =>
                {
                    st.activate(*generation);
                }
                Op::Checkpoint { checkpoint, .. } => st.route_checkpoint(*checkpoint),
                _ => {}
            }
            Some(st)
        }
    }
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

    fn ck(g: u32, o: u64) -> crate::Checkpoint {
        crate::Checkpoint {
            generation: g,
            cdx_offset: o,
        }
    }

    fn entry(kind: Kind, raw_size: u64, record_id: &str) -> Entry<'static> {
        use std::borrow::Cow;
        Entry {
            kind,
            segment: 1,
            offset: 0,
            raw_size,
            digest: 0,
            date: crate::WarcDate::new(2020, 1, 1, 0, 0, 0, 0).unwrap(),
            warc_type: Cow::Borrowed("response"),
            record_id: Cow::Owned(record_id.to_owned()),
            url: None,
        }
    }

    #[test]
    fn validate_insert_precedence() {
        assert!(matches!(
            validate_insert(3, &entry(Kind::Tombstone, 0, "a")),
            Err(Error::NotAnAdd { op: 3 })
        ));
        assert!(validate_insert(0, &entry(Kind::Add, 10, "a")).is_ok());
        assert!(matches!(
            validate_insert(2, &entry(Kind::Add, 10, "")),
            Err(Error::InvalidEntry { op: 2, .. })
        ));
    }

    #[test]
    fn check_op_cases() {
        let c = CollectionKey::new(1);
        let ins = |g| Op::Insert {
            collection: c,
            generation: g,
            entry: entry(Kind::Add, 10, "a"),
        };
        let tomb = |g| Op::Tombstone {
            collection: c,
            generation: g,
            position: p(1, 0),
        };
        let rep = |g| Op::ReplaceGeneration {
            collection: c,
            generation: g,
        };
        let cp = |g| Op::Checkpoint {
            collection: c,
            checkpoint: ck(g, 1),
        };
        let disc = |g| Op::DiscardStaged {
            collection: c,
            generation: g,
        };
        let active2 = CollectionState {
            active_generation: Some(2),
            ..CollectionState::default()
        };
        for op in [ins(1), tomb(1), rep(1), cp(1), disc(1), disc(7)] {
            assert!(check_op(0, None, &op).is_ok());
        }
        for op in [ins(1), tomb(1), rep(1), cp(1)] {
            assert!(matches!(
                check_op(4, Some(&active2), &op),
                Err(Error::StaleGeneration {
                    op: 4,
                    active: 2,
                    generation: 1,
                    ..
                })
            ));
        }
        for g in [2, 3] {
            for op in [ins(g), tomb(g), rep(g), cp(g)] {
                assert!(check_op(0, Some(&active2), &op).is_ok());
            }
        }
        assert!(matches!(
            check_op(1, Some(&active2), &disc(2)),
            Err(Error::NotStaged {
                op: 1,
                active: 2,
                ..
            })
        ));
        assert!(check_op(1, Some(&active2), &disc(3)).is_ok());
        let none_active = CollectionState::default();
        assert!(check_op(0, Some(&none_active), &disc(1)).is_ok());
        assert!(check_op(0, Some(&active2), &Op::DropCollection { collection: c }).is_ok());
    }

    #[test]
    fn step_state_cases() {
        let c = CollectionKey::new(1);
        let rep = |g| Op::ReplaceGeneration {
            collection: c,
            generation: g,
        };
        let cp = |g, o| Op::Checkpoint {
            collection: c,
            checkpoint: ck(g, o),
        };
        let disc = |g| Op::DiscardStaged {
            collection: c,
            generation: g,
        };
        let st = CollectionState {
            active_generation: Some(1),
            checkpoint: Some(ck(1, 10)),
            staged_checkpoint: Some(ck(2, 5)),
        };
        assert_eq!(
            step_state(Some(st), &Op::DropCollection { collection: c }),
            None
        );
        assert_eq!(step_state(None, &disc(2)), None);
        // Discard clears only the matching staged checkpoint.
        assert_eq!(
            step_state(Some(st), &disc(3)).unwrap().staged_checkpoint,
            Some(ck(2, 5))
        );
        assert_eq!(
            step_state(Some(st), &disc(2)).unwrap().staged_checkpoint,
            None
        );
        // Insert on None creates the default state.
        let ins = Op::Insert {
            collection: c,
            generation: 1,
            entry: entry(Kind::Add, 10, "a"),
        };
        assert_eq!(step_state(None, &ins), Some(CollectionState::default()));
        // Same-generation replace is unchanged.
        assert_eq!(step_state(Some(st), &rep(1)), Some(st));
        // Replace to the staged generation carries the staged checkpoint over.
        let after = step_state(Some(st), &rep(2)).unwrap();
        assert_eq!(after.active_generation, Some(2));
        assert_eq!(after.checkpoint, Some(ck(2, 5)));
        assert_eq!(after.staged_checkpoint, None);
        // Replace to an unrelated generation drops both checkpoints it does not match.
        let after = step_state(Some(st), &rep(3)).unwrap();
        assert_eq!(after.checkpoint, None);
        assert_eq!(after.staged_checkpoint, None);
        // Checkpoint routing.
        let after = step_state(Some(st), &cp(1, 99)).unwrap();
        assert_eq!(after.checkpoint, Some(ck(1, 99)));
        assert_eq!(after.staged_checkpoint, Some(ck(2, 5)));
        let after = step_state(Some(st), &cp(4, 7)).unwrap();
        assert_eq!(after.checkpoint, Some(ck(1, 10)));
        assert_eq!(after.staged_checkpoint, Some(ck(4, 7)));
    }
}
