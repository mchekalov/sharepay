//! The Price Distributor's internal API (spec §3.5) — plain async
//! functions taking `&SqlitePool`, called by route handlers owned by the
//! QR/Session Management component. **Not** wired to HTTP here: no axum
//! handlers, no router.
//!
//! Design notes for callers:
//! - `bill_id` generation (the 128-bit token scheme, spec §2.1) and
//!   `host_token` hashing are owned by the QR/Session component.
//!   [`create_bill`] takes an already-generated `bill_id` and
//!   `host_token_hash` and just persists them; on a (vanishingly rare)
//!   collision it returns [`PriceDistributorError::IdCollision`] and the
//!   caller is expected to regenerate and retry (spec §2.1).
//! - `participant_id` is a caller-generated unguessable 128-bit token
//!   (spec §2.4), the same style as `bill_id`/`host_token` — *not* a
//!   DB-assigned surrogate key. [`join_bill`] takes an already-generated
//!   `participant_id` and just persists it, same convention as
//!   [`create_bill`]. Token generation lives in the QR/Session component
//!   (`src/bill/token.rs`).
//! - Every mutation re-validates bill/item/participant ownership and bill
//!   status itself (defense against stale/tampered form fields, and so the
//!   invariants hold even if a route handler forgets to check first).
//! - Queries here use `sqlx::query`/`query_as` (runtime-checked), not the
//!   `query!`/`query_as!` macros, so `cargo build` doesn't require a live
//!   database or a committed `.sqlx` offline-query cache. Revisit if the
//!   project adopts `cargo sqlx prepare` later.
//! - Bill status lifecycle (spec §2.5): `draft`, `pending_ocr`,
//!   `awaiting_photo_retry`, and `pending_confirmation` are all "pre-open"
//!   states — items may be added/edited in any of them (this is when OCR
//!   parses items and the host reviews/edits them). Items lock once the
//!   bill reaches `open` or `closed`. [`PRE_OPEN_STATUSES`] is the
//!   authoritative list.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::pricing::split::{compute_split, ItemInput, MarkerInput};
use crate::pricing::PriceDistributorError as Error;

/// Bill statuses in which the item list is still mutable (spec §2.5 /
/// §2.5's "Items frozen once open" rule). Anything not in this list is
/// either `open` or `closed`, where items are locked, or `expired`.
pub const PRE_OPEN_STATUSES: &[&str] = &[
    "draft",
    "pending_ocr",
    "awaiting_photo_retry",
    "pending_confirmation",
];

fn is_pre_open(status: &str) -> bool {
    PRE_OPEN_STATUSES.contains(&status)
}

/// Input row for [`add_items`].
#[derive(Debug, Clone)]
pub struct NewItem {
    pub name: String,
    pub price_cents: i64,
}

/// One participant's mark on an item, as returned in [`BillState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerView {
    pub participant_id: String,
    pub display_name: String,
}

/// One item row plus its current markers and computed split, as returned
/// in [`BillState`].
#[derive(Debug, Clone)]
pub struct ItemView {
    pub id: i64,
    pub name: String,
    pub price_cents: i64,
    pub markers: Vec<MarkerView>,
    pub is_marked_by_me: bool,
    /// `price / marker_count` (floor). A same-for-everyone figure for
    /// display (e.g. "$4.00 each") — at most one cent lower than what a
    /// specific marker actually owes for this item, since the largest-
    /// remainder method (spec §3.3 Step A) gives a few markers one extra
    /// cent. The exact, authoritative per-participant amount is
    /// [`ParticipantView::total`]. `None` when the item has zero markers.
    pub per_marker_share: Option<i64>,
}

/// One participant's computed totals, as returned in [`BillState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantView {
    pub id: String,
    pub display_name: String,
    pub dish_subtotal: i64,
    pub tax_tip_share: i64,
    pub total: i64,
}

/// The polling endpoint's data source (spec §3.5). Computed fresh from
/// `item_markers` on every call — never cached (spec §3.4/§3.5's
/// "computed fresh, not cached" rationale).
#[derive(Debug, Clone)]
pub struct BillState {
    pub status: String,
    pub items: Vec<ItemView>,
    pub participants: Vec<ParticipantView>,
    /// `0` when `requesting_participant_id` is `None` or doesn't match any
    /// participant on this bill.
    pub my_total: i64,
    pub assigned_total: i64,
    pub unassigned_amount: i64,
    pub receipt_total: Option<i64>,
    pub tax_tip_amount: i64,
}

