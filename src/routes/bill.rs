//! HTTP routes implementing the QR/Session Management API surface (spec
//! §2.8), rendering the Mobile Web Client's templates (spec §4). Handlers
//! here own routing, state transitions, cookie/auth gating, **and**
//! presentation — real `askama` templates (`src/templates.rs`,
//! `templates/*.html`), not placeholder `format!` strings.
//!
//! `POST /b/{id}/photo` (the Receipt Recognizer's upload/OCR/reconcile
//! endpoint that actually drives `pending_ocr <-> awaiting_photo_retry ->
//! pending_confirmation`) lives in `src/routes/receipt.rs`, merged into
//! this module's router by `src/routes/mod.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::bill::{self, token::generate_token, token::hash_token};
use crate::config::Currency;
use crate::host_auth::{self, HostAuthError};
use crate::pricing::api::{self as pricing_api, PRE_OPEN_STATUSES};
use crate::pricing::PriceDistributorError;
use crate::qr;
use crate::receipt::ReceiptEngine;
use crate::templates::{
    self, render, ErrorTemplate, HostQrTemplate, HostUploadTemplate, JoinedFragmentTemplate,
    LandingTemplate, NameEntryTemplate, NotReadyTemplate,
};

/// Shared app state threaded through every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    /// Public base URL used to build the join link / QR payload, e.g.
    /// `https://sharepay.example`. No trailing slash required (stripped in
    /// [`qr::join_url`] if present).
    pub base_url: String,
    /// The receipt-recognition backend used by `POST /b/{id}/photo`
    /// (`src/routes/receipt.rs`). `Arc<dyn ReceiptEngine>` rather than a
    /// concrete type so the backend stays swappable (Tesseract vs. Claude,
    /// selected at startup by `config::AppConfig::ocr_engine`) and so the
    /// one long-lived engine instance (expensive to construct — loads
    /// trained-data from disk, or holds an HTTP client) is cheaply cloned
    /// across requests via `AppState::clone()`.
    pub receipt_engine: Arc<dyn ReceiptEngine>,
    /// Currency `src/templates.rs::fmt_cents` formats money in, selected at
    /// startup by `config::AppConfig::currency`.
    pub currency: Currency,
}

// ---------------------------------------------------------------------
// Error handling (spec §2.7)
// ---------------------------------------------------------------------

/// Uniform error type for this module's handlers. Deliberately coarse: the
/// spec calls for generic, non-leaking error responses (404-equivalent for
/// unknown/expired bills, generic 401-equivalent for host-auth failures
/// that never distinguish missing-vs-mismatched, etc.) rather than
/// detailed error taxonomies leaking to the client.
pub enum AppError {
    NotFound(String),
    Unauthorized,
    Conflict(String),
    BadRequest(String),
    Internal(String),
}

impl AppError {
    fn from_db(err: sqlx::Error) -> Self {
        AppError::Internal(format!("database error: {err}"))
    }
}

impl From<PriceDistributorError> for AppError {
    fn from(err: PriceDistributorError) -> Self {
        match err {
            PriceDistributorError::NotFound => AppError::NotFound("not found".into()),
            PriceDistributorError::NameTaken => {
                AppError::Conflict("that name is already taken on this bill".into())
            }
            PriceDistributorError::BillAlreadyOpen => {
                AppError::Conflict("items are locked — the bill is already open".into())
            }
            PriceDistributorError::BillClosed => AppError::Conflict("bill is closed".into()),
            PriceDistributorError::BillNotOpen => {
                AppError::Conflict("bill is not open yet".into())
            }
            PriceDistributorError::InvalidDisplayName => {
                AppError::BadRequest("invalid display name".into())
            }
            PriceDistributorError::IdCollision => AppError::Internal("id collision".into()),
            // Currency-agnostic fallback: the one call site that actually
            // produces this (`close_bill_handler`) intercepts it before
            // the `?`-driven `From` conversion runs, so it can format the
            // amount with `state.currency` (unavailable here — this `From`
            // impl has no `AppState` access).
            PriceDistributorError::UnclaimedItemsRemain { .. } => AppError::Conflict(
                "some items are still unclaimed — ask everyone to mark what they ordered before closing.".into(),
            ),
            PriceDistributorError::Database(e) => AppError::from_db(e),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, "Not found", msg),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Not authorized",
                "you don't have host access to this bill".to_string(),
            ),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "Can't do that", msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "Bad request", msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "Something broke", msg),
        };
        (
            status,
            render(ErrorTemplate {
                title: title.to_string(),
                message,
            }),
        )
            .into_response()
    }
}

