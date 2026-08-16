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

use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use axum_extra::extract::cookie::CookieJar;

use crate::bill;
use crate::config::Currency;
use crate::host_auth;
use crate::pricing::api::{self as pricing_api, NewItem};
use crate::receipt::parser::ConfidenceBucket;
use crate::receipt::preprocess::{self, PreprocessError};
use crate::receipt::reconcile::{self, OverallConfidence, ReconcileInput, ReconcileOutcome};
use crate::receipt::{OcrError, ReceiptEngine};
use crate::routes::bill::AppState;
use crate::templates::{self, render, ErrorTemplate, HostUploadTemplate};

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
    ImageUnreadable { bill_id: String },
    NoTextDetected { bill_id: String },
    OcrTimeout { bill_id: String },
    NoReceiptDetected { bill_id: String },
    ReceiptMismatch {
        bill_id: String,
        computed_sum_cents: i64,
        recognized_total_cents: i64,
        // Kept for API-shape parity with the pipeline's computed value and
        // for `Debug`/logging visibility even though the retry page's copy
        // (spec §4.3.3) only needs the two totals it's derived from.
        #[allow(dead_code)]
        discrepancy_cents: i64,
        /// Threaded through from `AppState::currency` at construction time
        /// — `into_response()` has no `AppState` access (it's an
        /// `IntoResponse` impl, not a handler), so this can't be read at
        /// render time the way route handlers read `state.currency`.
        currency: Currency,
    },
    InternalError(String),
}

/// Renders [`PhotoUploadError`] to HTML (spec §4.3.3's retry-photo screen
/// for every pipeline-level outcome; a generic error page for the
/// administrative failure modes that shouldn't occur in normal use).
///
/// Pipeline outcomes that represent a legitimate, expected UI state (the
/// host needs to retry the photo) render with `200 OK` — an HTMX
/// `hx-target="body" hx-swap="outerHTML"` full-page swap only applies on
/// success by default, and this *is* the successful rendering of the
/// "please retry" screen, not a transport failure. Administrative errors
/// (missing auth, unknown bill, oversized upload) keep real non-2xx status
/// codes, since those aren't states the upload form's own flow produces.
impl IntoResponse for PhotoUploadError {
    fn into_response(self) -> Response {
        match self {
            PhotoUploadError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                render(ErrorTemplate {
                    title: "Not found".to_string(),
                    message: msg,
                }),
            )
                .into_response(),
            PhotoUploadError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                render(ErrorTemplate {
                    title: "Not authorized".to_string(),
                    message: "you don't have host access to this bill".to_string(),
                }),
            )
                .into_response(),
            PhotoUploadError::Conflict(msg) => (
                StatusCode::CONFLICT,
                render(ErrorTemplate {
                    title: "Can't do that".to_string(),
                    message: msg,
                }),
            )
                .into_response(),
            PhotoUploadError::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                render(ErrorTemplate {
                    title: "Bad request".to_string(),
                    message: msg,
                }),
            )
                .into_response(),
            PhotoUploadError::ImageUnreadable { bill_id } => retry_page(
                bill_id,
                "That photo couldn't be read as an image — try again.",
            ),
            PhotoUploadError::NoTextDetected { bill_id } => retry_page(
                bill_id,
                "No text was detected in that photo — try better lighting or a closer shot.",
            ),
            PhotoUploadError::OcrTimeout { bill_id } => {
                retry_page(bill_id, "Reading the receipt took too long — please try again.")
            }
            PhotoUploadError::NoReceiptDetected { bill_id } => retry_page(
                bill_id,
                "That doesn't look like a receipt — no total could be found.",
            ),
            PhotoUploadError::ReceiptMismatch {
                bill_id,
                computed_sum_cents,
                recognized_total_cents,
                currency,
                ..
            } => retry_page(
                bill_id,
                &format!(
                    "The items we found add up to {}, but the printed total is {}. \
                     Can you try another photo? Good lighting and a flat receipt help a lot.",
                    templates::fmt_cents(computed_sum_cents, currency),
                    templates::fmt_cents(recognized_total_cents, currency),
                ),
            ),
            PhotoUploadError::InternalError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                render(ErrorTemplate {
                    title: "Something broke".to_string(),
                    message: msg,
                }),
            )
                .into_response(),
        }
    }
}