const MAX_DISPLAY_NAME_LEN: usize = 100;

/// Creates a bill row in `draft` status. `bill_id` and `host_token_hash`
/// are generated and hashed by the caller (QR/Session component) — see
/// module docs. Returns `Err(IdCollision)` if `bill_id` already exists.
pub async fn create_bill(
    pool: &SqlitePool,
    bill_id: &str,
    host_token_hash: &str,
) -> Result<String, Error> {
    let result = sqlx::query(
        "INSERT INTO bills (id, host_token_hash, status) VALUES (?, ?, 'draft')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(bill_id)
    .bind(host_token_hash)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(Error::IdCollision);
    }

    Ok(bill_id.to_string())
}

/// Adds items to a bill. Host-only (auth enforced by the caller), permitted
/// while the bill is in any pre-open status ([`PRE_OPEN_STATUSES`]: `draft`,
/// `pending_ocr`, `awaiting_photo_retry`, `pending_confirmation`) — this is
/// when OCR parses items and the host reviews/edits them (spec §2.5).
/// Returns the new items' ids in the same order as `items`. Items are
/// appended after any existing rows (`sort_order` continues from the
/// current count).
pub async fn add_items(
    pool: &SqlitePool,
    bill_id: &str,
    items: Vec<NewItem>,
) -> Result<Vec<i64>, Error> {
    let mut tx = pool.begin().await?;

    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(&mut *tx)
        .await?;
    let status = status.ok_or(Error::NotFound)?;
    if !is_pre_open(&status) {
        return Err(Error::BillAlreadyOpen);
    }

    let existing_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE bill_id = ?")
        .bind(bill_id)
        .fetch_one(&mut *tx)
        .await?;

    let mut ids = Vec::with_capacity(items.len());
    for (offset, item) in items.into_iter().enumerate() {
        let sort_order = existing_count + offset as i64;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO items (bill_id, name, price, sort_order) VALUES (?, ?, ?, ?)
             RETURNING id",
        )
        .bind(bill_id)
        .bind(item.name)
        .bind(item.price_cents)
        .bind(sort_order)
        .fetch_one(&mut *tx)
        .await?;
        ids.push(id);
    }

    tx.commit().await?;
    Ok(ids)
}

/// Any pre-open status -> `open` (spec §2.5: the real-world transition is
/// `pending_confirmation -> open` once the host confirms items, but this
/// function accepts any pre-open status as a defensive measure — the
/// bill/session state machine that decides *when* it's valid to call this
/// is owned by the QR/Session component). Sets `opened_at`; items lock from
/// this point onward (spec §3.6, enforced both here at the status gate and
/// by the DB triggers in `migrations/0001_init.sql`). Idempotent: calling
/// this on an already-`open` bill is a no-op success (spec §2.7's
/// idempotent-transition guidance).
pub async fn open_bill(pool: &SqlitePool, bill_id: &str) -> Result<(), Error> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await?;
    let status = status.ok_or(Error::NotFound)?;

    if status == "open" {
        return Ok(()); // idempotent double-tap
    }
    if status == "closed" {
        return Err(Error::BillClosed);
    }
    if is_pre_open(&status) {
        sqlx::query(
            "UPDATE bills SET status = 'open', opened_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?",
        )
        .bind(bill_id)
        .execute(pool)
        .await?;
        return Ok(());
    }
    unreachable!("unknown bill status {status:?}")
}

