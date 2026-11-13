//! Dictionary resolution.

use std::num::NonZeroU32;
use std::sync::Arc;

use masstuffy_zframe::{DecoderDict, EncoderDict};

/// Supplies prepared dictionaries. Implemented by the engine on a newtype over its dictionary
/// store.
///
/// Implementations must return dictionaries whose `id()` equals the requested id (and `level()`
/// the requested level); the collection verifies it and fails with `Error::DictMismatch`.
pub trait DictResolver {
    /// The decoder for `dict_id`.
    ///
    /// # Errors
    ///
    /// [`ResolveError::NotFound`] if the dictionary does not exist, or `Other`.
    fn decoder(&self, dict_id: NonZeroU32) -> Result<Arc<DecoderDict>, ResolveError>;

    /// The encoder for `dict_id` at compression `level`.
    ///
    /// # Errors
    ///
    /// [`ResolveError::NotFound`] if the dictionary does not exist, or `Other`.
    fn encoder(&self, dict_id: NonZeroU32, level: i32) -> Result<Arc<EncoderDict>, ResolveError>;
}

/// Why a dictionary could not be resolved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResolveError {
    /// No dictionary with this id.
    #[error("dictionary {0} not found")]
    NotFound(u32),
    /// The dictionary exists but could not be loaded.
    #[error("dictionary {dict_id} unavailable: {source}")]
    Other {
        /// The dictionary id.
        dict_id: u32,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
