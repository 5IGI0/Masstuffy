//! The secondary-index backend: which implementation is opened, and where.
//!
//! With the cargo feature `redb` (default) the index is a `RedbIndex` file at
//! `<dir>/data/index.redb`; without it, a `MemIndex` rebuilt from the CDX at every open. Test
//! builds can force `MemIndex` per directory (`testutil::use_memory_index`).

use std::path::Path;
use std::sync::Arc;

use masstuffy_index::SecondaryIndex;

use crate::config::Config;
use crate::error::Error;
#[cfg(feature = "redb")]
use crate::layout;

/// File name of the redb index inside `data/`.
#[cfg(feature = "redb")]
pub(crate) const INDEX_FILE: &str = "index.redb";

/// Path of the redb index file of the repository at `dir`.
#[cfg(feature = "redb")]
pub(crate) fn index_path(dir: &Path) -> std::path::PathBuf {
    layout::data_dir(dir).join(INDEX_FILE)
}

type FlushFn = Box<dyn Fn() -> Result<(), masstuffy_index::Error> + Send + Sync>;

/// The open index plus its flush. No enum: a `Memory` variant would be dead code in non-test
/// redb builds.
pub(crate) struct IndexBackend {
    pub(crate) index: Arc<dyn SecondaryIndex>,
    flush: FlushFn,
}

impl IndexBackend {
    /// Makes non-durable commits durable (`RedbIndex::flush`); no-op for `MemIndex`. Blocking.
    pub(crate) fn flush(&self) -> Result<(), masstuffy_index::Error> {
        (self.flush)()
    }

    #[cfg(any(test, not(feature = "redb")))]
    fn memory() -> IndexBackend {
        IndexBackend {
            index: Arc::new(masstuffy_index::MemIndex::new()),
            flush: Box::new(|| Ok(())),
        }
    }
}

/// Blocking. Opens (or recreates) the index. Returns the backend and whether the file was
/// recreated.
///
/// # Errors
///
/// `IndexOpen` (any open error that is not `needs_rebuild`, or a failure of the recreation),
/// `Io` (`remove_file`), `Durable` (`sync_dir`).
pub(crate) fn open_backend(dir: &Path, config: &Config) -> Result<(IndexBackend, bool), Error> {
    #[cfg(test)]
    if crate::testutil::memory_index_requested(dir) {
        return Ok((IndexBackend::memory(), false));
    }
    open_configured(dir, config)
}

#[cfg(feature = "redb")]
fn open_configured(dir: &Path, config: &Config) -> Result<(IndexBackend, bool), Error> {
    use std::num::NonZeroU32;

    use masstuffy_index_redb::{Options, RedbIndex};

    let mut o = Options::default();
    o.cache_bytes = usize::try_from(config.index_cache_bytes).unwrap_or(usize::MAX);
    o.sync_every = NonZeroU32::new(config.index_sync_every).unwrap_or(NonZeroU32::MIN);
    let path = index_path(dir);
    let backend = |r: RedbIndex| {
        let r = Arc::new(r);
        let f = Arc::clone(&r);
        IndexBackend {
            index: r,
            flush: Box::new(move || f.flush()),
        }
    };
    let open_err = |e: masstuffy_index_redb::OpenError| Error::IndexOpen {
        path: path.clone(),
        source: Box::new(e),
    };
    match RedbIndex::open(&path, &o) {
        Ok(r) => Ok((backend(r), false)),
        Err(e) if e.needs_rebuild() => {
            tracing::warn!(path = %path.display(), error = %e, "secondary index unusable, recreating it");
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(Error::Io {
                        op: "remove",
                        path,
                        source,
                    });
                }
            }
            masstuffy_durable::sync_dir(&layout::data_dir(dir))?;
            let r = RedbIndex::open(&path, &o).map_err(open_err)?;
            Ok((backend(r), true))
        }
        Err(e) => Err(open_err(e)),
    }
}

/// Without the `redb` feature: an in-memory index, empty at every open.
#[cfg(not(feature = "redb"))]
fn open_configured(_dir: &Path, _config: &Config) -> Result<(IndexBackend, bool), Error> {
    Ok((IndexBackend::memory(), false))
}
