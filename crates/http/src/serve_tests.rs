//! Real-listener tests: sockets, disconnects, drain, connection cap.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::options::ServeOptions;
use crate::serve::{Stop, serve};
use crate::testutil::*;

struct Srv {
    addr: SocketAddr,
    stop: watch::Sender<Stop>,
    handle: JoinHandle<()>,
    app: TestApp,
}

async fn spawn_server(opts: ServeOptions) -> Srv {
    let app = TestApp::new().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, rx) = watch::channel(Stop::Run);
    let router = app.router.clone();
    let handle = tokio::spawn(async move {
        serve(listener, router, opts, rx).await.unwrap();
    });
    Srv {
        addr,
        stop,
        handle,
        app,
    }
}

/// One request on a fresh connection, through the hyper client.
async fn send(addr: SocketAddr, r: Request<Body>) -> (StatusCode, Bytes) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut tx, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let resp = tx.send_request(r).await.unwrap();
    let (parts, body) = resp.into_parts();
    let mut out = Vec::new();
    let mut body = std::pin::pin!(body);
    while let Some(f) = std::future::poll_fn(|cx| {
        use hyper::body::Body as _;
        body.as_mut().poll_frame(cx)
    })
    .await
    {
        if let Ok(d) = f.unwrap().into_data() {
            out.extend_from_slice(&d);
        }
    }
    (parts.status, Bytes::from(out))
}

fn rq(method: Method, uri: &str, ctype: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "x");
    if let Some(c) = ctype {
        b = b.header("content-type", c);
    }
    b.body(Body::from(body)).unwrap()
}

