//! Test helpers.

use std::borrow::Cow;
use std::convert::Infallible;
use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use http_body::{Body as _, Frame};
use masstuffy::{Entry, Repository};
use masstuffy_cdx::{DEFAULT_MAX_LINE, Kind, digest, encode_entry, encode_header};
use masstuffy_warc::{Record, RecordBuilder, WarcDate};
use serde_json::Value;
use tokio::sync::mpsc;
use tower::ServiceExt;

use crate::options::HttpOptions;
use crate::router::router;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh temporary directory, removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new() -> TestDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let p = std::env::temp_dir().join(format!(
            "masstuffy-http-{}-{}-{nanos}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Fails the test if `f` takes more than 60 s.
pub(crate) async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(60), f)
        .await
        .expect("test timed out")
}

/// A repository in a temp dir with its router.
pub(crate) struct TestApp {
    /// Keeps the directory alive.
    pub _dir: TestDir,
    pub repo: Repository,
    pub router: Router,
}

impl TestApp {
    pub(crate) async fn new() -> TestApp {
        TestApp::with(HttpOptions::default()).await
    }

    pub(crate) async fn with(opts: HttpOptions) -> TestApp {
        let dir = TestDir::new();
        let repo = Repository::init(dir.path().join("repo")).await.unwrap();
        let router = router(repo.clone(), opts, None).unwrap();
        TestApp {
            _dir: dir,
            repo,
            router,
        }
    }
}

/// Collects a response body.
pub(crate) async fn collect(mut body: Body) -> Bytes {
    let mut out = Vec::new();
    while let Some(f) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        if let Ok(d) = f.unwrap().into_data() {
            out.extend_from_slice(&d);
        }
    }
    Bytes::from(out)
}

/// One in-process request.
pub(crate) async fn call(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    (parts.status, parts.headers, collect(body).await)
}

pub(crate) fn json(b: &Bytes) -> Value {
    serde_json::from_slice(b).unwrap_or_else(|e| panic!("not JSON ({e}): {b:?}"))
}

pub(crate) fn req(method: Method, uri: &str, ctype: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    b.body(Body::from(body)).unwrap()
}

pub(crate) async fn get(app: &TestApp, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    call(&app.router, req(Method::GET, uri, None, Vec::new())).await
}

pub(crate) async fn send_json(
    app: &TestApp,
    method: Method,
    uri: &str,
    body: &Value,
) -> (StatusCode, HeaderMap, Bytes) {
    let b = serde_json::to_vec(body).unwrap();
    call(&app.router, req(method, uri, Some("application/json"), b)).await
}

pub(crate) async fn post_json(
    app: &TestApp,
    uri: &str,
    body: &Value,
) -> (StatusCode, HeaderMap, Bytes) {
    send_json(app, Method::POST, uri, body).await
}

pub(crate) async fn post_warc(
    app: &TestApp,
    slug: &str,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Bytes) {
    call(
        &app.router,
        req(
            Method::POST,
            &format!("/v1/collections/{slug}/records"),
            Some("application/warc"),
            body,
        ),
    )
    .await
}

/// Creates a collection (`none` codec) and checks the 201.
pub(crate) async fn create(app: &TestApp, slug: &str) {
    let (s, _, b) = post_json(app, "/v1/collections", &serde_json::json!({"slug": slug})).await;
    assert_eq!(s, StatusCode::CREATED, "{b:?}");
}

/// Puts `recs`, returns the locator JSON objects.
pub(crate) async fn put(app: &TestApp, slug: &str, recs: &[Record]) -> Vec<Value> {
    let (s, _, b) = post_warc(app, slug, warc_bytes(recs)).await;
    assert_eq!(s, StatusCode::CREATED, "{b:?}");
    json(&b)["records"].as_array().unwrap().clone()
}

pub(crate) fn tok(l: &Value) -> String {
    l["locator"].as_str().unwrap().to_owned()
}

pub(crate) fn id_str(i: usize) -> String {
    format!("urn:uuid:00000000-0000-4000-8000-{i:012x}")
}

fn text(i: usize, n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ (i as u64).wrapping_mul(0x1000_0001);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b'a' + u8::try_from(x % 26).unwrap()
        })
        .collect()
}

/// A `resource` record with a fixed id, date (2024-01-01 plus `i` seconds) and URL.
pub(crate) fn record(i: usize, body_len: usize) -> Record {
    record_at(i, body_len, &format!("http://example.com/p/{i}"))
}

