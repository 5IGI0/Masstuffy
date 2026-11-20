//! Router tests (in-process, `oneshot`).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{Method, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::get as route_get;
use bytes::Bytes;
use masstuffy_warc::{Record, WarcReader};
use serde_json::{Value, json};

use crate::jobs::Jobs;
use crate::locator::decode_locator;
use crate::options::HttpOptions;
use crate::router::AppState;
use crate::testutil::*;

fn err(s: StatusCode, b: &Bytes, status: u16, code: &str) -> Value {
    assert_eq!(s.as_u16(), status, "{}", String::from_utf8_lossy(b));
    let v = json(b);
    assert_eq!(v["error"]["code"], code, "{v}");
    v["error"].clone()
}

async fn scan_all(app: &TestApp, slug: &str, limit: usize) -> Vec<Value> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut uri = format!("/v1/collections/{slug}/scan?limit={limit}");
        if let Some(c) = &cursor {
            uri.push_str(&format!("&cursor={c}"));
        }
        let (s, _, b) = get(app, &uri).await;
        assert_eq!(s, StatusCode::OK);
        let v = json(&b);
        let page = v["items"].as_array().unwrap();
        if page.is_empty() {
            assert!(v["next"].is_null());
            return items;
        }
        items.extend(page.iter().cloned());
        cursor = Some(v["next"].as_str().unwrap().to_owned());
    }
}

async fn get_record(app: &TestApp, slug: &str, l: &Value) -> (StatusCode, Bytes) {
    let (s, _, b) = get(app, &format!("/v1/collections/{slug}/records/{}", tok(l))).await;
    (s, b)
}

async fn sync(app: &TestApp) {
    let (s, _, _) = call(
        &app.router,
        req(Method::POST, "/v1/index/sync", None, vec![]),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

// ---- Health, status, fallbacks.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthz_ok_then_503_after_shutdown() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = get(&app, "/healthz").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json(&b), json!({"status": "ok"}));
        app.repo.shutdown().await.unwrap();
        let (s, _, b) = get(&app, "/healthz").await;
        err(s, &b, 503, "shutting_down");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_limits_and_raw_check() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        let (s, _, b) = get(&app, "/v1/status").await;
        assert_eq!(s, StatusCode::OK);
        let v = json(&b);
        assert_eq!(v["api"], 1);
        assert_eq!(v["raw_check"], "full");
        assert_eq!(v["collections"], 1);
        assert_eq!(v["dictionaries"], 0);
        assert_eq!(v["limits"]["max_put_records"], 10_000);
        assert!(v["limits"]["max_import_body_bytes"].is_null());
        assert_eq!(v["limits"]["request_timeout_ms"], 60_000);
        assert!(v["open_report"].is_null());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_route_404_json_and_405() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = get(&app, "/nope").await;
        err(s, &b, 404, "no_route");
        let (s, _, b) = call(
            &app.router,
            req(Method::DELETE, "/v1/collections", None, vec![]),
        )
        .await;
        err(s, &b, 405, "method_not_allowed");
        let (s, h, b) = call(&app.router, req(Method::HEAD, "/healthz", None, vec![])).await;
        assert_eq!(s, StatusCode::OK);
        assert!(b.is_empty());
        assert!(h.contains_key("content-type"));
    })
    .await;
}

// ---- Collections.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collection_crud() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, h, b) = post_json(&app, "/v1/collections", &json!({"slug": "a"})).await;
        assert_eq!(s, StatusCode::CREATED);
        assert_eq!(h["location"], "/v1/collections/a");
        let v = json(&b);
        assert_eq!(v["codec"], json!({"algo": "none"}));
        assert_eq!(v["generation"], 1);
        let (s, _, b) = post_json(&app, "/v1/collections", &json!({"slug": "a"})).await;
        err(s, &b, 409, "slug_taken");
        let (s, _, b) = post_json(&app, "/v1/collections", &json!({"slug": "A b"})).await;
        err(s, &b, 400, "invalid_slug");
        let (_, _, b) = get(&app, "/v1/collections").await;
        let v = json(&b);
        assert_eq!(v["collections"].as_array().unwrap().len(), 1);
        assert_eq!(v["collections"][0]["slug"], "a");
        assert!(v["collections"][0].get("stats").is_none());
        assert!(v["collections"][0]["size"]["records"].is_u64());
        let (_, _, b) = get(&app, "/v1/collections/a").await;
        assert!(json(&b)["stats"]["records"].is_u64());
        let (s, _, b) = send_json(
            &app,
            Method::PATCH,
            "/v1/collections/a",
            &json!({"slug": "b"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["slug"], "b");
        let (s, _, b) = get(&app, "/v1/collections/a").await;
        err(s, &b, 404, "unknown_collection");
        let (s, _, b) = call(
            &app.router,
            req(Method::DELETE, "/v1/collections/b", None, vec![]),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json(&b)["trash_removed"], true);
        let (s, _, b) = get(&app, "/v1/collections/b").await;
        err(s, &b, 404, "unknown_collection");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collection_size_json() {
    const KEYS: [&str; 7] = [
        "records",
        "records_total",
        "tombstones",
        "stored_bytes",
        "tombstoned_bytes",
        "segment_bytes",
        "cdx_bytes",
    ];
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = post_json(&app, "/v1/collections", &json!({"slug": "a"})).await;
        assert_eq!(s, StatusCode::CREATED);
        let size = json(&b)["size"].clone();
        assert_eq!(size["records"], 0);
        assert!(size["stored_bytes"].as_u64().unwrap() > 0);
        let recs: Vec<Record> = (0..3).map(|i| record(i, 50)).collect();
        let l = put(&app, "a", &recs).await;
        let (_, _, b) = get(&app, "/v1/collections/a").await;
        let v = json(&b);
        assert_eq!(v["size"]["records"], 3);
        assert_eq!(v["size"]["records_total"], 3);
        assert!(v.get("stats").is_some());
        let (_, _, b) = get(&app, "/v1/collections").await;
        let v = json(&b);
        assert_eq!(v["collections"][0]["size"]["records"], 3);
        assert!(v["collections"][0].get("stats").is_none());
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/a/records/delete",
            &json!({"locators": [tok(&l[0])]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        let (_, _, b) = get(&app, "/v1/collections/a").await;
        let size = json(&b)["size"].clone();
        assert_eq!(size["records"], 2);
        assert_eq!(size["tombstones"], 1);
        for k in KEYS {
            assert!(size[k].is_u64(), "{k}");
        }
        assert_eq!(size.as_object().unwrap().len(), KEYS.len());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_codec_errors() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = post_json(
            &app,
            "/v1/collections",
            &json!({"slug": "z", "codec": {"algo": "zstd", "dict_id": 40000}}),
        )
        .await;
        err(s, &b, 404, "dict_not_found");
        let (s, _, b) = post_json(
            &app,
            "/v1/collections",
            &json!({"slug": "z", "codec": {"algo": "zstd", "dict_id": 0}}),
        )
        .await;
        err(s, &b, 400, "invalid_codec");
        let (s, _, b) = post_json(
            &app,
            "/v1/collections",
            &json!({"slug": "z", "codec": {"algo": "zstd", "dict_id": 5, "level": 1000}}),
        )
        .await;
        err(s, &b, 400, "invalid_codec");
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/collections",
                Some("application/json"),
                b"{nope".to_vec(),
            ),
        )
        .await;
        err(s, &b, 400, "invalid_json");
        let (s, _, b) = post_json(&app, "/v1/collections", &json!({"slug": "z", "bogus": 1})).await;
        err(s, &b, 400, "invalid_json");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_body_too_large_413() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            max_json_body_bytes: 64,
            ..HttpOptions::default()
        })
        .await;
        let body = format!("{{\"slug\":\"{}\"}}", "a".repeat(60));
        assert!(body.len() > 64);
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/collections",
                Some("application/json"),
                body.into_bytes(),
            ),
        )
        .await;
        err(s, &b, 413, "payload_too_large");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_to_taken_409() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        create(&app, "b").await;
        let (s, _, b) = send_json(
            &app,
            Method::PATCH,
            "/v1/collections/a",
            &json!({"slug": "b"}),
        )
        .await;
        err(s, &b, 409, "slug_taken");
    })
    .await;
}

