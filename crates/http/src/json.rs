//! JSON wire shapes: output structs built from engine types, and request structs.

use std::num::NonZeroU32;

use masstuffy::{
    Codec, CollectionInfo, DeletedCollection, GrepEvents, GrepScope, ImportReport, Located,
    OpenReport, RawCheck, RebuildReport, RecordSkipReason, SkipReason, TrainReport, WriterStats,
};
use masstuffy_cdx::EnvelopeFormat;
use masstuffy_warc::WarcDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::locator::encode_locator;

/// One record position.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LocatorJson {
    pub locator: String,
    pub collection: String,
    pub generation: u32,
    pub segment: u32,
    pub offset: u64,
    pub raw_size: u64,
    pub digest: String,
    pub date: String,
    pub warc_type: String,
    pub record_id: String,
    pub url: Option<String>,
}

pub(crate) fn locator_json(at: &Located) -> LocatorJson {
    let e = &at.entry;
    LocatorJson {
        locator: encode_locator(at),
        collection: at.collection.to_string(),
        generation: at.generation,
        segment: e.segment,
        offset: e.offset,
        raw_size: e.raw_size,
        digest: format!("{:016x}", e.digest),
        date: e.date.to_string(),
        warc_type: e.warc_type.to_string(),
        record_id: e.record_id.to_string(),
        url: e.url.as_deref().map(str::to_owned),
    }
}

const fn default_level() -> i32 {
    3
}

/// Codec of a collection, as input and output.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "algo", rename_all = "lowercase", deny_unknown_fields)]
pub(crate) enum CodecJson {
    None,
    Zstd {
        dict_id: u32,
        #[serde(default = "default_level")]
        level: i32,
    },
}

impl CodecJson {
    pub(crate) fn to_codec(self) -> Result<Codec, ApiError> {
        match self {
            CodecJson::None => Ok(Codec::None),
            CodecJson::Zstd { dict_id, level } => {
                let dict_id = NonZeroU32::new(dict_id).ok_or_else(|| {
                    ApiError::invalid("invalid_codec", "dict_id must be non-zero")
                })?;
                masstuffy_zframe::check_level(level).map_err(|e| {
                    ApiError::invalid("invalid_codec", format!("invalid zstd level: {e}"))
                })?;
                Ok(Codec::Zstd { dict_id, level })
            }
        }
    }
}

impl From<Codec> for CodecJson {
    fn from(c: Codec) -> Self {
        match c {
            Codec::None => CodecJson::None,
            Codec::Zstd { dict_id, level } => CodecJson::Zstd {
                dict_id: dict_id.get(),
                level,
            },
        }
    }
}

/// Writer counters of a collection.
#[derive(Debug, Serialize)]
pub(crate) struct StatsJson {
    pub commits: u64,
    pub records: u64,
    pub cdx_len: u64,
}

impl From<WriterStats> for StatsJson {
    fn from(s: WriterStats) -> Self {
        StatsJson {
            commits: s.commits,
            records: s.records,
            cdx_len: s.cdx_len,
        }
    }
}

/// Smallest declared content size (bytes) for which a zstd collection accepts a frame without
/// dictionary: `PLAIN_FRAME_MIN + 1` (the test `plain_frame_min_matches_engine` pins it).
pub(crate) const DICTLESS_MIN_CONTENT_SIZE: u64 = 1_048_577;

/// The rule the server enforces on delegated (raw) payloads of a collection.
#[derive(Debug, Serialize)]
pub(crate) struct RawRulesJson {
    pub payload: &'static str,
    pub single_frame: bool,
    pub dict_id: Option<u32>,
    pub dictless_min_content_size: u64,
}

impl From<Codec> for RawRulesJson {
    fn from(c: Codec) -> Self {
        let (payload, dict_id) = match c {
            Codec::None => ("warc", None),
            Codec::Zstd { dict_id, .. } => ("zstd_frame", Some(dict_id.get())),
        };
        RawRulesJson {
            payload,
            single_frame: true,
            dict_id,
            dictless_min_content_size: DICTLESS_MIN_CONTENT_SIZE,
        }
    }
}

/// A collection.
#[derive(Debug, Serialize)]
pub(crate) struct CollectionJson {
    pub id: String,
    pub slug: String,
    pub generation: u32,
    pub codec: CodecJson,
    pub raw_rules: RawRulesJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<StatsJson>,
}

