//! Receipt Recognizer (spec §1): OCR + parsing + reconciliation pipeline
//! that turns an uploaded photo of a paper receipt into structured,
//! host-reviewable line items.
//!
//! Pipeline stages, each in their own module:
//! 1. [`preprocess`] — decode/validate, EXIF orientation fix, downscale,
//!    grayscale, contrast normalization, Otsu binarization (spec §1.2).
//! 2. [`ocr_engine`] — the [`OcrEngine`] trait + Tesseract-backed
//!    implementation, producing word-level text/confidence/layout (spec
//!    §1.1/§1.3).
//! 3. [`parser`] — line reconstruction, classification (item/subtotal/
//!    tax/tip/total/discount/noise), price/quantity extraction, total-line
//!    identification (spec §1.3).
//! 4. [`reconcile`] — tolerance-based validation of parsed items against
//!    the recognized subtotal/total, producing a hard-mismatch vs.
//!    success-with-confidence-flag outcome, all in integer cents (spec
//!    §1.4).
//!
//! [`receipt_engine`] sits above stages 2-3: [`ReceiptEngine`] is the
//! pluggable interface `src/main.rs`/`AppState` actually use, returning a
//! fully-parsed receipt directly rather than word-level OCR output.
//! [`receipt_engine::TesseractReceiptEngine`] runs the full stage 2→3
//! pipeline above internally; [`claude_engine::ClaudeReceiptEngine`] calls
//! the Anthropic API instead, skipping `ocr_engine`/`parser` entirely
//! (Claude reads the receipt directly). [`reconcile`] (stage 4) is shared,
//! engine-agnostic either way — selected at startup via
//! `config::AppConfig::ocr_engine`.
//!
//! `src/routes/receipt.rs` (owned by this component too) is the HTTP layer
//! that wires this pipeline into the bill state machine the QR/Session
//! component built (`pending_ocr` ⇄ `awaiting_photo_retry` →
//! `pending_confirmation`).

pub mod claude_engine;
pub mod ocr_engine;
pub mod parser;
pub mod preprocess;
pub mod receipt_engine;
pub mod reconcile;

pub use claude_engine::ClaudeReceiptEngine;
pub use ocr_engine::{OcrEngine, OcrError, OcrWord, TesseractEngine};
pub use receipt_engine::{ReceiptEngine, RecognizedItem, RecognizedReceipt, TesseractReceiptEngine};
