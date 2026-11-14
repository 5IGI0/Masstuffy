//! `DictResolver` over the repository dictionary store.

use std::num::NonZeroU32;
use std::sync::Arc;

use masstuffy_collection::{DictResolver, ResolveError};
use masstuffy_dictstore::{DictStore, Error as DictError};
use masstuffy_zframe::{DecoderDict, EncoderDict};

/// Newtype over the store (orphan rule: `collection` does not know `dictstore`).
///
/// Methods may do blocking I/O (lazy dictionary load): call only from `spawn_blocking`.
pub(crate) struct StoreResolver(pub(crate) Arc<DictStore>);

fn map(id: NonZeroU32, e: DictError) -> ResolveError {
    match e {
        DictError::NotFound { dict_id } => ResolveError::NotFound(dict_id),
        e => ResolveError::Other {
            dict_id: id.get(),
            source: Box::new(e),
        },
    }
}

impl DictResolver for StoreResolver {
    fn decoder(&self, id: NonZeroU32) -> Result<Arc<DecoderDict>, ResolveError> {
        self.0.decoder(id.get()).map_err(|e| map(id, e))
    }

    fn encoder(&self, id: NonZeroU32, level: i32) -> Result<Arc<EncoderDict>, ResolveError> {
        self.0.encoder(id.get(), level).map_err(|e| map(id, e))
    }
}