pub(crate) fn collection_json(info: &CollectionInfo, stats: Option<WriterStats>) -> CollectionJson {
    CollectionJson {
        id: info.id.to_string(),
        slug: info.slug.to_string(),
        generation: info.generation,
        codec: info.codec.into(),
        raw_rules: info.codec.into(),
        stats: stats.map(StatsJson::from),
    }
}

/// Result of a collection deletion.
#[derive(Debug, Serialize)]
pub(crate) struct DeletedCollectionJson {
    pub id: String,
    pub slug: String,
    pub trash_removed: bool,
    pub slug_reserved: bool,
    pub index_dropped: bool,
}

impl From<&DeletedCollection> for DeletedCollectionJson {
    fn from(d: &DeletedCollection) -> Self {
        DeletedCollectionJson {
            id: d.id.to_string(),
            slug: d.slug.to_string(),
            trash_removed: d.trash_removed,
            slug_reserved: d.slug_reserved,
            index_dropped: d.index_dropped,
        }
    }
}

/// Progress of an import.
pub(crate) fn import_report_json(r: &ImportReport) -> Value {
    let format = match r.format {
        Some(EnvelopeFormat::V1) => Value::from("v1"),
        Some(EnvelopeFormat::Legacy) => Value::from("legacy"),
        _ => Value::Null,
    };
    let rejections: Vec<Value> = r
        .rejections
        .iter()
        .map(|j| {
            json!({
                "offset": j.offset,
                "ordinal": j.ordinal,
                "record_id": j.record_id,
                "error": j.error.to_string(),
            })
        })
        .collect();
    json!({
        "format": format,
        "records": r.records,
        "batches": r.batches,
        "rejected": r.rejected,
        "rejections": rejections,
        "committed_offset": r.committed_offset,
    })
}

fn skip_reason(r: RecordSkipReason) -> &'static str {
    match r {
        RecordSkipReason::Digest => "digest",
        RecordSkipReason::Bounds => "bounds",
        RecordSkipReason::Frame => "frame",
        RecordSkipReason::Decode => "decode",
        RecordSkipReason::NotWarc => "not_warc",
        _ => "other",
    }
}

/// What a rebuild did.
pub(crate) fn rebuild_report_json(r: &RebuildReport) -> Value {
    let c = &r.copy;
    let skipped_first: Vec<Value> = c
        .skipped_first
        .iter()
        .map(|s| {
            json!({
                "segment": s.segment,
                "offset": s.offset,
                "record_id": s.record_id,
                "reason": skip_reason(s.reason),
            })
        })
        .collect();
    json!({
        "rounds": r.rounds,
        "index_switched": r.index_switched,
        "old_files_removed": r.old_files_removed,
        "from_generation": c.from_generation,
        "generation": c.generation,
        "codec": CodecJson::from(c.codec),
        "copied": c.copied,
        "raw_copied": c.raw_copied,
        "dropped_tombstoned": c.dropped_tombstoned,
        "tombstone_lines": c.tombstone_lines,
        "skipped": c.skipped,
        "skipped_first": skipped_first,
        "bytes_read": c.bytes_read,
        "bytes_written": c.bytes_written,
    })
}

/// What a training run did.
pub(crate) fn train_report_json(r: &TrainReport) -> Value {
    json!({
        "dict_id": r.dict_id,
        "created": r.created,
        "label": r.label,
        "generation": r.generation,
        "cdx_len": r.cdx_len,
        "candidates": r.candidates,
        "samples": r.samples,
        "sample_bytes": r.sample_bytes,
        "skipped": r.skipped,
        "dict_size": r.dict_size,
    })
}

/// The raw-import check in force.
pub(crate) fn raw_check_str(c: RawCheck) -> &'static str {
    match c {
        RawCheck::Full => "full",
        RawCheck::HeaderOnly => "header",
    }
}

fn open_skip(r: &SkipReason) -> (&'static str, String) {
    match r {
        SkipReason::Open(e) => ("open_failed", e.to_string()),
        SkipReason::UuidMismatch { manifest } => (
            "uuid_mismatch",
            format!("manifest uuid {manifest} differs from the directory name"),
        ),
        SkipReason::DuplicateSlug { slug } => {
            ("duplicate_slug", format!("slug {slug:?} is used twice"))
        }
        SkipReason::TrashFailed(e) => ("trash_failed", e.to_string()),
        _ => ("other", "skipped".to_owned()),
    }
}

