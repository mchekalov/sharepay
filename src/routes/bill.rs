//! HTTP routes implementing the QR/Session Management API surface (spec
//! §2.8). Handlers here own routing, state transitions, and cookie/auth
//! gating — **not** presentation. Responses are minimal placeholder
//! HTML/JSON; real templates are the Mobile Client component's job
//! (spec §4).
//!
//! ## What's stubbed / deferred here
//! - `POST /bills` creates the bill in `draft` and immediately force-moves
//!   it to `pending_ocr` (see `bill::force_status`'s doc comment) — there
//!   is no OCR pipeline yet, so nothing currently drives a bill out of
//!   `pending_ocr`. The Receipt Recognizer component owns
//!   `POST /b/{id}/photo`, the `pending_ocr <-> awaiting_photo_retry`
//!   loop, and the eventual `-> pending_confirmation` transition.
//! - There is no `GET /b/{id}/review` / item-CRUD route wired here yet
//!   either (also Recognizer/Mobile-Client territory) — but the
//!   underlying `pricing::api::add_items` already accepts calls in any
//!   pre-open status, so those routes can be added without further schema
//!   changes.
//! - No HTML templates: every response is a `format!`-built string.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::bill::{self, token::generate_token, token::hash_token};
use crate::host_auth::{self, HostAuthError};
use crate::pricing::api::{self as pricing_api, PRE_OPEN_STATUSES};
use crate::pricing::PriceDistributorError;
use crate::qr;

/// Shared app state threaded through every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    /// Public base URL used to build the join link / QR payload, e.g.
    /// `https://sharepay.example`. No trailing slash required (stripped in
    /// [`qr::join_url`] if present).
    pub base_url: String,
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
            PriceDistributorError::IdCollision => {
                AppError::Internal("id collision".into())
            }
            PriceDistributorError::Database(e) => AppError::from_db(e),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "you don't have host access to this bill".to_string(),
            ),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Html(format!("<p>{message}</p>"))).into_response()
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
    bill::get_status(pool, bill_id)
        .await
        .map_err(AppError::from_db)?
        .ok_or_else(|| AppError::NotFound("this bill link isn't valid or has expired".into()))
}

// ---------------------------------------------------------------------
// JSON DTOs for the polling endpoints (kept local to the HTTP layer so
// `pricing::api`'s types stay free of serde/HTTP concerns).
// ---------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct MarkerJson {
    participant_id: String,
    display_name: String,
}