// ---- Records.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_get_roundtrip_single() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let r = record(1, 500);
        let locs = put(&app, "c", std::slice::from_ref(&r)).await;
        assert_eq!(locs.len(), 1);
        let (s, h, b) = get(
            &app,
            &format!("/v1/collections/c/records/{}", tok(&locs[0])),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], r.as_bytes());
        assert_eq!(h["content-type"], "application/warc");
        assert_eq!(h["masstuffy-record-id"], id_str(1).as_str());
        let (s, h, b) = get(
            &app,
            &format!("/v1/collections/c/records/{}/raw", tok(&locs[0])),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], r.as_bytes());
        assert_eq!(h["masstuffy-encoding"], "warc");
        assert_eq!(h["content-type"], "application/warc");
        assert!(h.get("content-encoding").is_none());
        assert_eq!(
            h["masstuffy-raw-size"],
            r.as_bytes().len().to_string().as_str()
        );
        assert_eq!(h["masstuffy-digest"], locs[0]["digest"].as_str().unwrap());
        assert!(h.get("masstuffy-dict-id").is_none());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_many_contiguous_and_fills_id() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let recs: Vec<Record> = (0..5).map(|i| record(i, 100 + i)).collect();
        let locs = put(&app, "c", &recs).await;
        assert_eq!(locs.len(), 5);
        for i in 0..4 {
            assert_eq!(locs[i]["segment"], locs[i + 1]["segment"]);
            assert_eq!(
                locs[i]["offset"].as_u64().unwrap() + locs[i]["raw_size"].as_u64().unwrap(),
                locs[i + 1]["offset"].as_u64().unwrap()
            );
        }
        for (l, r) in locs.iter().zip(&recs) {
            let (s, b) = get_record(&app, "c", l).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(&b[..], r.as_bytes());
        }
        let anon =
            b"WARC/1.1\r\nWARC-Type: resource\r\nContent-Length: 2\r\n\r\nhi\r\n\r\n".to_vec();
        let (s, _, b) = post_warc(&app, "c", anon).await;
        assert_eq!(s, StatusCode::CREATED, "{b:?}");
        assert!(
            json(&b)["records"][0]["record_id"]
                .as_str()
                .unwrap()
                .starts_with("urn:uuid:")
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_accepts_warc_1_0() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let rec = format!(
            "WARC/1.0\r\nWARC-Type: resource\r\nWARC-Record-ID: <{}>\r\nWARC-Date: 2024-01-01T00:00:00Z\r\nContent-Length: 2\r\n\r\nhi\r\n\r\n",
            id_str(9)
        );
        let (s, _, b) = post_warc(&app, "c", rec.clone().into_bytes()).await;
        assert_eq!(s, StatusCode::CREATED, "{b:?}");
        let l = json(&b)["records"][0].clone();
        let (_, got) = get_record(&app, "c", &l).await;
        assert_eq!(&got[..], rec.as_bytes());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_malformed_400_nothing_written() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let mut body = warc_bytes(&[record(1, 50)]);
        body.extend_from_slice(b"this is not warc at all\r\n\r\n");
        let (s, _, b) = post_warc(&app, "c", body).await;
        let e = err(s, &b, 400, "invalid_warc");
        assert_eq!(e["details"]["record_index"], 1);
        assert_eq!(scan_all(&app, "c", 100).await.len(), 1);
        // Truncated record.
        let full = warc_bytes(&[record(2, 50)]);
        let (s, _, b) = post_warc(&app, "c", full[..full.len() - 10].to_vec()).await;
        err(s, &b, 400, "invalid_warc");
        assert_eq!(scan_all(&app, "c", 100).await.len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_request_validation() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            max_put_body_bytes: 1024,
            max_put_records: 2,
            ..HttpOptions::default()
        })
        .await;
        create(&app, "c").await;
        let (s, _, b) = post_warc(&app, "c", Vec::new()).await;
        err(s, &b, 400, "empty_body");
        let uri = "/v1/collections/c/records";
        let (s, _, b) = call(
            &app.router,
            req(Method::POST, uri, Some("text/plain"), vec![]),
        )
        .await;
        err(s, &b, 415, "unsupported_media_type");
        let mut r = req(
            Method::POST,
            uri,
            Some("application/warc"),
            warc_bytes(&[record(1, 10)]),
        );
        r.headers_mut()
            .insert("content-encoding", "gzip".parse().unwrap());
        let (s, _, b) = call(&app.router, r).await;
        err(s, &b, 415, "unsupported_media_type");
        let mut r = req(Method::POST, uri, Some("application/warc"), vec![]);
        r.headers_mut()
            .insert("content-length", "2048".parse().unwrap());
        let (s, _, b) = call(&app.router, r).await;
        err(s, &b, 413, "payload_too_large");
        let (s, _, b) = post_warc(
            &app,
            "c",
            warc_bytes(&[record(1, 10), record(2, 10), record(3, 10)]),
        )
        .await;
        err(s, &b, 413, "too_many_records");
        // Streamed without Content-Length: counted while streaming.
        let (tx, r) = streamed(Method::POST, uri, "application/warc");
        let call_task = tokio::spawn({
            let router = app.router.clone();
            async move { call(&router, r).await }
        });
        for _ in 0..21 {
            if tx.send(Bytes::from(vec![b'x'; 100])).await.is_err() {
                break;
            }
        }
        let (s, _, b) = call_task.await.unwrap();
        err(s, &b, 413, "payload_too_large");
        drop(tx);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_unknown_collection_404_without_reading() {
    bounded(async {
        let app = TestApp::new().await;
        let (tx, r) = streamed(
            Method::POST,
            "/v1/collections/nope/records",
            "application/warc",
        );
        let (s, _, b) = call(&app.router, r).await;
        err(s, &b, 404, "unknown_collection");
        drop(tx);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn put_body_idle_timeout_408() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            body_idle_timeout: Duration::from_millis(200),
            ..HttpOptions::default()
        })
        .await;
        create(&app, "c").await;
        let (tx, r) = streamed(
            Method::POST,
            "/v1/collections/c/records",
            "application/warc",
        );
        let full = warc_bytes(&[record(1, 100)]);
        tx.send(Bytes::copy_from_slice(&full[..20])).await.unwrap();
        let (s, _, b) = call(&app.router, r).await;
        err(s, &b, 408, "body_timeout");
        drop(tx);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_locator_errors() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        create(&app, "d").await;
        let (s, _, b) = get(&app, "/v1/collections/c/records/garbage").await;
        err(s, &b, 400, "invalid_locator");
        let locs = put(&app, "c", &[record(1, 10)]).await;
        let (s, _, b) = get(
            &app,
            &format!("/v1/collections/d/records/{}", tok(&locs[0])),
        )
        .await;
        err(s, &b, 400, "locator_collection_mismatch");
        let (s, _, b) = get(
            &app,
            &format!("/v1/collections/nope/records/{}", tok(&locs[0])),
        )
        .await;
        err(s, &b, 404, "unknown_collection");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_stale_locator_after_rebuild_409() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let r1 = record(1, 200);
        let locs = put(&app, "c", std::slice::from_ref(&r1)).await;
        let (s, h, b) = post_json(
            &app,
            "/v1/collections/c/rebuild",
            &json!({"codec": {"algo": "none"}}),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b:?}");
        let id = id_of_job(&b);
        assert_eq!(h["location"], format!("/v1/jobs/{id}").as_str());
        let j = wait_job(&app, &id).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(j["result"]["generation"], 2);
        sync(&app).await;
        let (s, b) = get_record(&app, "c", &locs[0]).await;
        let e = err(s, &b, 409, "stale_locator");
        assert_eq!(e["details"]["expected"], 2);
        let (s, _, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&sync=true", id_str(1)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], r1.as_bytes());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_batch_single_and_errors() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let recs: Vec<Record> = (0..3).map(|i| record(i, 50)).collect();
        let l = put(&app, "c", &recs).await;
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/records/delete",
            &json!({"locators": [tok(&l[0]), tok(&l[2])]}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["deleted"], 2);
        let items = scan_all(&app, "c", 100).await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["record_id"], id_str(1).as_str());
        // Still readable by explicit locator.
        assert_eq!(get_record(&app, "c", &l[0]).await.0, StatusCode::OK);
        // Single route.
        let (s, _, b) = call(
            &app.router,
            req(
                Method::DELETE,
                &format!("/v1/collections/c/records/{}", tok(&l[1])),
                None,
                vec![],
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["deleted"], 1);
        assert_eq!(scan_all(&app, "c", 100).await.len(), 1);
        // The warcinfo cannot be deleted.
        let info = scan_all(&app, "c", 100).await[0].clone();
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/records/delete",
            &json!({"locators": [tok(&info)]}),
        )
        .await;
        err(s, &b, 400, "invalid_delete_target");
        // A bad token: the index is reported, nothing deleted.
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/records/delete",
            &json!({"locators": [tok(&l[1]), "junk"]}),
        )
        .await;
        let e = err(s, &b, 400, "invalid_locator");
        assert_eq!(e["details"]["index"], 1);
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/records/delete",
            &json!({"locators": []}),
        )
        .await;
        assert_eq!(json(&b)["deleted"], 0, "{s}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_by_id_found_and_not_found() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let l = put(&app, "c", &[record(7, 50)]).await;
        sync(&app).await;
        let uri = "/v1/collections/c/records/delete-by-id";
        let (s, _, b) = post_json(&app, uri, &json!({"record_id": id_str(7)})).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["deleted"]["locator"], l[0]["locator"]);
        sync(&app).await;
        let (s, _, b) = post_json(&app, uri, &json!({"record_id": id_str(7)})).await;
        err(s, &b, 404, "record_not_found");
    })
    .await;
}