/// The startup report, rendered once at router construction.
pub(crate) fn open_report_json(r: &OpenReport) -> Value {
    let loaded: Vec<Value> = r
        .loaded
        .iter()
        .map(|l| {
            json!({
                "id": l.id.to_string(),
                "slug": &*l.slug,
                "generation": l.recovery.generation,
                "entries": l.recovery.entries,
                "cdx_len": l.recovery.cdx_len,
                "cdx_torn": l.recovery.cdx_torn.is_some(),
                "repaired": l.recovery.repair.is_some(),
            })
        })
        .collect();
    let skipped: Vec<Value> = r
        .skipped
        .iter()
        .map(|s| {
            let (reason, message) = open_skip(&s.reason);
            json!({
                "dir": s.dir.display().to_string(),
                "reason": reason,
                "message": message,
            })
        })
        .collect();
    json!({
        "loaded": loaded,
        "skipped": skipped,
        "trashed": r.trashed.len(),
        "unknown_count": r.unknown_count,
        "tmp_removed": r.tmp_removed,
        "trash_removed": r.trash_removed,
        "index_recreated": r.index_recreated,
    })
}

/// A job, as returned by the jobs routes.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JobJson {
    pub id: String,
    pub kind: &'static str,
    pub collection: Option<String>,
    pub collection_id: Option<String>,
    pub state: &'static str,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
}

/// `WarcDate` of now, as text; `None` if the clock is out of range.
pub(crate) fn now_string() -> Option<String> {
    WarcDate::from_system_time(std::time::SystemTime::now()).map(|d| d.to_string())
}

// ---- Requests.

/// `POST /v1/collections`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateCollectionJson {
    pub slug: String,
    #[serde(default)]
    pub codec: Option<CodecJson>,
}

/// `PATCH /v1/collections/{slug}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenameJson {
    pub slug: String,
}

/// `POST .../records/delete`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteLocatorsJson {
    pub locators: Vec<String>,
}

/// `POST .../records/delete-by-id`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteByIdJson {
    pub record_id: String,
}

/// `POST .../train`; every field optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrainOptionsJson {
    pub max_samples: Option<usize>,
    pub max_sample_bytes: Option<usize>,
    pub max_total_bytes: Option<u64>,
    pub max_dict_size: Option<usize>,
}

/// Damaged-record policy of a rebuild.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OnUnreadableJson {
    #[default]
    Fail,
    Skip,
}

/// `POST .../rebuild`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RebuildRequestJson {
    pub codec: CodecJson,
    #[serde(default)]
    pub recompress: bool,
    #[serde(default)]
    pub on_unreadable: OnUnreadableJson,
    #[serde(default)]
    pub workers: Option<usize>,
    #[serde(default)]
    pub max_inflight_bytes: Option<u64>,
}

/// `POST /v1/grep`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepRequestJson {
    pub collections: Vec<String>,
    pub queries: Vec<GrepQueryJson>,
    #[serde(default)]
    pub scope: GrepScopeJson,
    #[serde(default)]
    pub max_hits: Option<u64>,
    #[serde(default)]
    pub events: Option<Vec<GrepEventKindJson>>,
}

/// One query of a grep request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrepQueryJson {
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub case_insensitive: bool,
}

/// Which bytes of a record a grep searches.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepScopeJson {
    Payload,
    Headers,
    #[default]
    All,
}

impl GrepScopeJson {
    pub(crate) fn to_scope(self) -> GrepScope {
        match self {
            GrepScopeJson::Payload => GrepScope::Payload,
            GrepScopeJson::Headers => GrepScope::Headers,
            GrepScopeJson::All => GrepScope::All,
        }
    }
}

/// A line type the client may ask for. `End` is always sent and has no effect.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrepEventKindJson {
    Hit,
    Unreadable,
    Progress,
    End,
}

/// The engine event filter of a request: absent means everything.
pub(crate) fn grep_events(list: Option<&[GrepEventKindJson]>) -> GrepEvents {
    match list {
        None => GrepEvents::ALL,
        Some(l) => GrepEvents {
            hits: l.contains(&GrepEventKindJson::Hit),
            unreadable: l.contains(&GrepEventKindJson::Unreadable),
            progress: l.contains(&GrepEventKindJson::Progress),
        },
    }
}

