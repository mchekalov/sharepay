//! `POST /b/{id}/photo` — the Receipt Recognizer's HTTP surface (spec
//! §1.5): host-only multipart photo upload, running preprocess -> OCR ->
//! parse -> reconcile and driving the `pending_ocr <-> awaiting_photo_retry
//! -> pending_confirmation` transitions (spec §1.6/§2.5).
//!
//! ## Sync vs. async OCR (spec §1.5's open decision point)
//!
//! This implementation runs the pipeline **synchronously** within the
//! request (blocking work off-loaded to a `spawn_blocking` thread, with an
//! overall [`OCR_TIMEOUT`] budget) rather than the spec's `202 Accepted` +
//! poll pattern. Chosen because: (a) it's the spec's own stated v1
//! recommendation when OCR latency is untested ("recommend synchronous OCR
//! as the default... revisit only if real-world OCR latency proves it
//! necessary"), and (b) manual local testing on this dev machine (Tesseract
//! 5.5.3 on a small receipt-sized image after preprocessing) completes in
//! well under a second — nowhere near proxy-timeout territory. If
//! production VPS latency proves this wrong, the fix is localized: swap
//! this handler's direct `await` for a `202` response plus a
//! `GET .../receipt-status` poll target, without touching
//! `preprocess`/`parser`/`reconcile`.
//!
//! ## Error handling (spec §1.6)
//!
//! Every failure reason from the spec's table is represented in
//! [`PhotoUploadError`] and mapped to a JSON body of the shape
//! `{"reason": "...", "message": "..."}` plus best-effort extra fields
//! (`computed_sum_cents`/`discrepancy_cents` for mismatches). The bill row
//! and any participants (none exist yet pre-open, but the principle holds)
//! are never touched on failure — only a successful parse ever writes
//! items or flips status.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Serialize;

use crate::bill;
use crate::host_auth;
use crate::pricing::api::{self as pricing_api, NewItem};
use crate::receipt::parser::{self, ConfidenceBucket, LineCategory};
use crate::receipt::preprocess::{self, PreprocessError};
use crate::receipt::reconcile::{self, OverallConfidence, ReconcileInput, ReconcileOutcome};
use crate::receipt::{OcrEngine, OcrError};
use crate::routes::bill::AppState;

/// Max accepted upload size (spec §1.6: "Upload too large... 400 before
/// processing"). 15 MB comfortably covers a modern phone-camera JPEG while
/// bounding memory/latency.
const MAX_UPLOAD_BYTES: usize = 15 * 1024 * 1024;

/// Overall OCR budget (spec §1.6: "OCR timeout (~15-20s budget via
/// `tokio::time::timeout`)").
const OCR_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------
// Error handling (spec §1.6)
// ---------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum PhotoUploadError {
    /// Bill not found / expired, or wrong status for a photo upload —
    /// mapped generically like the rest of the QR/Session component's
    /// error handling (spec §2.7).
    NotFound(String),
    Unauthorized,
    Conflict(String),
    /// `400` before any processing: bad multipart shape, missing field,
    /// oversized upload, or an unrecognized content-type.
    BadRequest(String),
    ImageUnreadable,
    NoTextDetected,
    OcrTimeout,
    NoReceiptDetected,
    ReceiptMismatch {
        computed_sum_cents: i64,
        recognized_total_cents: i64,
        discrepancy_cents: i64,
    },
    InternalError(String),
}

#[derive(Serialize)]
struct ErrorBody {
    reason: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    computed_sum_cents: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recognized_total_cents: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    discrepancy_cents: Option<i64>,
}

