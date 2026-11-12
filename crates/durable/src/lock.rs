//! Exclusive per-directory process lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use crate::LOCK_FILE;
use crate::error::{Error, Op, io as io_err};

/// Exclusive advisory lock on `<dir>/.lock`, held until drop. Not `Clone`.
///
/// On Linux this is `flock(LOCK_EX | LOCK_NB)` through `File::try_lock`, per open file description:
/// a second `try_acquire` in the same process also fails. The kernel releases it when the process
/// dies, so there is no stale-lock handling. The file is never written, truncated or deleted
/// (deleting it while locked would let another opener lock a fresh inode). The handle is never
/// exposed or cloned, because a clone would share the lock.
#[derive(Debug)]
pub struct DirLock {
    // Owns the flock; released when the descriptor closes on drop.
    _file: File,
    lock_file: PathBuf,
}

impl DirLock {
    /// Takes the lock without waiting. Blocking syscalls only; call from `spawn_blocking`.
    ///
    /// Creates `<dir>/.lock` if missing.
    ///
    /// # Errors
    ///
    /// `Io { OpenLock }` if the file cannot be opened or created (missing directory, permissions),
    /// `Locked` if the lock is held elsewhere, `Io { Lock }` on any other locking failure.
    pub fn try_acquire(dir: &Path) -> Result<DirLock, Error> {
        let lock_file = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_file)
            .map_err(io_err(Op::OpenLock, &lock_file))?;
        match file.try_lock() {
            Ok(()) => Ok(DirLock {
                _file: file,
                lock_file,
            }),
            Err(TryLockError::WouldBlock) => Err(Error::Locked {
                dir: dir.to_path_buf(),
                lock_file,
            }),
            Err(TryLockError::Error(source)) => Err(Error::Io {
                op: Op::Lock,
                path: lock_file,
                source,
            }),
        }
    }

    /// Path of the lock file.
    #[must_use]
    pub fn lock_file(&self) -> &Path {
        &self.lock_file
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDir;
    use std::fs;
    use std::process::Command;

    const DIR_VAR: &str = "MASSTUFFY_DURABLE_CHILD_DIR";
    const EXPECT_VAR: &str = "MASSTUFFY_DURABLE_CHILD_EXPECT";

    #[test]
    fn second_handle_same_process_fails() {
        let t = TestDir::new();
        let a = DirLock::try_acquire(t.path()).unwrap();
        assert_eq!(a.lock_file(), t.path().join(".lock"));
        match DirLock::try_acquire(t.path()) {
            Err(Error::Locked { dir, .. }) => assert_eq!(dir, t.path()),
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(a);
        let _b = DirLock::try_acquire(t.path()).unwrap();
        assert!(t.path().join(".lock").exists());
    }

    #[test]
    fn lock_missing_dir() {
        let t = TestDir::new();
        let e = DirLock::try_acquire(&t.path().join("nope")).unwrap_err();
        assert_eq!(e.op(), Some(Op::OpenLock));
        assert_eq!(e.io_kind(), Some(std::io::ErrorKind::NotFound));
    }

    fn run_child(dir: &Path, expect: &str, marker: &str) {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "lock::tests::child_probe",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(DIR_VAR, dir)
            .env(EXPECT_VAR, expect)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(dir.join(marker).exists(), "child probe did not run");
    }

    #[test]
    fn lock_across_process() {
        let t = TestDir::new();
        let a = DirLock::try_acquire(t.path()).unwrap();
        run_child(t.path(), "locked", "child-ran-locked");
        drop(a);
        run_child(t.path(), "free", "child-ran-free");
    }

    #[test]
    fn child_probe() {
        let Some(dir) = std::env::var_os(DIR_VAR) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let expect = std::env::var(EXPECT_VAR).unwrap();
        let r = DirLock::try_acquire(&dir);
        match (expect.as_str(), &r) {
            ("locked", Err(Error::Locked { .. })) | ("free", Ok(_)) => {}
            _ => panic!("unexpected result {r:?} for {expect}"),
        }
        fs::write(dir.join(format!("child-ran-{expect}")), b"").unwrap();
    }

    #[test]
    fn lock_is_send_sync() {
        fn f<T: Send + Sync>() {}
        f::<DirLock>();
    }
}
