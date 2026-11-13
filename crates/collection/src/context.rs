//! Options and the per-generation context shared by prepare, commit and read.

use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use masstuffy_warc::Limits;
use masstuffy_zframe::{DecoderDict, EncoderDict};

use crate::error::Error;
use crate::id::CollectionId;
use crate::manifest::{Codec, Manifest};
use crate::resolve::DictResolver;

/// Tunables of a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Largest serialised (decompressed) WARC record accepted on write and on read. Must be
    /// non-zero. Default 1 GiB.
    pub max_record_bytes: u64,
    /// Bound of the segment file-handle LRU of the snapshot's reader. Default seglog's (64).
    pub max_open_files: NonZeroUsize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_record_bytes: 1 << 30,
            max_open_files: NonZeroUsize::new(masstuffy_seglog::DEFAULT_MAX_OPEN_FILES)
                .unwrap_or(NonZeroUsize::MIN),
        }
    }
}

/// Everything prepare, commit and read need to know about one generation of one collection.
/// Immutable, `Send + Sync`, shared as `Arc<GenContext>`.
pub struct GenContext {
    pub(crate) collection: CollectionId,
    pub(crate) generation: u32,
    pub(crate) codec: Codec,
    /// `Some` iff the codec is Zstd.
    pub(crate) encoder: Option<Arc<EncoderDict>>,
    /// `Some` iff the codec is Zstd.
    pub(crate) decoder: Option<Arc<DecoderDict>>,
    pub(crate) warc_limits: Limits,
    pub(crate) max_record_bytes: u64,
    pub(crate) max_stored_bytes: u64,
}

impl fmt::Debug for GenContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GenContext")
            .field("collection", &self.collection)
            .field("generation", &self.generation)
            .field("codec", &self.codec)
            .field("encoder", &self.encoder.as_ref().map(|e| e.id()))
            .field("decoder", &self.decoder.as_ref().map(|d| d.id()))
            .field("max_record_bytes", &self.max_record_bytes)
            .field("max_stored_bytes", &self.max_stored_bytes)
            .finish_non_exhaustive()
    }
}

impl GenContext {
    /// The collection this context belongs to.
    #[must_use]
    pub fn collection(&self) -> CollectionId {
        self.collection
    }

    /// The generation number.
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The generation's codec.
    #[must_use]
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Validates `opts`, resolves the dictionaries (Zstd only) and checks what the resolver
    /// returned.
    pub(crate) fn new(
        manifest: &Manifest,
        opts: &Options,
        resolver: &dyn DictResolver,
    ) -> Result<GenContext, Error> {
        if opts.max_record_bytes == 0 {
            return Err(Error::InvalidOptions("max_record_bytes"));
        }
        let active = manifest.active();
        let (encoder, decoder) = match active.codec {
            Codec::None => (None, None),
            Codec::Zstd { dict_id, level } => {
                let enc = resolver.encoder(dict_id, level)?;
                let dec = resolver.decoder(dict_id)?;
                let id = i64::from(dict_id.get());
                if i64::from(dec.id()) != id {
                    return Err(Error::DictMismatch {
                        what: "decoder id",
                        expected: id,
                        got: i64::from(dec.id()),
                    });
                }
                if i64::from(enc.id()) != id {
                    return Err(Error::DictMismatch {
                        what: "encoder id",
                        expected: id,
                        got: i64::from(enc.id()),
                    });
                }
                if enc.level() != level {
                    return Err(Error::DictMismatch {
                        what: "encoder level",
                        expected: i64::from(level),
                        got: i64::from(enc.level()),
                    });
                }
                (Some(enc), Some(dec))
            }
        };
        let max = opts.max_record_bytes;
        Ok(GenContext {
            collection: manifest.uuid(),
            generation: active.id,
            codec: active.codec,
            encoder,
            decoder,
            warc_limits: Limits {
                max_body_len: max,
                ..Limits::DEFAULT
            },
            max_record_bytes: max,
            max_stored_bytes: max.saturating_add(max / 128).saturating_add(128 * 1024),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::resolve::ResolveError;
    use crate::testutil::{MapResolver, manifest_zstd, other_dict, trained_id};

    #[test]
    fn options_zero_record_bytes_rejected() {
        let m = manifest_zstd();
        let opts = Options {
            max_record_bytes: 0,
            ..Options::default()
        };
        assert!(matches!(
            GenContext::new(&m, &opts, &MapResolver::standard()),
            Err(Error::InvalidOptions("max_record_bytes"))
        ));
    }

    #[test]
    fn resolver_wrong_id_rejected() {
        let m = manifest_zstd();
        let mut r = MapResolver::default();
        r.insert(trained_id(), other_dict());
        assert!(matches!(
            GenContext::new(&m, &Options::default(), &r),
            Err(Error::DictMismatch { .. })
        ));
        assert!(matches!(
            GenContext::new(&m, &Options::default(), &MapResolver::default()),
            Err(Error::Resolve(ResolveError::NotFound(_)))
        ));
        assert!(GenContext::new(&m, &Options::default(), &MapResolver::standard()).is_ok());
    }
}
