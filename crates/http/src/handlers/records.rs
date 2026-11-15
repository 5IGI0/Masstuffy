//! Record routes: put, get, raw get, deletes, import.

use std::io::BufReader;
use std::sync::Arc;

use axum::extract::RawQuery;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use masstuffy::{Error, ImportOptions, MAX_IMPORT_BATCH_RECORDS, OnInvalid, RawEncoding, Record};
use masstuffy_warc::{Limits, WarcReader};
use serde_json::json;

use super::{
    H_CONTENT_SIZE, H_DICT_ID, H_DIGEST, H_ENCODING, H_RAW_SIZE, H_RECORD_ID, WARC, bytes_response,
    collection, json_response, locator_for, path_err, set_header, set_header_num,
};
use crate::body::{
    BodyReader, PumpEnd, body_channel, check_content_length, collect_limited, pump_body,
    require_content_type, require_identity_encoding,
};
use crate::error::ApiError;
use crate::json::{
    DeleteByIdJson, DeleteLocatorsJson, import_report_json, locator_json, parse_json,
};
use crate::locator::decode_locator;
use crate::params::Params;
use crate::router::AppState;

type St = State<Arc<AppState>>;

/// Why the request body could not become records.
enum ParseFail {
    Warc {
        record_index: usize,
        offset: u64,
        message: String,
    },
    TooMany,
}

fn parse_records(
    reader: BodyReader,
    limits: Limits,
    max_records: usize,
) -> Result<Vec<Record>, ParseFail> {
    let mut r = WarcReader::with_limits(BufReader::with_capacity(64 * 1024, reader), limits);
    // No reservation from untrusted counts: the vector grows with the records that arrive.
    let mut out: Vec<Record> = Vec::new();
    loop {
        let offset = r.offset();
        match r.next_record() {
            Ok(None) => return Ok(out),
            Ok(Some(rec)) => {
                if out.len() >= max_records {
                    return Err(ParseFail::TooMany);
                }
                out.push(Record::from(rec));
            }
            Err(e) => {
                return Err(ParseFail::Warc {
                    record_index: out.len(),
                    offset,
                    message: e.to_string(),
                });
            }
        }
    }
}

fn pump_error(end: &PumpEnd) -> Option<ApiError> {
    match end {
        PumpEnd::TooLarge => Some(ApiError::too_large("request body exceeds its size limit")),
        PumpEnd::IdleTimeout => Some(ApiError::body_timeout()),
        PumpEnd::BodyError(m) => Some(ApiError::body_read(m.clone())),
        PumpEnd::Eof | PumpEnd::ReceiverGone => None,
    }
}

/// `POST /v1/collections/{slug}/records`.
pub(crate) async fn put(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let Path(slug) = slug.map_err(path_err)?;
    // Resolve first: a 404 never consumes an upload.
    let handle = collection(&st.repo, &slug)?;
    let (parts, body) = req.into_parts();
    require_content_type(&parts.headers, WARC)?;
    require_identity_encoding(&parts.headers)?;
    let cap = st.opts.max_put_body_bytes;
    check_content_length(&parts.headers, cap)?;
    let limits = Limits {
        max_header_bytes: 64 * 1024,
        max_headers: 256,
        max_body_len: st.repo.config().max_record_bytes.min(cap),
    };
    let max_records = st.opts.max_put_records;
    let (tx, reader) = body_channel();
    // The reader blocks: it lives on a blocking thread, never on a runtime worker.
    let parse = tokio::task::spawn_blocking(move || parse_records(reader, limits, max_records));
    let pump = pump_body(body, tx, cap, st.opts.body_idle_timeout);
    let (end, parsed) = tokio::join!(pump, parse);
    if let Some(e) = pump_error(&end) {
        return Err(e);
    }
    let records = match parsed {
        Err(e) => return Err(ApiError::internal(format!("parse task failed: {e}"))),
        Ok(Err(ParseFail::Warc {
            record_index,
            offset,
            message,
        })) => {
            return Err(ApiError::invalid("invalid_warc", message)
                .with_details(json!({"record_index": record_index, "offset": offset})));
        }
        Ok(Err(ParseFail::TooMany)) => {
            let mut e =
                ApiError::too_large(format!("more than {max_records} records in one request"));
            e.code = "too_many_records";
            return Err(e);
        }
        Ok(Ok(v)) => v,
    };
    if records.is_empty() {
        return Err(ApiError::invalid(
            "empty_body",
            "the request holds no WARC record",
        ));
    }
    let located = handle.put(records).await?;
    let items: Vec<_> = located.iter().map(locator_json).collect();
    json_response(StatusCode::CREATED, &json!({ "records": items }))
}

/// `GET /v1/collections/{slug}/records/{locator}`.
pub(crate) async fn get_record(
    State(st): St,
    p: Result<Path<(String, String)>, PathRejection>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let Path((slug, token)) = p.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let at = locator_for(&handle, &token)?;
    let bytes = st.repo.get(&at).await?;
    let mut r = bytes_response(StatusCode::OK, WARC, bytes);
    set_header(&mut r, H_RECORD_ID, &at.entry.record_id);
    Ok(r)
}