/// Verifies the host cookie for `bill_id`, mapping the result onto the two
/// HTTP-visible outcomes spec §2.7 distinguishes: an unknown bill is a
/// plain 404 ("invalid/unknown bill_id"), while a missing *or* mismatched
/// host token collapse into the same generic 401-equivalent ("never
/// distinguish missing vs. mismatched").
async fn require_host(pool: &SqlitePool, jar: &CookieJar, bill_id: &str) -> Result<(), AppError> {
    match host_auth::verify_host(pool, jar, bill_id).await {
        Ok(()) => Ok(()),
        Err(HostAuthError::BillNotFound) => {
            Err(AppError::NotFound("this bill link isn't valid or has expired".into()))
        }
        Err(HostAuthError::CookieMissing) | Err(HostAuthError::TokenMismatch) => {
            Err(AppError::Unauthorized)
        }
    }
}

async fn require_status(pool: &SqlitePool, bill_id: &str) -> Result<String, AppError> {
    match bill::get_status(pool, bill_id)
        .await
        .map_err(AppError::from_db)?
    {
        Some(s) if s != bill::EXPIRED_STATUS => Ok(s),
        _ => Err(AppError::NotFound(
            "this bill link isn't valid or has expired".into(),
        )),
    }
}

// ---------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------

/// Bounded regenerate-retry loop for `bill_id`/`host_token` collisions
/// (spec §2.1) — collision probability at 128 bits is negligible, this is
/// defensive coding only.
const MAX_ID_GENERATION_ATTEMPTS: u32 = 5;

async fn create_bill_with_retry(pool: &SqlitePool) -> Result<(String, String), AppError> {
    for _ in 0..MAX_ID_GENERATION_ATTEMPTS {
        let bill_id = generate_token();
        let host_token = generate_token();
        let host_token_hash = hash_token(&host_token);
        match pricing_api::create_bill(pool, &bill_id, &host_token_hash).await {
            Ok(_) => return Ok((bill_id, host_token)),
            Err(PriceDistributorError::IdCollision) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal(
        "failed to generate a unique bill id after several attempts".into(),
    ))
}

/// `GET /` — landing/upload page (spec §4.3.1).
pub async fn landing() -> impl IntoResponse {
    render(LandingTemplate)
}

/// `POST /bills` — create a bill in `draft`, issue the host cookie, and
/// `303`-redirect to the host page (spec §2.1/§4.3.1: creating the bill
/// first, as its own step, gives the host a stable `bill_id` to attach the
/// photo/OCR-retry loop to; the join URL/QR stays invalid until
/// confirmation regardless).
pub async fn create_bill_handler(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let (bill_id, host_token) = create_bill_with_retry(&state.pool).await?;

    // draft -> pending_ocr: the bill now waits for the host's first photo
    // upload (spec §2.1/§2.5).
    bill::mark_pending_ocr(&state.pool, &bill_id)
        .await
        .map_err(AppError::from_db)?;

    let jar = host_auth::set_host_cookie(jar, &bill_id, &host_token);
    Ok((
        StatusCode::SEE_OTHER,
        jar,
        Redirect::to(&format!("/b/{bill_id}/host")),
    ))
}

/// `GET /b/{id}/host` — host view (upload/retry/review/QR, contextual on
/// status). Host-gated.
pub async fn host_view(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    let status = require_status(&state.pool, &bill_id).await?;

    match status.as_str() {
        "pending_ocr" => Ok(render(HostUploadTemplate {
            bill_id,
            retry_message: None,
        })
        .into_response()),
        "awaiting_photo_retry" => Ok(render(HostUploadTemplate {
            bill_id,
            retry_message: Some(
                "Good lighting and a flat receipt help a lot — give it another shot.".to_string(),
            ),
        })
        .into_response()),
        "pending_confirmation" => {
            let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None).await?;
            Ok(render(templates::host_review_page(&bill_id, &bill_state, state.currency)).into_response())
        }
        "open" => Ok(render(host_qr_template(&state, &bill_id).await?).into_response()),
        "closed" => {
            let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None).await?;
            Ok(render(templates::participant_page(&bill_id, &bill_state, None, state.currency)).into_response())
        }
        other => Err(AppError::Internal(format!("unexpected bill status {other:?}"))),
    }
}

