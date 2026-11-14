//! Repository directory layout and its checks.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Name of the config file.
pub(crate) const CONFIG_NAME: &str = "config.json";

/// Longest slug accepted by `create_collection`.
const MAX_NEW_SLUG: usize = 128;

pub(crate) fn data_dir(dir: &Path) -> PathBuf {
    dir.join("data")
}

pub(crate) fn buffer_dir(dir: &Path) -> PathBuf {
    data_dir(dir).join("buffer")
}

pub(crate) fn dict_dir(dir: &Path) -> PathBuf {
    data_dir(dir).join("dict")
}

pub(crate) fn dict_zstd_dir(dir: &Path) -> PathBuf {
    dict_dir(dir).join("zstd")
}

pub(crate) fn repository_dir(dir: &Path) -> PathBuf {
    data_dir(dir).join("repository")
}

/// The five directories of a repository, in creation order.
pub(crate) fn data_paths(dir: &Path) -> [PathBuf; 5] {
    [
        data_dir(dir),
        buffer_dir(dir),
        dict_dir(dir),
        dict_zstd_dir(dir),
        repository_dir(dir),
    ]
}

fn io_err(op: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io { op, path, source }
}

/// Checks that `dir` holds nothing but the leftovers of an interrupted `init`: `.lock`, `.tmp-*`
/// names, and a `data` tree made only of the directories of the layout.
pub(crate) fn check_init_leftovers(dir: &Path) -> Result<(), Error> {
    let not_empty = |entry: PathBuf| Error::NotEmpty {
        dir: dir.to_path_buf(),
        entry,
    };
    for e in fs::read_dir(dir).map_err(io_err("read directory", dir))? {
        let e = e.map_err(io_err("read directory", dir))?;
        let name = e.file_name();
        if name.as_encoded_bytes() == masstuffy_durable::LOCK_FILE.as_bytes()
            || name
                .as_encoded_bytes()
                .starts_with(masstuffy_durable::TMP_PREFIX.as_bytes())
        {
            continue;
        }
        if name.as_encoded_bytes() == b"data" {
            check_tree(&e.path(), &["buffer", "dict", "repository"], &not_empty)?;
            continue;
        }
        return Err(not_empty(e.path()));
    }
    Ok(())
}

/// `path` must be a real directory whose entries are real directories among `allowed`; `dict` may
/// contain `zstd` (empty directory).
fn check_tree(
    path: &Path,
    allowed: &[&str],
    not_empty: &dyn Fn(PathBuf) -> Error,
) -> Result<(), Error> {
    let md = fs::symlink_metadata(path).map_err(io_err("stat", path))?;
    if !md.is_dir() {
        return Err(not_empty(path.to_path_buf()));
    }
    for e in fs::read_dir(path).map_err(io_err("read directory", path))? {
        let e = e.map_err(io_err("read directory", path))?;
        let name = e.file_name();
        let Some(n) = name.to_str() else {
            return Err(not_empty(e.path()));
        };
        let leaf_ok = path.file_name().is_some_and(|f| f == "dict") && n == "zstd";
        if !allowed.contains(&n) && !leaf_ok {
            return Err(not_empty(e.path()));
        }
        check_tree(&e.path(), &[], not_empty)?;
    }
    Ok(())
}

/// Checks that `data`, `data/dict/zstd` and `data/repository` are directories.
pub(crate) fn check_layout(dir: &Path) -> Result<(), Error> {
    for p in [data_dir(dir), dict_zstd_dir(dir), repository_dir(dir)] {
        match fs::symlink_metadata(&p) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return Err(Error::Layout { missing: p }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::Layout { missing: p });
            }
            Err(e) => return Err(io_err("stat", &p)(e)),
        }
    }
    Ok(())
}

/// The slug policy of `create_collection`: `[a-z0-9][a-z0-9_-]{0,127}`.
pub(crate) fn is_valid_new_slug(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > MAX_NEW_SLUG {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && (c == b'_' || c == b'-'))
    })
}