impl IntoResponse for PhotoUploadError {
    fn into_response(self) -> Response {
        let (status, reason, message, computed_sum_cents, recognized_total_cents, discrepancy_cents) =
            match self {
                PhotoUploadError::NotFound(msg) => {
                    (StatusCode::NOT_FOUND, "not_found", msg, None, None, None)
                }
                PhotoUploadError::Unauthorized => (
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "you don't have host access to this bill".to_string(),
                    None,
                    None,
                    None,
                ),
                PhotoUploadError::Conflict(msg) => {
                    (StatusCode::CONFLICT, "conflict", msg, None, None, None)
                }
                PhotoUploadError::BadRequest(msg) => {
                    (StatusCode::BAD_REQUEST, "bad_request", msg, None, None, None)
                }
                PhotoUploadError::ImageUnreadable => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "image_unreadable",
                    "that photo couldn't be read as an image — try again".to_string(),
                    None,
                    None,
                    None,
                ),
                PhotoUploadError::NoTextDetected => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no_text_detected",
                    "no text was detected in that photo — try better lighting or a closer shot"
                        .to_string(),
                    None,
                    None,
                    None,
                ),
                PhotoUploadError::OcrTimeout => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ocr_timeout",
                    "reading the receipt took too long — please try again".to_string(),
                    None,
                    None,
                    None,
                ),
                PhotoUploadError::NoReceiptDetected => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no_receipt_detected",
                    "that doesn't look like a receipt — no total could be found".to_string(),
                    None,
                    None,
                    None,
                ),
                PhotoUploadError::ReceiptMismatch {
                    computed_sum_cents,
                    recognized_total_cents,
                    discrepancy_cents,
                } => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "receipt_mismatch",
                    format!(
                        "the items we found add up to {computed_sum_cents} cents, but the \
                         printed total is {recognized_total_cents} cents — try another photo"
                    ),
                    Some(computed_sum_cents),
                    Some(recognized_total_cents),
                    Some(discrepancy_cents),
                ),
                PhotoUploadError::InternalError(msg) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    msg,
                    None,
                    None,
                    None,
                ),
            };
        (
            status,
            Json(ErrorBody {
                reason,
                message,
                computed_sum_cents,
                recognized_total_cents,
                discrepancy_cents,
            }),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------
// Success response shapes (spec §1.5's `needs_confirmation` fragment
// fields, §1.7's confidence exposure)
// ---------------------------------------------------------------------

#[derive(Serialize)]
struct ItemConfidenceJson {
    name: String,
    price_cents: i64,
    quantity_hint: Option<u32>,
    confidence: &'static str,
}

#[derive(Serialize)]
struct PhotoUploadResponse {
    status: &'static str,
    items: Vec<ItemConfidenceJson>,
    recognized_total_cents: i64,
    tax_tip_amount_cents: i64,
    overall_confidence: &'static str,
    tax_tip_unconfirmed: bool,
}

fn confidence_label(bucket: ConfidenceBucket) -> &'static str {
    match bucket {
        ConfidenceBucket::Low => "low",
        ConfidenceBucket::High => "high",
    }
}

// ---------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------

