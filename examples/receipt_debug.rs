//! Standalone debugging tool for the Receipt Recognizer pipeline (spec §1):
//! runs preprocess -> OCR -> parse -> reconcile directly against one or
//! more real image files on disk and prints a full trace (reconstructed
//! lines with their classification, the identified total, and the
//! reconciliation outcome), without going through the HTTP layer or a
//! database. Used to validate the pipeline against real Kazakhstani
//! receipt photos during Cyrillic/locale support development — not part
//! of the app itself.
//!
//! Usage:
//!   PKG_CONFIG_PATH="/opt/homebrew/opt/tesseract/lib/pkgconfig:/opt/homebrew/opt/leptonica/lib/pkgconfig" \
//!     cargo run --example receipt_debug -- <path-to-image> [more paths...]

use std::time::Instant;

use sharepay::receipt::parser::{self, LineCategory};
use sharepay::receipt::reconcile::{self, ReconcileInput, ReconcileOutcome};
use sharepay::receipt::{preprocess, OcrEngine, TesseractEngine};

const DEFAULT_OCR_LANGUAGE: &str = "rus+kaz+eng";

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: receipt_debug <path-to-image> [more paths...]");
        std::process::exit(1);
    }

    let tessdata_prefix = std::env::var("SHAREPAY_TESSDATA_PREFIX").ok();
    let lang = std::env::var("SHAREPAY_OCR_LANGUAGE").unwrap_or_else(|_| DEFAULT_OCR_LANGUAGE.to_string());
    let engine = TesseractEngine::new(tessdata_prefix.as_deref(), &lang)
        .expect("failed to initialize Tesseract engine");

    for path in paths {
        println!("\n================ {path} ================");
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("  FAILED to read file: {e}");
                continue;
            }
        };

        let t_total = Instant::now();

        let t0 = Instant::now();
        let preprocessed = match preprocess::preprocess(&raw) {
            Ok(p) => p,
            Err(e) => {
                println!("  FAILED at preprocess: {e}");
                continue;
            }
        };
        let preprocess_ms = t0.elapsed().as_millis();

        let t1 = Instant::now();
        let words = match engine.recognize_words(&preprocessed) {
            Ok(w) => w,
            Err(e) => {
                println!("  FAILED at OCR: {e}");
                continue;
            }
        };
        let ocr_ms = t1.elapsed().as_millis();

        if words.is_empty() {
            println!("  no words recognized at all (no_text_detected)");
            continue;
        }

        let lines = parser::parse_lines(&words);

        println!("  -- reconstructed/classified lines --");
        for l in &lines {
            println!(
                "  [{:>3}] {:<12?} price={:<10} name={:<30} conf={:.0}  text={:?}",
                l.line_index,
                l.category,
                l.price_cents.map(|c| format!("{:.2}", c as f64 / 100.0)).unwrap_or_default(),
                l.name.clone().unwrap_or_default(),
                l.mean_confidence,
                l.raw_text,
            );
        }

        let Some(total) = parser::identify_total(&lines) else {
            println!("  FAILED: no total identified (no_receipt_detected)");
            continue;
        };

        let item_lines: Vec<_> = lines.iter().filter(|l| l.category == LineCategory::Item).collect();
        if item_lines.is_empty() {
            println!("  FAILED: no item lines (no_receipt_detected)");
            continue;
        }

        let items_sum_cents: i64 = item_lines.iter().filter_map(|l| l.price_cents).sum();
        let subtotal_cents = lines
            .iter()
            .find(|l| l.category == LineCategory::Subtotal)
            .and_then(|l| l.price_cents);
        let tax_line_cents = lines
            .iter()
            .find(|l| matches!(l.category, LineCategory::Tax | LineCategory::TipService))
            .and_then(|l| l.price_cents);

        let outcome = reconcile::reconcile(&ReconcileInput {
            items_sum_cents,
            num_items: item_lines.len(),
            subtotal_cents,
            tax_line_cents,
            total_cents: total.price_cents,
        });

        println!("\n  -- items --");
        for l in &item_lines {
            println!(
                "    {:<40} {:>10}",
                l.name.clone().unwrap_or_default(),
                l.price_cents.map(|c| format!("{:.2}", c as f64 / 100.0)).unwrap_or_default()
            );
        }
        println!(
            "  recognized total: {:.2} (line_index {})",
            total.price_cents as f64 / 100.0,
            total.line_index
        );
        println!("  items sum: {:.2}", items_sum_cents as f64 / 100.0);
        println!("  subtotal_cents: {subtotal_cents:?}  tax_line_cents: {tax_line_cents:?}");

        match outcome {
            ReconcileOutcome::Success {
                tax_tip_amount_cents,
                overall_confidence,
                tax_tip_unconfirmed,
            } => {
                println!(
                    "  RESULT: SUCCESS  tax_tip={:.2}  confidence={:?}  unconfirmed={}",
                    tax_tip_amount_cents as f64 / 100.0,
                    overall_confidence,
                    tax_tip_unconfirmed
                );
            }
            ReconcileOutcome::Mismatch {
                computed_sum_cents,
                discrepancy_cents,
            } => {
                println!(
                    "  RESULT: MISMATCH  computed_sum={:.2}  discrepancy={:.2}",
                    computed_sum_cents as f64 / 100.0,
                    discrepancy_cents as f64 / 100.0
                );
            }
        }

        println!(
            "  timing: preprocess={preprocess_ms}ms  ocr={ocr_ms}ms  total={}ms",
            t_total.elapsed().as_millis()
        );
    }
}