/// Creates a participant on an `open` bill. `participant_id` is an
/// already-generated unguessable 128-bit token (spec §2.4), same convention
/// as [`create_bill`]'s `bill_id` parameter — generation is owned by the
/// QR/Session component. Returns `Err(NameTaken)` on a case-insensitive
/// display-name collision within the bill (the DB's
/// `UNIQUE (bill_id, display_name_normalized)` constraint, spec §3.2), or
/// `Err(IdCollision)` in the vanishingly rare case `participant_id` itself
/// collides.
pub async fn join_bill(
    pool: &SqlitePool,
    bill_id: &str,
    participant_id: &str,
    display_name: &str,
) -> Result<String, Error> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(Error::InvalidDisplayName);
    }

    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await?;
    let status = status.ok_or(Error::NotFound)?;
    if status == "closed" {
        return Err(Error::BillClosed);
    }
    if status != "open" {
        return Err(Error::BillNotOpen);
    }

    let result = sqlx::query(
        "INSERT INTO participants (id, bill_id, display_name) VALUES (?, ?, ?)",
    )
    .bind(participant_id)
    .bind(bill_id)
    .bind(trimmed)
    .execute(pool)
    .await;

    match result {
        Ok(_) => Ok(participant_id.to_string()),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            // Distinguish which UNIQUE constraint fired: the primary key
            // (participant_id collision, vanishingly rare) vs. the
            // per-bill case-insensitive display-name constraint (the
            // common case). SQLite's error message names the column.
            let message = db_err.message();
            if message.contains("participants.id") {
                Err(Error::IdCollision)
            } else {
                Err(Error::NameTaken)
            }
        }
        Err(e) => Err(Error::from(e)),
    }
}

/// Validates that `item_id` and `participant_id` both belong to `bill_id`,
/// and that the bill is `open`. Shared by `mark_item`/`unmark_item`.
async fn validate_open_bill_item_participant(
    pool: &SqlitePool,
    bill_id: &str,
    item_id: i64,
    participant_id: &str,
) -> Result<(), Error> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await?;
    let status = status.ok_or(Error::NotFound)?;
    if status == "closed" {
        return Err(Error::BillClosed);
    }
    if status != "open" {
        return Err(Error::BillNotOpen);
    }

    let item_ok: Option<i64> = sqlx::query_scalar("SELECT 1 FROM items WHERE id = ? AND bill_id = ?")
        .bind(item_id)
        .bind(bill_id)
        .fetch_optional(pool)
        .await?;
    if item_ok.is_none() {
        return Err(Error::NotFound);
    }

    let participant_ok: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM participants WHERE id = ? AND bill_id = ?")
            .bind(participant_id)
            .bind(bill_id)
            .fetch_optional(pool)
            .await?;
    if participant_ok.is_none() {
        return Err(Error::NotFound);
    }

    Ok(())
}

/// Marks `item_id` as eaten by `participant_id`. Idempotent against
/// double-taps/retries (`INSERT OR IGNORE`).
pub async fn mark_item(
    pool: &SqlitePool,
    bill_id: &str,
    item_id: i64,
    participant_id: &str,
) -> Result<(), Error> {
    validate_open_bill_item_participant(pool, bill_id, item_id, participant_id).await?;

    sqlx::query("INSERT OR IGNORE INTO item_markers (item_id, participant_id) VALUES (?, ?)")
        .bind(item_id)
        .bind(participant_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Unmarks `item_id` for `participant_id`. Naturally idempotent (`DELETE`).
pub async fn unmark_item(
    pool: &SqlitePool,
    bill_id: &str,
    item_id: i64,
    participant_id: &str,
) -> Result<(), Error> {
    validate_open_bill_item_participant(pool, bill_id, item_id, participant_id).await?;

    sqlx::query("DELETE FROM item_markers WHERE item_id = ? AND participant_id = ?")
        .bind(item_id)
        .bind(participant_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// `open -> closed`. Sets `closed_at`. Idempotent: calling this on an
/// already-`closed` bill is a no-op success.
pub async fn close_bill(pool: &SqlitePool, bill_id: &str) -> Result<(), Error> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bills WHERE id = ?")
        .bind(bill_id)
        .fetch_optional(pool)
        .await?;
    let status = status.ok_or(Error::NotFound)?;

    match status.as_str() {
        "open" => {
            sqlx::query(
                "UPDATE bills SET status = 'closed', closed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE id = ?",
            )
            .bind(bill_id)
            .execute(pool)
            .await?;
            Ok(())
        }
        "closed" => Ok(()), // idempotent double-tap
        "draft" => Err(Error::BillNotOpen),
        other => unreachable!("unknown bill status {other:?}"),
    }
}

struct ItemRow {
    id: i64,
    name: String,
    price: i64,
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for ItemRow {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            price: row.try_get("price")?,
        })
    }
}

struct MarkerRow {
    item_id: i64,
    participant_id: String,
    display_name: String,
    marked_at: String,
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for MarkerRow {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            item_id: row.try_get("item_id")?,
            participant_id: row.try_get("participant_id")?,
            display_name: row.try_get("display_name")?,
            marked_at: row.try_get("marked_at")?,
        })
    }
}

