//! Atomic directory deletion and startup sweeps.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use crate::atomic::sync_dir;
use crate::error::{Error, Op, io as io_err};
use crate::unique::{MAX_ATTEMPTS, unique_suffix};
use crate::{TMP_PREFIX, TRASH_PREFIX};

/// Atomically removes directory `path` from its parent's namespace, then deletes it.
///
/// Blocking: call from `spawn_blocking`. Order: `stat` (must be a real directory, not a symlink),
/// `rename` to `<parent>/.trash-<name>-<pid>-<seq>` (never replacing an existing entry),
/// `fsync(parent)`, then `remove_dir_all`. The commit point is the parent fsync: after it returns
/// `Ok` internally the directory is gone even after a crash. If the final removal fails, `path` is
/// already gone and the `.trash-*` leftover is removed by the next [`sweep_trash`] of the parent.
/// If the parent fsync fails, `path` is gone from the namespace but not durably: a crash may bring
/// it back. Symlinks inside the tree are not followed. Names longer than about 220 bytes may exceed
/// `NAME_MAX` once prefixed and fail with `Io { Rename }`; Masstuffy names are uuids.
///
/// # Errors
///
/// `InvalidName` if `path` has no final component; `Io` with op `Stat` (missing, or not a directory:
/// `NotADirectory`), `Rename`, `SyncDir` or `RemoveTrash`.
pub fn trash_dir(path: &Path) -> Result<(), Error> {
    trash_dir_with(path, &mut unique_suffix)
}

fn trash_dir_with(path: &Path, suffix: &mut dyn FnMut() -> String) -> Result<(), Error> {
    let invalid = || Error::InvalidName {
        dir: path.to_path_buf(),
        name: OsString::new(),
    };
    let name = path.file_name().ok_or_else(invalid)?;
    let parent = match path.parent() {
        None => return Err(invalid()),
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
    };
    let md = fs::symlink_metadata(path).map_err(io_err(Op::Stat, path))?;
    if !md.is_dir() {
        return Err(io_err(Op::Stat, path)(io::Error::from(
            io::ErrorKind::NotADirectory,
        )));
    }
    let mut last = None;
    let mut target = None;
    for _ in 0..MAX_ATTEMPTS {
        let mut n = OsString::from(TRASH_PREFIX);
        n.push(name);
        n.push("-");
        n.push(suffix());
        let cand = parent.join(n);
        // Linux rename silently replaces an empty directory: check existence first.
        if fs::symlink_metadata(&cand).is_ok() {
            last = Some(io::Error::from(io::ErrorKind::AlreadyExists));
            continue;
        }
        match fs::rename(path, &cand) {
            Ok(()) => {
                target = Some(cand);
                break;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                last = Some(e);
            }
            Err(e) => return Err(io_err(Op::Rename, path)(e)),
        }
    }
    let Some(target) = target else {
        let e = last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists));
        return Err(io_err(Op::Rename, path)(e));
    };
    sync_dir(parent)?;
    fs::remove_dir_all(&target).map_err(io_err(Op::RemoveTrash, &target))
}

/// Removes every `.trash-*` entry of `dir` (directories recursively, other entries as files).
///
/// Blocking. Startup only: call while holding the repository [`DirLock`](crate::DirLock) and before
/// any writer starts. Continues after a failing entry and returns the first error afterwards;
/// otherwise the number of entries removed.
///
/// # Errors
///
/// `Io { ReadDir }` on `dir`, `Io { Stat | Remove }` on an entry.
pub fn sweep_trash(dir: &Path) -> Result<usize, Error> {
    sweep_prefix(dir, TRASH_PREFIX)
}

/// Removes every `.tmp-*` entry of `dir` (files; a directory is removed recursively).
///
/// Blocking. Startup only, under the repository [`DirLock`](crate::DirLock) and before any writer
/// starts: it would delete the in-flight temp files of a running [`atomic_write`](crate::atomic_write).
/// Continues after a failing entry and returns the first error afterwards; otherwise the count.
///
/// # Errors
///
/// `Io { ReadDir }` on `dir`, `Io { Stat | Remove }` on an entry.
pub fn sweep_tmp(dir: &Path) -> Result<usize, Error> {
    sweep_prefix(dir, TMP_PREFIX)
}

fn sweep_prefix(dir: &Path, prefix: &str) -> Result<usize, Error> {
    let rd = fs::read_dir(dir).map_err(io_err(Op::ReadDir, dir))?;
    let mut count = 0usize;
    let mut first: Option<Error> = None;
    for entry in rd {
        let res = match entry {
            Err(e) => Err(io_err(Op::ReadDir, dir)(e)),
            Ok(entry) => {
                if entry
                    .file_name()
                    .as_encoded_bytes()
                    .starts_with(prefix.as_bytes())
                {
                    remove_entry(&entry.path()).map(|()| 1)
                } else {
                    Ok(0)
                }
            }
        };
        match res {
            Ok(n) => count = count.saturating_add(n),
            Err(e) => {
                first.get_or_insert(e);
            }
        }
    }
    first.map_or(Ok(count), Err)
}

