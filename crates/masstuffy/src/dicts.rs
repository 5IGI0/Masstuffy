//! Read access to the repository dictionary store (for clients that compress or decompress
//! themselves).

use std::sync::Arc;

use crate::error::Error;
use crate::query::read_permit;
use crate::repository::Repository;

/// One stored dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryInfo {
    /// The dictionary id (never 0).
    pub dict_id: u32,
    /// The label it was installed with.
    pub label: String,
    /// Size of its file in bytes.
    pub size: u64,
}

impl Repository {
    /// Every dictionary of the store, sorted by id. In memory, no I/O.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`.
    pub fn dictionaries(&self) -> Result<Vec<DictionaryInfo>, Error> {
        self.check_open()?;
        let mut v: Vec<DictionaryInfo> = self
            .core()
            .resolver
            .0
            .list()
            .iter()
            .map(|d| DictionaryInfo {
                dict_id: d.dict_id(),
                label: d.label().to_owned(),
                size: d.size(),
            })
            .collect();
        v.sort_by_key(|d| d.dict_id);
        Ok(v)
    }

    /// The dictionary file bytes (immutable once installed), read in `spawn_blocking` under one
    /// read permit, capped by `Config::dict_max_file_size`. Not cached.
    ///
    /// # Errors
    ///
    /// `ShuttingDown`, `DictNotFound(id)` (unknown id or 0), `Dict` (I/O or corrupt file), `Join`.
    pub async fn dictionary(&self, dict_id: u32) -> Result<Vec<u8>, Error> {
        let permit = read_permit(self.core()).await?;
        let store = Arc::clone(&self.core().resolver.0);
        let out = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            store.bytes(dict_id)
        })
        .await
        .map_err(Error::Join)?;
        out.map_err(|e| match e {
            masstuffy_dictstore::Error::NotFound { dict_id } => Error::DictNotFound(dict_id),
            e => Error::Dict(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use masstuffy_zframe::DictHeader;

    use super::*;
    use crate::config::Config;
    use crate::error::ErrorKind;
    use crate::testutil::{TestDir, bounded, record};
    use crate::train::TrainOptions as EngineTrain;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dictionary_after_train() {
        bounded(async {
            let td = TestDir::new();
            let repo = Repository::init_with_config(td.repo(), &Config::default())
                .await
                .unwrap();
            let h = repo
                .create_collection("c", masstuffy_collection::Codec::None)
                .await
                .unwrap();
            h.put((0..64).map(|i| record(i, 300)).collect())
                .await
                .unwrap();
            let r = h
                .train_dictionary(EngineTrain {
                    max_dict_size: 4096,
                    ..EngineTrain::default()
                })
                .await
                .unwrap();
            let bytes = repo.dictionary(r.dict_id).await.unwrap();
            assert_eq!(DictHeader::parse(&bytes).unwrap().dict_id, r.dict_id);
            let list = repo.dictionaries().unwrap();
            let me = list.iter().find(|d| d.dict_id == r.dict_id).unwrap();
            assert_eq!(me.label, r.label);
            assert_eq!(me.size, u64::try_from(bytes.len()).unwrap());
            repo.shutdown().await.unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dictionary_unknown_404_kind() {
        bounded(async {
            let td = TestDir::new();
            let repo = Repository::init(td.repo()).await.unwrap();
            for id in [12345, 0] {
                let e = repo.dictionary(id).await.unwrap_err();
                assert!(matches!(e, Error::DictNotFound(_)), "{e:?}");
                assert_eq!(e.kind(), ErrorKind::NotFound);
            }
            repo.shutdown().await.unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dictionaries_after_shutdown() {
        bounded(async {
            let td = TestDir::new();
            let repo = Repository::init(td.repo()).await.unwrap();
            repo.shutdown().await.unwrap();
            assert!(matches!(repo.dictionaries(), Err(Error::ShuttingDown)));
            assert!(matches!(repo.dictionary(1).await, Err(Error::ShuttingDown)));
        })
        .await;
    }
}