async fn host_qr_template(state: &AppState, bill_id: &str) -> Result<HostQrTemplate, AppError> {
    let url = qr::join_url(&state.base_url, bill_id);
    let svg_xml = qr::render_svg(&url).map_err(|e| AppError::Internal(e.to_string()))?;
    let joined_html = render(joined_fragment_template(&state.pool, bill_id).await?).0;
    Ok(HostQrTemplate {
        bill_id: bill_id.to_string(),
        join_url: url,
        qr_svg: svg_xml,
        joined_html,
    })
}

async fn joined_fragment_template(
    pool: &SqlitePool,
    bill_id: &str,
) -> Result<JoinedFragmentTemplate, AppError> {
    let bill_state = pricing_api::get_bill_state(pool, bill_id, None).await?;
    let names: Vec<String> = bill_state
        .participants
        .iter()
        .map(|p| p.display_name.clone())
        .collect();
    Ok(JoinedFragmentTemplate {
        count: names.len(),
        names,
    })
}

/// `GET /b/{id}/host/{host_token}` — one-time host recovery link: sets the
/// cookie then redirects to the clean host URL (spec §2.3), so the token
/// doesn't linger in the visible address bar/history.
pub async fn host_recovery(
    State(state): State<AppState>,
    Path((bill_id, host_token)): Path<(String, String)>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let stored_hash = bill::get_host_token_hash(&state.pool, &bill_id)
        .await
        .map_err(AppError::from_db)?
        .ok_or_else(|| AppError::NotFound("this bill link isn't valid or has expired".into()))?;

    if hash_token(&host_token) != stored_hash {
        return Err(AppError::Unauthorized);
    }

    let jar = host_auth::set_host_cookie(jar, &bill_id, &host_token);
    Ok((jar, Redirect::to(&format!("/b/{bill_id}/host"))))
}

/// `POST /b/{id}/items` — host-only: append a blank item row (spec §4.3.4's
/// "+ Add item"). Returns the new row's markup (appended `beforeend` into
/// `#items-table tbody`) plus an out-of-band totals-footer swap.
pub async fn add_item_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    let ids = pricing_api::add_items(
        &state.pool,
        &bill_id,
        vec![pricing_api::NewItem {
            name: String::new(),
            price_cents: 0,
        }],
    )
    .await?;
    let item_id = ids[0];
    let row_html = templates::review_row(&bill_id, item_id, "", 0);
    let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None).await?;
    let footer_html = templates::totals_footer(&bill_state, true, state.currency);
    Ok(Html(format!("{row_html}{footer_html}")))
}

/// `DELETE /b/{id}/items/{item_id}` — host-only: remove a row (spec
/// §4.3.4's 🗑 button). Response body is just the out-of-band
/// totals-footer swap; the (empty) remainder is what removes the row via
/// the client's `hx-target="closest tr" hx-swap="outerHTML"`.
pub async fn delete_item_handler(
    State(state): State<AppState>,
    Path((bill_id, item_id)): Path<(String, i64)>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    pricing_api::delete_item(&state.pool, &bill_id, item_id).await?;
    let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None).await?;
    Ok(Html(templates::totals_footer(&bill_state, true, state.currency)))
}

/// `POST /b/{id}/confirm` — host-gated, idempotent on an already-`open`
/// bill (spec §2.7). Applies any in-place item-name/price edits from the
/// review form (spec §4.3.4: "Server re-parses current form values,
/// recomputes the sum") before transitioning `pending_confirmation -> open`.
pub async fn confirm_bill(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
    Form(fields): Form<HashMap<String, String>>,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    let status = require_status(&state.pool, &bill_id).await?;

    if status != "pending_confirmation" && status != "open" {
        return Err(AppError::Conflict(format!(
            "bill is not ready to confirm (status={status})"
        )));
    }

    if status == "pending_confirmation" {
        apply_review_edits(&state.pool, &bill_id, &fields).await?;
    }

    pricing_api::open_bill(&state.pool, &bill_id).await?;

    Ok(render(host_qr_template(&state, &bill_id).await?))
}