/// `GET /v1/collections/{slug}/records/{locator}/raw`.
pub(crate) async fn get_raw(
    State(st): St,
    p: Result<Path<(String, String)>, PathRejection>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let Path((slug, token)) = p.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let at = locator_for(&handle, &token)?;
    let raw = st.repo.get_raw(&at).await?;
    let size = u64::try_from(raw.bytes.len()).unwrap_or(u64::MAX);
    // Never `Content-Encoding`: a generic client would try to decompress without the dictionary.
    let mut r = match raw.encoding {
        RawEncoding::Warc => {
            let mut r = bytes_response(StatusCode::OK, WARC, raw.bytes);
            set_header(&mut r, H_ENCODING, "warc");
            r
        }
        RawEncoding::Zstd { dict_id } => {
            let content = masstuffy_zframe::inspect_frame(&raw.bytes)
                .ok()
                .and_then(|i| i.content_size);
            let mut r = bytes_response(StatusCode::OK, "application/zstd", raw.bytes);
            set_header(&mut r, H_ENCODING, "zstd");
            set_header_num(&mut r, H_DICT_ID, u64::from(dict_id));
            if let Some(c) = content {
                set_header_num(&mut r, H_CONTENT_SIZE, c);
            }
            r
        }
    };
    set_header_num(&mut r, H_RAW_SIZE, size);
    // The engine verified the stored bytes against the entry digest before returning them.
    if let Ok(v) = HeaderValue::from_str(&format!("{:016x}", at.entry.digest)) {
        r.headers_mut().insert(H_DIGEST, v);
    }
    Ok(r)
}

/// `DELETE /v1/collections/{slug}/records/{locator}`.
pub(crate) async fn delete_one(
    State(st): St,
    p: Result<Path<(String, String)>, PathRejection>,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let Path((slug, token)) = p.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let at = locator_for(&handle, &token)?;
    let n = handle.delete(vec![at]).await?;
    json_response(StatusCode::OK, &json!({ "deleted": n }))
}

/// `POST /v1/collections/{slug}/records/delete`.
pub(crate) async fn delete_many(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let body = collect_limited(
        req.into_body(),
        st.opts.max_json_body_bytes,
        st.opts.body_idle_timeout,
    )
    .await?;
    let r: DeleteLocatorsJson = parse_json(&body)?;
    if r.locators.len() > masstuffy::MAX_DELETE_TARGETS {
        return Err(ApiError::invalid(
            "invalid_param",
            format!("more than {} locators", masstuffy::MAX_DELETE_TARGETS),
        ));
    }
    let mut targets = Vec::with_capacity(r.locators.len());
    for (index, t) in r.locators.iter().enumerate() {
        let at = decode_locator(t).map_err(|e| {
            ApiError::invalid("invalid_locator", e.to_string())
                .with_details(json!({"index": index}))
        })?;
        if at.collection != handle.id() {
            return Err(ApiError::invalid(
                "locator_collection_mismatch",
                "locator belongs to another collection",
            )
            .with_details(json!({"index": index})));
        }
        targets.push(at);
    }
    let n = handle.delete(targets).await?;
    json_response(StatusCode::OK, &json!({ "deleted": n }))
}

/// `POST /v1/collections/{slug}/records/delete-by-id`.
pub(crate) async fn delete_by_id(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    super::no_params(req.uri().query())?;
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let body = collect_limited(
        req.into_body(),
        st.opts.max_json_body_bytes,
        st.opts.body_idle_timeout,
    )
    .await?;
    let r: DeleteByIdJson = parse_json(&body)?;
    match handle.delete_by_id(&r.record_id).await? {
        Some(at) => json_response(StatusCode::OK, &json!({ "deleted": locator_json(&at) })),
        None => Err(ApiError::not_found(
            "record_not_found",
            "no visible record with this id",
        )),
    }
}

/// `POST /v1/collections/{slug}/import`.
pub(crate) async fn import(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let (parts, body) = req.into_parts();
    require_content_type(&parts.headers, "application/x-masstuffy-envelope")?;
    require_identity_encoding(&parts.headers)?;
    let params = Params::parse(
        parts.uri.query(),
        &["on_invalid", "batch_records", "batch_bytes"],
    )?;
    let on_invalid = match params.str("on_invalid") {
        None | Some("abort") => OnInvalid::Abort,
        Some("skip") => OnInvalid::Skip,
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_param",
                "on_invalid: expected abort or skip",
            ));
        }
    };
    let batch_records = params.u64_in(
        "batch_records",
        1..=u64::try_from(MAX_IMPORT_BATCH_RECORDS).unwrap_or(u64::MAX),
        4096,
    )?;
    let batch_bytes = match params.str("batch_bytes") {
        None => None,
        Some(_) => Some(params.u64_in("batch_bytes", 1..=u64::MAX, 1)?),
    };
    let opts = ImportOptions {
        on_invalid,
        batch_records: usize::try_from(batch_records)
            .map_err(|_| ApiError::invalid("invalid_param", "batch_records: too large"))?,
        batch_bytes,
    };
    let cap = st.opts.max_import_body_bytes.unwrap_or(u64::MAX);
    check_content_length(&parts.headers, cap)?;
    let (tx, reader) = body_channel();
    // `put_raw_envelope` consumes the reader on its own blocking thread.
    let (end, res) = tokio::join!(
        pump_body(body, tx, cap, st.opts.body_idle_timeout),
        handle.put_raw_envelope(reader, opts)
    );
    match (pump_error(&end), res) {
        (Some(mut e), Err(Error::Import { report, source })) => {
            // The engine's own abort cause (usually the I/O error the pump injected).
            let mut cause = ApiError::from(&*source).object();
            if let Some(m) = cause.as_object_mut() {
                m.remove("details");
            }
            e = e
                .with_detail("report", import_report_json(&report))
                .with_detail("cause", cause);
            Err(e)
        }
        (Some(e), Ok(_)) => Err(e),
        (_, Ok(report)) => json_response(StatusCode::OK, &import_report_json(&report)),
        (_, Err(e)) => Err(e.into()),
    }
}
