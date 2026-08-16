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
- `SHAREPAY_ANTHROPIC_API_KEY` — Anthropic API key, required only when `ocr_engine = "claude"` (see below); the server fails fast at startup if it's missing in that case.

### Config file

A TOML config file, read once at startup (`src/config.rs`), holds settings that aren't secrets and don't need an env var's ambient-global feel. Two fields:

```toml
# sharepay.toml
ocr_engine = "tesseract"   # or "claude"
currency = "kzt"           # or "usd"
```

Path defaults to `sharepay.toml` in the working directory, overridable via `SHAREPAY_CONFIG_PATH`. A missing file is fine (defaults to `ocr_engine = "tesseract"`, `currency = "kzt"`); a malformed file fails startup fast, same as the `.expect(...)` patterns elsewhere in `main.rs`. `ocr_engine = "claude"` calls the Anthropic API instead of running OCR locally — see "Receipt Recognizer" below — and requires `SHAREPAY_ANTHROPIC_API_KEY`. `currency` selects which symbol `templates::fmt_cents` formats money with (`$12.34` for `usd`, `12.34 ₸` for `kzt`) — defaults to tenge since that's this app's actual target market (see `OCR_LANGUAGE` below), not USD.

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

`close_bill` blocks (`Err(UnclaimedItemsRemain)`, surfaced as `409 Conflict`) while any item still has zero markers — a later product decision that overrides the original spec §3.3.1 "nag but never block" design (the spec's UI was meant to warn about unclaimed items while still letting the host close anyway).

### 2. QR/Session Management (`src/bill/`, `src/host_auth.rs`, `src/qr.rs`, `src/cleanup.rs`, most of `src/routes/bill.rs`)

Owns the bill lifecycle state machine: `draft → pending_ocr ⇄ awaiting_photo_retry → pending_confirmation → open → closed`, plus `expired` for abandoned pre-open bills (`src/bill/mod.rs`). Each real transition is a named, single-purpose function (`mark_pending_ocr`, `mark_awaiting_photo_retry`, `mark_pending_confirmation`, plus `pricing::api::open_bill`/`close_bill`) — no route accepts an arbitrary target status. `bill::force_status` is a **test-harness-only** escape hatch to jump a bill into a given state without running the real OCR pipeline; never call it from production code paths.

Two independent 128-bit tokens gate access, deliberately kept separate: the `bill_id` (public — it's the QR/join-link target, gates participant-level access) and a `host_token` (private — grants host actions, delivered via cookie or a one-time recovery link, hashed at rest in `bills.host_token_hash`). `src/host_auth.rs` documents why cookies are plain/unsigned rather than encrypted (the cookie value is already an unguessable bearer token re-validated against the DB on every request).

`src/cleanup.rs` is an in-process background sweep (spawned from `main.rs`, re-spawned on every restart) that expires stale pre-open bills, auto-closes forgotten `open` bills, and hard-deletes old `closed`/`expired` bills (cascades via `ON DELETE CASCADE`).

### 3. Receipt Recognizer (`src/receipt/`, `src/routes/receipt.rs`)

`POST /b/{id}/photo` runs synchronously (not the async-job pattern the original spec considered — local OCR latency proved fast enough not to need it). The pluggable unit is `ReceiptEngine` (`receipt_engine.rs`): `fn recognize_receipt(&self, image_bytes: &[u8]) -> Result<RecognizedReceipt, OcrError>`, a blocking call (run via `spawn_blocking`) returning a fully-parsed receipt (items + subtotal/tax/total). `AppState.receipt_engine: Arc<dyn ReceiptEngine>` is selected once at startup in `main.rs` by the config file's `ocr_engine` value (see "Config file" above) — `src/routes/receipt.rs`'s pipeline doesn't know or care which implementation it's talking to. Two implementations exist:

- **`TesseractReceiptEngine`** (`ocr_engine = "tesseract"`, the default) — runs the original local four-stage pipeline, each its own module:
  1. `preprocess.rs` — decode/validate, orientation correction (tries EXIF first, then falls back to Tesseract's own OSD orientation detection via shelling out to `tesseract --psm 0`, since `leptess` exposes no OSD API — many real phone photos have no usable EXIF orientation tag), downscale, grayscale, contrast normalization, Otsu binarization.
  2. `ocr_engine.rs` — the low-level `OcrEngine` trait + `TesseractEngine` (via `leptess`), producing word-level text/confidence/layout. OCR language is `rus+kaz+eng` (see `main.rs`'s `OCR_LANGUAGE` doc comment for why — real Kazakhstani receipts are bilingual Cyrillic). `recognize_words` is blocking/CPU-bound.
  3. `parser.rs` — reconstructs lines from Tesseract's word-level TSV-equivalent output, classifies each line (item/subtotal/tax/tip/total/discount/noise/value-label) via parallel English and Cyrillic keyword sets, extracts prices with locale-aware number parsing (handles both `1,234.56` and `1 234,56` formats), and identifies the total line. Also implements a backward-resolving heuristic that merges multi-line item layouts (name line → quantity×price line → separate value-label line) common on real retail receipts.
  `TesseractReceiptEngine::recognize_receipt` is the only place stages 2-3 are wired together; it's a thin adapter reproducing the shape `reconcile.rs` (below) expects.
- **`ClaudeReceiptEngine`** (`ocr_engine = "claude"`, `claude_engine.rs`) — calls the Anthropic Messages API (Claude Sonnet 5, model id hardcoded — started on Haiku 4.5, upgraded after local testing showed Haiku unreliably distinguishing subtotal from total on dense small-text Cyrillic receipts) with the receipt photo as a vision input, using forced tool-use to get structured items/subtotal/tax/total back directly as JSON. Skips `ocr_engine.rs`/`parser.rs` entirely — those heuristics only make sense for Tesseract's raw per-word output, not a model that reads the receipt itself. Requires `SHAREPAY_ANTHROPIC_API_KEY`; each item's `confidence` is always `None` (Claude has no per-word OCR-confidence analog).

Either way, **`reconcile.rs`** is shared and fully engine-agnostic: it validates the parsed item sum against the recognized total/subtotal within tolerance, in integer cents; a hard mismatch routes the bill to `awaiting_photo_retry` rather than ever silently accepting a broken parse.

Known limitation: `tokio::time::timeout` (the `OCR_TIMEOUT` budget in `routes/receipt.rs`) doesn't abort the underlying `spawn_blocking` thread when it fires — a wedged call keeps running on the blocking threadpool regardless of the timeout. Pre-existing for the Tesseract path; `ClaudeReceiptEngine` additionally bounds itself with its own 15s HTTP client timeout, which the Tesseract path has no equivalent of.

### 4. Mobile Web Client (`src/templates.rs`, `templates/*.html`, `static/`, response-rendering in `src/routes/bill.rs` and `src/routes/receipt.rs`)

Server-rendered Askama templates + HTMX, no SPA framework. `src/templates.rs` holds template structs and view-model builders; money is always formatted there via `fmt_cents`/`cents_to_input_value`, never in template arithmetic. The participant bill view polls a fragment endpoint on an interval; the polling `hx-trigger` lives on the fragment's own root element so it's re-included (and the timer restarts) on every swap, avoiding poll/toggle flicker. The entire hand-written JS surface is two small snippets in `static/app.js` (file-input auto-submit, delegated clipboard-copy handler) — everything else interactive goes through HTMX.

### Request flow across components

`POST /bills` (§2) creates a bill and immediately enters `pending_ocr` → host uploads a photo to `POST /b/{id}/photo` (§1), which drives OCR/parsing/reconciliation and transitions the bill to `awaiting_photo_retry` (loop back to another photo) or `pending_confirmation` (items persisted via `pricing::api::add_items`, §3) → host reviews/edits and confirms (`POST /b/{id}/confirm`, §2), which calls `open_bill` (§3), generates the QR (`src/qr.rs`), and transitions to `open` → participants join (`POST /b/{id}/join`, §2 issues a participant cookie) and mark items (`pricing::api::mark_item`/`unmark_item`, §3), with the Mobile Client (§4) rendering all of the above and polling for live state → host closes the bill (`POST /b/{id}/close`).

`src/routes/mod.rs` just merges `routes::bill::router` and `routes::receipt::router` into one `axum::Router`; route ownership by component is documented at the top of each router file.