/// Parses `item_name_{id}`/`item_price_{id}` fields posted by the review
/// form and persists any that changed via [`pricing_api::update_item`].
/// Price fields are parsed as decimal dollars (matching the
/// `<input type="number" step="0.01">` the client renders) and rounded to
/// the nearest cent.
async fn apply_review_edits(
    pool: &SqlitePool,
    bill_id: &str,
    fields: &HashMap<String, String>,
) -> Result<(), AppError> {
    for (key, value) in fields {
        let Some(id_str) = key.strip_prefix("item_name_") else {
            continue;
        };
        let Ok(item_id) = id_str.parse::<i64>() else {
            continue;
        };
        let name = value.trim();
        if name.is_empty() {
            continue; // never persist a blank name over the OCR'd one
        }
        let price_cents = fields
            .get(&format!("item_price_{item_id}"))
            .and_then(|p| p.trim().parse::<f64>().ok())
            .map(|dollars| (dollars * 100.0).round() as i64)
            .unwrap_or(0);
        pricing_api::update_item(pool, bill_id, item_id, name, price_cents).await?;
    }
    Ok(())
}

/// `POST /b/{id}/close` — `open -> closed`, host-gated, idempotent on an
/// already-`closed` bill.
pub async fn close_bill_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    match pricing_api::close_bill(&state.pool, &bill_id).await {
        Ok(()) => {}
        Err(PriceDistributorError::UnclaimedItemsRemain {
            unassigned_amount_cents,
        }) => {
            return Err(AppError::Conflict(format!(
                "{} of items are still unclaimed — ask everyone to mark what they ordered before closing.",
                templates::fmt_cents(unassigned_amount_cents, state.currency)
            )));
        }
        Err(e) => return Err(e.into()),
    }
    let bill_state = pricing_api::get_bill_state(&state.pool, &bill_id, None).await?;
    Ok(render(templates::participant_page(&bill_id, &bill_state, None, state.currency)))
}

/// `GET /b/{id}/qr.svg` — QR code for the join URL. Only served once
/// `open` (spec §2.2: "shown only after item-list confirmation").
pub async fn qr_svg(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let status = require_status(&state.pool, &bill_id).await?;
    if status != "open" {
        return Err(AppError::Conflict(
            "the QR code is only available once the bill is open".into(),
        ));
    }
    let url = qr::join_url(&state.base_url, &bill_id);
    let svg_xml = qr::render_svg(&url).map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "image/svg+xml")], svg_xml))
}

/// `GET /b/{id}/joined-fragment` — host's live join-count poll target
/// (spec §4.3.5), scoped narrowly to just this fragment. Host-gated (same
/// audience as the QR page it's embedded in).
pub async fn joined_fragment(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    Ok(render(joined_fragment_template(&state.pool, &bill_id).await?))
}

/// `GET /b/{id}` — participant entry point (spec §2.4/§2.7): routes to
/// name-entry, the marking UI, a "not ready yet" placeholder, or the
/// read-only closed summary, based on bill status + participant cookie.
pub async fn bill_entry(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let status = require_status(&state.pool, &bill_id).await?;

    if status == "closed" {
        let participant_id = host_auth::participant_id_from_jar(&jar, &bill_id);
        let bill_state =
            pricing_api::get_bill_state(&state.pool, &bill_id, participant_id.as_deref()).await?;
        return Ok(render(templates::participant_page(
            &bill_id,
            &bill_state,
            participant_id.as_deref(),
            state.currency,
        ))
        .into_response());
    }

    if PRE_OPEN_STATUSES.contains(&status.as_str()) {
        return Ok(render(NotReadyTemplate).into_response());
    }

    // status == "open"
    match host_auth::participant_id_from_jar(&jar, &bill_id) {
        Some(participant_id) => {
            let bill_state =
                pricing_api::get_bill_state(&state.pool, &bill_id, Some(&participant_id)).await?;
            Ok(
                render(templates::participant_page(&bill_id, &bill_state, Some(&participant_id), state.currency))
                    .into_response(),
            )
        }
        None => Ok(render(NameEntryTemplate {
            bill_id,
            error: None,
            name_value: String::new(),
        })
        .into_response()),
    }
}

#[derive(Debug, Deserialize)]
pub struct JoinForm {
    pub display_name: String,
}

