//! HTTP-level integration test for the Receipt Recognizer's
//! `POST /b/{id}/photo` endpoint (spec §1.5), exercising the *real*
//! pipeline end to end: preprocess -> Tesseract OCR -> parse -> reconcile
//! -> item persistence -> bill status transition -> HTML rendering (spec
//! §4.3.3/§4.3.4). Unlike `tests/bill_lifecycle.rs` (which bridges the OCR
//! gap with a direct `add_items` call + `force_status`), this test drives
//! the actual `upload_photo` handler with a synthetically rendered
//! receipt-like PNG.
//!
//! The synthetic image is rendered at test time via `imageproc`'s text
//! drawing against a macOS system font (`/System/Library/Fonts/
//! Supplemental/Arial.ttf`) — this ties the "happy path" test to macOS,
//! which is what this dev machine runs. If that font file isn't present
//! (e.g. running on Linux/CI), the happy-path test prints a note and
//! returns early rather than failing — the parsing/reconciliation logic
//! itself is already covered exhaustively by unit tests in
//! `src/receipt/parser.rs` and `src/receipt/reconcile.rs` that don't need
//! a real image or a real OCR pass.
//!
//! The failure-path test (`upload_unreadable_bytes_is_rejected_and_bill_state_persists`)
//! has no such dependency and always runs.

use std::sync::Arc;

use ab_glyph::{FontRef, PxScale};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;
use sharepay::db::{init_pool, DbConfig};
use sharepay::receipt::TesseractEngine;
use sharepay::routes::{router, AppState};
use tower::ServiceExt;

const TEST_FONT_PATH: &str = "/System/Library/Fonts/Supplemental/Arial.ttf";

async fn test_app() -> (axum::Router, sqlx::SqlitePool) {
    let pool = init_pool(&DbConfig::in_memory()).await.unwrap();
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

fn cookie_name_value(set_cookie_header: &str) -> String {
    set_cookie_header.split(';').next().unwrap().to_string()
}

/// Renders a simple receipt-like image: a handful of left-aligned
/// "Name    Price" lines against a white background, black text — a
/// deliberately easy target for Tesseract (high contrast, upright,
/// generously spaced, printed font) since the goal here is to exercise the
/// *pipeline wiring*, not to stress-test OCR accuracy (that's an inherent,
/// separately-acknowledged accuracy ceiling per spec §1.9, not something a
/// synthetic test image can meaningfully validate anyway).
fn render_synthetic_receipt() -> Option<Vec<u8>> {
    let font_bytes = std::fs::read(TEST_FONT_PATH).ok()?;
    let font = FontRef::try_from_slice(&font_bytes).ok()?;

    let mut img = RgbImage::from_pixel(600, 400, Rgb([255, 255, 255]));
    let scale = PxScale::from(32.0);
    let lines = [
        "Burger 12.00",
        "Fries 5.00",
        "Salad 9.00",
        "Subtotal 26.00",
        "Tax 2.00",
        "Total 28.00",
    ];
    for (i, line) in lines.iter().enumerate() {
        draw_text_mut(
            &mut img,
            Rgb([0, 0, 0]),
            30,
            30 + (i as i32) * 55,
            scale,
            &font,
            line,
        );
    }

    let mut out = Vec::new();
    DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    Some(out)
}

fn multipart_body(boundary: &str, field_name: &str, filename: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{field_name}\"; filename=\"{filename}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Creates a bill via `POST /bills`, returning `(bill_id, host_cookie)`.
/// The route now issues a `303` redirect to `/b/{id}/host` (spec §4.3.1)
/// rather than a body containing the bill id, so the id is pulled off the
/// `Location` header instead.
async fn create_bill(app: &axum::Router) -> (String, String) {
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
    let host_cookie = cookie_name_value(
        resp.headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap(),
    );
    let location = resp
        .headers()
        .get(axum::http::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    let bill_id = location
        .strip_prefix("/b/")
        .unwrap()
        .strip_suffix("/host")
        .unwrap()
        .to_string();
    (bill_id, host_cookie)
}

#[tokio::test]
async fn full_pipeline_against_a_synthetic_receipt_image() {
    let Some(photo_bytes) = render_synthetic_receipt() else {
        eprintln!(
            "skipping: test font not found at {TEST_FONT_PATH} (this test is macOS-specific; \
             parsing/reconciliation logic is covered by unit tests independent of a real image)"
        );
        return;
    };

    let (app, pool) = test_app().await;
    let (bill_id, host_cookie) = create_bill(&app).await;

    assert_eq!(
        sharepay::bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("pending_ocr")
    );

    let boundary = "test-boundary-sharepay";
    let body = multipart_body(boundary, "photo", "receipt.png", &photo_bytes);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/photo"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = resp.status();
    let html = body_text(resp).await;

    // OCR accuracy on a synthetically rendered image is not guaranteed to
    // be perfect, so reconciliation might land on either outcome. What
    // this test actually verifies is that the pipeline runs end to end
    // without a *pipeline-level* failure (image_unreadable/no_text_detected/
    // no_receipt_detected/internal_error/ocr_timeout would all indicate a
    // real bug, not just OCR imprecision) — both outcomes render `200 OK`
    // HTML (the review screen on success, the retry screen on mismatch),
    // distinguished by which heading is present.
    let bill_status_after = sharepay::bill::get_status(&pool, &bill_id).await.unwrap();
    assert_eq!(status, StatusCode::OK, "unexpected status; body={html}");
    if html.contains("Does this look right?") {
        assert_eq!(bill_status_after.as_deref(), Some("pending_confirmation"));
        println!(
            "full_pipeline_against_a_synthetic_receipt_image: OCR succeeded, review screen rendered"
        );
    } else if html.contains("We couldn't quite read that receipt") {
        assert_eq!(bill_status_after.as_deref(), Some("awaiting_photo_retry"));
        println!(
            "full_pipeline_against_a_synthetic_receipt_image: OCR ran but reconciliation \
             mismatched (acceptable — synthetic-image OCR accuracy isn't guaranteed)"
        );
    } else {
        panic!(
            "unexpected pipeline-level failure — neither the review nor retry screen rendered: \
             {html}"
        );
    }
}

#[tokio::test]
async fn upload_unreadable_bytes_is_rejected_and_bill_state_persists() {
    let (app, pool) = test_app().await;
    let (bill_id, host_cookie) = create_bill(&app).await;

    let boundary = "test-boundary-sharepay";
    let body = multipart_body(boundary, "photo", "not-an-image.png", b"this is not a valid png");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/photo"))
                .header(axum::http::header::COOKIE, &host_cookie)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // The pipeline-level failure renders the retry screen with 200 OK
    // (spec §4.3.3) — it's a legitimate UI state, not a transport error.
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("We couldn"));
    assert!(html.contains("read as an image"));

    // The bill (and, transitively, any participants -- none exist yet
    // pre-open) persists untouched: still pending_ocr, no bill_id/QR
    // invalidation, ready to accept another upload attempt (spec §1.6).
    assert_eq!(
        sharepay::bill::get_status(&pool, &bill_id).await.unwrap().as_deref(),
        Some("pending_ocr")
    );
}

#[tokio::test]
async fn upload_without_host_cookie_is_unauthorized() {
    let (app, _pool) = test_app().await;
    let (bill_id, _host_cookie) = create_bill(&app).await;

    let boundary = "test-boundary-sharepay";
    let body = multipart_body(boundary, "photo", "x.png", b"irrelevant");

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/b/{bill_id}/photo"))
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
