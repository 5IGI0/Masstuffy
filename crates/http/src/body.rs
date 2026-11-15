//! Request and response body plumbing.
//!
//! Request bodies reach blocking code (the WARC parser, the engine's import producer) through a
//! bounded channel: [`pump_body`] runs on the async side and feeds a [`BodyReader`] that runs on a
//! blocking thread.

use std::convert::Infallible;
use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, header};
use bytes::Bytes;
use http_body::{Body as _, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::error::ApiError;

/// Capacity of the pump channel, in chunks.
pub(crate) const PUMP_CHANNEL: usize = 8;

/// What ended a pumped request body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PumpEnd {
    Eof,
    TooLarge,
    IdleTimeout,
    BodyError(String),
    ReceiverGone,
}

async fn next_frame(body: &mut Body) -> Option<Result<Frame<Bytes>, axum::Error>> {
    std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
}

/// Pulls frames from `body` and forwards the data chunks into `tx`. Stops at:
///
/// - EOF: drops `tx`;
/// - more than `limit` bytes in total: sends `Err(other("body too large"))`;
/// - no frame for `idle`: sends `Err(TimedOut)`;
/// - a body error: sends `Err(other(..))`;
/// - `tx` closed (the consumer stopped): returns `ReceiverGone`.
///
/// Trailers are ignored.
pub(crate) async fn pump_body(
    mut body: Body,
    tx: mpsc::Sender<io::Result<Bytes>>,
    limit: u64,
    idle: Duration,
) -> PumpEnd {
    let mut total: u64 = 0;
    loop {
        let frame = match tokio::time::timeout(idle, next_frame(&mut body)).await {
            Err(_) => {
                let _ = tx
                    .send(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "body idle timeout",
                    )))
                    .await;
                return PumpEnd::IdleTimeout;
            }
            Ok(None) => return PumpEnd::Eof,
            Ok(Some(Err(e))) => {
                let msg = e.to_string();
                let _ = tx.send(Err(io::Error::other(msg.clone()))).await;
                return PumpEnd::BodyError(msg);
            }
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        total = match u64::try_from(data.len())
            .ok()
            .and_then(|n| total.checked_add(n))
        {
            Some(t) if t <= limit => t,
            _ => {
                let _ = tx.send(Err(io::Error::other("body too large"))).await;
                return PumpEnd::TooLarge;
            }
        };
        if data.is_empty() {
            continue;
        }
        if tx.send(Ok(data)).await.is_err() {
            return PumpEnd::ReceiverGone;
        }
    }
}

/// Blocking `Read` over the chunks of [`pump_body`].
///
/// Only for blocking threads (`spawn_blocking` or the engine's import producer): `blocking_recv`
/// panics on a runtime worker.
pub(crate) struct BodyReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    cur: Bytes,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                None => return Ok(0),
                Some(Err(e)) => return Err(e),
                Some(Ok(b)) => self.cur = b,
            }
        }
        let n = buf.len().min(self.cur.len());
        let chunk = self.cur.split_to(n);
        match buf.get_mut(..n) {
            Some(dst) => dst.copy_from_slice(&chunk),
            None => return Err(io::Error::other("buffer shorter than its own length")),
        }
        Ok(n)
    }
}

/// A pump sender and its blocking reader.
pub(crate) fn body_channel() -> (mpsc::Sender<io::Result<Bytes>>, BodyReader) {
    let (tx, rx) = mpsc::channel(PUMP_CHANNEL);
    (
        tx,
        BodyReader {
            rx,
            cur: Bytes::new(),
        },
    )
}

/// Collects a small body into memory, at most `limit` bytes.
///
/// # Errors
///
/// 413 `payload_too_large` past `limit`; 408 `body_timeout` when no frame arrives for `idle`;
/// 400 `body_read` if the stream fails.
pub(crate) async fn collect_limited(
    mut body: Body,
    limit: usize,
    idle: Duration,
) -> Result<Vec<u8>, ApiError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let frame = match tokio::time::timeout(idle, next_frame(&mut body)).await {
            Err(_) => return Err(ApiError::body_timeout()),
            Ok(None) => return Ok(out),
            Ok(Some(Err(e))) => return Err(ApiError::body_read(e.to_string())),
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        if out.len().checked_add(data.len()).is_none_or(|n| n > limit) {
            return Err(ApiError::too_large(format!(
                "request body exceeds {limit} bytes"
            )));
        }
        out.extend_from_slice(&data);
    }
}