// ---- Import.

async fn import(
    app: &TestApp,
    query: &str,
    body: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Bytes) {
    call(
        &app.router,
        req(
            Method::POST,
            &format!("/v1/collections/c/import{query}"),
            Some("application/x-masstuffy-envelope"),
            body,
        ),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_v1_and_legacy_ok() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let items: Vec<_> = (0..3).map(|i| plain_item(&record(i, 100))).collect();
        let env = envelope_v1(&items);
        let (s, _, b) = import(&app, "", env.clone()).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        let v = json(&b);
        assert_eq!(
            (v["records"].as_u64(), v["format"].as_str()),
            (Some(3), Some("v1"))
        );
        assert_eq!(v["committed_offset"], env.len());
        sync(&app).await;
        for i in 0..3 {
            let (s, _, _) = get(
                &app,
                &format!("/v1/lookup/by-id?id={}&fetch=locator", id_str(i)),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
        }
        let items: Vec<_> = (10..12).map(|i| plain_item(&record(i, 100))).collect();
        let (s, _, b) = import(&app, "", envelope_legacy(&items)).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["format"], "legacy");
        assert_eq!(json(&b)["records"], 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_streamed_tiny_chunks() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let items: Vec<_> = (0..3).map(|i| plain_item(&record(i, 100))).collect();
        let env = envelope_v1(&items);
        let (tx, r) = streamed(
            Method::POST,
            "/v1/collections/c/import?batch_records=1",
            "application/x-masstuffy-envelope",
        );
        let task = tokio::spawn({
            let router = app.router.clone();
            async move { call(&router, r).await }
        });
        for c in env.chunks(7) {
            tx.send(Bytes::copy_from_slice(c)).await.unwrap();
        }
        drop(tx);
        let (s, _, b) = task.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["records"], 3);
        assert_eq!(json(&b)["batches"], 3);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_invalid_policies_and_errors() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let mut items: Vec<_> = (0..3).map(|i| plain_item(&record(i, 100))).collect();
        items[1].digest ^= 1;
        let env = envelope_v1(&items);
        let (s, _, b) = import(&app, "?on_invalid=skip", env.clone()).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        let v = json(&b);
        assert_eq!(
            (v["records"].as_u64(), v["rejected"].as_u64()),
            (Some(2), Some(1))
        );
        assert_eq!(v["rejections"][0]["ordinal"], 1);
        let (s, _, b) = import(&app, "", env.clone()).await;
        let e = err(s, &b, 400, "import_failed");
        assert_eq!(e["details"]["report"]["records"], 1);
        assert_eq!(e["details"]["cause"]["code"], "invalid_record");
        let good = envelope_v1(&[plain_item(&record(20, 100)), plain_item(&record(21, 100))]);
        let (s, _, b) = import(&app, "", good[..good.len() - 20].to_vec()).await;
        let e = err(s, &b, 400, "import_failed");
        assert_eq!(e["details"]["cause"]["code"], "invalid_envelope");
        for q in ["?batch_records=0", "?on_invalid=maybe", "?bogus=1"] {
            let (s, _, b) = import(&app, q, Vec::new()).await;
            err(s, &b, 400, "invalid_param");
        }
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/collections/c/import",
                Some("application/json"),
                vec![],
            ),
        )
        .await;
        err(s, &b, 415, "unsupported_media_type");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_cap_413_with_report() {
    bounded(async {
        let items: Vec<_> = (0..3).map(|i| plain_item(&record(i, 100))).collect();
        let env = envelope_v1(&items);
        let app = TestApp::with(HttpOptions {
            max_import_body_bytes: Some(u64::try_from(env.len() / 2).unwrap()),
            ..HttpOptions::default()
        })
        .await;
        create(&app, "c").await;
        let (s, _, b) = import(&app, "", env).await;
        let e = err(s, &b, 413, "payload_too_large");
        assert!(e["details"]["report"]["records"].is_number(), "{e}");
    })
    .await;
}

