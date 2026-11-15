//! Lookups, search, scan and export.

use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use masstuffy::{
    DEFAULT_SEARCH_LIMIT, Fetch, Fetched, HostMatch, Located, MAX_SCAN_PAGE, MAX_SEARCH_LIMIT,
    PathFilter, PortFilter, ScanCursor, ScanOptions, SearchCursor, SearchQuery, UrlPattern,
};
use serde_json::json;

use super::{
    H_LOCATOR, H_LOCATOR_OMITTED, H_NEXT_CURSOR, H_RECORD_COUNT, H_RECORD_ID, H_UNREADABLE_COUNT,
    MAX_HEADER_VALUE, WARC, bytes_response, collection, json_response, path_err, set_header,
    set_header_num,
};
use crate::body::ChunksBody;
use crate::error::ApiError;
use crate::json::locator_json;
use crate::locator::encode_locator;
use crate::params::{Params, parse_date};
use crate::router::AppState;

type St = State<Arc<AppState>>;

#[derive(PartialEq, Eq, Clone, Copy)]
enum FetchMode {
    Record,
    Locator,
}

fn fetch_mode(p: &Params) -> Result<FetchMode, ApiError> {
    match p.str("fetch") {
        None | Some("record") => Ok(FetchMode::Record),
        Some("locator") => Ok(FetchMode::Locator),
        Some(_) => Err(ApiError::invalid(
            "invalid_param",
            "fetch: expected record or locator",
        )),
    }
}

async fn maybe_sync(st: &AppState, p: &Params) -> Result<(), ApiError> {
    if p.bool("sync", false)? {
        st.repo.sync_index().await?;
    }
    Ok(())
}

/// A hit as a response: the record bytes with its locator header, or the locator JSON.
fn hit_response(
    mode: FetchMode,
    hit: Option<(Located, Option<Vec<u8>>)>,
) -> Result<Response, ApiError> {
    let Some((at, bytes)) = hit else {
        return Err(ApiError::not_found(
            "record_not_found",
            "no matching record",
        ));
    };
    match (mode, bytes) {
        (FetchMode::Record, Some(bytes)) => {
            let mut r = bytes_response(StatusCode::OK, WARC, bytes);
            let token = encode_locator(&at);
            if token.len() > MAX_HEADER_VALUE || !set_header(&mut r, H_LOCATOR, &token) {
                set_header(&mut r, H_LOCATOR_OMITTED, "too-long");
            }
            set_header(&mut r, H_RECORD_ID, &at.entry.record_id);
            Ok(r)
        }
        _ => json_response(StatusCode::OK, &locator_json(&at)),
    }
}

/// `GET /v1/lookup/by-id`.
pub(crate) async fn by_id(State(st): St, req: Request) -> Result<Response, ApiError> {
    let p = Params::parse(req.uri().query(), &["id", "collection", "fetch", "sync"])?;
    let id = p.required("id")?;
    let mode = fetch_mode(&p)?;
    let coll = match p.str("collection") {
        Some(slug) => Some(collection(&st.repo, slug)?.id()),
        None => None,
    };
    maybe_sync(&st, &p).await?;
    let hit = match mode {
        FetchMode::Locator => st.repo.find_by_id(id, coll).await?.map(|at| (at, None)),
        FetchMode::Record => st
            .repo
            .get_by_id(id, coll)
            .await?
            .map(|(at, b)| (at, Some(b))),
    };
    hit_response(mode, hit)
}

/// `GET /v1/collections/{slug}/lookup/by-url`.
pub(crate) async fn by_url(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let p = Params::parse(req.uri().query(), &["url", "date", "fetch", "sync"])?;
    let url = p.required("url")?;
    let date = p.str("date").map(parse_date).transpose()?;
    let mode = fetch_mode(&p)?;
    maybe_sync(&st, &p).await?;
    let hit = match mode {
        FetchMode::Locator => handle.find_by_url(url, date).await?.map(|at| (at, None)),
        FetchMode::Record => handle
            .get_by_url(url, date)
            .await?
            .map(|(at, b)| (at, Some(b))),
    };
    hit_response(mode, hit)
}

/// `GET /v1/collections/{slug}/search`.
pub(crate) async fn search(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let p = Params::parse(
        req.uri().query(),
        &[
            "host",
            "subdomains",
            "port",
            "path",
            "path_match",
            "limit",
            "cursor",
            "sync",
        ],
    )?;
    let host = p.required("host")?;
    let host_match = if p.bool("subdomains", false)? {
        HostMatch::WithSubdomains
    } else {
        HostMatch::Exact
    };
    let port = match p.str("port") {
        None | Some("any") => PortFilter::Any,
        Some("default") => PortFilter::Default,
        Some(n) => {
            let bad =
                || ApiError::invalid("invalid_param", "port: expected any, default or 1..=65535");
            let v = crate::params::parse_canonical(n).ok_or_else(bad)?;
            match u16::try_from(v) {
                Ok(v) if v >= 1 => PortFilter::Explicit(v),
                _ => return Err(bad()),
            }
        }
    };
    let path = match (p.str("path"), p.str("path_match")) {
        (None, None) => PathFilter::Any,
        (None, Some(_)) => {
            return Err(ApiError::invalid("invalid_param", "path_match needs path"));
        }
        (Some(path), None | Some("prefix")) => PathFilter::Prefix(path),
        (Some(path), Some("exact")) => PathFilter::Exact(path),
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid(
                "invalid_param",
                "path_match: expected prefix or exact",
            ));
        }
    };
    let pattern = UrlPattern::new(host, host_match, port, path)
        .map_err(|e| ApiError::invalid("invalid_param", e.to_string()))?;
    let limit = p.u64_in(
        "limit",
        1..=u64::try_from(MAX_SEARCH_LIMIT).unwrap_or(u64::MAX),
        u64::try_from(DEFAULT_SEARCH_LIMIT).unwrap_or(100),
    )?;
    let after = p.str("cursor").map(SearchCursor::from_token).transpose()?;
    maybe_sync(&st, &p).await?;
    let q = SearchQuery {
        limit: usize::try_from(limit).unwrap_or(DEFAULT_SEARCH_LIMIT),
        after,
    };
    let page = handle.search(&pattern, &q).await?;
    let hits: Vec<_> = page.hits.iter().map(locator_json).collect();
    json_response(
        StatusCode::OK,
        &json!({ "hits": hits, "next": page.next.as_ref().map(SearchCursor::to_token) }),
    )
}