async fn gget(addr: SocketAddr, uri: &str) -> (StatusCode, Value) {
    let (s, b) = send(addr, rq(Method::GET, uri, None, Vec::new())).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn count_scan(addr: SocketAddr, slug: &str) -> usize {
    let (s, v) = gget(addr, &format!("/v1/collections/{slug}/scan?limit=1000")).await;
    assert_eq!(s, StatusCode::OK);
    v["items"].as_array().unwrap().len()
}

fn opts() -> ServeOptions {
    ServeOptions::default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_basic_get() {
    bounded(async {
        let srv = spawn_server(opts()).await;
        let (s, v) = gget(srv.addr, "/healthz").await;
        assert_eq!((s, v), (StatusCode::OK, json!({"status": "ok"})));
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_put_and_get_over_tcp() {
    bounded(async {
        let srv = spawn_server(opts()).await;
        let (s, b) = send(
            srv.addr,
            rq(
                Method::POST,
                "/v1/collections",
                Some("application/json"),
                br#"{"slug":"c"}"#.to_vec(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b:?}");
        let rec = record(1, 5000);
        let (s, b) = send(
            srv.addr,
            rq(
                Method::POST,
                "/v1/collections/c/records",
                Some("application/warc"),
                rec.as_bytes().to_vec(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b:?}");
        let tokn = serde_json::from_slice::<Value>(&b).unwrap()["records"][0]["locator"]
            .as_str()
            .unwrap()
            .to_owned();
        let (s, b) = send(
            srv.addr,
            rq(
                Method::GET,
                &format!("/v1/collections/c/records/{tokn}"),
                None,
                vec![],
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], rec.as_bytes());
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

fn chunked(data: &[u8]) -> Vec<u8> {
    let mut v = format!("{:x}\r\n", data.len()).into_bytes();
    v.extend_from_slice(data);
    v.extend_from_slice(b"\r\n");
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_mid_import_keeps_prefix() {
    bounded(async {
        let srv = spawn_server(opts()).await;
        create(&srv.app, "c").await;
        let items: Vec<_> = (0..4).map(|i| plain_item(&record(i, 200))).collect();
        let env = envelope_v1(&items);
        // Cut in the middle of the 4th item.
        let per = (env.len() - 24) / 4;
        let cut = env.len() - per / 2;
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(
            b"POST /v1/collections/c/import?batch_records=1 HTTP/1.1\r\nhost: x\r\ncontent-type: application/x-masstuffy-envelope\r\ntransfer-encoding: chunked\r\n\r\n",
        )
        .await
        .unwrap();
        s.write_all(&chunked(&env[..cut])).await.unwrap();
        // Test-only polling of an external effect: the first imported record shows up.
        loop {
            if count_scan(srv.addr, "c").await >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(s);
        let n = count_scan(srv.addr, "c").await;
        assert!((2..=4).contains(&n), "{n}"); // warcinfo + 1..=3 records
        tokio::time::sleep(Duration::from_millis(100)).await;
        let n = count_scan(srv.addr, "c").await;
        assert!((2..=4).contains(&n), "{n}");
        let (st, v) = gget(srv.addr, &format!("/v1/lookup/by-id?id={}&sync=true&fetch=locator", id_str(3))).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{v}");
        let (st, b) = send(
            srv.addr,
            rq(Method::POST, "/v1/collections/c/records", Some("application/warc"), warc_bytes(&[record(50, 10)])),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b:?}");
        assert_eq!(gget(srv.addr, "/healthz").await.0, StatusCode::OK);
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_mid_put_writes_nothing() {
    bounded(async {
        let srv = spawn_server(opts()).await;
        create(&srv.app, "c").await;
        let full = warc_bytes(&[record(1, 500)]);
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        let head = format!(
            "POST /v1/collections/c/records HTTP/1.1\r\nhost: x\r\ncontent-type: application/warc\r\ncontent-length: {}\r\n\r\n",
            full.len()
        );
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(&full[..full.len() / 2]).await.unwrap();
        s.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(s);
        // Short settle: the handler ends on its own once the body stream fails.
        assert_eq!(gget(srv.addr, "/healthz").await.0, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(count_scan(srv.addr, "c").await, 1);
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

/// A streamed put request through the hyper client, fed by the returned sender.
fn slow_put(
    addr: SocketAddr,
) -> (
    tokio::sync::mpsc::Sender<Bytes>,
    JoinHandle<(StatusCode, Bytes)>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let r = Request::builder()
        .method(Method::POST)
        .uri("/v1/collections/c/records")
        .header("host", "x")
        .header("content-type", "application/warc")
        .body(Body::new(ChannelBody::new(rx)))
        .unwrap();
    let h = tokio::spawn(send(addr, r));
    (tx, h)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_drain_waits_for_inflight_then_returns() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            drain_timeout: Duration::from_secs(2),
            ..opts()
        })
        .await;
        create(&srv.app, "c").await;
        let rec = warc_bytes(&[record(1, 1000)]);
        let (tx, put) = slow_put(srv.addr);
        let chunks: Vec<Bytes> = rec
            .chunks(rec.len() / 8 + 1)
            .map(Bytes::copy_from_slice)
            .collect();
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            tx.send(c).await.unwrap();
            if i == n / 2 {
                srv.stop.send(Stop::Drain).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(tx);
        let (s, b) = put.await.unwrap();
        assert_eq!(s, StatusCode::CREATED, "{b:?}");
        let t = Instant::now();
        srv.handle.await.unwrap();
        assert!(t.elapsed() < Duration::from_millis(1500));
        assert!(TcpStream::connect(srv.addr).await.is_err());
        srv.app.repo.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_deadline_aborts_stuck_connection() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            drain_timeout: Duration::from_millis(300),
            ..opts()
        })
        .await;
        create(&srv.app, "c").await;
        let (tx, put) = slow_put(srv.addr);
        tx.send(Bytes::from_static(b"WARC/1.1\r\n")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let t = Instant::now();
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(2300),
            "{:?}",
            t.elapsed()
        );
        srv.app.repo.shutdown().await.unwrap();
        drop(tx);
        put.abort();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_now_skips_drain() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            drain_timeout: Duration::from_secs(30),
            ..opts()
        })
        .await;
        create(&srv.app, "c").await;
        let (tx, put) = slow_put(srv.addr);
        tx.send(Bytes::from_static(b"WARC/1.1\r\n")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let t = Instant::now();
        srv.stop.send(Stop::Drain).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        srv.stop.send(Stop::Now).unwrap();
        srv.handle.await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        drop(tx);
        put.abort();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn header_read_timeout_closes() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            header_read_timeout: Duration::from_millis(200),
            ..opts()
        })
        .await;
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\n").await.unwrap();
        let mut buf = [0u8; 256];
        let r = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf))
            .await
            .expect("the server did not close the socket");
        // Either a clean close, a reset, or an error response followed by a close.
        if matches!(r, Ok(n) if n > 0) {
            let r = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
            assert!(r.is_ok());
        }
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

async fn raw_health(s: &mut TcpStream) -> String {
    s.write_all(b"GET /healthz HTTP/1.1\r\nhost: x\r\n\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "closed early");
        got.extend_from_slice(&buf[..n]);
        if got.ends_with(b"}") {
            return String::from_utf8_lossy(&got).into_owned();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_connections_backpressure() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            max_connections: 1,
            ..opts()
        })
        .await;
        let mut a = TcpStream::connect(srv.addr).await.unwrap();
        assert!(raw_health(&mut a).await.contains("200"));
        let mut b = TcpStream::connect(srv.addr).await.unwrap();
        let blocked = tokio::time::timeout(Duration::from_millis(300), raw_health(&mut b)).await;
        assert!(blocked.is_err(), "B was served while A held the only slot");
        drop(a);
        // B's request was written before the timeout dropped the future; read its answer.
        let mut got = Vec::new();
        let mut buf = [0u8; 512];
        let r = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let n = b.read(&mut buf).await.unwrap();
                assert!(n > 0);
                got.extend_from_slice(&buf[..n]);
                if got.ends_with(b"}") {
                    break;
                }
            }
        })
        .await;
        assert!(r.is_ok());
        assert!(String::from_utf8_lossy(&got).contains("200"));
        drop(b);
        srv.stop.send(Stop::Drain).unwrap();
        srv.handle.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_sender_drains_without_aborting() {
    bounded(async {
        let srv = spawn_server(ServeOptions {
            drain_timeout: Duration::from_millis(800),
            ..opts()
        })
        .await;
        create(&srv.app, "c").await;
        let (tx, put) = slow_put(srv.addr);
        tx.send(Bytes::from_static(b"WARC/1.1\r\n")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let t = Instant::now();
        drop(srv.stop);
        srv.handle.await.unwrap();
        // The stuck connection was waited for until the drain deadline, not aborted at once.
        assert!(
            t.elapsed() >= Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        srv.app.repo.shutdown().await.unwrap();
        drop(tx);
        put.abort();
    })
    .await;
}