/// `POST /b/{id}/photo` — host-only. Accepts a multipart upload with a
/// `photo` field. Valid starting states: `pending_ocr` (first attempt) and
/// `awaiting_photo_retry` (retry after a mismatch) — the same handler
/// serves both (spec §1.5).
pub(crate) async fn upload_photo(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, PhotoUploadError> {
    require_host(&state.pool, &jar, &bill_id).await?;

    let status = bill::get_status(&state.pool, &bill_id)
        .await
        .map_err(|e| PhotoUploadError::InternalError(e.to_string()))?
        .ok_or_else(|| PhotoUploadError::NotFound("this bill link isn't valid or has expired".into()))?;

    if !bill::PHOTO_UPLOAD_STATUSES.contains(&status.as_str()) {
        return Err(PhotoUploadError::Conflict(format!(
            "bill is not waiting for a photo upload (status={status})"
        )));
    }

    let photo_bytes = extract_photo_field(&mut multipart).await?;
    if photo_bytes.is_empty() {
        return Err(PhotoUploadError::BadRequest("empty photo upload".into()));
    }
    if photo_bytes.len() > MAX_UPLOAD_BYTES {
        return Err(PhotoUploadError::BadRequest(format!(
            "photo exceeds the {}MB upload limit",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        )));
    }

    let outcome = run_pipeline(state.ocr_engine.clone(), photo_bytes).await?;

    match outcome {
        PipelineOutcome::Success {
            items,
            recognized_total_cents,
            tax_tip_amount_cents,
            overall_confidence,
            tax_tip_unconfirmed,
        } => {
            let new_items: Vec<NewItem> = items
                .iter()
                .map(|i| NewItem {
                    name: i.name.clone(),
                    price_cents: i.price_cents,
                })
                .collect();
            pricing_api::add_items(&state.pool, &bill_id, new_items)
                .await
                .map_err(|e| PhotoUploadError::InternalError(e.to_string()))?;

            bill::mark_pending_confirmation(
                &state.pool,
                &bill_id,
                recognized_total_cents,
                tax_tip_amount_cents,
            )
            .await
            .map_err(|e| PhotoUploadError::InternalError(e.to_string()))?;

            let response = PhotoUploadResponse {
                status: "pending_confirmation",
                items: items
                    .into_iter()
                    .map(|i| ItemConfidenceJson {
                        name: i.name,
                        price_cents: i.price_cents,
                        quantity_hint: i.quantity_hint,
                        confidence: confidence_label(i.confidence),
                    })
                    .collect(),
                recognized_total_cents,
                tax_tip_amount_cents,
                overall_confidence: match overall_confidence {
                    OverallConfidence::High => "high",
                    OverallConfidence::Low => "low",
                },
                tax_tip_unconfirmed,
            };
            Ok((StatusCode::OK, Json(response)).into_response())
        }
        PipelineOutcome::Mismatch {
            computed_sum_cents,
            recognized_total_cents,
            discrepancy_cents,
        } => {
            bill::mark_awaiting_photo_retry(&state.pool, &bill_id)
                .await
                .map_err(|e| PhotoUploadError::InternalError(e.to_string()))?;
            Err(PhotoUploadError::ReceiptMismatch {
                computed_sum_cents,
                recognized_total_cents,
                discrepancy_cents,
            })
        }
    }
}

async fn extract_photo_field(multipart: &mut Multipart) -> Result<Vec<u8>, PhotoUploadError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| PhotoUploadError::BadRequest(format!("invalid multipart upload: {e}")))?
    {
        if field.name() == Some("photo") {
            let bytes = field
                .bytes()
                .await
                .map_err(|e| PhotoUploadError::BadRequest(format!("failed to read upload: {e}")))?;
            return Ok(bytes.to_vec());
        }
        // Any other field is ignored — the client is only expected to send
        // one `photo` file field per spec §1.5/§4.3.1's upload form.
    }
    Err(PhotoUploadError::BadRequest(
        "expected a multipart field named \"photo\"".into(),
    ))
}

async fn require_host(pool: &sqlx::SqlitePool, jar: &CookieJar, bill_id: &str) -> Result<(), PhotoUploadError> {
    match host_auth::verify_host(pool, jar, bill_id).await {
        Ok(()) => Ok(()),
        Err(host_auth::HostAuthError::BillNotFound) => Err(PhotoUploadError::NotFound(
            "this bill link isn't valid or has expired".into(),
        )),
        Err(host_auth::HostAuthError::CookieMissing) | Err(host_auth::HostAuthError::TokenMismatch) => {
            Err(PhotoUploadError::Unauthorized)
        }
    }
}

// ---------------------------------------------------------------------
// Pipeline: preprocess -> OCR -> parse -> reconcile
// ---------------------------------------------------------------------

struct PipelineItem {
    name: String,
    price_cents: i64,
    quantity_hint: Option<u32>,
    confidence: ConfidenceBucket,
}

enum PipelineOutcome {
    Success {
        items: Vec<PipelineItem>,
        recognized_total_cents: i64,
        tax_tip_amount_cents: i64,
        overall_confidence: OverallConfidence,
        tax_tip_unconfirmed: bool,
    },
    Mismatch {
        computed_sum_cents: i64,
        recognized_total_cents: i64,
        discrepancy_cents: i64,
    },
}

