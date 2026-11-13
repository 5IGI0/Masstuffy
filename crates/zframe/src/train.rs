//! Dictionary training from a bounded in-memory sample set.

use crate::dict::{DictError, DictHeader};
use zstd::zstd_safe::{get_error_name, train_from_buffer};

/// Smallest dictionary buffer ZDICT accepts.
const MIN_DICT_SIZE: usize = 256;
/// Fewest samples worth passing to ZDICT.
const MIN_SAMPLES: usize = 8;

/// Caps on a training run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrainOptions {
    /// Maximum number of samples kept.
    pub max_samples: usize,
    /// Maximum total sample bytes kept.
    pub max_total_bytes: u64,
    /// Samples longer than this are truncated to this prefix.
    pub max_sample_bytes: usize,
    /// Size of the dictionary buffer (maximum dictionary size), at least 256.
    pub max_dict_size: usize,
}

impl Default for TrainOptions {
    /// 100 000 samples, 512 MiB, 128 KiB per sample, 5 MiB dictionary.
    fn default() -> Self {
        TrainOptions {
            max_samples: 100_000,
            max_total_bytes: 512 * 1024 * 1024,
            max_sample_bytes: 128 * 1024,
            max_dict_size: 5 * 1024 * 1024,
        }
    }
}

/// Reserves room for `extra` more elements, doubling the capacity but never beyond `max` (or
/// beyond exactly what is needed, when that is larger).
fn grow_capped<T>(v: &mut Vec<T>, extra: usize, max: usize) {
    let needed = v.len().saturating_add(extra);
    if needed > v.capacity() {
        let target = v.capacity().saturating_mul(2).min(max).max(needed);
        v.reserve_exact(target.saturating_sub(v.len()));
    }
}

/// A bounded set of training samples held contiguously in memory.
///
/// Memory is at most `max_total_bytes + size_of::<usize>() * max_samples` bytes: buffers grow
/// geometrically but never past those caps, and nothing is allocated up front.
#[derive(Debug)]
pub struct SampleSet {
    opts: TrainOptions,
    data: Vec<u8>,
    sizes: Vec<usize>,
}

impl SampleSet {
    /// Creates an empty set with the given caps.
    #[must_use]
    pub fn new(opts: TrainOptions) -> SampleSet {
        SampleSet {
            opts,
            data: Vec::new(),
            sizes: Vec::new(),
        }
    }

    /// Appends at most `max_sample_bytes` of `sample` (its prefix). Empty samples are ignored.
    ///
    /// # Errors
    ///
    /// [`TrainError::Full`] if adding the sample would exceed `max_samples` or `max_total_bytes`;
    /// the set is then unchanged.
    pub fn push(&mut self, sample: &[u8]) -> Result<(), TrainError> {
        let take = sample.len().min(self.opts.max_sample_bytes);
        let Some(part) = sample.get(..take) else {
            return Ok(());
        };
        if part.is_empty() {
            return Ok(());
        }
        if self.sizes.len() >= self.opts.max_samples {
            return Err(TrainError::Full);
        }
        let new_total = self
            .total_bytes()
            .checked_add(u64::try_from(take).map_err(|_| TrainError::Full)?)
            .ok_or(TrainError::Full)?;
        if new_total > self.opts.max_total_bytes {
            return Err(TrainError::Full);
        }
        // Grow geometrically (amortised O(1) per byte) but never past the caps, so capacity stays
        // within the documented bound (plain `extend`/`push` could double past it).
        let max_bytes = usize::try_from(self.opts.max_total_bytes).unwrap_or(usize::MAX);
        grow_capped(&mut self.data, part.len(), max_bytes);
        grow_capped(&mut self.sizes, 1, self.opts.max_samples);
        self.data.extend_from_slice(part);
        self.sizes.push(take);
        Ok(())
    }

    /// Number of samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    /// Whether the set has no sample.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }

    /// Total sample bytes kept.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        u64::try_from(self.data.len()).unwrap_or(u64::MAX)
    }
}