fn retry_page(bill_id: String, message: &str) -> Response {
    (
        StatusCode::OK,
        render(HostUploadTemplate {
            bill_id,
            retry_message: Some(message.to_string()),
        }),
    )
        .into_response()
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

    let outcome = run_pipeline(state.receipt_engine.clone(), photo_bytes, &bill_id).await?;

    match outcome {
        PipelineOutcome::Success {
            items,
            recognized_total_cents,
            tax_tip_amount_cents,
            overall_confidence: _,
            tax_tip_unconfirmed: _,
        } => {
            // Quantity hints fold into the display name (spec §4.3.4's
            // mockup shows "Sparkling Water x2") since the schema has no
            // separate `quantity` column (spec §1.9: deliberate
            // simplification — the parsed price is already the line
            // total, never multiplied).
            let new_items: Vec<NewItem> = items
                .iter()
                .map(|i| NewItem {
                    name: match i.quantity_hint {
                        Some(q) if q > 1 => format!("{} x{q}", i.name),
                        _ => i.name.clone(),
                    },
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

            let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None)
                .await
                .map_err(|e| PhotoUploadError::InternalError(e.to_string()))?;
            Ok((
                StatusCode::OK,
                render(templates::host_review_page(&bill_id, &bill_state, state.currency)),
            )
                .into_response())
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
                bill_id,
                computed_sum_cents,
                recognized_total_cents,
                discrepancy_cents,
                currency: state.currency,
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
    // Per-item confidence (spec §1.7) isn't surfaced in the review-screen
    // UI yet (a v2 enhancement: highlighting likely-wrong rows) — computed
    // and threaded through regardless, so wiring it into the template
    // later is a template-only change. `None` for engines with no
    // per-item confidence signal (e.g. Claude).
    #[allow(dead_code)]
    confidence: Option<ConfidenceBucket>,
}

enum PipelineOutcome {
    Success {
        items: Vec<PipelineItem>,
        recognized_total_cents: i64,
        tax_tip_amount_cents: i64,
        // Same "not surfaced yet" note as `PipelineItem::confidence`.
        #[allow(dead_code)]
        overall_confidence: OverallConfidence,
        #[allow(dead_code)]
        tax_tip_unconfirmed: bool,
    },
    Mismatch {
        computed_sum_cents: i64,
        recognized_total_cents: i64,
        discrepancy_cents: i64,
    },
}

async fn run_pipeline(
    engine: Arc<dyn ReceiptEngine>,
    photo_bytes: Vec<u8>,
    bill_id: &str,
) -> Result<PipelineOutcome, PhotoUploadError> {
    // Preprocessing is CPU-bound; keep it off the async executor. Only the
    // engine-agnostic "light" steps (decode, orient, downscale, still
    // color) happen here — engine-specific finishing (e.g. Tesseract's
    // grayscale/binarization) is each `ReceiptEngine` impl's own job, so a
    // vision-model engine isn't handed a Tesseract-tuned binarized image.
    let preprocessed = tokio::task::spawn_blocking(move || preprocess::preprocess_light(&photo_bytes))
        .await
        .map_err(|e| PhotoUploadError::InternalError(format!("preprocessing task panicked: {e}")))?
        .map_err(|e| match e {
            PreprocessError::Decode(_) => PhotoUploadError::ImageUnreadable {
                bill_id: bill_id.to_string(),
            },
            PreprocessError::Encode(msg) => PhotoUploadError::InternalError(msg),
        })?;

    // Recognition is potentially slow (CPU-bound for Tesseract, network-bound
    // for an API engine); run it blocking, with an overall timeout budget
    // (spec §1.6).
    let ocr_call = tokio::task::spawn_blocking(move || engine.recognize_receipt(&preprocessed));
    let receipt = match tokio::time::timeout(OCR_TIMEOUT, ocr_call).await {
        Ok(Ok(Ok(receipt))) => receipt,
        Ok(Ok(Err(OcrError::NoTextDetected))) => {
            return Err(PhotoUploadError::NoTextDetected {
                bill_id: bill_id.to_string(),
            });
        }
        Ok(Ok(Err(
            OcrError::Init(msg)
            | OcrError::ImageLoad(msg)
            | OcrError::InvalidOutput(msg)
            | OcrError::RequestFailed(msg),
        ))) => {
            return Err(PhotoUploadError::InternalError(msg));
        }
        Ok(Err(join_err)) => {
            return Err(PhotoUploadError::InternalError(format!(
                "OCR task panicked: {join_err}"
            )));
        }
        Err(_elapsed) => {
            return Err(PhotoUploadError::OcrTimeout {
                bill_id: bill_id.to_string(),
            })
        }
    };

    if receipt.items.is_empty() {
        return Err(PhotoUploadError::NoReceiptDetected {
            bill_id: bill_id.to_string(),
        });
    }

    let total_cents = receipt.total_cents.ok_or_else(|| PhotoUploadError::NoReceiptDetected {
        bill_id: bill_id.to_string(),
    })?;

    let items_sum_cents: i64 = receipt.items.iter().map(|i| i.price_cents).sum();

    let reconcile_input = ReconcileInput {
        items_sum_cents,
        num_items: receipt.items.len(),
        subtotal_cents: receipt.subtotal_cents,
        tax_line_cents: receipt.tax_cents,
        total_cents,
    };

    match reconcile::reconcile(&reconcile_input) {
        ReconcileOutcome::Mismatch {
            computed_sum_cents,
            discrepancy_cents,
        } => Ok(PipelineOutcome::Mismatch {
            computed_sum_cents,
            recognized_total_cents: total_cents,
            discrepancy_cents,
        }),
        ReconcileOutcome::Success {
            tax_tip_amount_cents,
            overall_confidence,
            tax_tip_unconfirmed,
        } => {
            let items = receipt
                .items
                .into_iter()
                .map(|i| PipelineItem {
                    name: i.name,
                    price_cents: i.price_cents,
                    quantity_hint: i.quantity_hint,
                    confidence: i.confidence,
                })
                .collect();
            Ok(PipelineOutcome::Success {
                items,
                recognized_total_cents: total_cents,
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
        // axum's own default body-size limit (2 MB) sits in front of this
        // handler's `MAX_UPLOAD_BYTES` check and applies first. Left at the
        // default, any upload over 2 MB (i.e. most modern phone-camera
        // photos) gets its request body truncated mid-stream, which then
        // fails as an opaque multipart-parse error rather than the clean,
        // informative 400 `MAX_UPLOAD_BYTES` is meant to produce. Raise the
        // ceiling here so that check is actually the one that fires; a
        // small margin above `MAX_UPLOAD_BYTES` accounts for multipart
        // boundary/header framing overhead.
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 1024 * 1024))
        .with_state(state)
}
