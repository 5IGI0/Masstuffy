//! `config.json` (version 1), the limits derived from it, and the raw-import check from the
//! environment.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;

use masstuffy_collection::{CHECKPOINT_MIN_GROWTH, DEFAULT_SPLIT_THRESHOLD, Options, RawCheck};
use masstuffy_dictstore::{DEFAULT_CACHE_BYTES, DEFAULT_MAX_FILE_SIZE};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::layout::CONFIG_NAME;

/// Largest accepted `config.json`, in bytes.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Environment variable selecting the raw-import check (`full` or `header`), read once at open.
pub const RAW_IMPORT_CHECK_ENV: &str = "MASSTUFFY_RAW_IMPORT_CHECK";

const MIB: u64 = 1 << 20;
const TIB: u64 = 1 << 40;

/// Repository configuration, stored as `config.json` (schema version 1).
///
/// Unknown fields are rejected; missing fields take their default. `init` writes every field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Largest serialised WARC record. Default 1 GiB. At least 1.
    pub max_record_bytes: u64,
    /// Open segment files kept per collection snapshot. Default 64. `1..=65536`.
    pub max_open_files: u32,
    /// Byte budget of the prepared-dictionary cache. Default 256 MiB.
    pub dict_cache_bytes: u64,
    /// Largest dictionary file. Default 64 MiB. At least 8.
    pub dict_max_file_size: u64,
    /// Segment size cap given to new collections.
    pub default_split_threshold: NonZeroU64,
    /// Capacity of each collection's request channel (requests, not records). Default 64.
    /// `1..=65536`.
    pub writer_queue: u32,
    /// Most requests merged into one commit. Default 128. `1..=65536`.
    pub max_group_requests: u32,
    /// Most stored bytes merged into one commit (a single larger request is committed alone). The
    /// effective cap of a collection is the smaller of this and its split threshold. Default
    /// 64 MiB. At least 1.
    pub max_group_bytes: u64,
    /// Repository-wide budget of input bytes admitted but not yet committed. Default 256 MiB.
    /// `1 MiB..=1 TiB`.
    pub max_inflight_put_bytes: u64,
    /// Idle `Preparer`s kept per collection. Default 4. `1..=1024`.
    pub preparers_per_collection: u32,
    /// Repository-wide concurrent prepare jobs. `None`: the available parallelism. `Some(1..=4096)`.
    pub max_concurrent_prepares: Option<u32>,
    /// Repository-wide concurrent reads, also the idle read-context pool cap. Default 64.
    /// `1..=4096`.
    pub max_concurrent_reads: u32,
    /// Collections opened in parallel at startup. Default 4. `1..=256`.
    pub open_concurrency: u32,
    /// CDX growth (bytes) after which a writer task writes a checkpoint. Default 64 MiB. At least
    /// `CHECKPOINT_MIN_GROWTH`.
    pub checkpoint_every_bytes: u64,
    /// Every N-th secondary-index commit is fsynced (redb backend); structural batches always
    /// are. Default 1. `1..=65536`. Added after the first release of schema 1: an older binary
    /// rejects a config written by this one (`deny_unknown_fields`).
    pub index_sync_every: u32,
    /// Page cache of the redb secondary index, in bytes. Default 64 MiB. `1 MiB..=1 TiB`.
    pub index_cache_bytes: u64,
    /// CDX lines per secondary-index batch (shared by the collections of one indexer pass).
    /// Default 8192. `64..=65536`.
    pub index_batch_ops: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_record_bytes: 1 << 30,
            max_open_files: 64,
            dict_cache_bytes: DEFAULT_CACHE_BYTES,
            dict_max_file_size: DEFAULT_MAX_FILE_SIZE,
            default_split_threshold: DEFAULT_SPLIT_THRESHOLD,
            writer_queue: 64,
            max_group_requests: 128,
            max_group_bytes: 64 * MIB,
            max_inflight_put_bytes: 256 * MIB,
            preparers_per_collection: 4,
            max_concurrent_prepares: None,
            max_concurrent_reads: 64,
            open_concurrency: 4,
            checkpoint_every_bytes: 64 * MIB,
            index_sync_every: 1,
            index_cache_bytes: 64 * MIB,
            index_batch_ops: 8192,
        }
    }
}

/// Why a config is invalid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file is larger than [`MAX_CONFIG_BYTES`].
    #[error("config of {len} bytes exceeds the cap of {cap}")]
    TooLarge {
        /// Size found.
        len: u64,
        /// The cap.
        cap: u64,
    },
    /// Not valid JSON for the schema.
    #[error("invalid config JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The `version` field is not 1.
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
    /// A value is out of range (the field).
    #[error("invalid value for {0}")]
    Invalid(&'static str),
}

#[derive(Serialize)]
struct Versioned<'a> {
    version: u32,
    #[serde(flatten)]
    config: &'a Config,
}

#[derive(Deserialize)]
struct Probe {
    version: u32,
}