/// Response body over already-read chunks: one frame per chunk, exact `size_hint`.
pub(crate) struct ChunksBody {
    chunks: std::vec::IntoIter<Bytes>,
    remaining: u64,
}

impl ChunksBody {
    pub(crate) fn new(chunks: Vec<Bytes>) -> ChunksBody {
        let remaining = chunks.iter().fold(0u64, |a, c| {
            a.saturating_add(u64::try_from(c.len()).unwrap_or(u64::MAX))
        });
        ChunksBody {
            chunks: chunks.into_iter(),
            remaining,
        }
    }
}

impl http_body::Body for ChunksBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = &mut *self;
        match this.chunks.next() {
            Some(c) => {
                this.remaining = this
                    .remaining
                    .saturating_sub(u64::try_from(c.len()).unwrap_or(u64::MAX));
                Poll::Ready(Some(Ok(Frame::data(c))))
            }
            None => Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.chunks.len() == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Wraps a response body and keeps `permit` until the body is dropped.
pub(crate) struct PermitBody {
    inner: Body,
    _permit: OwnedSemaphorePermit,
}

impl PermitBody {
    pub(crate) fn new(inner: Body, permit: OwnedSemaphorePermit) -> PermitBody {
        PermitBody {
            inner,
            _permit: permit,
        }
    }
}

impl http_body::Body for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Media type of `Content-Type` (before `;`, trimmed), if present and readable.
fn media_type(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    Some(v.split(';').next().unwrap_or(v).trim())
}

/// Requires `Content-Type: <expected>` (ASCII case-insensitive, parameters ignored).
///
/// # Errors
///
/// 415 `unsupported_media_type`.
pub(crate) fn require_content_type(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    match media_type(headers) {
        Some(m) if m.eq_ignore_ascii_case(expected) => Ok(()),
        _ => Err(ApiError::unsupported_media(format!(
            "Content-Type must be {expected}"
        ))),
    }
}

/// Rejects a `Content-Encoding` other than `identity`.
///
/// # Errors
///
/// 415 `unsupported_media_type`.
pub(crate) fn require_identity_encoding(headers: &HeaderMap) -> Result<(), ApiError> {
    for v in headers.get_all(header::CONTENT_ENCODING) {
        let ok = v
            .to_str()
            .is_ok_and(|s| s.trim().eq_ignore_ascii_case("identity"));
        if !ok {
            return Err(ApiError::unsupported_media(
                "Content-Encoding other than identity is not supported",
            ));
        }
    }
    Ok(())
}

/// Rejects a declared `Content-Length` above `cap` before any byte is read.
///
/// # Errors
///
/// 413 `payload_too_large`; 400 `invalid_param` for an unparseable value.
pub(crate) fn check_content_length(headers: &HeaderMap, cap: u64) -> Result<(), ApiError> {
    let Some(v) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(());
    };
    let n: u64 = v
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| ApiError::invalid("invalid_param", "invalid Content-Length"))?;
    if n > cap {
        return Err(ApiError::too_large(format!(
            "request body of {n} bytes exceeds the {cap} byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    use super::*;
    use crate::testutil::{ChannelBody, bounded};

    fn body_of(chunks: &[&[u8]]) -> (Body, usize) {
        let (tx, rx) = mpsc::channel(chunks.len().max(1));
        for c in chunks {
            tx.try_send(Bytes::copy_from_slice(c)).unwrap();
        }
        drop(tx);
        (Body::new(ChannelBody::new(rx)), chunks.len())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn body_reader_reads_across_chunks() {
        bounded(async {
            let (tx, mut reader) = body_channel();
            let data: Vec<u8> = (0..14u8).collect();
            let (body, _) = body_of(&[&data[..1], &data[1..4], &data[4..14]]);
            let pump = tokio::spawn(pump_body(body, tx, 1000, Duration::from_secs(5)));
            let got = tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                let mut buf = [0u8; 4];
                loop {
                    let n = reader.read(&mut buf).unwrap();
                    if n == 0 {
                        return out;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
            })
            .await
            .unwrap();
            assert_eq!(got, data);
            assert_eq!(pump.await.unwrap(), PumpEnd::Eof);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn body_reader_propagates_error_then_eof() {
        bounded(async {
            let (tx, mut reader) = body_channel();
            tx.send(Ok(Bytes::from_static(b"ab"))).await.unwrap();
            tx.send(Err(io::Error::other("boom"))).await.unwrap();
            drop(tx);
            tokio::task::spawn_blocking(move || {
                let mut buf = [0u8; 8];
                assert_eq!(reader.read(&mut buf).unwrap(), 2);
                assert!(reader.read(&mut buf).is_err());
                assert_eq!(reader.read(&mut buf).unwrap(), 0);
            })
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pump_too_large_and_reader_gets_err() {
        bounded(async {
            let (tx, mut reader) = body_channel();
            let (body, _) = body_of(&[b"123456", b"78901"]);
            let end = pump_body(body, tx, 10, Duration::from_secs(5)).await;
            assert_eq!(end, PumpEnd::TooLarge);
            tokio::task::spawn_blocking(move || {
                let mut buf = [0u8; 16];
                assert_eq!(reader.read(&mut buf).unwrap(), 6);
                assert!(reader.read(&mut buf).is_err());
            })
            .await
            .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pump_idle_timeout() {
        bounded(async {
            let (btx, brx) = mpsc::channel::<Bytes>(1);
            let (tx, _reader) = body_channel();
            let end = pump_body(
                Body::new(ChannelBody::new(brx)),
                tx,
                100,
                Duration::from_millis(200),
            )
            .await;
            assert_eq!(end, PumpEnd::IdleTimeout);
            drop(btx);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pump_receiver_gone() {
        bounded(async {
            let (btx, brx) = mpsc::channel::<Bytes>(64);
            for _ in 0..32 {
                btx.send(Bytes::from_static(b"x")).await.unwrap();
            }
            let (tx, reader) = body_channel();
            drop(reader);
            let end = pump_body(
                Body::new(ChannelBody::new(brx)),
                tx,
                1000,
                Duration::from_secs(5),
            )
            .await;
            assert_eq!(end, PumpEnd::ReceiverGone);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn collect_limited_ok_and_413() {
        bounded(async {
            let idle = Duration::from_secs(5);
            let (b, _) = body_of(&[b"ab", b"cd"]);
            assert_eq!(collect_limited(b, 4, idle).await.unwrap(), b"abcd");
            let (b, _) = body_of(&[b"ab", b"cde"]);
            let e = collect_limited(b, 4, idle).await.unwrap_err();
            assert_eq!((e.status.as_u16(), e.code), (413, "payload_too_large"));
            let (_btx, brx) = mpsc::channel::<Bytes>(1);
            let e = collect_limited(
                Body::new(ChannelBody::new(brx)),
                4,
                Duration::from_millis(100),
            )
            .await
            .unwrap_err();
            assert_eq!((e.status.as_u16(), e.code), (408, "body_timeout"));
        })
        .await;
    }

    #[test]
    fn chunks_body_yields_all_with_exact_size_hint() {
        use http_body::Body as _;
        let body = ChunksBody::new(vec![Bytes::from_static(b"abc"), Bytes::from_static(b"de")]);
        assert_eq!(body.size_hint().exact(), Some(5));
        assert!(!body.is_end_stream());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut body = Body::new(body);
        let got = rt.block_on(async {
            let mut out = Vec::new();
            while let Some(f) = next_frame(&mut body).await {
                out.push(f.unwrap().into_data().unwrap());
            }
            out
        });
        assert_eq!(
            got,
            vec![Bytes::from_static(b"abc"), Bytes::from_static(b"de")]
        );
    }

    #[test]
    fn permit_body_releases_on_drop() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&sem).try_acquire_owned().unwrap();
        let body = PermitBody::new(Body::empty(), permit);
        assert_eq!(sem.available_permits(), 0);
        drop(body);
        assert_eq!(sem.available_permits(), 1);
    }

    #[test]
    fn header_checks() {
        use axum::http::HeaderValue;
        let mut h = HeaderMap::new();
        assert!(require_content_type(&h, "application/warc").is_err());
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(" Application/WARC ; x=y"),
        );
        assert!(require_content_type(&h, "application/warc").is_ok());
        assert!(require_identity_encoding(&h).is_ok());
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        assert_eq!(
            require_identity_encoding(&h).unwrap_err().status.as_u16(),
            415
        );
        h.insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        assert!(require_identity_encoding(&h).is_ok());
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("10"));
        assert!(check_content_length(&h, 10).is_ok());
        assert_eq!(
            check_content_length(&h, 9).unwrap_err().status.as_u16(),
            413
        );
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("x"));
        assert_eq!(
            check_content_length(&h, 9).unwrap_err().status.as_u16(),
            400
        );
    }
}
