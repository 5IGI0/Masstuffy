//! Rebuild, train, index maintenance and jobs.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::RawQuery;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use masstuffy::{RebuildOptions, TrainOptions, Unreadable};
use serde_json::json;

use super::{collection, json_response, path_err};
use crate::body::collect_limited;
use crate::error::ApiError;
use crate::jobs::JobKind;
use crate::json::{
    JobJson, OnUnreadableJson, RebuildRequestJson, TrainOptionsJson, parse_json,
    rebuild_report_json, train_report_json,
};
use crate::params::{Params, parse_canonical};
use crate::router::AppState;

type St = State<Arc<AppState>>;

fn accepted(job: &JobJson) -> Result<Response, ApiError> {
    let mut r = json_response(StatusCode::ACCEPTED, job)?;
    if let Ok(v) = HeaderValue::from_str(&format!("/v1/jobs/{}", job.id)) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    Ok(r)
}

/// `POST /v1/collections/{slug}/rebuild`.
pub(crate) async fn rebuild(
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
    let r: RebuildRequestJson = parse_json(&body)?;
    let codec = r.codec.to_codec()?;
    let workers = match r.workers {
        None => None,
        Some(n) => Some(
            NonZeroUsize::new(n)
                .ok_or_else(|| ApiError::invalid("invalid_param", "workers must be non-zero"))?,
        ),
    };
    let opts = RebuildOptions {
        recompress: r.recompress,
        on_unreadable: match r.on_unreadable {
            OnUnreadableJson::Fail => Unreadable::Fail,
            OnUnreadableJson::Skip => Unreadable::Skip,
        },
        workers,
        max_inflight_bytes: r.max_inflight_bytes,
    };
    let job = st.jobs.spawn(
        JobKind::Rebuild,
        Some((handle.slug(), handle.id())),
        async move {
            handle
                .rebuild(codec, opts)
                .await
                .map(|r| rebuild_report_json(&r))
        },
    )?;
    accepted(&job)
}

/// `POST /v1/collections/{slug}/train`.
pub(crate) async fn train(
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
    let r: TrainOptionsJson = if body.is_empty() {
        TrainOptionsJson::default()
    } else {
        parse_json(&body)?
    };
    let d = TrainOptions::default();
    let opts = TrainOptions {
        max_samples: r.max_samples.unwrap_or(d.max_samples),
        max_sample_bytes: r.max_sample_bytes.unwrap_or(d.max_sample_bytes),
        max_total_bytes: r.max_total_bytes.unwrap_or(d.max_total_bytes),
        max_dict_size: r.max_dict_size.unwrap_or(d.max_dict_size),
    };
    let job = st.jobs.spawn(
        JobKind::Train,
        Some((handle.slug(), handle.id())),
        async move {
            handle
                .train_dictionary(opts)
                .await
                .map(|r| train_report_json(&r))
        },
    )?;
    accepted(&job)
}

/// `POST /v1/index/rebuild`.
pub(crate) async fn index_rebuild(
    State(st): St,
    RawQuery(q): RawQuery,
) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    let repo = st.repo.clone();
    let job = st.jobs.spawn(JobKind::RebuildIndex, None, async move {
        repo.rebuild_index().await.map(|()| json!({}))
    })?;
    accepted(&job)
}

/// `POST /v1/index/sync`.
pub(crate) async fn index_sync(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    st.repo.sync_index().await?;
    let mut r = Response::new(axum::body::Body::empty());
    *r.status_mut() = StatusCode::NO_CONTENT;
    Ok(r)
}

/// `GET /v1/jobs`.
pub(crate) async fn jobs_list(State(st): St, RawQuery(q): RawQuery) -> Result<Response, ApiError> {
    super::no_params(q.as_deref())?;
    json_response(StatusCode::OK, &json!({ "jobs": st.jobs.list() }))
}

/// `GET /v1/jobs/{id}?wait=<seconds>`.
pub(crate) async fn job_get(
    State(st): St,
    id: Result<Path<String>, PathRejection>,
    req: Request,
) -> Result<Response, ApiError> {
    let Path(id) = id.map_err(path_err)?;
    let p = Params::parse(req.uri().query(), &["wait"])?;
    let wait = p.u64_in("wait", 0..=st.opts.max_job_wait.as_secs(), 0)?;
    let id = parse_canonical(&id)
        .ok_or_else(|| ApiError::invalid("invalid_param", "malformed job id"))?;
    let (job, mut done) = st
        .jobs
        .get(id)
        .ok_or_else(|| ApiError::not_found("job_not_found", "no such job"))?;
    if job.state == "running" && wait > 0 {
        // The job's own bound: `wait <= max_job_wait`.
        let _ = tokio::time::timeout(Duration::from_secs(wait), done.wait_for(|d| *d)).await;
        let job = st.jobs.get(id).map_or(job, |(j, _)| j);
        return json_response(StatusCode::OK, &job);
    }
    json_response(StatusCode::OK, &job)
}