#[derive(Debug, Serialize)]
struct ItemJson {
    id: i64,
    name: String,
    price_cents: i64,
    markers: Vec<MarkerJson>,
    is_marked_by_me: bool,
    per_marker_share: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ParticipantJson {
    id: String,
    display_name: String,
    dish_subtotal: i64,
    tax_tip_share: i64,
    total: i64,
}

#[derive(Debug, Serialize)]
struct BillStateJson {
    status: String,
    items: Vec<ItemJson>,
    participants: Vec<ParticipantJson>,
    my_total: i64,
    assigned_total: i64,
    unassigned_amount: i64,
    receipt_total: Option<i64>,
    tax_tip_amount: i64,
}

impl From<pricing_api::BillState> for BillStateJson {
    fn from(s: pricing_api::BillState) -> Self {
        BillStateJson {
            status: s.status,
            items: s
                .items
                .into_iter()
                .map(|i| ItemJson {
                    id: i.id,
                    name: i.name,
                    price_cents: i.price_cents,
                    markers: i
                        .markers
                        .into_iter()
                        .map(|m| MarkerJson {
                            participant_id: m.participant_id,
                            display_name: m.display_name,
                        })
                        .collect(),
                    is_marked_by_me: i.is_marked_by_me,
                    per_marker_share: i.per_marker_share,
                })
                .collect(),
            participants: s
                .participants
                .into_iter()
                .map(|p| ParticipantJson {
                    id: p.id,
                    display_name: p.display_name,
                    dish_subtotal: p.dish_subtotal,
                    tax_tip_share: p.tax_tip_share,
                    total: p.total,
                })
                .collect(),
            my_total: s.my_total,
            assigned_total: s.assigned_total,
            unassigned_amount: s.unassigned_amount,
            receipt_total: s.receipt_total,
            tax_tip_amount: s.tax_tip_amount,
        }
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

/// `POST /bills` — create a bill in `draft`, issue the host cookie.
pub async fn create_bill_handler(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let (bill_id, host_token) = create_bill_with_retry(&state.pool).await?;

    // STUB (see module docs): no OCR pipeline exists yet, so we
    // immediately move the bill from draft to pending_ocr ourselves. The
    // Receipt Recognizer component replaces this with a real
    // `POST /b/{id}/photo` handler that performs this transition (and the
    // subsequent ones) for real.
    bill::force_status(&state.pool, &bill_id, "pending_ocr")
        .await
        .map_err(AppError::from_db)?;

    let jar = host_auth::set_host_cookie(jar, &bill_id, &host_token);
    let recovery_url = format!("/b/{bill_id}/host/{host_token}");
    let body = format!(
        "<h1>Bill created</h1>\
         <p>bill_id={bill_id}</p>\
         <p>status=pending_ocr (OCR not yet wired up — stub)</p>\
         <p>Save this host recovery link for cross-device access: \
         <a href=\"{recovery_url}\">{recovery_url}</a></p>\
         <p><a href=\"/b/{bill_id}/host\">Continue to host view</a></p>"
    );
    Ok((StatusCode::CREATED, jar, Html(body)))
}

/// `GET /b/{id}/host` — host view (QR/join link once open, controls
/// otherwise). Host-gated.
pub async fn host_view(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    let status = require_status(&state.pool, &bill_id).await?;

    let mut body = format!("<h1>Host view</h1><p>bill_id={bill_id}</p><p>status={status}</p>");
    if status == "open" {
        let url = qr::join_url(&state.base_url, &bill_id);
        let svg_xml = qr::render_svg(&url).map_err(|e| AppError::Internal(e.to_string()))?;
        body.push_str(&format!(
            "<p>Join link: <a href=\"{url}\">{url}</a></p>\
             <div>{svg_xml}</div>\
             <p><img src=\"/b/{bill_id}/qr.svg\" alt=\"QR code to join this bill\"></p>\
             <form method=\"post\" action=\"/b/{bill_id}/close\"><button type=\"submit\">Close bill</button></form>"
        ));
    } else if status == "pending_confirmation" {
        body.push_str(&format!(
            "<form method=\"post\" action=\"/b/{bill_id}/confirm\"><button type=\"submit\">Confirm and generate QR code</button></form>"
        ));
    }
    Ok(Html(body))
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

/// `POST /b/{id}/confirm` — `pending_confirmation -> open`, host-gated,
/// idempotent on an already-`open` bill (spec §2.7).
pub async fn confirm_bill(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    let status = require_status(&state.pool, &bill_id).await?;

    if status != "pending_confirmation" && status != "open" {
        return Err(AppError::Conflict(format!(
            "bill is not ready to confirm (status={status})"
        )));
    }

    pricing_api::open_bill(&state.pool, &bill_id).await?;

    let url = qr::join_url(&state.base_url, &bill_id);
    let svg_xml = qr::render_svg(&url).map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(format!(
        "<h1>Ready! Show this to your friends</h1>\
         <div>{svg_xml}</div>\
         <p>{url}</p>"
    )))
}

/// `POST /b/{id}/close` — `open -> closed`, host-gated, idempotent on an
/// already-`closed` bill.
pub async fn close_bill_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    require_host(&state.pool, &jar, &bill_id).await?;
    pricing_api::close_bill(&state.pool, &bill_id).await?;
    Ok(Html(format!(
        "<h1>Bill closed</h1><p>bill_id={bill_id}</p>"
    )))
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

fn render_name_entry(bill_id: &str) -> String {
    format!(
        "<h1>What's your name?</h1>\
         <form method=\"post\" action=\"/b/{bill_id}/join\">\
         <input type=\"text\" name=\"display_name\" maxlength=\"40\" required autofocus>\
         <button type=\"submit\">Join</button></form>"
    )
}

fn render_bill_state_html(bill_id: &str, state: &pricing_api::BillState) -> String {
    let mut rows = String::new();
    for item in &state.items {
        let names: Vec<&str> = item
            .markers
            .iter()
            .map(|m| m.display_name.as_str())
            .collect();
        rows.push_str(&format!(
            "<li>{} — ${:.2} [{}]{}</li>",
            item.name,
            item.price_cents as f64 / 100.0,
            names.join(", "),
            if item.is_marked_by_me { " (you)" } else { "" }
        ));
    }
    format!(
        "<h1>Bill {bill_id}</h1><p>status={}</p><ul>{rows}</ul>\
         <p>Your total: ${:.2}</p>\
         <p>Unassigned: ${:.2}</p>",
        state.status,
        state.my_total as f64 / 100.0,
        state.unassigned_amount as f64 / 100.0
    )
}

/// `GET /b/{id}` — participant entry point (spec §2.4/§2.7): routes to
/// name-entry, the marking UI, a "not ready yet" placeholder, or the
/// read-only closed summary, based on bill status + participant cookie.
pub async fn bill_entry(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
) -> Result<impl IntoResponse, AppError> {
    let status = bill::get_status(&state.pool, &bill_id)
        .await
        .map_err(AppError::from_db)?;
    let status = match status {
        Some(s) if s != "expired" => s,
        _ => {
            return Err(AppError::NotFound(
                "this bill link isn't valid or has expired".into(),
            ))
        }
    };

    if status == "closed" {
        let participant_id = host_auth::participant_id_from_jar(&jar, &bill_id);
        let bill_state =
            pricing_api::get_bill_state(&state.pool, &bill_id, participant_id.as_deref()).await?;
        return Ok(Html(format!(
            "<h1>Bill closed — final totals</h1>{}",
            render_bill_state_html(&bill_id, &bill_state)
        )));
    }

    if PRE_OPEN_STATUSES.contains(&status.as_str()) {
        return Ok(Html(format!(
            "<h1>This bill isn't ready yet</h1><p>Ask the host. (status={status})</p>"
        )));
    }

    // status == "open"
    match host_auth::participant_id_from_jar(&jar, &bill_id) {
        Some(participant_id) => {
            let bill_state =
                pricing_api::get_bill_state(&state.pool, &bill_id, Some(&participant_id)).await?;
            Ok(Html(render_bill_state_html(&bill_id, &bill_state)))
        }
        None => Ok(Html(render_name_entry(&bill_id))),
    }
}

#[derive(Debug, Deserialize)]
pub struct JoinForm {
    pub display_name: String,
}

/// `POST /b/{id}/join` — name entry, creates the `Participant`, issues the
/// participant cookie.
pub async fn join_bill_handler(
    State(state): State<AppState>,
    Path(bill_id): Path<String>,
    jar: CookieJar,
    Form(form): Form<JoinForm>,
) -> Result<impl IntoResponse, AppError> {
    let participant_id = generate_token();
    let id =
        pricing_api::join_bill(&state.pool, &bill_id, &participant_id, &form.display_name)
            .await?;
    let jar = host_auth::set_participant_cookie(jar, &bill_id, &id);
    Ok((jar, Redirect::to(&format!("/b/{bill_id}"))))
}

/// `GET /b/{id}/items` — polling endpoint: item list + marks + running
/// totals. Gated to `open`/`closed`; scopes `is_marked_by_me`/`my_total` to
/// the participant cookie if present.
pub async fn items_poll(
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
    Ok(Json(BillStateJson::from(bill_state)))
}

/// `POST /b/{id}/items/{item_id}/mark` — toggle on. Participant-cookie
/// gated, `open` only.
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
    Ok(Json(BillStateJson::from(bill_state)))
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
    Ok(Json(BillStateJson::from(bill_state)))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/bills", post(create_bill_handler))
        .route("/b/{bill_id}/confirm", post(confirm_bill))
        .route("/b/{bill_id}/host", get(host_view))
        .route("/b/{bill_id}/host/{host_token}", get(host_recovery))
        .route("/b/{bill_id}/close", post(close_bill_handler))
        .route("/b/{bill_id}/qr.svg", get(qr_svg))
        .route("/b/{bill_id}/join", post(join_bill_handler))
        .route("/b/{bill_id}/items", get(items_poll))
        .route(
            "/b/{bill_id}/items/{item_id}/mark",
            post(mark_item_handler).delete(unmark_item_handler),
        )
        .route("/b/{bill_id}", get(bill_entry))
        .with_state(state)
}