async fn run_pipeline(
    engine: Arc<dyn OcrEngine>,
    photo_bytes: Vec<u8>,
) -> Result<PipelineOutcome, PhotoUploadError> {
    // Preprocessing is CPU-bound; keep it off the async executor.
    let preprocessed = tokio::task::spawn_blocking(move || preprocess::preprocess(&photo_bytes))
        .await
        .map_err(|e| PhotoUploadError::InternalError(format!("preprocessing task panicked: {e}")))?
        .map_err(|e| match e {
            PreprocessError::Decode(_) => PhotoUploadError::ImageUnreadable,
            PreprocessError::Encode(msg) => PhotoUploadError::InternalError(msg),
        })?;

    // OCR is CPU-bound and potentially slow; run it blocking, with an
    // overall timeout budget (spec §1.6).
    let ocr_call = tokio::task::spawn_blocking(move || engine.recognize_words(&preprocessed));
    let words = match tokio::time::timeout(OCR_TIMEOUT, ocr_call).await {
        Ok(Ok(Ok(words))) => words,
        Ok(Ok(Err(OcrError::Init(msg))))
        | Ok(Ok(Err(OcrError::ImageLoad(msg))))
        | Ok(Ok(Err(OcrError::InvalidOutput(msg)))) => {
            return Err(PhotoUploadError::InternalError(msg));
        }
        Ok(Err(join_err)) => {
            return Err(PhotoUploadError::InternalError(format!(
                "OCR task panicked: {join_err}"
            )));
        }
        Err(_elapsed) => return Err(PhotoUploadError::OcrTimeout),
    };

    if words.is_empty() {
        return Err(PhotoUploadError::NoTextDetected);
    }

    let lines = parser::parse_lines(&words);

    let total = parser::identify_total(&lines).ok_or(PhotoUploadError::NoReceiptDetected)?;

    let item_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.category == LineCategory::Item)
        .collect();
    if item_lines.is_empty() {
        return Err(PhotoUploadError::NoReceiptDetected);
    }

    let items_sum_cents: i64 = item_lines.iter().filter_map(|l| l.price_cents).sum();
    let subtotal_cents = lines
        .iter()
        .find(|l| l.category == LineCategory::Subtotal)
        .and_then(|l| l.price_cents);
    let tax_line_cents = lines
        .iter()
        .find(|l| matches!(l.category, LineCategory::Tax | LineCategory::TipService))
        .and_then(|l| l.price_cents);

    let reconcile_input = ReconcileInput {
        items_sum_cents,
        num_items: item_lines.len(),
        subtotal_cents,
        tax_line_cents,
        total_cents: total.price_cents,
    };

    match reconcile::reconcile(&reconcile_input) {
        ReconcileOutcome::Mismatch {
            computed_sum_cents,
            discrepancy_cents,
        } => Ok(PipelineOutcome::Mismatch {
            computed_sum_cents,
            recognized_total_cents: total.price_cents,
            discrepancy_cents,
        }),
        ReconcileOutcome::Success {
            tax_tip_amount_cents,
            overall_confidence,
            tax_tip_unconfirmed,
        } => {
            let items = item_lines
                .into_iter()
                .filter_map(|l| {
                    let price_cents = l.price_cents?;
                    let name = l.name.clone().unwrap_or_default();
                    Some(PipelineItem {
                        name,
                        price_cents,
                        quantity_hint: l.quantity_hint,
                        confidence: parser::confidence_bucket(l.mean_confidence),
                    })
                })
                .collect();
            Ok(PipelineOutcome::Success {
                items,
                recognized_total_cents: total.price_cents,
                tax_tip_amount_cents,
                overall_confidence,
                tax_tip_unconfirmed,
            })
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/b/{bill_id}/photo", post(upload_photo))
        .with_state(state)
}