struct ParticipantRow {
    id: String,
    display_name: String,
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for ParticipantRow {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            display_name: row.try_get("display_name")?,
        })
    }
}

/// Computes the full bill state fresh from `item_markers` (spec §3.5's
/// "computed fresh, not cached"). `requesting_participant_id` scopes
/// `is_marked_by_me`/`my_total`; pass `None` for a non-participant
/// (e.g. host-only) view.
pub async fn get_bill_state(
    pool: &SqlitePool,
    bill_id: &str,
    requesting_participant_id: Option<&str>,
) -> Result<BillState, Error> {
    let bill_row: Option<(String, Option<i64>, i64)> = sqlx::query_as(
        "SELECT status, receipt_total, tax_tip_amount FROM bills WHERE id = ?",
    )
    .bind(bill_id)
    .fetch_optional(pool)
    .await?;
    let (status, receipt_total, tax_tip_amount) = bill_row.ok_or(Error::NotFound)?;

    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT id, name, price FROM items WHERE bill_id = ? ORDER BY sort_order ASC",
    )
    .bind(bill_id)
    .fetch_all(pool)
    .await?;

    let markers: Vec<MarkerRow> = sqlx::query_as(
        "SELECT im.item_id, im.participant_id, p.display_name, im.marked_at
         FROM item_markers im
         JOIN items i ON i.id = im.item_id
         JOIN participants p ON p.id = im.participant_id
         WHERE i.bill_id = ?
         ORDER BY im.marked_at ASC, im.participant_id ASC",
    )
    .bind(bill_id)
    .fetch_all(pool)
    .await?;

    let participants: Vec<ParticipantRow> = sqlx::query_as(
        "SELECT id, display_name FROM participants WHERE bill_id = ? ORDER BY joined_at ASC, id ASC",
    )
    .bind(bill_id)
    .fetch_all(pool)
    .await?;

    // Build split::ItemInput list, grouping markers by item.
    let split_items: Vec<ItemInput> = items
        .iter()
        .map(|item| {
            let item_markers = markers
                .iter()
                .filter(|m| m.item_id == item.id)
                .map(|m| MarkerInput {
                    participant_id: m.participant_id.clone(),
                    marked_at: m.marked_at.clone(),
                })
                .collect();
            ItemInput {
                item_id: item.id,
                price_cents: item.price,
                markers: item_markers,
            }
        })
        .collect();

    let split = compute_split(&split_items, tax_tip_amount);

    let item_views: Vec<ItemView> = items
        .iter()
        .map(|item| {
            let item_markers: Vec<MarkerView> = markers
                .iter()
                .filter(|m| m.item_id == item.id)
                .map(|m| MarkerView {
                    participant_id: m.participant_id.clone(),
                    display_name: m.display_name.clone(),
                })
                .collect();
            let marker_count = item_markers.len() as i64;
            let is_marked_by_me = requesting_participant_id
                .map(|me| item_markers.iter().any(|m| m.participant_id == me))
                .unwrap_or(false);
            let per_marker_share = if marker_count > 0 {
                Some(item.price / marker_count)
            } else {
                None
            };
            ItemView {
                id: item.id,
                name: item.name.clone(),
                price_cents: item.price,
                markers: item_markers,
                is_marked_by_me,
                per_marker_share,
            }
        })
        .collect();

    let participant_views: Vec<ParticipantView> = participants
        .iter()
        .map(|p| {
            let split_entry = split.participants.get(&p.id).copied().unwrap_or_default();
            ParticipantView {
                id: p.id.clone(),
                display_name: p.display_name.clone(),
                dish_subtotal: split_entry.dish_subtotal,
                tax_tip_share: split_entry.tax_tip_share,
                total: split_entry.total,
            }
        })
        .collect();

    let my_total = requesting_participant_id
        .and_then(|me| participant_views.iter().find(|p| p.id == me))
        .map(|p| p.total)
        .unwrap_or(0);

    Ok(BillState {
        status,
        items: item_views,
        participants: participant_views,
        my_total,
        assigned_total: split.assigned_total,
        unassigned_amount: split.unassigned_amount,
        receipt_total,
        tax_tip_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{init_pool, DbConfig};
    use crate::pricing::PriceDistributorError;
    use sqlx::SqlitePool;

    async fn test_pool() -> SqlitePool {
        init_pool(&DbConfig::in_memory()).await.unwrap()
    }

    /// End-to-end run of the full lifecycle, reproducing spec §3.3's
    /// worked example through the actual API layer (not just
    /// `split::compute_split` directly): create -> add items -> open ->
    /// join three participants -> mark items -> read back BillState ->
    /// close.
    #[tokio::test]
    async fn full_lifecycle_reproduces_worked_example() {
        let pool = test_pool().await;

        let bill_id = create_bill(&pool, "test-bill-1", "hashed-token")
            .await
            .unwrap();
        assert_eq!(bill_id, "test-bill-1");

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
                NewItem {
                    name: "Salad".into(),
                    price_cents: 900,
                },
            ],
        )
        .await
        .unwrap();
        assert_eq!(item_ids.len(), 3);
        let (burger_id, fries_id, _salad_id) = (item_ids[0], item_ids[1], item_ids[2]);

        // tax_tip_amount is set directly for this test (in the real system
        // it's set by the Receipt Recognizer / QR-Session confirm step).
        sqlx::query("UPDATE bills SET tax_tip_amount = 490, receipt_total = 3090 WHERE id = ?")
            .bind(&bill_id)
            .execute(&pool)
            .await
            .unwrap();

        open_bill(&pool, &bill_id).await.unwrap();
        // Idempotent double-open.
        open_bill(&pool, &bill_id).await.unwrap();

        let alice = join_bill(&pool, &bill_id, "participant-alice", "Alice")
            .await
            .unwrap();
        let bob = join_bill(&pool, &bill_id, "participant-bob", "Bob")
            .await
            .unwrap();
        let carol = join_bill(&pool, &bill_id, "participant-carol", "Carol")
            .await
            .unwrap();

        // Case-insensitive duplicate name rejected (even with a distinct
        // participant_id token).
        let dup = join_bill(&pool, &bill_id, "participant-alice-2", "  alice ").await;
        assert!(matches!(dup, Err(PriceDistributorError::NameTaken)));

        // Items are locked once open.
        let locked = add_items(
            &pool,
            &bill_id,
            vec![NewItem {
                name: "Late item".into(),
                price_cents: 100,
            }],
        )
        .await;
        assert!(matches!(
            locked,
            Err(PriceDistributorError::BillAlreadyOpen)
        ));

        mark_item(&pool, &bill_id, burger_id, &alice).await.unwrap();
        mark_item(&pool, &bill_id, burger_id, &bob).await.unwrap();
        mark_item(&pool, &bill_id, burger_id, &carol).await.unwrap();
        mark_item(&pool, &bill_id, fries_id, &alice).await.unwrap();
        mark_item(&pool, &bill_id, fries_id, &bob).await.unwrap();
        // Double-mark is idempotent (INSERT OR IGNORE).
        mark_item(&pool, &bill_id, fries_id, &bob).await.unwrap();
        // Salad is left unmarked -> unassigned.

        let state = get_bill_state(&pool, &bill_id, Some(&alice)).await.unwrap();
        assert_eq!(state.status, "open");
        assert_eq!(state.assigned_total, 1700);
        assert_eq!(state.unassigned_amount, 900);
        assert_eq!(state.tax_tip_amount, 490);
        assert_eq!(state.receipt_total, Some(3090));

        let alice_view = state.participants.iter().find(|p| p.id == alice).unwrap();
        let bob_view = state.participants.iter().find(|p| p.id == bob).unwrap();
        let carol_view = state.participants.iter().find(|p| p.id == carol).unwrap();

        assert_eq!(alice_view.total, 838);
        assert_eq!(bob_view.total, 837);
        assert_eq!(carol_view.total, 515);
        assert_eq!(state.my_total, 838, "requesting participant was Alice");

        let sum: i64 = state.participants.iter().map(|p| p.total).sum();
        assert_eq!(sum + state.unassigned_amount, 2190 + 900);

        let burger_view = state.items.iter().find(|i| i.id == burger_id).unwrap();
        assert!(burger_view.is_marked_by_me); // Alice marked the burger
        assert_eq!(burger_view.markers.len(), 3);
        assert_eq!(burger_view.per_marker_share, Some(400));

        // Unmark and re-check.
        unmark_item(&pool, &bill_id, burger_id, &carol).await.unwrap();
        let state2 = get_bill_state(&pool, &bill_id, None).await.unwrap();
        assert_eq!(state2.my_total, 0, "no requesting participant this time");
        let burger_view2 = state2.items.iter().find(|i| i.id == burger_id).unwrap();
        assert_eq!(burger_view2.markers.len(), 2);

        close_bill(&pool, &bill_id).await.unwrap();
        // Idempotent double-close.
        close_bill(&pool, &bill_id).await.unwrap();

        let closed_state = get_bill_state(&pool, &bill_id, None).await.unwrap();
        assert_eq!(closed_state.status, "closed");

        // Marking after close is rejected.
        let mark_after_close = mark_item(&pool, &bill_id, fries_id, &alice).await;
        assert!(matches!(
            mark_after_close,
            Err(PriceDistributorError::BillClosed)
        ));

        // Joining after close is also rejected.
        let join_after_close = join_bill(&pool, &bill_id, "participant-dave", "Dave").await;
        assert!(matches!(
            join_after_close,
            Err(PriceDistributorError::BillClosed)
        ));
    }

    #[tokio::test]
    async fn create_bill_collision_is_reported() {
        let pool = test_pool().await;
        create_bill(&pool, "dup-id", "hash-a").await.unwrap();
        let result = create_bill(&pool, "dup-id", "hash-b").await;
        assert!(matches!(result, Err(PriceDistributorError::IdCollision)));
    }

    #[tokio::test]
    async fn mark_item_validates_item_and_participant_belong_to_bill() {
        let pool = test_pool().await;

        let bill_a = create_bill(&pool, "bill-a", "hash").await.unwrap();
        let bill_b = create_bill(&pool, "bill-b", "hash").await.unwrap();

        let item_a = add_items(
            &pool,
            &bill_a,
            vec![NewItem {
                name: "A-item".into(),
                price_cents: 100,
            }],
        )
        .await
        .unwrap()[0];

        open_bill(&pool, &bill_a).await.unwrap();
        open_bill(&pool, &bill_b).await.unwrap();

        let participant_b = join_bill(&pool, &bill_b, "participant-eve", "Eve")
            .await
            .unwrap();

        // participant_b belongs to bill_b, not bill_a — must be rejected.
        let result = mark_item(&pool, &bill_a, item_a, &participant_b).await;
        assert!(matches!(result, Err(PriceDistributorError::NotFound)));
    }

    #[tokio::test]
    async fn join_bill_rejects_empty_display_name() {
        let pool = test_pool().await;
        let bill_id = create_bill(&pool, "bill-empty-name", "hash").await.unwrap();
        open_bill(&pool, &bill_id).await.unwrap();

        let result = join_bill(&pool, &bill_id, "participant-empty", "   ").await;
        assert!(matches!(
            result,
            Err(PriceDistributorError::InvalidDisplayName)
        ));
    }

    /// Items remain addable/editable through every pre-open state (spec
    /// §2.5), locking only once the bill is `open`.
    #[tokio::test]
    async fn items_addable_in_every_pre_open_state_locked_once_open() {
        let pool = test_pool().await;

        for status in super::PRE_OPEN_STATUSES {
            let bill_id = format!("bill-status-{status}");
            create_bill(&pool, &bill_id, "hash").await.unwrap();
            sqlx::query("UPDATE bills SET status = ? WHERE id = ?")
                .bind(*status)
                .bind(&bill_id)
                .execute(&pool)
                .await
                .unwrap();

            let result = add_items(
                &pool,
                &bill_id,
                vec![NewItem {
                    name: "Item".into(),
                    price_cents: 100,
                }],
            )
            .await;
            assert!(
                result.is_ok(),
                "expected add_items to succeed while status = {status}"
            );
        }

        let bill_id = create_bill(&pool, "bill-open-locks-items", "hash")
            .await
            .unwrap();
        open_bill(&pool, &bill_id).await.unwrap();
        let result = add_items(
            &pool,
            &bill_id,
            vec![NewItem {
                name: "Too late".into(),
                price_cents: 100,
            }],
        )
        .await;
        assert!(matches!(result, Err(PriceDistributorError::BillAlreadyOpen)));
    }
}