/// `POST /b/{id}/join` — name entry, creates the `Participant`, issues the
/// participant cookie. On a duplicate/invalid name, re-renders the name
/// entry form with an inline error and the entered value preserved (spec
/// §4.4.1), rather than a bare error page.
pub async fn join_bill_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
    Form(form): Form<JoinForm>,
) -> Result<Response, AppError> {
    let participant_id = generate_token();
    match pricing_api::join_bill(&state.pool, &bill_id, &participant_id, &form.display_name).await
    {
        Ok(id) => {
            let jar = host_auth::set_participant_cookie(jar, &bill_id, &id);
            Ok((jar, Redirect::to(&format!("/b/{bill_id}"))).into_response())
        }
        Err(PriceDistributorError::NameTaken) => Ok(render(NameEntryTemplate {
            bill_id,
            error: Some("That name's taken — try adding a last initial.".to_string()),
            name_value: form.display_name,
        })
        .into_response()),
        Err(PriceDistributorError::InvalidDisplayName) => Ok(render(NameEntryTemplate {
            bill_id,
            error: Some("Please enter a name.".to_string()),
            name_value: form.display_name,
        })
        .into_response()),
        Err(e) => Err(e.into()),
    }
}

/// `GET /b/{id}/fragment` — polling endpoint (spec §4.4.2/§4.4.3):
/// authoritative item list + marks + your-total, as the bare
/// `#bill-fragment` partial (no page shell). Gated to `open`/`closed`;
/// scopes `is_marked_by_me`/`my_total`/"You" labeling to the participant
/// cookie if present.
pub async fn fragment_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let status = require_status(&state.pool, &bill_id).await?;
    if status != "open" && status != "closed" {
        return Err(AppError::Conflict(format!(
            "bill is not open or closed yet (status={status})"
        )));
    }
    let participant_id = host_auth::participant_id_from_jar(&jar, &bill_id);
    let bill_state =
        pricing_api::get_bill_state(&state.pool, &bill_id, participant_id.as_deref()).await?;
    Ok(render(templates::bill_fragment(
        &bill_id,
        &bill_state,
        participant_id.as_deref(),
        state.currency,
    )))
}

/// `POST /b/{id}/items/{item_id}/mark` — toggle on. Participant-cookie
/// gated, `open` only. Returns the same `#bill-fragment` partial the poll
/// endpoint does (spec §4.4.3: "Response is the full bill-fragment").
pub async fn mark_item_handler(
    State(state): State<AppState>,
    Path((bill_id, item_id)): Path<(String, i64)>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let participant_id =
        host_auth::participant_id_from_jar(&jar, &bill_id).ok_or(AppError::Unauthorized)?;
    pricing_api::mark_item(&state.pool, &bill_id, item_id, &participant_id).await?;
    let bill_state =
        pricing_api::get_bill_state(&state.pool, &bill_id, Some(&participant_id)).await?;
    Ok(render(templates::bill_fragment(
        &bill_id,
        &bill_state,
        Some(&participant_id),
        state.currency,
    )))
}

/// `DELETE /b/{id}/items/{item_id}/mark` — toggle off.
pub async fn unmark_item_handler(
    State(state): State<AppState>,
    Path((bill_id, item_id)): Path<(String, i64)>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let participant_id =
        host_auth::participant_id_from_jar(&jar, &bill_id).ok_or(AppError::Unauthorized)?;
    pricing_api::unmark_item(&state.pool, &bill_id, item_id, &participant_id).await?;
    let bill_state =
        pricing_api::get_bill_state(&state.pool, &bill_id, Some(&participant_id)).await?;
    Ok(render(templates::bill_fragment(
        &bill_id,
        &bill_state,
        Some(&participant_id),
        state.currency,
    )))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(landing))
        .route("/bills", post(create_bill_handler))
        .route("/b/{bill_id}/confirm", post(confirm_bill))
        .route("/b/{bill_id}/host", get(host_view))
        .route("/b/{bill_id}/host/{host_token}", get(host_recovery))
        .route("/b/{bill_id}/close", post(close_bill_handler))
        .route("/b/{bill_id}/qr.svg", get(qr_svg))
        .route("/b/{bill_id}/joined-fragment", get(joined_fragment))
        .route("/b/{bill_id}/join", post(join_bill_handler))
        .route("/b/{bill_id}/fragment", get(fragment_handler))
        .route(
            "/b/{bill_id}/items",
            post(add_item_handler),
        )
        .route("/b/{bill_id}/items/{item_id}", delete(delete_item_handler))
        .route(
            "/b/{bill_id}/items/{item_id}/mark",
            post(mark_item_handler).delete(unmark_item_handler),
        )
        .route("/b/{bill_id}", get(bill_entry))
        .with_state(state)
}