// ---- Queries.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookup_by_id_and_url() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        create(&app, "other").await;
        let r = record(1, 100);
        let l = put(&app, "c", std::slice::from_ref(&r)).await;
        let (s, h, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&sync=true", id_str(1)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], r.as_bytes());
        let at = decode_locator(h["masstuffy-locator"].to_str().unwrap()).unwrap();
        assert_eq!(crate::locator::encode_locator(&at), tok(&l[0]));
        let (_, _, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&fetch=locator", id_str(1)),
        )
        .await;
        assert_eq!(json(&b), l[0]);
        let (s, _, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&collection=other", id_str(1)),
        )
        .await;
        err(s, &b, 404, "record_not_found");
        let (s, _, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&collection=nope", id_str(1)),
        )
        .await;
        err(s, &b, 404, "unknown_collection");
        let (s, _, b) = get(&app, "/v1/lookup/by-id").await;
        err(s, &b, 400, "invalid_param");
        // By URL: three captures of one URL.
        let recs: Vec<Record> = [(10, 0), (11, 1), (12, 2)]
            .iter()
            .map(|&(i, _)| record_at(i, 50, "http://cap.example.com/page"))
            .collect();
        // record_at dates are 2024-01-01T00:00:{i}Z: 10, 11, 12 seconds.
        put(&app, "c", &recs).await;
        sync(&app).await;
        let by = |q: &str| {
            format!("/v1/collections/c/lookup/by-url?url=http%3A%2F%2Fcap.example.com%2Fpage&{q}")
        };
        let (s, _, b) = get(&app, &by("date=20240101000011&fetch=locator")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json(&b)["record_id"], id_str(11).as_str());
        let (_, _, b) = get(&app, &by("fetch=locator")).await;
        assert_eq!(json(&b)["record_id"], id_str(12).as_str());
        let (_, _, b) = get(&app, &by("date=2024-01-01T00:00:10Z&fetch=locator")).await;
        assert_eq!(json(&b)["record_id"], id_str(10).as_str());
        let (s, _, b) = get(&app, &by("date=bad")).await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = get(
            &app,
            "/v1/collections/c/lookup/by-url?url=http%3A%2F%2Fnone.example%2F",
        )
        .await;
        err(s, &b, 404, "record_not_found");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_patterns_and_paging() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let urls = [
            "http://a.example.com/x/1",
            "http://b.example.com/x/2",
            "http://example.com/y",
            "http://example.org/x",
        ];
        let recs: Vec<Record> = urls
            .iter()
            .enumerate()
            .map(|(i, u)| record_at(i, 30, u))
            .collect();
        put(&app, "c", &recs).await;
        let q = |s: &str| format!("/v1/collections/c/search?sync=true&{s}");
        let hits = |b: &Bytes| json(b)["hits"].as_array().unwrap().len();
        let (s, _, b) = get(&app, &q("host=example.com&subdomains=true")).await;
        assert_eq!((s, hits(&b)), (StatusCode::OK, 3));
        let (_, _, b) = get(&app, &q("host=example.com")).await;
        assert_eq!(hits(&b), 1);
        let (_, _, b) = get(
            &app,
            &q("host=example.com&subdomains=true&path=/x&path_match=prefix"),
        )
        .await;
        assert_eq!(hits(&b), 2);
        // Paging.
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut uri = q("host=example.com&subdomains=true&limit=1");
            if let Some(c) = &cursor {
                uri.push_str(&format!("&cursor={c}"));
            }
            let (_, _, b) = get(&app, &uri).await;
            let v = json(&b);
            seen.extend(
                v["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|h| h["record_id"].as_str().unwrap().to_owned()),
            );
            match v["next"].as_str() {
                Some(c) => cursor = Some(c.to_owned()),
                None => break,
            }
        }
        assert_eq!(seen.len(), 3);
        let mut d = seen.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), 3);
        let (s, _, b) = get(&app, &q("host=example.com&port=99999")).await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = get(&app, &q("host=example.com&path_match=exact")).await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = get(&app, &q("host=example.com&cursor=garbage")).await;
        err(s, &b, 400, "invalid_query");
        let (s, _, b) = get(&app, &q("host=")).await;
        err(s, &b, 400, "invalid_param");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_paging_visits_all() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let recs: Vec<Record> = (0..5).map(|i| record(i, 50)).collect();
        put(&app, "c", &recs).await;
        let items = scan_all(&app, "c", 2).await;
        assert_eq!(items.len(), 6);
        assert_eq!(items[0]["warc_type"], "warcinfo");
        let mut t: Vec<_> = items.iter().map(tok).collect();
        t.sort();
        t.dedup();
        assert_eq!(t.len(), 6);
        let (s, _, b) = get(&app, "/v1/collections/c/scan?cursor=zzz").await;
        err(s, &b, 400, "invalid_query");
        let (s, _, b) = get(&app, "/v1/collections/c/scan?limit=0").await;
        err(s, &b, 400, "invalid_param");
    })
    .await;
}

fn parse_warc(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut r = WarcReader::new(std::io::Cursor::new(bytes));
    let mut out = Vec::new();
    while let Some(rec) = r.next_record().unwrap() {
        out.push(rec.as_bytes().to_vec());
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_pages_concatenate() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let recs: Vec<Record> = (0..5).map(|i| record(i, 1024)).collect();
        put(&app, "c", &recs).await;
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut uri = "/v1/collections/c/export?max_bytes=1".to_owned();
            if let Some(c) = &cursor {
                uri.push_str(&format!("&cursor={c}"));
            }
            let (s, h, b) = get(&app, &uri).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(h["content-type"], "application/warc");
            match h.get("masstuffy-next-cursor") {
                Some(c) => {
                    assert_eq!(h["masstuffy-record-count"], "1");
                    cursor = Some(c.to_str().unwrap().to_owned());
                    all.extend_from_slice(&b);
                    pages += 1;
                }
                None => {
                    assert!(b.is_empty());
                    break;
                }
            }
        }
        assert_eq!(pages, 6);
        let parsed = parse_warc(&all);
        assert_eq!(parsed.len(), 6);
        for (p, r) in parsed[1..].iter().zip(&recs) {
            assert_eq!(&p[..], r.as_bytes());
        }
        // One big page.
        let (_, h, b) = get(&app, "/v1/collections/c/export").await;
        assert_eq!(h["masstuffy-record-count"], "6");
        assert_eq!(parse_warc(&b).len(), 6);
        assert_eq!(h["content-length"], b.len().to_string().as_str());
        // Bad params.
        let (s, _, b) = get(&app, "/v1/collections/c/export?max_bytes=999999999999").await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = get(&app, "/v1/collections/c/export?on_unreadable=x").await;
        err(s, &b, 400, "invalid_param");
        let (_, h, _) = get(&app, "/v1/collections/c/export?on_unreadable=skip").await;
        assert_eq!(h["masstuffy-unreadable-count"], "0");
    })
    .await;
}

// ---- Dictionaries, jobs.

