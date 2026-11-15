//! Router construction.

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use masstuffy::{OpenReport, Repository};
use tokio::sync::Semaphore;

use crate::error::ApiError;
use crate::handlers::{admin, collections, dicts, query, records};
use crate::jobs::Jobs;
use crate::json::open_report_json;
use crate::middleware;
use crate::options::{HttpOptions, OptionsError};

/// Shared state of every handler. Immutable; the only mutable parts are inside `Jobs` and the
/// semaphores.
pub(crate) struct AppState {
    pub repo: Repository,
    pub opts: HttpOptions,
    pub jobs: Arc<Jobs>,
    pub requests: Arc<Semaphore>,
    pub open_report: Option<serde_json::Value>,
}

/// Builds the API router (every route of `docs/http-api.md`). The router holds a clone of `repo`;
/// the caller still owns the shutdown (`Repository::shutdown`).
///
/// # Errors
///
/// `OptionsError` if `opts` is invalid.
pub fn router(
    repo: Repository,
    opts: HttpOptions,
    report: Option<&OpenReport>,
) -> Result<Router, OptionsError> {
    opts.validate()?;
    let st = Arc::new(AppState {
        jobs: Jobs::new(opts.max_running_jobs, opts.max_retained_jobs),
        requests: Arc::new(Semaphore::new(opts.max_concurrent_requests)),
        open_report: report.map(open_report_json),
        repo,
        opts,
    });
    let timed: Router<Arc<AppState>> = Router::new()
        .route("/v1/status", get(collections::status))
        .route(
            "/v1/collections",
            get(collections::list).post(collections::create),
        )
        .route(
            "/v1/collections/{slug}",
            get(collections::get_one)
                .patch(collections::rename)
                .delete(collections::delete),
        )
        .route(
            "/v1/collections/{slug}/records/{locator}",
            get(records::get_record).delete(records::delete_one),
        )
        .route(
            "/v1/collections/{slug}/records/{locator}/raw",
            get(records::get_raw),
        )
        .route(
            "/v1/collections/{slug}/records/delete",
            post(records::delete_many),
        )
        .route(
            "/v1/collections/{slug}/records/delete-by-id",
            post(records::delete_by_id),
        )
        .route("/v1/lookup/by-id", get(query::by_id))
        .route("/v1/collections/{slug}/lookup/by-url", get(query::by_url))
        .route("/v1/collections/{slug}/search", get(query::search))
        .route("/v1/collections/{slug}/scan", get(query::scan))
        .route("/v1/collections/{slug}/export", get(query::export))
        .route("/v1/collections/{slug}/rebuild", post(admin::rebuild))
        .route("/v1/collections/{slug}/train", post(admin::train))
        .route("/v1/index/rebuild", post(admin::index_rebuild))
        .route("/v1/index/sync", post(admin::index_sync))
        .route("/v1/jobs", get(admin::jobs_list))
        .route("/v1/dictionaries", get(dicts::list))
        .route("/v1/dictionaries/{dict_id}", get(dicts::download))
        .layer(from_fn_with_state(Arc::clone(&st), middleware::timeout));
    let untimed: Router<Arc<AppState>> = Router::new()
        .route("/v1/collections/{slug}/records", post(records::put))
        .route("/v1/collections/{slug}/import", post(records::import))
        .route("/v1/jobs/{id}", get(admin::job_get));
    // `/healthz` is outside the concurrency limit (and the timeout): a liveness probe must answer
    // under load.
    let health: Router<Arc<AppState>> = Router::new()
        .route("/healthz", get(collections::healthz))
        .method_not_allowed_fallback(method_not_allowed);
    Ok(timed
        .merge(untimed)
        .fallback(no_route)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(from_fn_with_state(Arc::clone(&st), middleware::concurrency))
        .merge(health)
        .with_state(st))
}

async fn no_route() -> axum::response::Response {
    ApiError::not_found("no_route", "no such route").into_response()
}

async fn method_not_allowed() -> axum::response::Response {
    ApiError::method_not_allowed().into_response()
}
