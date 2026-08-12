-- Initial schema for SharePay.
--
-- Note: PRAGMA foreign_keys / journal_mode / synchronous / busy_timeout are
-- connection-level settings, not persisted schema state, so they are NOT set
-- here. They are configured per-connection via SqliteConnectOptions in
-- src/db/pool.rs (see spec §3.4).

CREATE TABLE bills (
    id                  TEXT PRIMARY KEY,        -- 128-bit random token (QR/Session component)
    host_token_hash     TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'draft'
                        CHECK (status IN ('draft', 'open', 'closed')),
    receipt_total       INTEGER,                 -- cents; nullable until parsed/confirmed
    tax_tip_amount      INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    opened_at           TEXT,
    closed_at           TEXT
);

CREATE TABLE items (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    bill_id      TEXT NOT NULL REFERENCES bills(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    price        INTEGER NOT NULL CHECK (price >= 0),
    sort_order   INTEGER NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX idx_items_bill_id ON items(bill_id, sort_order);

CREATE TABLE participants (
    id                       INTEGER PRIMARY KEY AUTOINCREMENT,
    bill_id                  TEXT NOT NULL REFERENCES bills(id) ON DELETE CASCADE,
    display_name             TEXT NOT NULL CHECK (length(trim(display_name)) > 0),
    display_name_normalized  TEXT GENERATED ALWAYS AS (lower(trim(display_name))) STORED,
    joined_at                TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    UNIQUE (bill_id, display_name_normalized)
);
CREATE INDEX idx_participants_bill_id ON participants(bill_id);

CREATE TABLE item_markers (
    item_id         INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    participant_id  INTEGER NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    marked_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (item_id, participant_id)
);
CREATE INDEX idx_item_markers_participant ON item_markers(participant_id, item_id);

-- Defense-in-depth (spec §3.6): items are locked once the parent bill leaves
-- 'draft'. The API layer (src/pricing/api.rs) is the primary enforcement
-- point (returns a typed BillAlreadyOpen error); these triggers are a
-- backstop against a future code path forgetting the check.
CREATE TRIGGER trg_items_no_insert_after_draft
BEFORE INSERT ON items
WHEN (SELECT status FROM bills WHERE id = NEW.bill_id) != 'draft'
BEGIN
    SELECT RAISE(ABORT, 'items are locked once the bill is no longer draft');
END;

CREATE TRIGGER trg_items_no_update_after_draft
BEFORE UPDATE ON items
WHEN (SELECT status FROM bills WHERE id = OLD.bill_id) != 'draft'
BEGIN
    SELECT RAISE(ABORT, 'items are locked once the bill is no longer draft');
END;

CREATE TRIGGER trg_items_no_delete_after_draft
BEFORE DELETE ON items
WHEN (SELECT status FROM bills WHERE id = OLD.bill_id) != 'draft'
BEGIN
    SELECT RAISE(ABORT, 'items are locked once the bill is no longer draft');
END;
