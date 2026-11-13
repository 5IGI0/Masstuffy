//! Removal of a whole log.

use std::fs;
use std::io;
use std::path::Path;

use crate::error::{Error, Op};
use crate::naming::{Naming, scan};
use crate::segment_path;

/// Deletes every segment file of this log in `dir`, highest number first, then fsyncs `dir`.
///
/// Leaves every other entry (other stems, CDX, manifest, reserved names) untouched. Idempotent:
/// missing files are skipped. Unlinks are not fsynced one by one, so a crash part-way may leave a
/// gap (`Appender::open` then returns `Gap`); a second `remove_all` finishes the job.
/// Returns the number of files removed.
///
/// The caller must have dropped the log's `Appender`. Readers keep serving already-cached
/// segments of removed files and return `SegmentNotFound` for the others: call
/// [`Reader::forget_all`](crate::Reader::forget_all) (or drop every clone) to release disk space.
///
/// # Errors
///
/// `BadSegmentName` (before any removal); `Io{ReadDir|Stat|Remove}`; `Durable` (directory fsync).
pub fn remove_all(dir: &Path, naming: &Naming) -> Result<usize, Error> {
    let mut nums: Vec<u32> = Vec::new();
    scan(dir, naming, &mut |n, _, _| {
        nums.push(n);
        Ok(())
    })?;
    nums.sort_unstable();
    let mut removed = 0usize;
    for &n in nums.iter().rev() {
        let path = segment_path(dir, naming, n);
        match fs::remove_file(&path) {
            Ok(()) => removed = removed.saturating_add(1),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Io {
                    op: Op::Remove,
                    path,
                    source: e,
                });
            }
        }
    }
    if removed > 0 {
        masstuffy_durable::sync_dir(dir)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Appender;
    use crate::testutil::{TestDir, names};
    use std::num::NonZeroU64;

    fn naming() -> Naming {
        Naming::new("seg.1", "warc").unwrap()
    }

    #[test]
    fn remove_all_only_ours() {
        let d = TestDir::new();
        let mut a = Appender::create(d.path(), naming(), NonZeroU64::new(10).unwrap()).unwrap();
        a.append_batch(&[b"12345"]).unwrap();
        a.append_batch(&[b"12345678"]).unwrap();
        a.append_batch(&[b"12345678"]).unwrap();
        assert_eq!(a.current_segment(), 3);
        a.sync().unwrap();
        drop(a);
        for s in ["index.1.cdx", "seg.2.1.warc", ".lock"] {
            fs::write(d.path().join(s), b"x").unwrap();
        }
        assert_eq!(remove_all(d.path(), &naming()).unwrap(), 3);
        assert_eq!(names(d.path()), [".lock", "index.1.cdx", "seg.2.1.warc"]);
        assert_eq!(remove_all(d.path(), &naming()).unwrap(), 0);
        assert!(matches!(
            Appender::open(d.path(), naming(), NonZeroU64::new(10).unwrap()),
            Err(Error::NoSegments { .. })
        ));
    }

    #[test]
    fn remove_all_rejects_malformed() {
        let d = TestDir::new();
        fs::write(d.path().join("seg.1.1.warc"), b"x").unwrap();
        fs::write(d.path().join("seg.1.007.warc"), b"x").unwrap();
        assert!(matches!(
            remove_all(d.path(), &naming()),
            Err(Error::BadSegmentName { .. })
        ));
        assert_eq!(names(d.path()).len(), 2);
    }
}
