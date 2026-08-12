# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

SharePay: a Rust web app for splitting a restaurant/cafe bill among a group. A host photographs a paper receipt; the server OCRs it into structured line items, the host reviews/corrects them, and a QR code is generated. Friends scan the QR code (no accounts), mark which dishes they ate, and the server computes what each person owes (even split per dish, tax/tip distributed proportionally). Single Rust binary + SQLite; server-rendered HTML via Askama + HTMX (no SPA framework, minimal hand-written JS).

The full design spec (architecture, data model, algorithms, and the reasoning behind them, section by section) lives at `/Users/mihailchekalov/.claude/plans/let-s-create-plan-for-fizzy-lantern.md`. Source files reference it throughout as `spec §N` — read that doc for the "why" behind a decision before changing it.

## Commands

### Environment setup (required before build/test/run)

OCR depends on Tesseract/Leptonica native libraries (via the `leptess` crate), installed locally through Homebrew:

```bash
brew install tesseract tesseract-lang pkg-config   # tesseract-lang adds rus/kaz trained data
export PKG_CONFIG_PATH="/opt/homebrew/opt/tesseract/lib/pkgconfig:/opt/homebrew/opt/leptonica/lib/pkgconfig"
```

The `PKG_CONFIG_PATH` export is required for every `cargo build`/`test`/`run`/`clippy` invocation in this repo (not just once) — it's not persisted anywhere in the project. `eng`/`osd` trained data ships with the base `tesseract` formula; `rus`/`kaz` come from `tesseract-lang`. On Linux deployment targets, the equivalent is `apt install libtesseract-dev libleptonica-dev tesseract-ocr` plus `pkg-config`.

### Build, run, test

```bash
cargo build
cargo run                      # serves on 0.0.0.0:3000
cargo test                     # full suite: lib unit tests + tests/bill_lifecycle.rs + tests/receipt_photo.rs
cargo test <substring>         # run tests matching a name substring, e.g. `cargo test split::tests`
cargo test --test bill_lifecycle   # just one integration test file
cargo clippy --all-targets
```

Config is via environment variables, all optional with sane local-dev defaults (see `src/main.rs`, `src/db/pool.rs`):

- `SHAREPAY_DB_PATH` — SQLite file path (default `sharepay.db` in the working directory; migrations in `migrations/` run automatically on startup).
- `SHAREPAY_BASE_URL` — public base URL used to build join links/QR payloads (default `http://localhost:3000`; **must** be set to the real HTTPS domain in production — join-link/QR generation and the `Secure` cookie flag both assume HTTPS).
- `SHAREPAY_TESSDATA_PREFIX` — override for where `<lang>.traineddata` files live; unset works on this dev machine (resolves to `/opt/homebrew/share/tessdata`), needed on deployment targets where Tesseract's own auto-resolution doesn't land correctly.

### Debugging the OCR pipeline directly

`examples/receipt_debug.rs` runs preprocess → OCR → parse → reconcile against real image files on disk, with a full trace (reconstructed lines + classification, identified total, reconciliation outcome) — no HTTP layer or database involved. Use this instead of curling a running server when iterating on OCR/parsing logic:

```bash
cargo run --example receipt_debug -- path/to/receipt.jpg [more paths...]
```

## Architecture

The app is one Cargo binary (`sharepay`) built up as four components, each owning a distinct part of the system. `src/lib.rs` is the module root; `src/main.rs` is a thin binary entry point that wires a `TesseractEngine`, the DB pool, and the retention sweep into the router and starts serving.

### 1. Database + Price Distributor (`src/pricing/`, `migrations/0001_init.sql`)

SQLite schema: `bills`, `items`, `participants`, `item_markers` (many-to-many: which participants marked which items). All money is stored and computed as **integer cents**, never floats — this matters throughout the codebase, not just here. `bills.id` and `participants.id` are both random unguessable 128-bit tokens (`TEXT`, not autoincrement integers) since both are used as bearer-token-like identifiers in cookies/URLs; only `items.id` is a plain autoincrement integer (not security-sensitive, since `bill_id` already gates access).

`src/pricing/split.rs` implements the split algorithm: each item's price is divided evenly among everyone who marked it, using the largest-remainder method so per-item shares always sum exactly to the item price (no floating-point drift). Tax/tip is then distributed proportionally to each participant's dish subtotal, using the same largest-remainder approach so the grand total reconciles exactly. Items marked by nobody are surfaced as `unassigned_amount` rather than silently dropped or redistributed.

`src/pricing/api.rs` is the DB-facing API the other three components call (`create_bill`, `add_items`, `join_bill`, `mark_item`/`unmark_item`, `get_bill_state`, `open_bill`/`close_bill`, etc.) — plain async functions over `&SqlitePool`, no HTTP awareness. `get_bill_state` recomputes totals fresh on every call rather than caching (deliberate: the data volume is tiny per bill, and a stale cached total shown to someone deciding what to pay is the one failure mode this app can least afford).

