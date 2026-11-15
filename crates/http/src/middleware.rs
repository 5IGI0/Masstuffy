//! Concurrency limit and request timeout.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::body::PermitBody;
use crate::error::{ApiError, ErrorCode};
use crate::router::AppState;

/// Admits at most `max_concurrent_requests` requests; the permit is held until the response body
/// is dropped, so streaming responses count too. Rejects at once (no queue) with 503 `overloaded`.
pub(crate) async fn concurrency(
    State(st): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let Ok(permit) = Arc::clone(&st.requests).try_acquire_owned() else {
        return ApiError::unavailable("overloaded", "too many concurrent requests", 1)
            .into_response();
    };
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let started = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status();
    let code = resp.extensions().get::<ErrorCode>().map(|c| c.0);
    if status.is_server_error() {
        tracing::warn!(%method, path, status = status.as_u16(), code, "request failed");
    } else {
        tracing::debug!(%method, path, status = status.as_u16(), code, elapsed = ?started.elapsed(), "request");
    }
    let (parts, body) = resp.into_parts();
    Response::from_parts(parts, Body::new(PermitBody::new(body, permit)))
}

/// Bounds the time to produce the response head by `request_timeout`. The message says that the
/// outcome of a mutation is unknown; nothing is retried.
pub(crate) async fn timeout(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    match tokio::time::timeout(st.opts.request_timeout, next.run(req)).await {
        Ok(r) => r,
        Err(_) => ApiError::unavailable(
            "timeout",
            "request timed out; a mutation may still complete",
            1,
        )
        .into_response(),
    }
}
