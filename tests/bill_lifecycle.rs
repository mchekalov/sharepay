//! HTTP-level integration test for the QR/Session Management routes
//! (spec §2), exercising the router the same way a browser would: real
//! `Request`/`Response` objects, cookies threaded manually between calls
//! (mirroring what a browser's cookie jar does automatically).
//!
//! Bridges the OCR-shaped gap in the state machine the same way the
//! in-code doc comments describe: `bill::force_status` and a direct
//! `pricing::api::add_items` call stand in for "the Receipt Recognizer
//! parsed some items and reconciled the total" (out of scope for this
//! component — see `src/routes/bill.rs` module docs).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use sharepay::db::{init_pool, DbConfig};
use sharepay::pricing::api::{add_items, NewItem};
use sharepay::receipt::TesseractEngine;
use sharepay::routes::{router, AppState};
use sharepay::{bill, pricing};
use tower::ServiceExt;

async fn test_app() -> (axum::Router, sqlx::SqlitePool) {
    let pool = init_pool(&DbConfig::in_memory()).await.unwrap();
    // Real Tesseract engine (relies on this dev machine's Homebrew install
    // resolving `eng.traineddata` via its compiled-in default path — see
    // `src/main.rs`'s `TESSDATA_PREFIX_ENV_VAR` docs). This test file
    // doesn't exercise `POST /b/{id}/photo` itself (that's covered in
    // `tests/receipt_photo.rs`), so the engine is constructed but never
    // called here.
    let ocr_engine: Arc<dyn sharepay::receipt::OcrEngine> =
        Arc::new(TesseractEngine::new(None, "eng").expect("Tesseract engine should initialize"));
    let state = AppState {
        pool: pool.clone(),
        base_url: "https://sharepay.example".to_string(),
        ocr_engine,
    };
    (router(state), pool)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Pulls `<p>bill_id=...</p>` out of the placeholder HTML `POST /bills`
/// returns — good enough for a test harness, since real templates are the
/// Mobile Client's job.
fn extract_between<'a>(haystack: &'a str, start: &str, end: &str) -> &'a str {
    let after_start = &haystack[haystack.find(start).unwrap() + start.len()..];
    &after_start[..after_start.find(end).unwrap()]
}

/// Extracts just the `name=value` portion of a `Set-Cookie` header (drops
/// attributes), suitable for echoing back in a `Cookie` request header the
/// way a browser would.
fn cookie_name_value(set_cookie_header: &str) -> String {
    set_cookie_header.split(';').next().unwrap().to_string()
}

fn all_set_cookies(response: &axum::response::Response) -> Vec<String> {
    response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .map(|v| cookie_name_value(v.to_str().unwrap()))
        .collect()
}

#[tokio::test]
async fn full_bill_lifecycle_over_http() {
    let (app, pool) = test_app().await;

    // 1. POST /bills -> draft, immediately advanced to pending_ocr (real
    //    transition, waiting for the host's first photo), host cookie
    //    issued.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/bills")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let host_cookies = all_set_cookies(&resp);
    assert_eq!(host_cookies.len(), 1, "expected exactly one host cookie");
    let host_cookie = host_cookies[0].clone();
    let body = body_text(resp).await;
    let bill_id = extract_between(&body, "bill_id=", "</p>").to_string();
    assert_eq!(bill_id.len(), 22, "bill_id should be a 22-char token");

    // Confirm the stubbed transition landed on pending_ocr.
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("pending_ocr")
    );

    // 2. Bridge the OCR gap (out of scope for this component): manually
    //    add an item and force the bill into pending_confirmation, as if
    //    OCR had parsed+reconciled it.
    let item_ids = add_items(
        &pool,
        &bill_id,
        vec![NewItem {
            name: "Burger".into(),
            price_cents: 1200,
        }],
    )
    .await
    .unwrap();
    let item_id = item_ids[0];
    bill::force_status(&pool, &bill_id, "pending_confirmation")
        .await
        .unwrap();

    // 3. Host-gated routes reject a missing/garbage host cookie
    //    generically (401), and reject an unknown bill_id as 404.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/b/this-bill-does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // 4. POST /b/{id}/confirm with the host cookie -> pending_confirmation
    //    -> open, QR SVG in the response.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("<svg"), "confirm response should embed the QR SVG");
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("open")
    );

    // Idempotent double-confirm.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. GET /b/{id}/qr.svg is now servable.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/qr.svg"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
        "image/svg+xml"
    );

    // 6. GET /b/{id} with no participant cookie -> name entry form.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("What's your name?"));

    // 7. POST /b/{id}/join -> participant cookie issued, redirect back to
    //    the clean bill URL.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/join"))
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("display_name=Alice"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let participant_cookies = all_set_cookies(&resp);
    assert_eq!(participant_cookies.len(), 1);
    let participant_cookie = participant_cookies[0].clone();

    // 8. GET /b/{id}/items (poll) — item present, unmarked.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/items"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &resp.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    assert_eq!(body["items"][0]["is_marked_by_me"], false);
    assert_eq!(body["my_total"], 0);

    // 9. POST /b/{id}/items/{item_id}/mark — marks it, my_total updates.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/items/{item_id}/mark"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &resp.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    assert_eq!(body["my_total"], 1200);
    assert_eq!(body["items"][0]["is_marked_by_me"], true);

    // Marking/unmarking without a participant cookie is rejected.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/items/{item_id}/mark"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // 10. DELETE .../mark — unmarks it.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/b/{bill_id}/items/{item_id}/mark"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 11. POST /b/{id}/close, host-gated, then closed-summary view for the
    //     participant.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/close"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("closed")
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("closed"));

    // Joining a closed bill is rejected at the pricing layer (mapped to
    // 409 Conflict here).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/join"))
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from("display_name=Bob"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Sanity: pricing module is reachable from this integration test too.
    let _ = pricing::PriceDistributorError::NotFound;
}

#[tokio::test]
async fn host_recovery_link_sets_cookie_and_redirects() {
    let (app, pool) = test_app().await;

    let bill_id = "recovery-test-bill";
    let host_token = sharepay::bill::token::generate_token();
    pricing::api::create_bill(&pool, bill_id, &sharepay::bill::token::hash_token(&host_token))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/host/{host_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get(axum::http::header::LOCATION).unwrap(),
        &format!("/b/{bill_id}/host")
    );
    assert!(resp.headers().get(axum::http::header::SET_COOKIE).is_some());

    // Wrong token -> generic 401, not a leak about which part was wrong.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/host/not-the-real-token-000000"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
