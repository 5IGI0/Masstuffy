//! Test helpers.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use masstuffy_zframe::{DictHeader, SampleSet, TrainOptions, set_dict_id, train};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary directory removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> TestDir {
        let p = std::env::temp_dir().join(format!(
            "masstuffy-dictstore-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        TestDir(p)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Sorted entry names of `dir`.
pub(crate) fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A small dictionary trained once per test process (cheap: few samples, tiny dictionary).
pub(crate) fn base_dict() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(|| {
        let mut set = SampleSet::new(TrainOptions {
            max_dict_size: 4 * 1024,
            ..TrainOptions::default()
        });
        let mut x: u64 = 3;
        for i in 0..400 {
            let mut s = format!(
                "WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: https://example.org/{i}\r\n\r\n"
            )
            .into_bytes();
            for _ in 0..100 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(b'a' + u8::try_from(x % 26).unwrap());
            }
            set.push(&s).unwrap();
        }
        train(&set).unwrap()
    })
}

/// The id of [`base_dict`] (the "trained id" of every [`variant`]).
pub(crate) fn trained_id() -> u32 {
    DictHeader::parse(base_dict()).unwrap().dict_id
}

/// `base_dict()` with distinct raw content appended: a different valid dictionary, same header id,
/// same length for `n < 1000`.
pub(crate) fn variant(n: u32) -> Vec<u8> {
    let mut v = base_dict().to_vec();
    v.extend_from_slice(format!("variant-{n:03}").as_bytes());
    v
}

pub(crate) fn with_id(mut d: Vec<u8>, id: u32) -> Vec<u8> {
    set_dict_id(&mut d, id).unwrap();
    d
}

/// Places a store file (test-only, plain write).
pub(crate) fn put(dir: &Path, name: &str, bytes: &[u8]) {
    fs::write(dir.join(name), bytes).unwrap();
}

/// Whether directory permissions are not enforced (running as root).
pub(crate) fn is_root(dir: &Path) -> bool {
    let probe = dir.join("probe");
    if fs::File::create(&probe).is_ok() {
        let _ = fs::remove_file(&probe);
        true
    } else {
        false
    }
}
