//! HTTP-level integration test for the QR/Session Management routes
//! (spec §2) plus the Mobile Web Client's HTML rendering (spec §4),
//! exercising the router the same way a browser would: real
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
use sharepay::receipt::TesseractReceiptEngine;
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
    let receipt_engine: Arc<dyn sharepay::receipt::ReceiptEngine> =
        Arc::new(TesseractReceiptEngine::new(None, "eng").expect("Tesseract engine should initialize"));
    let state = AppState {
        pool: pool.clone(),
        base_url: "https://sharepay.example".to_string(),
        receipt_engine,
        // USD here is just this test file's fixture currency (unrelated to
        // what's under test) — kept so existing "$X.XX" assertions below
        // stay valid.
        currency: sharepay::config::Currency::Usd,
    };
    (router(state), pool)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
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

/// `POST /bills` now issues a `303` redirect to `/b/{id}/host` (spec
/// §4.3.1) rather than rendering the bill id in the body — the id is
/// pulled off the `Location` header instead.
fn bill_id_from_location(response: &axum::response::Response) -> String {
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    // "/b/{id}/host" -> "{id}"
    location
        .strip_prefix("/b/")
        .unwrap()
        .strip_suffix("/host")
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn full_bill_lifecycle_over_http() {
    let (app, pool) = test_app().await;

    // 1. POST /bills -> draft, immediately advanced to pending_ocr (real
    //    transition, waiting for the host's first photo), host cookie
    //    issued, 303 redirect to the host page.
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let host_cookies = all_set_cookies(&resp);
    assert_eq!(host_cookies.len(), 1, "expected exactly one host cookie");
    let host_cookie = host_cookies[0].clone();
    let bill_id = bill_id_from_location(&resp);
    assert_eq!(bill_id.len(), 22, "bill_id should be a 22-char token");

    // Confirm the stubbed transition landed on pending_ocr.
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("pending_ocr")
    );

    // 1b. GET /b/{id}/host with the host cookie renders the upload screen.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/host"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("Photograph your receipt"));
    assert!(body.contains(&format!("/b/{bill_id}/photo")));

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

    // 2b. GET /b/{id}/host now renders the review/edit screen.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/host"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("Does this look right?"));
    assert!(body.contains("Burger"));
    assert!(body.contains(&format!("/b/{bill_id}/confirm")));

    // 3. Host-gated routes reject a missing/garbage host cookie
    //    generically (401), and reject an unknown bill_id as 404.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
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
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("<svg"), "confirm response should embed the QR SVG");
    assert!(body.contains("Ready! Show this to your friends"));
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
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
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

    // 5b. GET /b/{id}/joined-fragment (host-gated) shows nobody yet.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/joined-fragment"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("Nobody has joined yet"));

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

    // 7b. Duplicate name -> re-renders the name-entry form with an inline
    //     error, preserving the entered value (spec §4.4.1).
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
    let body = body_text(resp).await;
    assert!(body.contains("already taken") || body.contains("taken"));
    assert!(body.contains("value=\"Alice\""));

    // 7c. Host's joined-fragment now shows Alice.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/joined-fragment"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(resp).await;
    assert!(body.contains("Alice"));
    assert!(body.contains("1 person"));

    // 8. GET /b/{id}/fragment (poll) — item present, unmarked.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/b/{bill_id}/fragment"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("id=\"bill-fragment\""));
    assert!(body.contains("Burger"));
    assert!(body.contains("Your total: <strong>$0.00</strong>"));

    // 9. POST /b/{id}/items/{item_id}/mark — marks it, my_total updates,
    //    same #bill-fragment shape returned.
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
    let body = body_text(resp).await;
    assert!(body.contains("Your total: <strong>$12.00</strong>"));
    assert!(body.contains("You"));

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
    let body = body_text(resp).await;
    assert!(body.contains("Your total: <strong>$0.00</strong>"));

    // Re-mark it so the bill is fully claimed before closing — closing
    // while anything is unmarked is covered by its own dedicated test,
    // `close_is_blocked_until_every_item_is_marked` below.
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