Item mutations are only valid while a bill is in a pre-open status (`PRE_OPEN_STATUSES`); this is enforced both here and via SQLite triggers in the migration (defense in depth).

### 2. QR/Session Management (`src/bill/`, `src/host_auth.rs`, `src/qr.rs`, `src/cleanup.rs`, most of `src/routes/bill.rs`)

Owns the bill lifecycle state machine: `draft → pending_ocr ⇄ awaiting_photo_retry → pending_confirmation → open → closed`, plus `expired` for abandoned pre-open bills (`src/bill/mod.rs`). Each real transition is a named, single-purpose function (`mark_pending_ocr`, `mark_awaiting_photo_retry`, `mark_pending_confirmation`, plus `pricing::api::open_bill`/`close_bill`) — no route accepts an arbitrary target status. `bill::force_status` is a **test-harness-only** escape hatch to jump a bill into a given state without running the real OCR pipeline; never call it from production code paths.

Two independent 128-bit tokens gate access, deliberately kept separate: the `bill_id` (public — it's the QR/join-link target, gates participant-level access) and a `host_token` (private — grants host actions, delivered via cookie or a one-time recovery link, hashed at rest in `bills.host_token_hash`). `src/host_auth.rs` documents why cookies are plain/unsigned rather than encrypted (the cookie value is already an unguessable bearer token re-validated against the DB on every request).

`src/cleanup.rs` is an in-process background sweep (spawned from `main.rs`, re-spawned on every restart) that expires stale pre-open bills, auto-closes forgotten `open` bills, and hard-deletes old `closed`/`expired` bills (cascades via `ON DELETE CASCADE`).

### 3. Receipt Recognizer (`src/receipt/`, `src/routes/receipt.rs`)

`POST /b/{id}/photo` runs synchronously (not the async-job pattern the original spec considered — local OCR latency proved fast enough not to need it) through four stages, each its own module:

1. `preprocess.rs` — decode/validate, orientation correction (tries EXIF first, then falls back to Tesseract's own OSD orientation detection via shelling out to `tesseract --psm 0`, since `leptess` exposes no OSD API — many real phone photos have no usable EXIF orientation tag), downscale, grayscale, contrast normalization, Otsu binarization.
2. `ocr_engine.rs` — the `OcrEngine` trait + `TesseractEngine` (via `leptess`), isolated behind a trait specifically so the OCR backend stays swappable later (e.g. to a pure-Rust engine for a truly self-contained binary) without touching parsing logic. OCR language is `rus+kaz+eng` (see `main.rs`'s `OCR_LANGUAGE` doc comment for why — real Kazakhstani receipts are bilingual Cyrillic). `recognize_words` is blocking/CPU-bound; callers must run it via `spawn_blocking`.
3. `parser.rs` — reconstructs lines from Tesseract's word-level TSV-equivalent output, classifies each line (item/subtotal/tax/tip/total/discount/noise/value-label) via parallel English and Cyrillic keyword sets, extracts prices with locale-aware number parsing (handles both `1,234.56` and `1 234,56` formats), and identifies the total line. Also implements a backward-resolving heuristic that merges multi-line item layouts (name line → quantity×price line → separate value-label line) common on real retail receipts.
4. `reconcile.rs` — validates parsed item sum against the recognized total/subtotal within tolerance, in integer cents; a hard mismatch routes the bill to `awaiting_photo_retry` rather than ever silently accepting a broken parse.

### 4. Mobile Web Client (`src/templates.rs`, `templates/*.html`, `static/`, response-rendering in `src/routes/bill.rs` and `src/routes/receipt.rs`)

Server-rendered Askama templates + HTMX, no SPA framework. `src/templates.rs` holds template structs and view-model builders; money is always formatted there via `fmt_cents`/`cents_to_input_value`, never in template arithmetic. The participant bill view polls a fragment endpoint on an interval; the polling `hx-trigger` lives on the fragment's own root element so it's re-included (and the timer restarts) on every swap, avoiding poll/toggle flicker. The entire hand-written JS surface is two small snippets in `static/app.js` (file-input auto-submit, delegated clipboard-copy handler) — everything else interactive goes through HTMX.

### Request flow across components

`POST /bills` (§2) creates a bill and immediately enters `pending_ocr` → host uploads a photo to `POST /b/{id}/photo` (§1), which drives OCR/parsing/reconciliation and transitions the bill to `awaiting_photo_retry` (loop back to another photo) or `pending_confirmation` (items persisted via `pricing::api::add_items`, §3) → host reviews/edits and confirms (`POST /b/{id}/confirm`, §2), which calls `open_bill` (§3), generates the QR (`src/qr.rs`), and transitions to `open` → participants join (`POST /b/{id}/join`, §2 issues a participant cookie) and mark items (`pricing::api::mark_item`/`unmark_item`, §3), with the Mobile Client (§4) rendering all of the above and polling for live state → host closes the bill (`POST /b/{id}/close`).

`src/routes/mod.rs` just merges `routes::bill::router` and `routes::receipt::router` into one `axum::Router`; route ownership by component is documented at the top of each router file.