/// Trains a dictionary on a scratch collection and returns its id.
async fn trained_dict(app: &TestApp) -> u32 {
    create(app, "src").await;
    let recs: Vec<Record> = (0..64).map(|i| record(i, 300)).collect();
    put(app, "src", &recs).await;
    let (s, h, b) = post_json(
        app,
        "/v1/collections/src/train",
        &json!({"max_dict_size": 4096}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b:?}");
    assert!(h.contains_key("location"));
    let j = wait_job(app, &id_of_job(&b)).await;
    assert_eq!(j["state"], "succeeded", "{j}");
    u32::try_from(j["result"]["dict_id"].as_u64().unwrap()).unwrap()
}

async fn create_zstd(app: &TestApp, slug: &str, dict: u32) {
    let (s, _, b) = post_json(
        app,
        "/v1/collections",
        &json!({"slug": slug, "codec": {"algo": "zstd", "dict_id": dict, "level": 1}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dictionaries_list_and_download() {
    bounded(async {
        let app = TestApp::new().await;
        let id = trained_dict(&app).await;
        let (_, _, b) = get(&app, "/v1/dictionaries").await;
        let list = json(&b);
        assert!(
            list["dictionaries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["dict_id"] == id)
        );
        let (s, h, b) = get(&app, &format!("/v1/dictionaries/{id}")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h["content-type"], "application/octet-stream");
        assert_eq!(h["masstuffy-dict-id"], id.to_string().as_str());
        assert_eq!(h["cache-control"], "public, max-age=31536000, immutable");
        let etag = h["etag"].to_str().unwrap().to_owned();
        assert!(etag.starts_with('"') && etag.len() == 18, "{etag}");
        assert_eq!(masstuffy_zframe::DictHeader::parse(&b).unwrap().dict_id, id);
        let r = |inm: &str| {
            let mut r = req(Method::GET, &format!("/v1/dictionaries/{id}"), None, vec![]);
            r.headers_mut()
                .insert("if-none-match", inm.parse().unwrap());
            r
        };
        let (s, h, b) = call(&app.router, r(&etag)).await;
        assert_eq!(s, StatusCode::NOT_MODIFIED);
        assert!(b.is_empty());
        assert_eq!(h["etag"].to_str().unwrap(), etag);
        let (s, _, _) = call(&app.router, r(&format!("\"zz\", {etag}"))).await;
        assert_eq!(s, StatusCode::NOT_MODIFIED);
        let (s, _, _) = call(&app.router, r("*")).await;
        assert_eq!(s, StatusCode::NOT_MODIFIED);
        let (s, _, _) = call(&app.router, r("\"other\"")).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _, b) = get(&app, "/v1/dictionaries/12345").await;
        err(s, &b, 404, "dict_not_found");
        for bad in ["abc", "01", "4294967296", "-1"] {
            let (s, _, b) = get(&app, &format!("/v1/dictionaries/{bad}")).await;
            err(s, &b, 400, "invalid_param");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn collection_info_codec_and_rules() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "n").await;
        let (_, _, b) = get(&app, "/v1/collections/n").await;
        let v = json(&b);
        assert_eq!(v["codec"], json!({"algo": "none"}));
        assert_eq!(v["raw_rules"]["payload"], "warc");
        assert!(v["raw_rules"]["dict_id"].is_null());
        let id = trained_dict(&app).await;
        create_zstd(&app, "z", id).await;
        let (_, _, b) = get(&app, "/v1/collections").await;
        let list = json(&b);
        let z = list["collections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["slug"] == "z")
            .unwrap()
            .clone();
        assert_eq!(
            z["codec"],
            json!({"algo": "zstd", "dict_id": id, "level": 1})
        );
        assert_eq!(z["raw_rules"]["payload"], "zstd_frame");
        assert_eq!(z["raw_rules"]["single_frame"], true);
        assert_eq!(z["raw_rules"]["dict_id"], id);
        assert_eq!(z["raw_rules"]["dictless_min_content_size"], 1_048_577);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_headers_zstd_roundtrip_client_side() {
    bounded(async {
        let app = TestApp::new().await;
        let dict = trained_dict(&app).await;
        create_zstd(&app, "z", dict).await;
        let r = record(1, 2000);
        let l = put(&app, "z", std::slice::from_ref(&r)).await;
        let (s, h, body) = get(
            &app,
            &format!("/v1/collections/z/records/{}/raw", tok(&l[0])),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h["content-type"], "application/zstd");
        assert_eq!(h["masstuffy-encoding"], "zstd");
        assert_eq!(h["masstuffy-dict-id"], dict.to_string().as_str());
        assert_eq!(h["masstuffy-raw-size"], body.len().to_string().as_str());
        assert_eq!(
            h["masstuffy-content-size"],
            r.as_bytes().len().to_string().as_str()
        );
        assert!(h.get("content-encoding").is_none());
        assert_eq!(
            h["masstuffy-digest"],
            format!("{:016x}", masstuffy_cdx::digest(&body)).as_str()
        );
        let (_, _, dbytes) = get(
            &app,
            &format!(
                "/v1/dictionaries/{}",
                h["masstuffy-dict-id"].to_str().unwrap()
            ),
        )
        .await;
        let dec = masstuffy_zframe::DecoderDict::new(&dbytes).unwrap();
        let mut out = Vec::new();
        masstuffy_zframe::Decompressor::new()
            .unwrap()
            .decompress(&body, Some(&dec), 1 << 24, &mut out)
            .unwrap();
        let (_, decoded) = get_record(&app, "z", &l[0]).await;
        assert_eq!(out, &decoded[..]);
        assert_eq!(out, r.as_bytes());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_import_with_fetched_dict() {
    bounded(async {
        let app = TestApp::new().await;
        let dict = trained_dict(&app).await;
        create_zstd(&app, "z", dict).await;
        let (_, _, dbytes) = get(&app, &format!("/v1/dictionaries/{dict}")).await;
        let enc = masstuffy_zframe::EncoderDict::new(&dbytes, 1).unwrap();
        let rec = record(500, 1500);
        let mut frame = Vec::new();
        masstuffy_zframe::Compressor::new()
            .unwrap()
            .compress(rec.as_bytes(), &enc, &mut frame)
            .unwrap();
        let mut item = plain_item(&rec);
        item.digest = masstuffy_cdx::digest(&frame);
        item.payload = frame;
        let imp = |env: Vec<u8>| {
            call(
                &app.router,
                req(
                    Method::POST,
                    "/v1/collections/z/import",
                    Some("application/x-masstuffy-envelope"),
                    env,
                ),
            )
        };
        let (s, _, b) = imp(envelope_v1(std::slice::from_ref(&item))).await;
        assert_eq!(s, StatusCode::OK, "{b:?}");
        assert_eq!(json(&b)["records"], 1);
        sync(&app).await;
        let (s, _, b) = get(
            &app,
            &format!("/v1/lookup/by-id?id={}&collection=z", id_str(500)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(&b[..], rec.as_bytes());
        // A frame with another dictionary id (none) is rejected.
        let mut plain = Vec::new();
        masstuffy_zframe::Compressor::new()
            .unwrap()
            .compress_plain(rec.as_bytes(), 1, &mut plain)
            .unwrap();
        let mut bad = plain_item(&record(501, 1500));
        bad.digest = masstuffy_cdx::digest(&plain);
        bad.payload = plain;
        let (s, _, b) = imp(envelope_v1(&[bad])).await;
        let e = err(s, &b, 400, "import_failed");
        assert_eq!(e["details"]["cause"]["code"], "invalid_record");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn train_then_rebuild_to_zstd() {
    bounded(async {
        let app = TestApp::new().await;
        let dict = trained_dict(&app).await;
        let (s, h, b) = post_json(
            &app,
            "/v1/collections/src/rebuild",
            &json!({"codec": {"algo": "zstd", "dict_id": dict, "level": 3}}),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b:?}");
        assert!(h.contains_key("location"));
        let j = wait_job(&app, &id_of_job(&b)).await;
        assert_eq!(j["state"], "succeeded", "{j}");
        assert_eq!(j["result"]["generation"], 2);
        assert_eq!(scan_all(&app, "src", 20).await.len(), 66);
        let (_, h, body) = get(&app, "/v1/collections/src/export?max_records=100").await;
        assert_eq!(h["masstuffy-record-count"], "66");
        let parsed = parse_warc(&body);
        for (p, i) in parsed[parsed.len() - 64..].iter().zip(0..64) {
            assert_eq!(&p[..], record(i, 300).as_bytes());
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_list_and_errors() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/nope/rebuild",
            &json!({"codec": {"algo": "none"}}),
        )
        .await;
        err(s, &b, 404, "unknown_collection");
        create(&app, "c").await;
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/rebuild",
            &json!({"codec": {"algo": "zstd", "dict_id": 0}}),
        )
        .await;
        err(s, &b, 400, "invalid_codec");
        let (s, _, b) = post_json(
            &app,
            "/v1/collections/c/rebuild",
            &json!({"codec": {"algo": "none"}, "workers": 0}),
        )
        .await;
        err(s, &b, 400, "invalid_param");
        assert!(
            json(&get(&app, "/v1/jobs").await.2)["jobs"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let (s, _, b) = call(
            &app.router,
            req(Method::POST, "/v1/index/rebuild", None, vec![]),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b:?}");
        let j = wait_job(&app, &id_of_job(&b)).await;
        assert_eq!(
            (j["state"].as_str(), j["kind"].as_str()),
            (Some("succeeded"), Some("rebuild_index"))
        );
        let (_, _, b) = get(&app, "/v1/jobs").await;
        assert_eq!(json(&b)["jobs"].as_array().unwrap().len(), 1);
        let (s, _, b) = get(&app, "/v1/jobs/999").await;
        err(s, &b, 404, "job_not_found");
        let (s, _, b) = get(&app, "/v1/jobs/abc").await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = get(&app, "/v1/jobs/1?wait=3601").await;
        err(s, &b, 400, "invalid_param");
    })
    .await;
}

// ---- Middleware.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_limit_503() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            max_concurrent_requests: 1,
            ..HttpOptions::default()
        })
        .await;
        create(&app, "c").await;
        let (tx, r) = streamed(
            Method::POST,
            "/v1/collections/c/records",
            "application/warc",
        );
        let a = tokio::spawn({
            let router = app.router.clone();
            async move { call(&router, r).await }
        });
        // A holds the only permit until its body is done. Give it time to start: B would
        // otherwise win the race for the permit (the test task is already running, A is not).
        // Test-only waiting on an external effect.
        let mut refused = None;
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let (s, h, b) = get(&app, "/v1/status").await;
            if s == StatusCode::SERVICE_UNAVAILABLE {
                refused = Some((h, b));
                break;
            }
            assert!(!a.is_finished(), "A ended before B was refused");
        }
        let (h, b) = refused.expect("B was never refused");
        err(StatusCode::SERVICE_UNAVAILABLE, &b, 503, "overloaded");
        assert_eq!(h["retry-after"], "1");
        // The liveness probe is exempt from the limit.
        let (s, _, _) = get(&app, "/healthz").await;
        assert_eq!(s, StatusCode::OK);
        tx.send(Bytes::from_static(b"junk")).await.unwrap();
        drop(tx);
        let (s, _, b) = a.await.unwrap();
        err(s, &b, 400, "invalid_warc");
        let (s, _, _) = get(&app, "/v1/status").await;
        assert_eq!(s, StatusCode::OK);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_timeout_503() {
    bounded(async {
        let app = TestApp::new().await;
        let opts = HttpOptions {
            request_timeout: Duration::from_millis(100),
            ..HttpOptions::default()
        };
        let st = Arc::new(AppState {
            repo: app.repo.clone(),
            jobs: Jobs::new(1, 1),
            requests: Arc::new(tokio::sync::Semaphore::new(4)),
            open_report: None,
            opts,
        });
        let router: Router = Router::new()
            .route(
                "/slow",
                route_get(|| async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    "late"
                }),
            )
            .layer(from_fn_with_state(
                Arc::clone(&st),
                crate::middleware::timeout,
            ))
            .with_state(st);
        let (s, h, b) = call(&router, req(Method::GET, "/slow", None, vec![])).await;
        let e = err(s, &b, 503, "timeout");
        assert!(e["message"].as_str().unwrap().contains("mutation"));
        assert!(h.contains_key("retry-after"));
    })
    .await;
}

// ---- Concurrency.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_16_tasks() {
    bounded(async {
        let app = Arc::new(TestApp::new().await);
        create(&app, "c").await;
        let mut tasks = Vec::new();
        for t in 0..16usize {
            let app = Arc::clone(&app);
            tasks.push(tokio::spawn(async move {
                let mut out = Vec::new();
                for k in 0..10usize {
                    let base = 1000 + t * 100 + k * 2;
                    let recs = [record(base, 80), record(base + 1, 80)];
                    out.extend(put(&app, "c", &recs).await.into_iter().zip(recs));
                }
                out
            }));
        }
        let mut all = Vec::new();
        for t in tasks {
            all.extend(t.await.unwrap());
        }
        assert_eq!(all.len(), 320);
        let mut toks: Vec<String> = all.iter().map(|(l, _)| tok(l)).collect();
        toks.sort();
        toks.dedup();
        assert_eq!(toks.len(), 320);
        for (l, r) in &all {
            let (s, b) = get_record(&app, "c", l).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(&b[..], r.as_bytes());
        }
        assert_eq!(scan_all(&app, "c", 500).await.len(), 321);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_and_reads() {
    bounded(async {
        let app = Arc::new(TestApp::new().await);
        create(&app, "c").await;
        let mut tasks = Vec::new();
        for t in 0..4usize {
            let app = Arc::clone(&app);
            tasks.push(tokio::spawn(async move {
                for k in 0..10usize {
                    put(&app, "c", &[record(2000 + t * 20 + k, 200)]).await;
                }
            }));
        }
        for _ in 0..4 {
            let app = Arc::clone(&app);
            tasks.push(tokio::spawn(async move {
                for _ in 0..10 {
                    let (s, _, _) = get(&app, "/v1/collections/c/scan?limit=7").await;
                    assert_eq!(s, StatusCode::OK);
                    let (s, _, b) = get(&app, "/v1/collections/c/export?max_records=50").await;
                    assert_eq!(s, StatusCode::OK, "{b:?}");
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(scan_all(&app, "c", 500).await.len(), 41);
        let (_, h, _) = get(&app, "/v1/collections/c/export?max_records=100").await;
        assert_eq!(h["masstuffy-record-count"], "41");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes_without_parameters_reject_unknown_ones() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        for uri in [
            "/v1/collections/a?bogus=1",
            "/v1/collections?bogus=1",
            "/v1/status?bogus=1",
            "/healthz?bogus=1",
            "/v1/jobs?bogus=1",
            "/v1/dictionaries?bogus=1",
        ] {
            let (s, _, b) = get(&app, uri).await;
            err(s, &b, 400, "invalid_param");
        }
        let (s, _, b) = post_json(&app, "/v1/collections?x=1", &json!({"slug": "q"})).await;
        err(s, &b, 400, "invalid_param");
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/collections/a/records?x=1",
                Some("application/warc"),
                vec![],
            ),
        )
        .await;
        err(s, &b, 400, "invalid_param");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn export_fail_mode_reports_a_corrupt_record() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "c").await;
        let l = put(&app, "c", &[record(1, 400), record(2, 400)]).await;
        let id = l[0]["collection"].as_str().unwrap();
        let seg = app
            ._dir
            .path()
            .join(format!("repo/data/repository/{id}/seg.1.1.warc"));
        let mut bytes = std::fs::read(&seg).unwrap();
        // A byte inside the last record's body.
        let at = usize::try_from(
            l[1]["offset"].as_u64().unwrap() + l[1]["raw_size"].as_u64().unwrap() - 20,
        )
        .unwrap();
        bytes[at] ^= 0xff;
        std::fs::write(&seg, bytes).unwrap();
        let (s, _, b) = get(&app, "/v1/collections/c/export").await;
        let e = err(s, &b, 500, "unreadable_record");
        assert_eq!(e["details"]["locator"], l[1]["locator"]);
        assert_eq!(e["kind"], "corrupt");
        let (s, h, b) = get(&app, "/v1/collections/c/export?on_unreadable=skip").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h["masstuffy-unreadable-count"], "1");
        assert_eq!(h["masstuffy-record-count"], "2");
        assert_eq!(parse_warc(&b).len(), 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn import_idle_timeout_408_carries_report_and_cause() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            body_idle_timeout: Duration::from_millis(200),
            ..HttpOptions::default()
        })
        .await;
        create(&app, "c").await;
        let env = envelope_v1(&[plain_item(&record(1, 100)), plain_item(&record(2, 100))]);
        let (tx, r) = streamed(
            Method::POST,
            "/v1/collections/c/import",
            "application/x-masstuffy-envelope",
        );
        tx.send(Bytes::copy_from_slice(&env[..env.len() - 30]))
            .await
            .unwrap();
        let (s, _, b) = call(&app.router, r).await;
        let e = err(s, &b, 408, "body_timeout");
        assert!(e["details"]["report"]["records"].is_number(), "{e}");
        assert!(e["details"]["cause"]["code"].is_string(), "{e}");
        drop(tx);
    })
    .await;
}

// ---- Grep.

/// A record whose body is `body`; `http` makes it an `application/http` response.
fn grep_record(i: usize, http: bool, body: &[u8]) -> Record {
    let mut b =
        masstuffy_warc::RecordBuilder::new(if http { "response" } else { "resource" }).unwrap();
    b.record_id(&id_str(i)).unwrap();
    b.date(masstuffy_warc::WarcDate::new(2024, 1, 1, 0, 0, 0, 0).unwrap());
    b.target_uri(&format!("http://example.com/g/{i}")).unwrap();
    if http {
        b.header("Content-Type", "application/http; msgtype=response")
            .unwrap();
    }
    b.build(body).unwrap()
}

fn ndjson(b: &Bytes) -> Vec<Value> {
    b.split(|&c| c == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap_or_else(|e| panic!("bad line ({e}): {l:?}")))
        .collect()
}

async fn grep_lines(app: &TestApp, body: &Value) -> Vec<Value> {
    let (s, h, b) = post_json(app, "/v1/grep", body).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(h["content-type"], "application/x-ndjson");
    assert_eq!(h["cache-control"], "no-store");
    ndjson(&b)
}

fn lines_of<'a>(lines: &'a [Value], ty: &str) -> Vec<&'a Value> {
    lines.iter().filter(|l| l["type"] == ty).collect()
}

async fn put_needles(app: &TestApp, slug: &str, total: usize, every: usize) -> Vec<Value> {
    let recs: Vec<Record> = (0..total)
        .map(|i| {
            let body = if i % every == 0 {
                format!("xx needle {i} yy")
            } else {
                format!("plain text {i}")
            };
            grep_record(i, false, body.as_bytes())
        })
        .collect();
    put(app, slug, &recs).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_streams_ndjson() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        put_needles(&app, "a", 10, 3).await;
        let lines = grep_lines(
            &app,
            &json!({"collections": ["a"], "queries": [{"pattern": "needle"}]}),
        )
        .await;
        assert_eq!(lines[0]["type"], "progress");
        assert_eq!(lines[0]["slug"], "a");
        assert_eq!(lines[0]["cdx_done"], 0);
        assert!(lines[0]["cdx_total"].as_u64().unwrap() > 0);
        let hits = lines_of(&lines, "hit");
        assert_eq!(hits.len(), 4);
        for h in &hits {
            assert_eq!(h["slug"], "a");
            assert_eq!(h["queries"], json!([0]));
            let (s, b) = get_record(&app, "a", &h["record"]).await;
            assert_eq!(s, StatusCode::OK);
            assert!(String::from_utf8_lossy(&b).contains("needle"));
        }
        let end = lines.last().unwrap();
        assert_eq!(end["type"], "end");
        assert_eq!(end["hits"], 4);
        assert_eq!(end["truncated"], false);
        let c = &end["collections"][0];
        assert_eq!(c["slug"], "a");
        assert_eq!(c["status"], "complete");
        // 10 puts plus the warcinfo record every new collection starts with.
        assert_eq!(c["records"], 11);
        assert_eq!(c["hits"], 4);
        assert_eq!(c["unreadable"], 0);
        assert!(c["error"].is_null());
        assert_eq!(c["cdx_done"], c["cdx_total"]);
        assert_eq!(c["cdx_total"], lines[0]["cdx_total"]);
        assert_eq!(lines_of(&lines, "end").len(), 1);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_scope_and_queries() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        let recs = vec![
            grep_record(0, false, b"alpha-body"),
            grep_record(
                1,
                true,
                b"HTTP/1.1 200 OK\r\nX-Mark: hmark\r\n\r\npmark payload",
            ),
        ];
        let locs = put(&app, "a", &recs).await;
        let hit_ids = |body: Value| {
            let app = &app;
            let locs = &locs;
            async move {
                let lines = grep_lines(app, &body).await;
                let mut out: Vec<(usize, Value)> = Vec::new();
                for h in lines_of(&lines, "hit") {
                    let i = locs
                        .iter()
                        .position(|l| l["locator"] == h["record"]["locator"])
                        .unwrap();
                    out.push((i, h["queries"].clone()));
                }
                out.sort_by_key(|(i, _)| *i);
                out
            }
        };
        let q = |p: &str| json!([{"pattern": p}]);
        let one = |i: usize, qs: Value| vec![(i, qs)];
        assert_eq!(
            hit_ids(json!({"collections":["a"],"queries":q("hmark"),"scope":"headers"})).await,
            one(1, json!([0]))
        );
        assert!(
            hit_ids(json!({"collections":["a"],"queries":q("hmark"),"scope":"payload"}))
                .await
                .is_empty()
        );
        assert_eq!(
            hit_ids(json!({"collections":["a"],"queries":q("pmark"),"scope":"payload"})).await,
            one(1, json!([0]))
        );
        assert!(
            hit_ids(json!({"collections":["a"],"queries":q("pmark"),"scope":"headers"}))
                .await
                .is_empty()
        );
        assert_eq!(
            hit_ids(json!({"collections":["a"],"queries":q("alpha"),"scope":"payload"})).await,
            one(0, json!([0]))
        );
        // Default scope is `all`; several queries OR; regex and case folding.
        let got = hit_ids(json!({"collections":["a"],"queries":[
            {"pattern":"ALPHA","case_insensitive":true},
            {"pattern":"p[m]ark","regex":true},
            {"pattern":"hmark"},
            {"pattern":"absent"},
        ]}))
        .await;
        assert_eq!(got, vec![(0, json!([0])), (1, json!([1, 2]))]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_errors_before_stream() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        let ok_q = json!([{"pattern": "x"}]);
        let (s, _, b) = post_json(
            &app,
            "/v1/grep",
            &json!({"collections": ["nope"], "queries": ok_q}),
        )
        .await;
        err(s, &b, 404, "unknown_collection");
        let (s, _, b) = post_json(
            &app,
            "/v1/grep",
            &json!({"collections": ["a"], "queries": [{"pattern":"ok"},{"pattern":"(","regex":true}]}),
        )
        .await;
        let e = err(s, &b, 400, "invalid_pattern");
        assert_eq!(e["details"]["index"], 1);
        let (s, _, b) = post_json(
            &app,
            "/v1/grep",
            &json!({"collections": ["a"], "queries": ok_q, "bogus": 1}),
        )
        .await;
        err(s, &b, 400, "invalid_json");
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/grep",
                Some("text/plain"),
                br#"{"collections":["a"],"queries":[{"pattern":"x"}]}"#.to_vec(),
            ),
        )
        .await;
        err(s, &b, 415, "unsupported_media_type");
        let (s, _, b) = post_json(&app, "/v1/grep", &json!({"collections": ["a"], "queries": []}))
            .await;
        err(s, &b, 400, "invalid_query");
        let many: Vec<String> = (0..17).map(|i| format!("c{i}")).collect();
        let (s, _, b) = post_json(&app, "/v1/grep", &json!({"collections": many, "queries": ok_q}))
            .await;
        err(s, &b, 400, "invalid_query");
        let (s, _, b) = get(&app, "/v1/grep").await;
        err(s, &b, 405, "method_not_allowed");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_max_hits_truncated() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        put_needles(&app, "a", 10, 1).await;
        let lines = grep_lines(
            &app,
            &json!({"collections":["a"],"queries":[{"pattern":"needle"}],"max_hits":3}),
        )
        .await;
        assert_eq!(lines_of(&lines, "hit").len(), 3);
        let end = lines.last().unwrap();
        assert_eq!(end["type"], "end");
        assert_eq!(end["truncated"], true);
        assert_eq!(end["hits"], 3);
        assert_eq!(end["collections"][0]["status"], "truncated");
        assert!(end["collections"][0]["error"].is_null());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_events_filter() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        put_needles(&app, "a", 10, 2).await;
        let body = |events: Value| {
            json!({"collections":["a"],"queries":[{"pattern":"needle"}],"events":events})
        };
        let lines = grep_lines(&app, &body(json!([]))).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "end");
        assert_eq!(lines[0]["hits"], 5);
        assert_eq!(lines[0]["collections"][0]["status"], "complete");
        let lines = grep_lines(&app, &body(json!(["hit"]))).await;
        assert_eq!(lines_of(&lines, "hit").len(), 5);
        assert!(lines_of(&lines, "progress").is_empty());
        assert!(lines_of(&lines, "unreadable").is_empty());
        assert_eq!(lines.last().unwrap()["type"], "end");
        let lines = grep_lines(&app, &body(json!(["progress", "end"]))).await;
        assert!(!lines_of(&lines, "progress").is_empty());
        assert!(lines_of(&lines, "hit").is_empty());
        assert_eq!(lines.last().unwrap()["hits"], 5);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_events_invalid() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        let (s, _, b) = post_json(
            &app,
            "/v1/grep",
            &json!({"collections":["a"],"queries":[{"pattern":"x"}],"events":["hits"]}),
        )
        .await;
        let e = err(s, &b, 400, "invalid_json");
        assert!(e["message"].as_str().unwrap().contains("hits"), "{e}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_disconnect_releases_slot() {
    bounded(async {
        let app = TestApp::new().await;
        create(&app, "a").await;
        put_needles(&app, "a", 3, 1).await;
        let body =
            serde_json::to_vec(&json!({"collections":["a"],"queries":[{"pattern":"needle"}]}))
                .unwrap();
        let open = || {
            call_head(
                &app.router,
                req(
                    Method::POST,
                    "/v1/grep",
                    Some("application/json"),
                    body.clone(),
                ),
            )
        };
        let mut open_bodies = Vec::new();
        for _ in 0..masstuffy::MAX_CONCURRENT_GREPS {
            let (s, _, b) = open().await;
            assert_eq!(s, StatusCode::OK);
            open_bodies.push(b);
        }
        let (s, h, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/grep",
                Some("application/json"),
                body.clone(),
            ),
        )
        .await;
        err(s, &b, 503, "too_many_greps");
        assert_eq!(h["retry-after"], "1");
        drop(open_bodies.pop());
        let (s, _, b) = call(
            &app.router,
            req(
                Method::POST,
                "/v1/grep",
                Some("application/json"),
                body.clone(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let lines = ndjson(&b);
        assert_eq!(lines.last().unwrap()["type"], "end");
        assert_eq!(
            lines.last().unwrap()["collections"][0]["status"],
            "complete"
        );
    })
    .await;
}

/// Sends `req` and returns the response head with the unread body.
async fn call_head(
    router: &Router,
    req: axum::http::Request<axum::body::Body>,
) -> (StatusCode, axum::http::HeaderMap, axum::body::Body) {
    use tower::ServiceExt;
    let resp = router.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    (parts.status, parts.headers, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn grep_not_cut_by_request_timeout() {
    bounded(async {
        let app = TestApp::with(HttpOptions {
            request_timeout: Duration::from_secs(1),
            ..HttpOptions::default()
        })
        .await;
        create(&app, "a").await;
        put_needles(&app, "a", 1000, 1).await;
        let body =
            serde_json::to_vec(&json!({"collections":["a"],"queries":[{"pattern":"needle"}]}))
                .unwrap();
        let (s, _, mut b) = call_head(
            &app.router,
            req(Method::POST, "/v1/grep", Some("application/json"), body),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let mut out = Vec::new();
        let first = std::future::poll_fn(|cx| {
            use http_body::Body as _;
            std::pin::Pin::new(&mut b).poll_frame(cx)
        })
        .await
        .unwrap()
        .unwrap();
        out.extend_from_slice(&first.into_data().unwrap());
        // The first frame hit the size cap: the grep is still streaming, no `end` yet.
        assert!(!String::from_utf8_lossy(&out).contains(r#""type":"end""#));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        out.extend_from_slice(&crate::testutil::collect(b).await);
        let lines = ndjson(&Bytes::from(out));
        assert_eq!(lines.last().unwrap()["type"], "end");
        assert_eq!(lines.last().unwrap()["hits"], 1000);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_grep_limits() {
    bounded(async {
        let app = TestApp::new().await;
        let (s, _, b) = get(&app, "/v1/status").await;
        assert_eq!(s, StatusCode::OK);
        let g = &json(&b)["limits"]["grep"];
        assert_eq!(g["max_queries"], 64);
        assert_eq!(g["max_pattern_bytes"], 4096);
        assert_eq!(g["max_collections"], 16);
        assert_eq!(g["max_hits"], 1_000_000);
        assert_eq!(g["default_max_hits"], 10_000);
        assert_eq!(g["max_concurrent"], 32);
    })
    .await;
}
