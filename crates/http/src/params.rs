//! Query-string parsing and date parameters.

use std::ops::RangeInclusive;

use masstuffy_warc::WarcDate;

use crate::error::ApiError;

/// Longest raw query string accepted.
const MAX_QUERY_STRING: usize = 1 << 20;

/// A parsed query string: each key at most once, values percent-decoded (`+` is a space).
pub(crate) struct Params {
    map: Vec<(String, String)>,
}

impl Params {
    /// Parses `query`. `allowed` lists every accepted key.
    ///
    /// # Errors
    ///
    /// `invalid_param` for an unknown or duplicate key, or a query string over 1 MiB.
    pub(crate) fn parse(query: Option<&str>, allowed: &[&str]) -> Result<Params, ApiError> {
        let q = query.unwrap_or("");
        if q.len() > MAX_QUERY_STRING {
            return Err(ApiError::invalid("invalid_param", "query string too long"));
        }
        let mut map: Vec<(String, String)> = Vec::new();
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if !allowed.contains(&&*k) {
                return Err(ApiError::invalid(
                    "invalid_param",
                    format!("unknown query parameter {k:?}"),
                ));
            }
            if map.iter().any(|(e, _)| *e == *k) {
                return Err(ApiError::invalid(
                    "invalid_param",
                    format!("duplicate query parameter {k:?}"),
                ));
            }
            map.push((k.into_owned(), v.into_owned()));
        }
        Ok(Params { map })
    }

    pub(crate) fn str(&self, k: &str) -> Option<&str> {
        self.map
            .iter()
            .find(|(e, _)| e == k)
            .map(|(_, v)| v.as_str())
    }

    pub(crate) fn required(&self, k: &str) -> Result<&str, ApiError> {
        self.str(k).ok_or_else(|| {
            ApiError::invalid("invalid_param", format!("missing query parameter {k:?}"))
        })
    }

    /// A canonical decimal in `range`, or `default` when absent.
    pub(crate) fn u64_in(
        &self,
        k: &str,
        range: RangeInclusive<u64>,
        default: u64,
    ) -> Result<u64, ApiError> {
        let Some(v) = self.str(k) else {
            return Ok(default);
        };
        let n = parse_canonical(v).ok_or_else(|| {
            ApiError::invalid("invalid_param", format!("{k}: expected a decimal number"))
        })?;
        if !range.contains(&n) {
            return Err(ApiError::invalid(
                "invalid_param",
                format!("{k}: must be in {}..={}", range.start(), range.end()),
            ));
        }
        Ok(n)
    }

    /// `true`/`false`/`1`/`0`, or `default` when absent.
    pub(crate) fn bool(&self, k: &str, default: bool) -> Result<bool, ApiError> {
        match self.str(k) {
            None => Ok(default),
            Some("true" | "1") => Ok(true),
            Some("false" | "0") => Ok(false),
            Some(_) => Err(ApiError::invalid(
                "invalid_param",
                format!("{k}: expected true or false"),
            )),
        }
    }
}

