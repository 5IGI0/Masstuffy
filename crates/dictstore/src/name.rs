//! File-name grammar: `<label>.<dict_id>.zstdict`.

use crate::{EXT, MAX_LABEL_LEN};

/// Whether `label` is 1..=[`MAX_LABEL_LEN`] bytes of `[A-Za-z0-9_-]`.
///
/// Such a label has no `.`, `/` or NUL, is never empty and is never a reserved name (those start
/// with `.`). Generated labels are `<slug>_<YYYYmmddHHMMSS>`; slugs need not be valid file names, so
/// the engine maps each byte outside `[A-Za-z0-9_-]` to `_`, truncates, and checks the result with
/// this function.
#[must_use]
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `(label, id)` if `name` is exactly `<label>.<id>.zstdict`, with a canonical decimal non-zero
/// `u32` id (no sign, no leading zero).
pub(crate) fn parse_file_name(name: &str) -> Option<(&str, u32)> {
    let mut parts = name.split('.');
    let label = parts.next()?;
    let id = parts.next()?;
    let ext = parts.next()?;
    if parts.next().is_some() || ext != EXT || !is_valid_label(label) {
        return None;
    }
    if id.is_empty()
        || id.len() > 10
        || !id.bytes().all(|b| b.is_ascii_digit())
        || (id.len() > 1 && id.starts_with('0'))
    {
        return None;
    }
    let id: u32 = id.parse().ok()?;
    (id != 0).then_some((label, id))
}

/// `<label>.<id>.zstdict`; the caller has validated `label` and `id != 0`.
pub(crate) fn file_name(label: &str, id: u32) -> String {
    format!("{label}.{id}.{EXT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok() {
        assert_eq!(parse_file_name("a.40000.zstdict"), Some(("a", 40000)));
        assert_eq!(
            parse_file_name("Slug_20261113120000.4294967295.zstdict"),
            Some(("Slug_20261113120000", u32::MAX))
        );
        assert_eq!(parse_file_name("x-y_Z9.1.zstdict"), Some(("x-y_Z9", 1)));
    }

    #[test]
    fn parse_rejects() {
        let long = format!("{}.1.zstdict", "a".repeat(129));
        let ok_long = format!("{}.1.zstdict", "a".repeat(128));
        for bad in [
            "a.b.c.zstdict",
            "a.40000",
            "a.40000.zstdict.bak",
            "a.40000.ZSTDICT",
            ".40000.zstdict",
            "a..zstdict",
            "a.0.zstdict",
            "a.040000.zstdict",
            "a.+40000.zstdict",
            "a.-1.zstdict",
            "a.4294967296.zstdict",
            "a.99999999999.zstdict",
            "a b.40000.zstdict",
            "é.40000.zstdict",
            "a/b.1.zstdict",
            "",
            long.as_str(),
        ] {
            assert_eq!(parse_file_name(bad), None, "{bad:?}");
        }
        assert!(parse_file_name(&ok_long).is_some());
    }

    #[test]
    fn label_validity() {
        assert!(is_valid_label("a"));
        assert!(is_valid_label(&"a".repeat(128)));
        assert!(is_valid_label("my-coll_2026"));
        assert!(!is_valid_label(""));
        assert!(!is_valid_label(&"a".repeat(129)));
        for bad in ["a.b", "a/b", "a\0", ".tmp-x", "ü"] {
            assert!(!is_valid_label(bad), "{bad:?}");
        }
    }

    #[test]
    fn file_name_roundtrip() {
        assert_eq!(
            parse_file_name(&file_name("lbl", 40000)),
            Some(("lbl", 40000))
        );
    }
}