/// Error from training.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrainError {
    /// The sample set is full.
    #[error("sample set full")]
    Full,
    /// `max_dict_size` is below 256.
    #[error("invalid training options")]
    InvalidOptions,
    /// Fewer than 8 samples.
    #[error("too few samples")]
    TooFewSamples,
    /// zstd reported an error.
    #[error("zstd: {0}")]
    Zstd(&'static str),
    /// The trained dictionary has an invalid header.
    #[error("trained dictionary invalid: {0}")]
    Dict(#[from] DictError),
}

/// Trains a formatted dictionary. Deterministic for identical samples and options.
///
/// The id is zstd's own, within `[DICT_ID_ALLOC_MIN, DICT_ID_ALLOC_END)`; callers rewrite it with
/// [`set_dict_id`](crate::set_dict_id). CPU heavy: run it in `spawn_blocking`.
///
/// # Errors
///
/// [`TrainError::InvalidOptions`], [`TrainError::TooFewSamples`], [`TrainError::Zstd`] when zstd
/// fails (for instance on degenerate samples), [`TrainError::Dict`] if the result is malformed.
pub fn train(samples: &SampleSet) -> Result<Vec<u8>, TrainError> {
    if samples.opts.max_dict_size < MIN_DICT_SIZE {
        return Err(TrainError::InvalidOptions);
    }
    if samples.len() < MIN_SAMPLES {
        return Err(TrainError::TooFewSamples);
    }
    let mut dict = Vec::with_capacity(samples.opts.max_dict_size);
    train_from_buffer(&mut dict, &samples.data, &samples.sizes)
        .map_err(|code| TrainError::Zstd(get_error_name(code)))?;
    DictHeader::parse(&dict)?;
    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Compressor, DecoderDict, Decompressor, EncoderDict, inspect_single_frame,
        is_allocatable_dict_id, set_dict_id,
    };

    fn opts(max_dict_size: usize) -> TrainOptions {
        TrainOptions {
            max_dict_size,
            ..TrainOptions::default()
        }
    }

    fn samples(n: usize, max_dict_size: usize) -> SampleSet {
        let mut x: u64 = 3;
        let mut set = SampleSet::new(opts(max_dict_size));
        for i in 0..n {
            let mut s = format!(
                "WARC/1.1\r\nWARC-Type: response\r\nWARC-Target-URI: https://example.org/{i}\r\n\r\n"
            )
            .into_bytes();
            for _ in 0..200 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                s.push(b'a' + u8::try_from(x % 26).unwrap());
            }
            set.push(&s).unwrap();
        }
        set
    }

    #[test]
    fn train_ok() {
        let set = samples(2000, 16 * 1024);
        let d = train(&set).unwrap();
        let h = DictHeader::parse(&d).unwrap();
        assert!(is_allocatable_dict_id(h.dict_id));
        assert!(d.len() <= 16 * 1024);
        assert_eq!(DecoderDict::new(&d).unwrap().id(), h.dict_id);
        assert_eq!(EncoderDict::new(&d, 3).unwrap().id(), h.dict_id);
    }

    #[test]
    fn train_deterministic() {
        let set = samples(2000, 16 * 1024);
        assert_eq!(train(&set).unwrap(), train(&set).unwrap());
    }

    #[test]
    fn rewrite_id_then_use() {
        let mut d = train(&samples(2000, 16 * 1024)).unwrap();
        set_dict_id(&mut d, 40_000).unwrap();
        let e = EncoderDict::new(&d, 3).unwrap();
        let dd = DecoderDict::new(&d).unwrap();
        let mut c = Compressor::new().unwrap();
        let mut dc = Decompressor::new().unwrap();
        let mut f = Vec::new();
        let mut back = Vec::new();
        c.compress(b"WARC/1.1 hello hello hello", &e, &mut f)
            .unwrap();
        assert_eq!(inspect_single_frame(&f).unwrap().dict_id, 40_000);
        dc.decompress(&f, Some(&dd), 1 << 20, &mut back).unwrap();
        assert_eq!(back, b"WARC/1.1 hello hello hello");
    }

    #[test]
    fn push_caps() {
        let mut set = SampleSet::new(TrainOptions {
            max_samples: 3,
            max_total_bytes: 10,
            max_sample_bytes: 4,
            max_dict_size: 1024,
        });
        set.push(b"").unwrap();
        assert!(set.is_empty());
        set.push(b"abcdefgh").unwrap();
        assert_eq!((set.len(), set.total_bytes()), (1, 4));
        set.push(b"abcd").unwrap();
        assert_eq!(set.total_bytes(), 8);
        // 8 + 4 > 10
        assert!(matches!(set.push(b"abcd"), Err(TrainError::Full)));
        assert_eq!((set.len(), set.total_bytes()), (2, 8));
        set.push(b"ab").unwrap();
        assert_eq!((set.len(), set.total_bytes()), (3, 10));
        assert!(matches!(set.push(b"a"), Err(TrainError::Full)));
        assert_eq!((set.len(), set.total_bytes()), (3, 10));
    }

    #[test]
    fn too_few_samples() {
        let set = samples(7, 4096);
        assert!(matches!(train(&set), Err(TrainError::TooFewSamples)));
    }

    #[test]
    fn invalid_options() {
        let set = samples(100, 100);
        assert!(matches!(train(&set), Err(TrainError::InvalidOptions)));
    }
}