/// Host item CRUD (spec §4.3.4): add a blank row, edit it via the confirm
/// form, delete a different row.
#[tokio::test]
async fn host_review_screen_add_edit_delete_items() {
    let (app, pool) = test_app().await;

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
    let host_cookie = all_set_cookies(&resp)[0].clone();
    let bill_id = bill_id_from_location(&resp);

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
    let burger_id = item_ids[0];
    bill::force_status(&pool, &bill_id, "pending_confirmation")
        .await
        .unwrap();

    // Add a blank row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/items"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("<tr id=\"item-row-"));
    assert!(body.contains("id=\"totals-footer\" hx-swap-oob=\"true\""));

    let state = sharepay::pricing::api::get_bill_state(&pool, &bill_id, None)
        .await
        .unwrap();
    assert_eq!(state.items.len(), 2, "blank row should have been persisted");
    let blank_id = state.items.iter().find(|i| i.id != burger_id).unwrap().id;

    // Delete the blank row again.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/b/{bill_id}/items/{blank_id}"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let state = sharepay::pricing::api::get_bill_state(&pool, &bill_id, None)
        .await
        .unwrap();
    assert_eq!(state.items.len(), 1);

    // Confirm with an edited name/price for the Burger row.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::from(format!(
                    "item_name_{burger_id}=Cheeseburger&item_price_{burger_id}=13.50"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("open")
    );

    let state = sharepay::pricing::api::get_bill_state(&pool, &bill_id, None)
        .await
        .unwrap();
    let burger = state.items.iter().find(|i| i.id == burger_id).unwrap();
    assert_eq!(burger.name, "Cheeseburger");
    assert_eq!(burger.price_cents, 1350);
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

/// `GET /` renders the landing page, and static files are reachable
/// (spec §4.1/§4.6) — smoke-checks the parts of the router `main.rs`
/// wires up (`ServeDir`) that this in-process router (built directly via
/// `sharepay::routes::router`, bypassing `main.rs`) doesn't itself mount;
/// this test only covers the landing route.
#[tokio::test]
async fn landing_page_renders() {
    let (app, _pool) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("SharePay"));
    assert!(body.contains("Start a new bill"));
}

/// Verifies the host cannot close a bill while any item still has zero
/// markers — closing is blocked outright (409 Conflict), not merely
/// nagged about, per an explicit product decision that overrides the
/// original spec §3.3.1 "nag but never block" design (see
/// `pricing::error::PriceDistributorError::UnclaimedItemsRemain`'s doc
/// comment). One host, one participant. Items are seeded directly (no
/// OCR involved), the same way `full_bill_lifecycle_over_http` bridges
/// the OCR gap.
#[tokio::test]
async fn close_is_blocked_until_every_item_is_marked() {
    let (app, pool) = test_app().await;

    // Host creates the bill.
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
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let host_cookie = all_set_cookies(&resp).remove(0);
    let bill_id = bill_id_from_location(&resp);

    // Bridge the OCR gap: two generated items, forced straight to
    // pending_confirmation.
    let item_ids = add_items(
        &pool,
        &bill_id,
        vec![
            NewItem {
                name: "Burger".into(),
                price_cents: 1200,
            },
            NewItem {
                name: "Fries".into(),
                price_cents: 500,
            },
        ],
    )
    .await
    .unwrap();
    let (burger_id, fries_id) = (item_ids[0], item_ids[1]);
    bill::force_status(&pool, &bill_id, "pending_confirmation")
        .await
        .unwrap();

    // Confirm -> open.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/confirm"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // One participant joins.
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
    let participant_cookie = all_set_cookies(&resp).remove(0);

    // Host tries to close before anything is marked -> blocked, bill
    // stays open.
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
    // A blocked close is a legitimate expected UI state, not an HTTP
    // error: 200 OK, re-rendering the same host page (still open) with an
    // inline "unclaimed" message appended, rather than a separate error
    // page.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_text(resp).await;
    assert!(body.contains("unclaimed"));
    assert_eq!(
        bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("open"),
        "a blocked close must not transition the bill"
    );

    // Alice marks only the Burger — Fries is still unclaimed.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/items/{burger_id}/mark"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Still blocked — Fries is unmarked.
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
        Some("open")
    );

    // Alice marks Fries too — everything is now claimed.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/items/{fries_id}/mark"))
                .header(axum::http::header::COOKIE, &participant_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now closing succeeds.
    let resp = app
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
}
