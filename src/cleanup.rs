//! Periodic retention/cleanup sweep (spec §2.6).
//!
//! An in-process tokio background task, spawned once from `main.rs` and
//! re-spawned on every process restart (not relied upon to survive a
//! restart, per spec). On each tick it:
//! 1. Expires pre-open bills that have been inactive too long.
//! 2. Auto-closes `open` bills nobody explicitly closed.
//! 3. Hard-deletes `closed`/`expired` bills past their retention window
//!    (cascades through `items`/`participants`/`item_markers` via the
//!    schema's `ON DELETE CASCADE`).
//!
//! Simplification: "2 hours of inactivity" for pre-open bills (spec §2.6)
//! is measured from `created_at`, since the schema has no separate
//! last-activity timestamp — the pre-open flow already touches the bill
//! frequently enough (photo upload, item edits) in the real system that
//! this is a reasonable proxy, and this is explicitly a "correctness of
//! policy matters more than sophistication" area per the task brief.
//! Likewise, `expired` bills (no `expired_at` column) use `created_at` as
//! the retention-window anchor.

use std::time::Duration;

use sqlx::SqlitePool;

/// How often the sweep runs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Pre-open bills (`draft`, `pending_ocr`, `awaiting_photo_retry`,
/// `pending_confirmation`) with no activity for this long become `expired`.
/// No participant could have joined yet (the QR isn't shown pre-open), so
/// there is zero user-facing cost to expiring quickly.
pub const PRE_OPEN_EXPIRY: Duration = Duration::from_secs(2 * 60 * 60);

/// `open` bills auto-close this long after `opened_at` if the host never
/// explicitly closes them.
pub const OPEN_AUTO_CLOSE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// `closed`/`expired` bills are hard-deleted this long after they entered
/// that state, giving stragglers a window to revisit the read-only
/// summary.
pub const RETENTION_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Runs one sweep pass. Exposed separately from [`spawn`] so tests can
/// invoke it directly without waiting on a real timer.
pub async fn sweep_once(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    expire_stale_pre_open_bills(pool).await?;
    auto_close_stale_open_bills(pool).await?;
    hard_delete_past_retention(pool).await?;
    Ok(())
}

async fn expire_stale_pre_open_bills(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let cutoff_seconds = PRE_OPEN_EXPIRY.as_secs() as i64;
    sqlx::query(
        "UPDATE bills
         SET status = 'expired'
         WHERE status IN ('draft', 'pending_ocr', 'awaiting_photo_retry', 'pending_confirmation')
           AND created_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds')",
    )
    .bind(format!("-{cutoff_seconds}"))
    .execute(pool)
    .await?;
    Ok(())
}

async fn auto_close_stale_open_bills(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let cutoff_seconds = OPEN_AUTO_CLOSE_AFTER.as_secs() as i64;
    sqlx::query(
        "UPDATE bills
         SET status = 'closed', closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE status = 'open'
           AND opened_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds')",
    )
    .bind(format!("-{cutoff_seconds}"))
    .execute(pool)
    .await?;
    Ok(())
}

async fn hard_delete_past_retention(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let cutoff_seconds = RETENTION_WINDOW.as_secs() as i64;
    let cutoff_arg = format!("-{cutoff_seconds}");
    // `closed` bills: anchor on closed_at. `expired` bills: anchor on
    // created_at (no expired_at column — see module docs).
    sqlx::query(
        "DELETE FROM bills
         WHERE (status = 'closed' AND closed_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds'))
            OR (status = 'expired' AND created_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds'))",
    )
    .bind(&cutoff_arg)
    .bind(&cutoff_arg)
    .execute(pool)
    .await?;
    Ok(())
}

/// Spawns the periodic sweep as a detached tokio task. Errors from a single
/// pass are logged (via `eprintln!` — this project has no tracing/logging
/// setup yet) and do not stop the loop; a transient DB hiccup on one tick
/// shouldn't kill the sweep for the process lifetime.
pub fn spawn(pool: SqlitePool) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = sweep_once(&pool).await {
                eprintln!("cleanup sweep error: {err}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{init_pool, DbConfig};
    use crate::pricing::api::create_bill;

    async fn test_pool() -> SqlitePool {
        init_pool(&DbConfig::in_memory()).await.unwrap()
    }

    async fn set_created_at(pool: &SqlitePool, bill_id: &str, seconds_ago: i64) {
        sqlx::query(
            "UPDATE bills SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds') WHERE id = ?",
        )
        .bind(format!("-{seconds_ago}"))
        .bind(bill_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn set_opened_at(pool: &SqlitePool, bill_id: &str, seconds_ago: i64) {
        sqlx::query(
            "UPDATE bills SET status = 'open', opened_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds') WHERE id = ?",
        )
        .bind(format!("-{seconds_ago}"))
        .bind(bill_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn set_closed_at(pool: &SqlitePool, bill_id: &str, seconds_ago: i64) {
        sqlx::query(
            "UPDATE bills SET status = 'closed', closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now', ? || ' seconds') WHERE id = ?",
        )
        .bind(format!("-{seconds_ago}"))
        .bind(bill_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn status_of(pool: &SqlitePool, bill_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
            .bind(bill_id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn expires_stale_pre_open_bills_but_not_fresh_ones() {
        let pool = test_pool().await;
        create_bill(&pool, "stale-draft", "hash").await.unwrap();
        create_bill(&pool, "fresh-draft", "hash").await.unwrap();
        set_created_at(&pool, "stale-draft", 3 * 60 * 60).await; // 3h ago
        set_created_at(&pool, "fresh-draft", 60).await; // 1 min ago

        sweep_once(&pool).await.unwrap();

        assert_eq!(status_of(&pool, "stale-draft").await.as_deref(), Some("expired"));
        assert_eq!(status_of(&pool, "fresh-draft").await.as_deref(), Some("draft"));
    }

    #[tokio::test]
    async fn auto_closes_stale_open_bills_but_not_fresh_ones() {
        let pool = test_pool().await;
        create_bill(&pool, "stale-open", "hash").await.unwrap();
        create_bill(&pool, "fresh-open", "hash").await.unwrap();
        set_opened_at(&pool, "stale-open", 25 * 60 * 60).await; // 25h ago
        set_opened_at(&pool, "fresh-open", 60).await; // 1 min ago

        sweep_once(&pool).await.unwrap();

        assert_eq!(status_of(&pool, "stale-open").await.as_deref(), Some("closed"));
        assert_eq!(status_of(&pool, "fresh-open").await.as_deref(), Some("open"));
    }

    #[tokio::test]
    async fn hard_deletes_bills_past_retention_but_not_within_it() {
        let pool = test_pool().await;
        create_bill(&pool, "old-closed", "hash").await.unwrap();
        create_bill(&pool, "recent-closed", "hash").await.unwrap();
        set_closed_at(&pool, "old-closed", 8 * 24 * 60 * 60).await; // 8 days ago
        set_closed_at(&pool, "recent-closed", 60).await; // 1 min ago

        sweep_once(&pool).await.unwrap();

        assert_eq!(status_of(&pool, "old-closed").await, None, "should be hard-deleted");
        assert_eq!(
            status_of(&pool, "recent-closed").await.as_deref(),
            Some("closed"),
            "within retention window, should survive"
        );
    }
}
