//! Typed errors for the Price Distributor API (spec §3.5 / §3.6).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PriceDistributorError {
    /// A bill, item, or participant id that does not exist (or does not
    /// belong to the bill it was looked up under).
    #[error("not found")]
    NotFound,

    /// `join_bill` was called with a `display_name` that collides,
    /// case-insensitively, with an existing participant on the same bill
    /// (spec §3.2's `UNIQUE (bill_id, display_name_normalized)`).
    #[error("that name is already taken on this bill")]
    NameTaken,

    /// A `draft`-only operation (e.g. `add_items`, `open_bill`) was
    /// attempted on a bill that is no longer `draft`.
    #[error("bill is already open (or closed) — items are locked")]
    BillAlreadyOpen,

    /// An `open`-only operation (e.g. `mark_item`, `join_bill`) was
    /// attempted on a bill that has been closed.
    #[error("bill is closed")]
    BillClosed,

    /// An operation that requires the bill to be `open` (e.g. joining,
    /// marking) was attempted while the bill is still `draft`.
    #[error("bill is not open yet")]
    BillNotOpen,

    /// `display_name` failed basic validation (empty after trimming, or
    /// too long) before ever reaching the DB unique constraint.
    #[error("invalid display name")]
    InvalidDisplayName,

    /// `create_bill` was given a `bill_id` that already exists. Caller
    /// (the QR/Session component) is expected to generate a fresh 128-bit
    /// token and retry, per spec §2.1's "bounded regenerate-retry loop" —
    /// this component only reports the collision.
    #[error("bill id already exists")]
    IdCollision,

    /// Underlying database error that doesn't map to one of the above
    /// well-known conditions.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}
