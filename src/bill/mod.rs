//! Bill lifecycle helpers (spec §2.5) that sit above the DB-agnostic
//! `pricing::api` layer: status lookups and the "force" transition used by
//! the OCR-pipeline stub (see [`force_status`]).

pub mod token;

pub use token::{generate_token, hash_token};

use sqlx::SqlitePool;

/// Bill statuses in which the join link/QR is valid and participants can
/// interact with the bill (spec §2.5). Currently just `open` — kept as a
/// named constant (rather than a bare string literal scattered through
/// route handlers) alongside [`crate::pricing::api::PRE_OPEN_STATUSES`] and
/// `"closed"`/`"expired"` for symmetry.
pub const OPEN_STATUS: &str = "open";
pub const CLOSED_STATUS: &str = "closed";
pub const EXPIRED_STATUS: &str = "expired";

/// Looks up a bill's current status. `Ok(None)` means the bill does not
/// exist (never existed, or was hard-deleted by the retention sweep) — the
/// caller is expected to treat this identically to `expired` per spec
/// §2.7 ("no distinction shown between never-existed and expired").
pub async fn get_status(pool: &SqlitePool, bill_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await
}

/// Looks up a bill's `host_token_hash`, for host-cookie verification.
pub async fn get_host_token_hash(
    pool: &SqlitePool,
    bill_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT host_token_hash FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await
}

/// Directly sets a bill's status, bypassing `pricing::api`'s transition
/// validation. **Test-harness-only**: drives a bill into a specific
/// pre-open state to exercise `confirm`/`open`/`close` logic without
/// running the real Receipt Recognizer pipeline. Not reachable from any
/// HTTP route directly (no route accepts an arbitrary target status from a
/// client) — production status transitions go through the named,
/// single-purpose functions below ([`mark_pending_ocr`],
/// [`mark_awaiting_photo_retry`], [`mark_pending_confirmation`]) or
/// `pricing::api::open_bill`/`close_bill`, each of which encodes exactly
/// one real transition rather than accepting an arbitrary target status.
pub async fn force_status(
    pool: &SqlitePool,
    bill_id: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bills SET status = ? WHERE id = ?")
        .bind(status)
        .bind(bill_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Bill statuses from which `POST /b/{id}/photo` accepts an upload (spec
/// §1.5/§2.5): the initial attempt (`pending_ocr`, entered immediately at
/// bill creation) or a retry after a reconciliation mismatch
/// (`awaiting_photo_retry`) — the same handler serves both.
pub const PHOTO_UPLOAD_STATUSES: &[&str] = &["pending_ocr", "awaiting_photo_retry"];

/// `draft -> pending_ocr`: the bill has been created and is now waiting for
/// the host's first receipt photo (spec §2.1: "the Bill row is created as
/// soon as the host starts the upload flow"). A real, single-purpose
/// production transition (unlike [`force_status`]) — called once, directly
/// after `pricing::api::create_bill`, by `POST /bills`.
pub async fn mark_pending_ocr(pool: &SqlitePool, bill_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bills SET status = 'pending_ocr' WHERE id = ?")
        .bind(bill_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `pending_ocr`/`awaiting_photo_retry -> awaiting_photo_retry`: OCR ran
/// but reconciliation hard-mismatched (spec §1.4/§1.6). The bill and its
/// participants persist untouched — pre-open bills have no participants
/// yet, and no receipt-derived rows are written on this path — the host
/// simply retries the upload against the same `bill_id`.
pub async fn mark_awaiting_photo_retry(pool: &SqlitePool, bill_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE bills SET status = 'awaiting_photo_retry' WHERE id = ?")
        .bind(bill_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `pending_ocr`/`awaiting_photo_retry -> pending_confirmation`: OCR and
/// reconciliation succeeded. Persists the reconciled `receipt_total`/
/// `tax_tip_amount` in the same statement as the status flip. Parsed item
/// rows themselves are persisted separately via
/// `pricing::api::add_items` (valid in every pre-open status, spec §2.5) —
/// callers should add items before calling this, though the ordering
/// doesn't affect correctness since `pending_confirmation` is itself
/// pre-open.
pub async fn mark_pending_confirmation(
    pool: &SqlitePool,
    bill_id: &str,
    receipt_total_cents: i64,
    tax_tip_amount_cents: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE bills SET status = 'pending_confirmation', receipt_total = ?, tax_tip_amount = ? \
         WHERE id = ?",
    )
    .bind(receipt_total_cents)
    .bind(tax_tip_amount_cents)
    .bind(bill_id)
    .execute(pool)
    .await?;
    Ok(())
}
