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
//! `src/routes/receipt.rs` (owned by this component too) is the HTTP layer
//! that wires this pipeline into the bill state machine the QR/Session
//! component built (`pending_ocr` ⇄ `awaiting_photo_retry` →
//! `pending_confirmation`).

pub mod ocr_engine;
pub mod parser;
pub mod preprocess;
pub mod reconcile;

pub use ocr_engine::{OcrEngine, OcrError, OcrWord, TesseractEngine};
