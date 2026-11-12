//! Atomic file replacement and directory fsync.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::error::{Error, Op, io as io_err};
use crate::unique::{MAX_ATTEMPTS, unique_suffix};
use crate::{TMP_PREFIX, is_reserved};

/// `fsync(2)` on directory `dir` (opened read-only), persisting its entries.
///
/// Blocking. Errors: `Io { OpenDir | SyncDir }` on `dir`.
pub fn sync_dir(dir: &Path) -> Result<(), Error> {
    let d = File::open(dir).map_err(io_err(Op::OpenDir, dir))?;
    d.sync_all().map_err(io_err(Op::SyncDir, dir))
}

fn validate_name(dir: &Path, name: &str) -> Result<(), Error> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
        || is_reserved(OsStr::new(name));
    if bad {
        return Err(Error::InvalidName {
            dir: dir.to_path_buf(),
            name: name.into(),
        });
    }
    Ok(())
}

/// Atomically replaces (or creates) `dir/name` with exactly `bytes`.
///
/// Blocking: call from `spawn_blocking`. Steps: write a unique `.tmp-<pid>-<seq>` file in `dir`
/// (`create_new`), `fsync` it, `rename` over `dir/name`, `fsync` `dir`. The temp file is removed
/// (best effort) on any failure before the rename; after the rename nothing is removed. `fsync` is
/// never retried. The new file gets mode `0o666 & !umask`; the previous file's mode is not kept.
///
/// `dir/name` is never observed torn, by a concurrent reader or after a crash:
///
/// | stops after                    | `dir/name` after power loss | leftover               |
/// |--------------------------------|-----------------------------|------------------------|
/// | create, write or file fsync    | old content (or absent)     | maybe `.tmp-*` (sweep) |
/// | rename, before dir fsync ends  | old or new, each complete   | none                   |
/// | `Ok(())`                       | new, complete               | none                   |
/// | error at dir fsync             | old or new                  | none                   |
///
/// An error at the final directory fsync means the new content is visible but not known durable: do
/// not treat it as committed; retrying is safe. With several concurrent writers of one name the last
/// rename wins.
///
/// # Errors
///
/// `InvalidName` if `name` is empty, `.`, `..`, contains `/` or NUL, or is reserved (checked before
/// any I/O). `Io` with op `CreateTmp`, `Write`, `SyncFile`, `Rename` or `SyncDir` otherwise.
pub fn atomic_write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), Error> {
    validate_name(dir, name)?;
    let (mut file, tmp) = create_tmp(dir)?;
    let mut res = file
        .write_all(bytes)
        .map_err(io_err(Op::Write, &tmp))
        .and_then(|()| file.sync_all().map_err(io_err(Op::SyncFile, &tmp)));
    drop(file);
    let target = dir.join(name);
    if res.is_ok() {
        res = fs::rename(&tmp, &target).map_err(io_err(Op::Rename, &target));
    }
    if let Err(e) = res {
        // Best effort: the original error is the one worth reporting.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(dir)
}

fn create_tmp(dir: &Path) -> Result<(File, std::path::PathBuf), Error> {
    let mut last = None;
    for _ in 0..MAX_ATTEMPTS {
        let tmp = dir.join(format!("{TMP_PREFIX}{}", unique_suffix()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last = Some(io_err(Op::CreateTmp, tmp)(e))
            }
            Err(e) => return Err(io_err(Op::CreateTmp, tmp)(e)),
        }
    }
    // MAX_ATTEMPTS > 0, so `last` is always set here.
    Err(last.unwrap_or_else(|| Error::InvalidName {
        dir: dir.to_path_buf(),
        name: TMP_PREFIX.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TestDir, names};
    use std::os::unix::fs::PermissionsExt;

    fn is_root(dir: &Path) -> bool {
        let probe = dir.join("probe");
        if File::create(&probe).is_ok() {
            let _ = fs::remove_file(&probe);
            true
        } else {
            false
        }
    }

    #[test]
    fn creates_new_file() {
        let t = TestDir::new();
        atomic_write(t.path(), "m.json", b"abc").unwrap();
        assert_eq!(fs::read(t.path().join("m.json")).unwrap(), b"abc");
        assert_eq!(names(t.path()), ["m.json"]);
    }

    #[test]
    fn overwrites_existing() {
        let t = TestDir::new();
        atomic_write(t.path(), "m", b"v1").unwrap();
        atomic_write(t.path(), "m", b"v2").unwrap();
        assert_eq!(fs::read(t.path().join("m")).unwrap(), b"v2");
        assert_eq!(names(t.path()), ["m"]);
    }

    #[test]
    fn empty_bytes() {
        let t = TestDir::new();
        atomic_write(t.path(), "e", b"").unwrap();
        assert_eq!(fs::metadata(t.path().join("e")).unwrap().len(), 0);
    }

    #[test]
    fn leftover_tmp_ignored_then_swept() {
        let t = TestDir::new();
        fs::write(t.path().join(".tmp-999-0"), b"garbage").unwrap();
        atomic_write(t.path(), "m.json", b"ok").unwrap();
        assert_eq!(fs::read(t.path().join("m.json")).unwrap(), b"ok");
        assert!(t.path().join(".tmp-999-0").exists());
        assert_eq!(crate::sweep_tmp(t.path()).unwrap(), 1);
        assert_eq!(names(t.path()), ["m.json"]);
    }

    #[test]
    fn invalid_names_rejected() {
        let t = TestDir::new();
        for n in ["", ".", "..", "a/b", "a\0b", ".lock", ".tmp-x", ".trash-x"] {
            let e = atomic_write(t.path(), n, b"x").unwrap_err();
            assert!(matches!(e, Error::InvalidName { .. }), "{n:?}");
            assert_eq!(e.path(), t.path());
        }
        assert!(names(t.path()).is_empty());
    }

    #[test]
    fn missing_dir() {
        let t = TestDir::new();
        let d = t.path().join("nope");
        let e = atomic_write(&d, "m", b"x").unwrap_err();
        assert_eq!(e.op(), Some(Op::CreateTmp));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::NotFound));
        assert!(e.path().starts_with(&d));
    }

    #[test]
    fn target_is_directory() {
        let t = TestDir::new();
        fs::create_dir(t.path().join("m")).unwrap();
        let e = atomic_write(t.path(), "m", b"x").unwrap_err();
        assert_eq!(e.op(), Some(Op::Rename));
        assert_eq!(e.path(), t.path().join("m"));
        assert_eq!(names(t.path()), ["m"]);
    }

    #[test]
    fn read_only_dir() {
        let t = TestDir::new();
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o555)).unwrap();
        if is_root(t.path()) {
            fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipped: running as root");
            return;
        }
        let e = atomic_write(t.path(), "m", b"x").unwrap_err();
        fs::set_permissions(t.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(e.op(), Some(Op::CreateTmp));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn sync_dir_cases() {
        let t = TestDir::new();
        sync_dir(t.path()).unwrap();
        let e = sync_dir(&t.path().join("nope")).unwrap_err();
        assert_eq!(e.op(), Some(Op::OpenDir));
        assert_eq!(e.io_kind(), Some(io::ErrorKind::NotFound));
    }

    #[test]
    fn concurrent_same_name() {
        const LEN: usize = 256 * 1024;
        let t = TestDir::new();
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let dir = t.path();
                s.spawn(move || {
                    let buf = vec![i; LEN];
                    for _ in 0..20 {
                        atomic_write(dir, "x", &buf).unwrap();
                    }
                });
            }
        });
        let data = fs::read(t.path().join("x")).unwrap();
        assert_eq!(data.len(), LEN);
        let first = data[0];
        assert!(first < 8);
        assert!(data.iter().all(|&b| b == first));
        assert_eq!(names(t.path()), ["x"]);
    }

    #[test]
    fn concurrent_distinct_names() {
        let t = TestDir::new();
        std::thread::scope(|s| {
            for i in 0..8u8 {
                let dir = t.path();
                s.spawn(move || atomic_write(dir, &format!("f{i}"), &[i; 100]).unwrap());
            }
        });
        for i in 0..8u8 {
            assert_eq!(fs::read(t.path().join(format!("f{i}"))).unwrap(), [i; 100]);
        }
        assert_eq!(names(t.path()).len(), 8);
    }

    #[test]
    fn error_display_mentions_path() {
        let t = TestDir::new();
        let d = t.path().join("nope");
        let e = atomic_write(&d, "m", b"x").unwrap_err();
        assert!(e.to_string().contains(&*d.to_string_lossy()));
    }
}
