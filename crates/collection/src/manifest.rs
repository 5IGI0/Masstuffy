//! The versioned manifest (`manifest.json`).

use std::fs::File;
use std::io::Read;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::MAX_MANIFEST_BYTES;
use crate::error::{Error, ManifestError};
use crate::id::CollectionId;
use crate::layout::MANIFEST_NAME;

const MAX_SLUG_BYTES: usize = 256;

/// Compression policy of a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Records are stored as plain WARC.
    None,
    /// Records are stored as one zstd frame each, with dictionary `dict_id` (large records may
    /// use no dictionary, see [`PLAIN_FRAME_MIN`](crate::PLAIN_FRAME_MIN)).
    Zstd {
        /// Dictionary id.
        dict_id: NonZeroU32,
        /// Compression level.
        level: i32,
    },
}

/// One generation (spec 06): `id >= 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// Generation number, at least 1.
    pub id: u32,
    /// Its compression policy.
    pub codec: Codec,
}

/// Validated manifest. Immutable; shared as `Arc<Manifest>`.
///
/// Invariants: slug valid (1..=256 bytes, no control character), `active.id >= 1`,
/// `next_gen_id > active.id`, zstd level in zstd's range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    uuid: CollectionId,
    slug: String,
    split_threshold: NonZeroU64,
    next_gen_id: u32,
    active: Generation,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV1 {
    version: u32,
    uuid: String,
    slug: String,
    split_threshold: NonZeroU64,
    next_gen_id: u32,
    active: GenerationV1,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationV1 {
    id: u32,
    codec: CodecV1,
}

// `None` must be a struct variant: a unit variant in an internally tagged enum silently accepts
// unknown fields, `None {}` rejects them.
#[derive(Serialize, Deserialize)]
#[serde(tag = "algo", rename_all = "lowercase", deny_unknown_fields)]
enum CodecV1 {
    None {},
    Zstd { dict_id: NonZeroU32, level: i32 },
}

/// Rejects the array form of the manifest, `active` and `codec` objects.
fn require_objects(v: &serde_json::Value) -> Result<(), ManifestError> {
    use serde::de::Error as _;
    let active = v.get("active");
    let codec = active.and_then(|a| a.get("codec"));
    for x in [Some(v), active, codec].into_iter().flatten() {
        if !x.is_object() {
            return Err(ManifestError::Json(serde_json::Error::custom(
                "expected a JSON object",
            )));
        }
    }
    Ok(())
}

impl Manifest {
    /// Builds and validates a manifest.
    ///
    /// # Errors
    ///
    /// `InvalidSlug`, `InvalidGeneration`, `InvalidLevel`.
    pub fn new(
        uuid: CollectionId,
        slug: &str,
        split_threshold: NonZeroU64,
        active: Generation,
        next_gen_id: u32,
    ) -> Result<Manifest, ManifestError> {
        if slug.is_empty() {
            return Err(ManifestError::InvalidSlug("empty"));
        }
        if slug.len() > MAX_SLUG_BYTES {
            return Err(ManifestError::InvalidSlug("too long"));
        }
        if slug.chars().any(char::is_control) {
            return Err(ManifestError::InvalidSlug("control character"));
        }
        if active.id < 1 || next_gen_id <= active.id {
            return Err(ManifestError::InvalidGeneration {
                active: active.id,
                next: next_gen_id,
            });
        }
        if let Codec::Zstd { level, .. } = active.codec {
            masstuffy_zframe::check_level(level).map_err(|_| ManifestError::InvalidLevel(level))?;
        }
        Ok(Manifest {
            uuid,
            slug: slug.to_owned(),
            split_threshold,
            next_gen_id,
            active,
        })
    }

    /// The collection uuid.
    #[must_use]
    pub fn uuid(&self) -> CollectionId {
        self.uuid
    }

    /// The slug.
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The segment size cap.
    #[must_use]
    pub fn split_threshold(&self) -> NonZeroU64 {
        self.split_threshold
    }

    /// The id the next generation will get.
    #[must_use]
    pub fn next_gen_id(&self) -> u32 {
        self.next_gen_id
    }

    /// The active generation.
    #[must_use]
    pub fn active(&self) -> Generation {
        self.active
    }

    /// Parses and validates. Unknown fields, duplicate keys and trailing data are rejected.
    ///
    /// # Errors
    ///
    /// `TooLarge` (before parsing), `Json`, `UnsupportedVersion`, `InvalidUuid`, and the errors of
    /// [`Manifest::new`].
    pub fn from_json(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if len > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                len,
                cap: MAX_MANIFEST_BYTES,
            });
        }
        let probe: VersionProbe = serde_json::from_slice(bytes)?;
        if probe.version != 1 {
            return Err(ManifestError::UnsupportedVersion(probe.version));
        }
        // serde structs also accept JSON arrays; the schema is objects only. The `Value` is used
        // for this shape check only (it drops duplicate keys; the typed parse below rejects them).
        require_objects(&serde_json::from_slice::<serde_json::Value>(bytes)?)?;
        let m: ManifestV1 = serde_json::from_slice(bytes)?;
        let codec = match m.active.codec {
            CodecV1::None {} => Codec::None,
            CodecV1::Zstd { dict_id, level } => Codec::Zstd { dict_id, level },
        };
        Manifest::new(
            m.uuid.parse()?,
            &m.slug,
            m.split_threshold,
            Generation {
                id: m.active.id,
                codec,
            },
            m.next_gen_id,
        )
    }

    /// Compact JSON, no trailing newline, in schema field order.
    ///
    /// # Errors
    ///
    /// `Json` if serialisation fails (not expected).
    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        let codec = match self.active.codec {
            Codec::None => CodecV1::None {},
            Codec::Zstd { dict_id, level } => CodecV1::Zstd { dict_id, level },
        };
        let m = ManifestV1 {
            version: 1,
            uuid: self.uuid.to_string(),
            slug: self.slug.clone(),
            split_threshold: self.split_threshold,
            next_gen_id: self.next_gen_id,
            active: GenerationV1 {
                id: self.active.id,
                codec,
            },
        };
        Ok(serde_json::to_vec(&m)?)
    }

    /// Reads `dir/manifest.json` (at most `MAX_MANIFEST_BYTES + 1` bytes) and parses it.
    ///
    /// # Errors
    ///
    /// `ManifestMissing` if the file does not exist, `Io`, `Manifest`.
    pub fn load(dir: &Path) -> Result<Manifest, Error> {
        let path = dir.join(MANIFEST_NAME);
        let file = File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::ManifestMissing { path: path.clone() }
            } else {
                Error::Io {
                    op: "open",
                    path: path.clone(),
                    source: e,
                }
            }
        })?;
        let mut buf = Vec::with_capacity(4096);
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Error::Io {
                op: "read",
                path: path.clone(),
                source: e,
            })?;
        Manifest::from_json(&buf).map_err(|source| Error::Manifest { path, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestDir;

    const UUID: &str = "1b4e28ba-2fa1-41d2-883f-0016d3cca427";

    fn id() -> CollectionId {
        UUID.parse().unwrap()
    }

    fn mk(codec: Codec) -> Manifest {
        Manifest::new(
            id(),
            "my-collection",
            NonZeroU64::new(4_294_967_295).unwrap(),
            Generation { id: 1, codec },
            2,
        )
        .unwrap()
    }

    #[test]
    fn manifest_round_trip_none() {
        let m = mk(Codec::None);
        let j = m.to_json().unwrap();
        assert_eq!(
            std::str::from_utf8(&j).unwrap(),
            format!(
                r#"{{"version":1,"uuid":"{UUID}","slug":"my-collection","split_threshold":4294967295,"next_gen_id":2,"active":{{"id":1,"codec":{{"algo":"none"}}}}}}"#
            )
        );
        assert_eq!(Manifest::from_json(&j).unwrap(), m);
    }

    #[test]
    fn manifest_round_trip_zstd() {
        let m = mk(Codec::Zstd {
            dict_id: NonZeroU32::new(1_234_567_890).unwrap(),
            level: 3,
        });
        let j = m.to_json().unwrap();
        assert_eq!(
            std::str::from_utf8(&j).unwrap(),
            format!(
                r#"{{"version":1,"uuid":"{UUID}","slug":"my-collection","split_threshold":4294967295,"next_gen_id":2,"active":{{"id":1,"codec":{{"algo":"zstd","dict_id":1234567890,"level":3}}}}}}"#
            )
        );
        assert_eq!(Manifest::from_json(&j).unwrap(), m);
    }

    fn json(version: &str, uuid: &str, slug: &str, thr: &str, next: &str, active: &str) -> String {
        format!(
            r#"{{"version":{version},"uuid":"{uuid}","slug":{slug},"split_threshold":{thr},"next_gen_id":{next},"active":{active}}}"#
        )
    }

    fn ok_json() -> String {
        json(
            "1",
            UUID,
            r#""s""#,
            "100",
            "2",
            r#"{"id":1,"codec":{"algo":"none"}}"#,
        )
    }

    #[test]
    fn manifest_rejects() {
        let none = r#"{"id":1,"codec":{"algo":"none"}}"#;
        let zstd =
            |x: &str| format!(r#"{{"id":1,"codec":{{"algo":"zstd","dict_id":5,"level":3{x}}}}}"#);
        let long_slug = format!("\"{}\"", "a".repeat(257));
        let cases: Vec<(&str, String)> = vec![
            ("not json", "garbage".into()),
            ("array", "[]".into()),
            ("trailing", format!("{}x", ok_json())),
            (
                "missing slug",
                format!(
                    r#"{{"version":1,"uuid":"{UUID}","split_threshold":100,"next_gen_id":2,"active":{none}}}"#
                ),
            ),
            (
                "unknown top",
                ok_json().replacen("{\"version\"", "{\"x\":1,\"version\"", 1),
            ),
            (
                "unknown in active",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"x":1,"codec":{"algo":"none"}}"#,
                ),
            ),
            (
                "unknown in codec none",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"codec":{"algo":"none","x":1}}"#,
                ),
            ),
            (
                "unknown in codec zstd",
                json("1", UUID, r#""s""#, "100", "2", &zstd(r#","x":1"#)),
            ),
            (
                "gzip",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"codec":{"algo":"gzip"}}"#,
                ),
            ),
            (
                "missing version",
                format!(
                    r#"{{"uuid":"{UUID}","slug":"s","split_threshold":100,"next_gen_id":2,"active":{none}}}"#
                ),
            ),
            (
                "duplicate version",
                ok_json().replacen("\"version\":1,", "\"version\":1,\"version\":1,", 1),
            ),
            ("zero threshold", json("1", UUID, r#""s""#, "0", "2", none)),
            (
                "zero dict id",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"codec":{"algo":"zstd","dict_id":0,"level":3}}"#,
                ),
            ),
            (
                "level 1000",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"codec":{"algo":"zstd","dict_id":5,"level":1000}}"#,
                ),
            ),
            (
                "active 0",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":0,"codec":{"algo":"none"}}"#,
                ),
            ),
            (
                "next <= active",
                json("1", UUID, r#""s""#, "100", "1", none),
            ),
            ("bad uuid", json("1", "nope", r#""s""#, "100", "2", none)),
            ("empty slug", json("1", UUID, r#""""#, "100", "2", none)),
            ("long slug", json("1", UUID, &long_slug, "100", "2", none)),
            (
                "newline slug",
                json("1", UUID, r#""a\nb""#, "100", "2", none),
            ),
            (
                "bell slug",
                json("1", UUID, r#""a\u0007b""#, "100", "2", none),
            ),
            ("negative", json("1", UUID, r#""s""#, "-1", "2", none)),
            ("float", json("1", UUID, r#""s""#, "1.5", "2", none)),
            ("null", json("1", UUID, "null", "100", "2", none)),
            (
                "array active",
                json("1", UUID, r#""s""#, "100", "2", r#"[1,{"algo":"none"}]"#),
            ),
            (
                "array codec",
                json(
                    "1",
                    UUID,
                    r#""s""#,
                    "100",
                    "2",
                    r#"{"id":1,"codec":["none"]}"#,
                ),
            ),
            ("version 2", json("2", UUID, r#""s""#, "100", "2", none)),
        ];
        for (name, j) in cases {
            let r = Manifest::from_json(j.as_bytes());
            let ok = match name {
                "version 2" => matches!(r, Err(ManifestError::UnsupportedVersion(2))),
                "level 1000" => matches!(r, Err(ManifestError::InvalidLevel(1000))),
                "active 0" | "next <= active" => {
                    matches!(r, Err(ManifestError::InvalidGeneration { .. }))
                }
                "bad uuid" => matches!(r, Err(ManifestError::InvalidUuid)),
                "empty slug" | "long slug" | "newline slug" | "bell slug" => {
                    matches!(r, Err(ManifestError::InvalidSlug(_)))
                }
                _ => matches!(r, Err(ManifestError::Json(_))),
            };
            assert!(ok, "{name}: {r:?}");
        }
        assert!(Manifest::from_json(ok_json().as_bytes()).is_ok());
    }

    #[test]
    fn manifest_too_large() {
        let mut v = vec![b' '; usize::try_from(MAX_MANIFEST_BYTES).unwrap() + 1];
        v.extend_from_slice(ok_json().as_bytes());
        assert!(matches!(
            Manifest::from_json(&v),
            Err(ManifestError::TooLarge { .. })
        ));
        let d = TestDir::new();
        std::fs::write(d.path().join(MANIFEST_NAME), vec![b' '; 1 << 20]).unwrap();
        assert!(matches!(
            Manifest::load(d.path()),
            Err(Error::Manifest {
                source: ManifestError::TooLarge { .. },
                ..
            })
        ));
    }

    #[test]
    fn manifest_load_missing() {
        let d = TestDir::new();
        assert!(matches!(
            Manifest::load(d.path()),
            Err(Error::ManifestMissing { .. })
        ));
    }
}
