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
/// validation. Two legitimate uses, both documented at call sites:
///
/// 1. The stubbed `draft -> pending_ocr` transition in
///    `POST /bills` (`src/routes/bill.rs`) — until the Receipt Recognizer
///    component replaces it with the real OCR pipeline (which owns
///    `pending_ocr -> awaiting_photo_retry` and
///    `pending_ocr -> pending_confirmation`), nothing else drives a bill
///    out of `pending_ocr`.
/// 2. Test harnesses that need to drive a bill into a specific pre-open
///    state to exercise `confirm`/`open`/`close` without a real OCR
///    pipeline wired up yet.
///
/// Not reachable from any HTTP route directly (no route accepts an
/// arbitrary target status from a client).
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