/// Parses a JSON request body, mapping failures to `invalid_json`.
pub(crate) fn parse_json<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::invalid("invalid_json", e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use masstuffy::{CollectionId, Entry};
    use masstuffy_cdx::Kind;

    use super::*;

    fn codec(s: &str) -> Result<Codec, ApiError> {
        parse_json::<CodecJson>(s.as_bytes())?.to_codec()
    }

    #[test]
    fn codec_json_parse() {
        assert_eq!(codec(r#"{"algo":"none"}"#).unwrap(), Codec::None);
        match codec(r#"{"algo":"zstd","dict_id":32768}"#).unwrap() {
            Codec::Zstd { dict_id, level } => {
                assert_eq!(dict_id.get(), 32768);
                assert_eq!(level, 3);
            }
            Codec::None => panic!(),
        }
        assert_eq!(
            codec(r#"{"algo":"zstd","dict_id":0}"#).unwrap_err().code,
            "invalid_codec"
        );
        assert_eq!(
            codec(r#"{"algo":"zstd","dict_id":5,"level":1000}"#)
                .unwrap_err()
                .code,
            "invalid_codec"
        );
        assert_eq!(
            codec(r#"{"algo":"zstd","dict_id":5,"bogus":1}"#)
                .unwrap_err()
                .code,
            "invalid_json"
        );
        assert_eq!(codec(r#"{"algo":"lz4"}"#).unwrap_err().code, "invalid_json");
        let out = serde_json::to_value(CodecJson::from(Codec::None)).unwrap();
        assert_eq!(out, json!({"algo": "none"}));
        let out = serde_json::to_value(CodecJson::Zstd {
            dict_id: 9,
            level: 2,
        })
        .unwrap();
        assert_eq!(out, json!({"algo": "zstd", "dict_id": 9, "level": 2}));
    }

    #[test]
    fn plain_frame_min_matches_engine() {
        assert_eq!(
            DICTLESS_MIN_CONTENT_SIZE,
            masstuffy_collection::PLAIN_FRAME_MIN + 1
        );
    }

    #[test]
    fn locator_json_fields() {
        let mut at = Located {
            collection: CollectionId::from_u128(1),
            generation: 1,
            entry: Entry {
                kind: Kind::Add,
                segment: 1,
                offset: 0,
                raw_size: 10,
                digest: 0xab,
                date: WarcDate::new(2024, 1, 2, 3, 4, 5, 0).unwrap(),
                warc_type: Cow::Borrowed("response"),
                record_id: Cow::Borrowed("urn:x"),
                url: None,
            },
        };
        let j = serde_json::to_value(locator_json(&at)).unwrap();
        assert_eq!(j["digest"], "00000000000000ab");
        assert_eq!(j["digest"].as_str().unwrap().len(), 16);
        assert!(j["url"].is_null());
        assert_eq!(j["date"], "2024-01-02T03:04:05Z");
        at.entry.url = Some(Cow::Borrowed("http://a/"));
        let j = serde_json::to_value(locator_json(&at)).unwrap();
        assert_eq!(j["url"], "http://a/");
        assert!(j["locator"].as_str().unwrap().starts_with("L1"));
    }

    #[test]
    fn grep_request_parse() {
        let r: GrepRequestJson =
            parse_json(br#"{"collections":["a"],"queries":[{"pattern":"x"}]}"#).unwrap();
        assert_eq!(r.scope, GrepScopeJson::All);
        assert!(r.max_hits.is_none());
        assert!(!r.queries[0].regex && !r.queries[0].case_insensitive);
        assert_eq!(grep_events(r.events.as_deref()), GrepEvents::ALL);
        let r: GrepRequestJson = parse_json(
            br#"{"collections":[],"queries":[],"scope":"payload","max_hits":3,"events":["hit","end"]}"#,
        )
        .unwrap();
        assert_eq!(r.scope.to_scope(), GrepScope::Payload);
        let e = grep_events(r.events.as_deref());
        assert!(e.hits && !e.unreadable && !e.progress);
        assert_eq!(grep_events(Some(&[])), GrepEvents::NONE);
        for bad in [
            &br#"{"collections":[],"queries":[],"bogus":1}"#[..],
            br#"{"collections":[],"queries":[{"pattern":"x","bogus":1}]}"#,
            br#"{"collections":[],"queries":[],"scope":"body"}"#,
            br#"{"collections":[],"queries":[],"events":["hits"]}"#,
            br#"{"collections":[]}"#,
        ] {
            assert_eq!(
                parse_json::<GrepRequestJson>(bad).unwrap_err().code,
                "invalid_json"
            );
        }
    }
}