fn in_range(v: u64, lo: u64, hi: u64, field: &'static str) -> Result<(), ConfigError> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(ConfigError::Invalid(field))
    }
}

impl Config {
    /// Checks every range.
    ///
    /// # Errors
    ///
    /// `Invalid(field)`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_record_bytes < 1 {
            return Err(ConfigError::Invalid("max_record_bytes"));
        }
        in_range(self.max_open_files.into(), 1, 65536, "max_open_files")?;
        if self.dict_max_file_size < 8 {
            return Err(ConfigError::Invalid("dict_max_file_size"));
        }
        in_range(self.writer_queue.into(), 1, 65536, "writer_queue")?;
        in_range(
            self.max_group_requests.into(),
            1,
            65536,
            "max_group_requests",
        )?;
        if self.max_group_bytes < 1 {
            return Err(ConfigError::Invalid("max_group_bytes"));
        }
        in_range(
            self.max_inflight_put_bytes,
            MIB,
            TIB,
            "max_inflight_put_bytes",
        )?;
        in_range(
            self.preparers_per_collection.into(),
            1,
            1024,
            "preparers_per_collection",
        )?;
        if let Some(n) = self.max_concurrent_prepares {
            in_range(n.into(), 1, 4096, "max_concurrent_prepares")?;
        }
        in_range(
            self.max_concurrent_reads.into(),
            1,
            4096,
            "max_concurrent_reads",
        )?;
        in_range(self.open_concurrency.into(), 1, 256, "open_concurrency")?;
        if self.checkpoint_every_bytes < CHECKPOINT_MIN_GROWTH {
            return Err(ConfigError::Invalid("checkpoint_every_bytes"));
        }
        in_range(self.index_sync_every.into(), 1, 65536, "index_sync_every")?;
        in_range(self.index_cache_bytes, MIB, TIB, "index_cache_bytes")?;
        in_range(self.index_batch_ops.into(), 64, 65536, "index_batch_ops")?;
        Ok(())
    }

    /// The full config as pretty JSON with a `"version": 1` field, followed by a newline.
    ///
    /// # Errors
    ///
    /// `Json` (unreachable for this type).
    pub fn to_json(&self) -> Result<Vec<u8>, ConfigError> {
        let mut v = serde_json::to_vec_pretty(&Versioned {
            version: 1,
            config: self,
        })?;
        v.push(b'\n');
        Ok(v)
    }

    /// Parses and validates a config.
    ///
    /// # Errors
    ///
    /// `Json` (syntax, not an object, unknown field, missing `version`, wrong type),
    /// `UnsupportedVersion`, `Invalid`.
    pub fn from_json(bytes: &[u8]) -> Result<Config, ConfigError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let serde_json::Value::Object(mut map) = value else {
            return Err(ConfigError::Json(
                <serde_json::Error as serde::de::Error>::custom("config must be a JSON object"),
            ));
        };
        let probe: Probe = serde_json::from_value(serde_json::Value::Object(map.clone()))?;
        if probe.version != 1 {
            return Err(ConfigError::UnsupportedVersion(probe.version));
        }
        map.remove("version");
        let cfg: Config = serde_json::from_value(serde_json::Value::Object(map))?;
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Reads and parses `<dir>/config.json`.
///
/// Blocking. `NotFound` becomes [`Error::NotARepository`].
pub(crate) fn load(dir: &Path) -> Result<Config, Error> {
    let path = dir.join(CONFIG_NAME);
    let cfg_err = |source: ConfigError| Error::Config {
        path: path.clone(),
        source,
    };
    let io_err = |op: &'static str| {
        let path = path.clone();
        move |source| Error::Io { op, path, source }
    };
    let file = File::open(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotARepository {
                path: dir.to_path_buf(),
            }
        } else {
            io_err("open")(e)
        }
    })?;
    let len = file.metadata().map_err(io_err("stat"))?.len();
    if len > MAX_CONFIG_BYTES {
        return Err(cfg_err(ConfigError::TooLarge {
            len,
            cap: MAX_CONFIG_BYTES,
        }));
    }
    let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(io_err("read"))?;
    let got = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    if got > MAX_CONFIG_BYTES {
        return Err(cfg_err(ConfigError::TooLarge {
            len: got,
            cap: MAX_CONFIG_BYTES,
        }));
    }
    Config::from_json(&buf).map_err(cfg_err)
}

/// Runtime values derived from a validated [`Config`], computed once at open.
pub(crate) struct Limits {
    pub(crate) prepare_permits: usize,
    pub(crate) read_permits: usize,
    /// `ceil(max_inflight_put_bytes / 1024)`; one permit is one KiB.
    pub(crate) inflight_kib: u32,
    pub(crate) collection_opts: Options,
    pub(crate) dict_opts: masstuffy_dictstore::Options,
}

