//! One-record zstd frames and zstd dictionaries.
//!
//! # Features
//!
//! - none: a std-only, allocation-free inspector for zstd frame headers ([`inspect_frame`],
//!   [`inspect_single_frame`]) and formatted-dictionary headers ([`DictHeader`], [`set_dict_id`]).
//!   Thin clients use it, for instance to check a delegated frame on raw import.
//! - `codec` (default): prepared dictionaries (`DecoderDict`, `EncoderDict`, shared via `Arc`),
//!   reusable `Compressor` / `Decompressor` contexts, one record per complete frame, and
//!   decompression under a hard output cap.
//! - `train`: dictionary training (`SampleSet`, `train()`); implies `codec`.
//!
//! (Feature-gated items are named in plain code spans so the docs build with any feature set.)
//!
//! Frames written by this crate carry the dictionary id and the content size, and no content
//! checksum. Frames without a dictionary have `dict_id = 0`.
//!
//! # Blocking
//!
//! Everything is synchronous CPU work. Async callers run compression, decompression and training
//! inside `spawn_blocking`. Contexts take `&mut self`: use one per thread at a time (a bounded
//! checkout pool is the intended use); they hold no dictionary between calls, so one pool serves
//! every dictionary.
#![forbid(unsafe_code)]

#[cfg(feature = "codec")]
mod codec;
mod dict;
mod frame;
#[cfg(feature = "train")]
mod train;

#[cfg(feature = "codec")]
pub use codec::{CodecError, Compressor, DecoderDict, Decompressor, EncoderDict, check_level};
pub use dict::{
    DICT_HEADER_LEN, DICT_ID_ALLOC_END, DICT_ID_ALLOC_MIN, DICT_MAGIC, DictError, DictHeader,
    is_allocatable_dict_id, set_dict_id,
};
pub use frame::{
    FRAME_MAGIC, FrameError, FrameInfo, MAX_FRAME_HEADER_LEN, inspect_frame, inspect_single_frame,
};
#[cfg(feature = "train")]
pub use train::{SampleSet, TrainError, TrainOptions, train};
