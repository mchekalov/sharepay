//! The higher-level receipt-recognition abstraction. Moved up one level
//! from word-level OCR ([`OcrEngine`]): [`ReceiptEngine`] returns a
//! fully-parsed receipt directly, so `src/routes/receipt.rs` doesn't need
//! to know whether the underlying engine is Tesseract-word-shaped or not.
//! This exists because an API-based engine (e.g.
//! [`crate::receipt::claude_engine::ClaudeReceiptEngine`]) can read a
//! receipt directly and return structured items/total, bypassing
//! [`crate::receipt::parser`]'s word-grouping/line-reconstruction/
//! classification heuristics entirely — those heuristics only make sense
//! for Tesseract's raw per-word TSV output. [`TesseractReceiptEngine`] is
//! the only adapter that still calls into `parser`; it reproduces the
//! extraction logic that used to live directly in
//! `src/routes/receipt.rs::run_pipeline`, so the Tesseract path's
//! observable behavior is unchanged.

use crate::receipt::ocr_engine::{OcrEngine, OcrError, TesseractEngine};
use crate::receipt::parser::{self, ConfidenceBucket, LineCategory};
use crate::receipt::preprocess::{self, PreprocessError};

/// One recognized line item.
pub struct RecognizedItem {
    pub name: String,
    pub price_cents: i64,
    pub quantity_hint: Option<u32>,
    /// `None` when the engine has no per-item confidence signal (e.g.
    /// Claude has no analog to Tesseract's per-word OCR confidence).
    pub confidence: Option<ConfidenceBucket>,
}

/// A fully-parsed receipt, ready for [`crate::receipt::reconcile`].
pub struct RecognizedReceipt {
    pub items: Vec<RecognizedItem>,
    pub subtotal_cents: Option<i64>,
    pub tax_cents: Option<i64>,
    /// `None` means no total could be identified — callers treat this the
    /// same as "no receipt detected".
    pub total_cents: Option<i64>,
}

/// Abstraction over a receipt-recognition backend. Takes *lightly*
/// preprocessed image bytes (decoded, orientation-corrected, downscaled,
/// still full color — see [`crate::receipt::preprocess::preprocess_light`])
/// and returns the fully-parsed receipt. Implementations that need further
/// engine-specific processing (e.g. [`TesseractReceiptEngine`]'s grayscale/
/// binarization) do it themselves. `recognize_receipt` is a **blocking
/// call** — same calling convention as [`OcrEngine::recognize_words`]:
/// callers on an async runtime must run it via
/// `tokio::task::spawn_blocking`.
pub trait ReceiptEngine: Send + Sync {
    fn recognize_receipt(&self, image_bytes: &[u8]) -> Result<RecognizedReceipt, OcrError>;
}

/// Adapts the existing word-level [`TesseractEngine`] to [`ReceiptEngine`]
/// by running it through the existing `parser` pipeline. This is the only
/// place [`parser::parse_lines`]/[`parser::identify_total`] are still
/// called.
pub struct TesseractReceiptEngine {
    inner: TesseractEngine,
}

impl TesseractReceiptEngine {
    pub fn new(data_path: Option<&str>, lang: &str) -> Result<Self, OcrError> {
        Ok(Self {
            inner: TesseractEngine::new(data_path, lang)?,
        })
    }
}

impl ReceiptEngine for TesseractReceiptEngine {
    fn recognize_receipt(&self, image_bytes: &[u8]) -> Result<RecognizedReceipt, OcrError> {
        // `image_bytes` here is the *lightly* preprocessed (oriented,
        // downscaled, still-color) image shared across engines — finish
        // the Tesseract-specific grayscale/contrast/binarization steps
        // before handing off to OCR.
        let binarized = preprocess::finish_binarization(image_bytes).map_err(|e| match e {
            PreprocessError::Decode(err) => OcrError::ImageLoad(err.to_string()),
            PreprocessError::Encode(msg) => OcrError::InvalidOutput(msg),
        })?;
        let words = self.inner.recognize_words(&binarized)?;
        if words.is_empty() {
            return Err(OcrError::NoTextDetected);
        }

        let lines = parser::parse_lines(&words);
        let total = parser::identify_total(&lines);

        let item_lines: Vec<_> = lines
            .iter()
            .filter(|l| l.category == LineCategory::Item)
            .collect();

        let subtotal_cents = lines
            .iter()
            .find(|l| l.category == LineCategory::Subtotal)
            .and_then(|l| l.price_cents);
        let tax_cents = lines
            .iter()
            .find(|l| matches!(l.category, LineCategory::Tax | LineCategory::TipService))
            .and_then(|l| l.price_cents);

        let items = item_lines
            .into_iter()
            .filter_map(|l| {
                let price_cents = l.price_cents?;
                let name = l.name.clone().unwrap_or_default();
                Some(RecognizedItem {
                    name,
                    price_cents,
                    quantity_hint: l.quantity_hint,
                    confidence: Some(parser::confidence_bucket(l.mean_confidence)),
                })
            })
            .collect();

        Ok(RecognizedReceipt {
            items,
            subtotal_cents,
            tax_cents,
            total_cents: total.map(|t| t.price_cents),
        })
    }
}