/// Decimal digits only, no leading zero (except `0` itself), fits in a `u64`.
pub(crate) fn parse_canonical(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

const DATE_MSG: &str = "date: expected YYYY-MM-DDThh:mm:ss[.f]Z or YYYY[MM[DD[hh[mm[ss]]]]]";

/// A WARC date, or a Wayback timestamp of 4, 6, 8, 10, 12 or 14 digits (missing trailing
/// components default to month 01, day 01, 00:00:00).
pub(crate) fn parse_date(s: &str) -> Result<WarcDate, ApiError> {
    let bad = || ApiError::invalid("invalid_param", DATE_MSG);
    if let Ok(d) = WarcDate::parse(s) {
        return Ok(d);
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) || !matches!(s.len(), 4 | 6 | 8 | 10 | 12 | 14) {
        return Err(bad());
    }
    let num = |from: usize, to: usize, default: u32| -> Option<u32> {
        match s.get(from..to) {
            Some(t) => t.parse().ok(),
            None => Some(default),
        }
    };
    let year = u16::try_from(num(0, 4, 0).ok_or_else(bad)?).map_err(|_| bad())?;
    let field = |from, to, default| -> Result<u8, ApiError> {
        u8::try_from(num(from, to, default).ok_or_else(bad)?).map_err(|_| bad())
    };
    WarcDate::new(
        year,
        field(4, 6, 1)?,
        field(6, 8, 1)?,
        field(8, 10, 0)?,
        field(10, 12, 0)?,
        field(12, 14, 0)?,
        0,
    )
    .ok_or_else(bad)
}

/// Percent-encodes everything outside `[A-Za-z0-9._~-]`.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_rejects_unknown_and_duplicate() {
        assert!(Params::parse(Some("a=1&b=2"), &["a", "b"]).is_ok());
        assert_eq!(
            Params::parse(Some("a=1&c=2"), &["a", "b"])
                .err()
                .unwrap()
                .code,
            "invalid_param"
        );
        assert_eq!(
            Params::parse(Some("a=1&a=2"), &["a"]).err().unwrap().code,
            "invalid_param"
        );
        assert!(Params::parse(None, &["a"]).is_ok());
        let long = format!("a={}", "x".repeat(MAX_QUERY_STRING));
        assert!(Params::parse(Some(&long), &["a"]).is_err());
        let p = Params::parse(Some("a=1"), &["a", "b"]).unwrap();
        assert!(p.required("b").is_err());
        assert_eq!(p.required("a").unwrap(), "1");
    }

    #[test]
    fn params_percent_decoding() {
        let p = Params::parse(Some("url=http%3A%2F%2Fa%2Fb%3Fx%3D1&q=a+b"), &["url", "q"]).unwrap();
        assert_eq!(p.str("url"), Some("http://a/b?x=1"));
        assert_eq!(p.str("q"), Some("a b"));
    }

    #[test]
    fn params_numbers() {
        let get = |q: &str| {
            Params::parse(Some(q), &["n"])
                .unwrap()
                .u64_in("n", 1..=100, 5)
        };
        assert_eq!(get("n=42").unwrap(), 42);
        assert_eq!(
            Params::parse(None, &["n"])
                .unwrap()
                .u64_in("n", 1..=100, 5)
                .unwrap(),
            5
        );
        for bad in [
            "n=%2B5",
            "n=+5",
            "n=-5",
            "n=007",
            "n=18446744073709551616",
            "n=0",
            "n=101",
            "n=",
            "n=1.5",
        ] {
            assert_eq!(get(bad).unwrap_err().code, "invalid_param", "{bad}");
        }
        let b = |q: &str| Params::parse(Some(q), &["b"]).unwrap().bool("b", false);
        assert!(b("b=true").unwrap());
        assert!(b("b=1").unwrap());
        assert!(!b("b=0").unwrap());
        assert!(b("b=yes").is_err());
    }

    #[test]
    fn date_warc_and_wayback() {
        let d = parse_date("2024-01-02T03:04:05Z").unwrap();
        assert_eq!(
            (
                d.year(),
                d.month(),
                d.day(),
                d.hour(),
                d.minute(),
                d.second()
            ),
            (2024, 1, 2, 3, 4, 5)
        );
        let d = parse_date("2024-01-02T03:04:05.5Z").unwrap();
        assert_eq!(d.nanos(), 500_000_000);
        let f = |s: &str| {
            let d = parse_date(s).unwrap();
            (
                d.year(),
                d.month(),
                d.day(),
                d.hour(),
                d.minute(),
                d.second(),
                d.nanos(),
            )
        };
        assert_eq!(f("2024"), (2024, 1, 1, 0, 0, 0, 0));
        assert_eq!(f("202406"), (2024, 6, 1, 0, 0, 0, 0));
        assert_eq!(f("20240702"), (2024, 7, 2, 0, 0, 0, 0));
        assert_eq!(f("2024070203"), (2024, 7, 2, 3, 0, 0, 0));
        assert_eq!(f("202407020304"), (2024, 7, 2, 3, 4, 0, 0));
        assert_eq!(f("20240702030405"), (2024, 7, 2, 3, 4, 5, 0));
        for bad in ["2024010", "20241301", "20240230", "abcd", "2024-01-02", ""] {
            assert_eq!(parse_date(bad).unwrap_err().code, "invalid_param", "{bad}");
        }
    }

    #[test]
    fn percent_encode_escapes() {
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a b/é"), "a%20b%2F%C3%A9");
    }
}