pub(crate) fn record_at(i: usize, body_len: usize, url: &str) -> Record {
    let mut b = RecordBuilder::new("resource").unwrap();
    b.record_id(&id_str(i)).unwrap();
    let i32_ = u32::try_from(i).unwrap();
    b.date(
        WarcDate::new(
            2024,
            1,
            1,
            0,
            u8::try_from(i32_ / 60 % 60).unwrap(),
            u8::try_from(i32_ % 60).unwrap(),
            0,
        )
        .unwrap(),
    );
    b.target_uri(url).unwrap();
    b.build(&text(i, body_len)).unwrap()
}

pub(crate) fn warc_bytes(recs: &[Record]) -> Vec<u8> {
    recs.iter()
        .flat_map(|r| r.as_bytes().iter().copied())
        .collect()
}

/// One envelope item: the claims and the stored bytes.
pub(crate) struct Item {
    pub record_id: String,
    pub date: WarcDate,
    pub warc_type: String,
    pub url: Option<String>,
    pub payload: Vec<u8>,
    pub digest: u64,
}

/// An item for `rec` stored as is (a `none` collection).
pub(crate) fn plain_item(rec: &Record) -> Item {
    let v = rec.view();
    Item {
        record_id: v.record_id().unwrap().to_owned(),
        date: v.date().unwrap(),
        warc_type: v.warc_type().to_owned(),
        url: v.target_uri().map(str::to_owned),
        payload: rec.as_bytes().to_vec(),
        digest: digest(rec.as_bytes()),
    }
}

/// A `#masstuffy-cdx/1` envelope.
pub(crate) fn envelope_v1(items: &[Item]) -> Vec<u8> {
    let mut v = Vec::new();
    encode_header(0, &mut v);
    for it in items {
        let e = Entry {
            kind: Kind::Add,
            segment: 0,
            offset: 0,
            raw_size: u64::try_from(it.payload.len()).unwrap(),
            digest: it.digest,
            date: it.date,
            warc_type: Cow::Borrowed(it.warc_type.as_str()),
            record_id: Cow::Borrowed(it.record_id.as_str()),
            url: it.url.as_deref().map(Cow::Borrowed),
        };
        encode_entry(&e, DEFAULT_MAX_LINE, &mut v).unwrap();
        v.extend_from_slice(&it.payload);
    }
    v
}

/// A legacy envelope (no header, no digest).
pub(crate) fn envelope_legacy(items: &[Item]) -> Vec<u8> {
    let mut v = Vec::new();
    for it in items {
        let d = it.date;
        let line = format!(
            "{} {} {} {:04}{:02}{:02}{:02}{:02}{:02} f 0 {}\n",
            it.url.as_deref().unwrap_or("-"),
            it.warc_type,
            it.record_id,
            d.year(),
            d.month(),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            it.payload.len()
        );
        v.extend_from_slice(line.as_bytes());
        v.extend_from_slice(&it.payload);
    }
    v
}

/// A request body fed from a channel: streamed or stalled uploads.
pub(crate) struct ChannelBody {
    rx: mpsc::Receiver<Bytes>,
}

impl ChannelBody {
    pub(crate) fn new(rx: mpsc::Receiver<Bytes>) -> ChannelBody {
        ChannelBody { rx }
    }
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.rx.poll_recv(cx).map(|o| o.map(|b| Ok(Frame::data(b))))
    }
}

/// A streamed request: the sender feeds the body.
pub(crate) fn streamed(
    method: Method,
    uri: &str,
    ctype: &str,
) -> (mpsc::Sender<Bytes>, Request<Body>) {
    let (tx, rx) = mpsc::channel(64);
    let r = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", ctype)
        .body(Body::new(ChannelBody::new(rx)))
        .unwrap();
    (tx, r)
}

/// Polls a job until it is no longer running.
pub(crate) async fn wait_job(app: &TestApp, id: &str) -> Value {
    for _ in 0..30 {
        let (s, _, b) = get(app, &format!("/v1/jobs/{id}?wait=60")).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        let j = json(&b);
        if j["state"] != "running" {
            return j;
        }
    }
    panic!("job {id} did not finish");
}

pub(crate) fn id_of_job(b: &Bytes) -> String {
    json(b)["id"].as_str().unwrap().to_owned()
}