fn remove_entry(path: &Path) -> Result<(), Error> {
    let md = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_err(Op::Stat, path)(e)),
    };
    let r = if md.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match r {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err(Op::Remove, path)(e)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::is_reserved;
    use crate::testutil::{TestDir, names};
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::unix::fs::PermissionsExt;

    fn tree(c: &Path) {
        fs::create_dir_all(c.join("a/b")).unwrap();
        fs::write(c.join("a/b/file"), b"x").unwrap();
    }

    fn seq<'a>(v: &'a [&'a str]) -> impl FnMut() -> String + 'a {
        let mut it = v.iter().cycle();
        move || (*it.next().unwrap_or(&"0")).to_string()
    }

    #[test]
    fn trash_removes_dir() {
        let t = TestDir::new();
        let c = t.path().join("c");
        tree(&c);
        trash_dir(&c).unwrap();
        assert!(!c.exists());
        assert!(names(t.path()).is_empty());
    }

    #[test]
    fn trash_name_collision() {
        for empty in [false, true] {
            let t = TestDir::new();
            let c = t.path().join("c");
            tree(&c);
            let pre = t.path().join(".trash-c-1");
            fs::create_dir(&pre).unwrap();
            if !empty {
                fs::write(pre.join("keep"), b"k").unwrap();
            }
            let mut s = seq(&["1", "2"]);
            // First call hands out "1" (taken), then the second attempt takes "2" and succeeds.
            let mut first = true;
            let mut f = || {
                if first {
                    first = false;
                    "1".to_string()
                } else {
                    s()
                }
            };
            trash_dir_with(&c, &mut f).unwrap();
            assert!(!c.exists());
            assert!(pre.is_dir(), "empty={empty}");
            assert_eq!(pre.join("keep").exists(), !empty);
            assert!(!t.path().join(".trash-c-2").exists());
        }
        let t = TestDir::new();
        let c = t.path().join("c");
        tree(&c);
        fs::create_dir(t.path().join(".trash-c-1")).unwrap();
        let e = trash_dir_with(&c, &mut seq(&["1"])).unwrap_err();
        assert_eq!(e.op(), Some(Op::Rename));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::AlreadyExists));
        assert!(c.exists());
    }

    #[test]
    fn trash_not_a_dir() {
        let t = TestDir::new();
        let f = t.path().join("f");
        fs::write(&f, b"x").unwrap();
        let e = trash_dir(&f).unwrap_err();
        assert_eq!(e.op(), Some(Op::Stat));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::NotADirectory));
        assert!(f.exists());
    }

    #[test]
    fn trash_missing() {
        let t = TestDir::new();
        let e = trash_dir(&t.path().join("nope")).unwrap_err();
        assert_eq!(e.op(), Some(Op::Stat));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::NotFound));
    }

    #[test]
    fn sweep_trash_removes_leftovers() {
        let t = TestDir::new();
        tree(&t.path().join(".trash-x-1"));
        fs::write(t.path().join(".trash-y"), b"").unwrap();
        fs::create_dir(t.path().join("keep")).unwrap();
        fs::write(t.path().join(".tmp-1"), b"").unwrap();
        assert_eq!(sweep_trash(t.path()).unwrap(), 2);
        assert_eq!(names(t.path()), [".tmp-1", "keep"]);
    }

    #[test]
    fn sweep_on_missing_dir() {
        let t = TestDir::new();
        let e = sweep_trash(&t.path().join("nope")).unwrap_err();
        assert_eq!(e.op(), Some(Op::ReadDir));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::NotFound));
        assert!(sweep_tmp(&t.path().join("nope")).is_err());
    }

    #[test]
    fn sweep_remove_failure_continues() {
        let t = TestDir::new();
        let a = t.path().join(".trash-a");
        let inner = a.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("f"), b"x").unwrap();
        fs::create_dir(t.path().join(".trash-b")).unwrap();
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = inner.join("probe");
        if File::create(&probe).is_ok() {
            let _ = fs::remove_file(&probe);
            eprintln!("skipped: running as root");
            return;
        }
        let e = sweep_trash(t.path()).unwrap_err();
        assert_eq!(e.op(), Some(Op::Remove));
        assert!(!t.path().join(".trash-b").exists());
        fs::set_permissions(&inner, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(sweep_trash(t.path()).unwrap(), 1);
    }

    #[test]
    fn is_reserved_cases() {
        for n in [".lock", ".tmp-", ".trash-x"] {
            assert!(is_reserved(OsStr::new(n)), "{n}");
        }
        for n in ["lock", ".tmpx", "trash-", "manifest.json"] {
            assert!(!is_reserved(OsStr::new(n)), "{n}");
        }
    }
}