impl Limits {
    pub(crate) fn new(c: &Config) -> Limits {
        let prepare_permits = match c.max_concurrent_prepares {
            Some(n) => usize::try_from(n).unwrap_or(1),
            None => std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
        };
        let kib = c.max_inflight_put_bytes.div_ceil(1024);
        Limits {
            prepare_permits: prepare_permits.max(1),
            read_permits: usize::try_from(c.max_concurrent_reads).unwrap_or(1).max(1),
            // validate() caps max_inflight_put_bytes at 1 TiB, so kib <= 2^30.
            inflight_kib: u32::try_from(kib).unwrap_or(u32::MAX),
            collection_opts: Options {
                max_record_bytes: c.max_record_bytes,
                max_open_files: NonZeroUsize::new(usize::try_from(c.max_open_files).unwrap_or(1))
                    .unwrap_or(NonZeroUsize::MIN),
            },
            dict_opts: masstuffy_dictstore::Options {
                max_file_size: c.dict_max_file_size,
                cache_bytes: c.dict_cache_bytes,
            },
        }
    }
}

/// `None` (unset) and `full` select `Full`, `header` selects `HeaderOnly`; anything else
/// (including non-UTF-8, empty, other case) is an error.
pub(crate) fn raw_check_from(value: Option<&OsStr>) -> Result<RawCheck, Error> {
    match value {
        None => Ok(RawCheck::Full),
        Some(v) if v == "full" => Ok(RawCheck::Full),
        Some(v) if v == "header" => Ok(RawCheck::HeaderOnly),
        Some(v) => Err(Error::InvalidEnv {
            var: RAW_IMPORT_CHECK_ENV,
            value: v.to_string_lossy().into_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::testutil::TestDir;

    #[test]
    fn config_default_round_trip() {
        let json = Config::default().to_json().unwrap();
        assert!(
            std::str::from_utf8(&json)
                .unwrap()
                .contains("\"version\": 1")
        );
        assert_eq!(Config::from_json(&json).unwrap(), Config::default());
    }

    #[test]
    fn config_missing_fields_default() {
        assert_eq!(
            Config::from_json(br#"{"version":1}"#).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn config_rejects_unknown_field() {
        assert!(matches!(
            Config::from_json(br#"{"version":1,"bogus":1}"#),
            Err(ConfigError::Json(_))
        ));
    }

    #[test]
    fn config_rejects_version_2() {
        assert!(matches!(
            Config::from_json(br#"{"version":2}"#),
            Err(ConfigError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn config_rejects_missing_version() {
        assert!(matches!(
            Config::from_json(br#"{"writer_queue":3}"#),
            Err(ConfigError::Json(_))
        ));
    }

    #[test]
    fn config_rejects_array_top_level() {
        assert!(matches!(
            Config::from_json(b"[1]"),
            Err(ConfigError::Json(_))
        ));
    }

    #[test]
    fn config_rejects_invalid_values() {
        for (field, v) in [
            ("max_record_bytes", "0"),
            ("writer_queue", "0"),
            ("max_group_requests", "0"),
            ("max_group_bytes", "0"),
            ("max_inflight_put_bytes", "1024"),
            ("max_concurrent_prepares", "0"),
            ("open_concurrency", "0"),
            ("checkpoint_every_bytes", "1"),
            ("default_split_threshold", "0"),
            ("preparers_per_collection", "0"),
            ("max_concurrent_reads", "0"),
            ("max_open_files", "0"),
            ("dict_max_file_size", "4"),
            ("index_sync_every", "0"),
            ("index_sync_every", "65537"),
            ("index_cache_bytes", "1024"),
            ("index_cache_bytes", "1099511627777"),
            ("index_batch_ops", "63"),
            ("index_batch_ops", "65537"),
        ] {
            let json = format!(r#"{{"version":1,"{field}":{v}}}"#);
            assert!(
                matches!(
                    Config::from_json(json.as_bytes()),
                    Err(ConfigError::Invalid(_) | ConfigError::Json(_))
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn config_load_too_large_and_missing() {
        let td = TestDir::new();
        assert!(matches!(load(td.path()), Err(Error::NotARepository { .. })));
        let big = vec![b' '; usize::try_from(MAX_CONFIG_BYTES).unwrap() + 1];
        std::fs::write(td.path().join(CONFIG_NAME), big).unwrap();
        assert!(matches!(
            load(td.path()),
            Err(Error::Config {
                source: ConfigError::TooLarge { .. },
                ..
            })
        ));
    }

    #[test]
    fn raw_check_values() {
        assert_eq!(raw_check_from(None).unwrap(), RawCheck::Full);
        assert_eq!(
            raw_check_from(Some(OsStr::new("full"))).unwrap(),
            RawCheck::Full
        );
        assert_eq!(
            raw_check_from(Some(OsStr::new("header"))).unwrap(),
            RawCheck::HeaderOnly
        );
        for bad in [
            OsStr::new(""),
            OsStr::new("FULL"),
            OsStr::new("headers"),
            OsStr::from_bytes(&[0xff]),
        ] {
            assert!(matches!(
                raw_check_from(Some(bad)),
                Err(Error::InvalidEnv { .. })
            ));
        }
    }
}