fn scan_cursor(p: &Params) -> Result<Option<ScanCursor>, ApiError> {
    p.str("cursor")
        .map(ScanCursor::from_token)
        .transpose()
        .map_err(ApiError::from)
}

fn page_records(p: &Params, key: &str) -> Result<usize, ApiError> {
    let n = p.u64_in(
        key,
        1..=u64::try_from(MAX_SCAN_PAGE).unwrap_or(u64::MAX),
        1000,
    )?;
    usize::try_from(n).map_err(|_| ApiError::invalid("invalid_param", "page size too large"))
}

/// `GET /v1/collections/{slug}/scan`.
pub(crate) async fn scan(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let p = Params::parse(req.uri().query(), &["cursor", "limit"])?;
    let opts = ScanOptions {
        after: scan_cursor(&p)?,
        page_records: page_records(&p, "limit")?,
        fetch: Fetch::None,
        ..ScanOptions::default()
    };
    let mut scan = handle.scan(opts)?;
    match scan.next_page().await? {
        Some(page) => {
            let items: Vec<_> = page
                .items
                .iter()
                .map(|i| locator_json(&i.located))
                .collect();
            json_response(
                StatusCode::OK,
                &json!({ "items": items, "next": page.next.to_token() }),
            )
        }
        None => json_response(StatusCode::OK, &json!({ "items": [], "next": null })),
    }
}

/// `GET /v1/collections/{slug}/export`.
pub(crate) async fn export(
    State(st): St,
    slug: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(slug) = slug.map_err(path_err)?;
    let handle = collection(&st.repo, &slug)?;
    let p = Params::parse(
        req.uri().query(),
        &["cursor", "max_records", "max_bytes", "on_unreadable"],
    )?;
    let skip = match p.str("on_unreadable") {
        None | Some("fail") => false,
        Some("skip") => true,
        Some(_) => {
            return Err(ApiError::invalid(
                "invalid_param",
                "on_unreadable: expected fail or skip",
            ));
        }
    };
    let cap = st.opts.max_export_page_bytes;
    let page_bytes = p.u64_in("max_bytes", 1..=cap, cap.min(8 << 20))?;
    let opts = ScanOptions {
        after: scan_cursor(&p)?,
        page_records: page_records(&p, "max_records")?,
        page_bytes,
        fetch: Fetch::Decoded,
    };
    let mut scan = handle.scan(opts)?;
    let Some(page) = scan.next_page().await? else {
        return Ok(bytes_response(StatusCode::OK, WARC, Vec::new()));
    };
    let mut chunks: Vec<Bytes> = Vec::with_capacity(page.items.len());
    let mut unreadable: u64 = 0;
    for item in page.items {
        match item.record {
            Some(Ok(Fetched::Decoded(v))) => chunks.push(Bytes::from(v)),
            Some(Err(e)) if skip => {
                tracing::debug!(error = %e, "export skips an unreadable record");
                unreadable = unreadable.saturating_add(1);
            }
            Some(Err(e)) => {
                let cause = ApiError::from(e).object();
                let mut err = ApiError::internal("a record could not be read");
                err.kind = "corrupt";
                err.code = "unreadable_record";
                err.details = Some(json!({
                    "locator": encode_locator(&item.located),
                    "cause": cause,
                }));
                return Err(err);
            }
            other => {
                // No record, or not the decoded form we asked for: never count it silently.
                if skip {
                    unreadable = unreadable.saturating_add(1);
                } else {
                    let mut err = ApiError::internal("a record page item has no decoded record");
                    err.details = Some(json!({
                        "locator": encode_locator(&item.located),
                        "found": if other.is_none() { "none" } else { "raw" },
                    }));
                    return Err(err);
                }
            }
        }
    }
    let count = u64::try_from(chunks.len()).unwrap_or(u64::MAX);
    let mut resp = Response::new(axum::body::Body::new(ChunksBody::new(chunks)));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(WARC),
    );
    set_header_num(&mut resp, H_RECORD_COUNT, count);
    if skip {
        set_header_num(&mut resp, H_UNREADABLE_COUNT, unreadable);
    }
    set_header(&mut resp, H_NEXT_CURSOR, &page.next.to_token());
    Ok(resp)
}
