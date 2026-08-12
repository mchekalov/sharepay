//! OCR engine abstraction (spec §1.1). Wraps Tesseract (via `leptess`)
//! behind a small trait, [`OcrEngine`], so the concrete implementation is
//! isolated and swappable — the spec's flagged follow-up migration path is
//! `ocrs`/`rten` for a truly self-contained single binary, at which point
//! only this module (not `parser`/`reconcile`) needs to change. Only the
//! Tesseract-backed implementation, [`TesseractEngine`], is built for v1.

use std::sync::Mutex;

use leptess::LepTess;

/// One recognized word from OCR output, with layout + confidence metadata
/// — the raw material for [`crate::receipt::parser`]'s line reconstruction
/// (spec §1.3: "TSV output: word text + confidence + bounding box +
/// block/par/line_num grouping — not a flat text dump, which loses
/// column/price alignment").
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    /// Tesseract's word-level confidence, 0-100. Rows with a negative
    /// confidence (Tesseract emits -1 for structural, non-word TSV rows)
    /// are filtered out before this struct is ever constructed — see
    /// [`parse_tsv`].
    pub confidence: f32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub block_num: i32,
    pub par_num: i32,
    pub line_num: i32,
    pub word_num: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    #[error("failed to initialize OCR engine: {0}")]
    Init(String),
    #[error("failed to load image into OCR engine: {0}")]
    ImageLoad(String),
    #[error("OCR engine produced invalid UTF-8 output: {0}")]
    InvalidOutput(String),
}

/// Abstraction over an OCR backend (spec §1.1). Takes already-preprocessed
/// image bytes (see [`crate::receipt::preprocess`]) and returns the
/// recognized words with their layout/confidence. `recognize_words` is a
/// **blocking, CPU-bound call** — callers on an async runtime must run it
/// via `tokio::task::spawn_blocking` (see `src/routes/receipt.rs`), not
/// call it directly from an async fn.
pub trait OcrEngine: Send + Sync {
    fn recognize_words(&self, image_bytes: &[u8]) -> Result<Vec<OcrWord>, OcrError>;
}

/// Tesseract-backed implementation via `leptess` (spec §1.1's recommended
/// v1 choice: in-process TSV access, avoids a second language runtime).
///
/// Wraps a `Mutex<LepTess>` rather than constructing a fresh `LepTess` per
/// call: `LepTess`/the underlying Tesseract C API instance is not safely
/// shareable across concurrent calls, and re-initializing it per request
/// would re-load the `*.traineddata` file from disk every time. One
/// long-lived engine is reused, serialized by the mutex — acceptable since
/// OCR is CPU-bound and this app's request volume (a handful of receipt
/// photos per bill) never demands concurrent OCR calls.
pub struct TesseractEngine {
    inner: Mutex<LepTess>,
}

impl TesseractEngine {
    /// `data_path`: directory containing `<lang>.traineddata` (e.g.
    /// `/opt/homebrew/share/tessdata` on this dev machine, or
    /// `/usr/share/tesseract-ocr/5/tessdata` on a typical Debian/Ubuntu
    /// VPS), or `None` to use Tesseract's own compiled-in default /
    /// `TESSDATA_PREFIX` env var resolution. `lang`: e.g. `"eng"`, or a
    /// `+`-joined combination such as `"rus+kaz+eng"` — Tesseract's native
    /// `TessBaseAPIInit` accepts this directly, no special handling needed
    /// on this crate's side. Measured impact of going from `eng`-only to
    /// `rus+kaz+eng`, same preprocessed (downscaled to this pipeline's
    /// ~2200px-long-edge cap) real receipt photos, on this dev machine:
    /// OCR recognition itself went from ~750-800ms to ~1000-2000ms per
    /// photo (full request round trip, preprocess included, landed around
    /// 1.4s-2.7s total either way) — roughly 1.3-2.5x slower, still well
    /// within an acceptable per-upload budget, so no fallback/single-
    /// language retry path was added. Tesseract evaluates all listed
    /// languages' models concurrently rather than picking one upfront, so
    /// this cost scales with language count.
    pub fn new(data_path: Option<&str>, lang: &str) -> Result<Self, OcrError> {
        let lt = LepTess::new(data_path, lang).map_err(|e| OcrError::Init(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(lt),
        })
    }
}

impl OcrEngine for TesseractEngine {
    fn recognize_words(&self, image_bytes: &[u8]) -> Result<Vec<OcrWord>, OcrError> {
        let mut lt = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lt.set_image_from_mem(image_bytes)
            .map_err(|e| OcrError::ImageLoad(e.to_string()))?;
        // Our preprocessed PNGs carry no DPI metadata; suppress Tesseract's
        // "Invalid resolution 0 dpi" warning/degraded-accuracy path with a
        // reasonable fallback (spec preprocessing targets a ~2000-2200px
        // long edge, roughly consistent with a receipt photographed at
        // typical phone-camera distance).
        lt.set_fallback_source_resolution(70);
        let tsv = lt
            .get_tsv_text(0)
            .map_err(|e| OcrError::InvalidOutput(e.to_string()))?;
        Ok(parse_tsv(&tsv))
    }
}

/// Parses Tesseract's TSV output format:
/// `level  page_num  block_num  par_num  line_num  word_num  left  top  width  height  conf  text`
///
/// Tesseract emits one row per level of its layout hierarchy (page/block/
/// paragraph/line/word); only `level == 5` (word) rows carry real text and
/// a meaningful confidence score (spec §1.3's line-reconstruction is
/// entirely word-driven — the higher-level rows are structural and
/// discarded here). Malformed rows (too few columns, unparsable numeric
/// fields) are skipped defensively rather than failing the whole batch —
/// Tesseract's TSV output is not expected to be malformed in practice, but
/// a single bad row shouldn't lose every other recognized word.
pub(crate) fn parse_tsv(tsv: &str) -> Vec<OcrWord> {
    const WORD_LEVEL: i32 = 5;
    let mut words = Vec::new();
    let mut lines = tsv.lines();
    lines.next(); // header row

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.splitn(12, '\t').collect();
        if cols.len() < 12 {
            continue;
        }
        let Ok(level) = cols[0].parse::<i32>() else {
            continue;
        };
        if level != WORD_LEVEL {
            continue;
        }
        let text = cols[11];
        if text.trim().is_empty() {
            continue;
        }
        let Ok(confidence) = cols[10].parse::<f32>() else {
            continue;
        };
        if confidence < 0.0 {
            continue;
        }
        let (block_num, par_num, line_num, word_num, left, top, width, height) = (
            cols[2].parse().unwrap_or(0),
            cols[3].parse().unwrap_or(0),
            cols[4].parse().unwrap_or(0),
            cols[5].parse().unwrap_or(0),
            cols[6].parse().unwrap_or(0),
            cols[7].parse().unwrap_or(0),
            cols[8].parse().unwrap_or(0),
            cols[9].parse().unwrap_or(0),
        );
        words.push(OcrWord {
            text: text.to_string(),
            confidence,
            left,
            top,
            width,
            height,
            block_num,
            par_num,
            line_num,
            word_num,
        });
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
1\t1\t0\t0\t0\t0\t0\t0\t100\t200\t-1\t\n\
2\t1\t1\t0\t0\t0\t0\t0\t100\t20\t-1\t\n\
5\t1\t1\t1\t1\t1\t10\t5\t40\t15\t92.5\tBurger\n\
5\t1\t1\t1\t1\t2\t60\t5\t20\t15\t88.0\t12.00\n\
5\t1\t1\t1\t2\t1\t10\t25\t30\t15\t-1\tunreadable\n";

    #[test]
    fn parse_tsv_keeps_only_word_level_rows_with_nonnegative_confidence() {
        let words = parse_tsv(SAMPLE_TSV);
        assert_eq!(words.len(), 2, "the block/page rows and the -1-confidence word row should be dropped");
        assert_eq!(words[0].text, "Burger");
        assert_eq!(words[0].confidence, 92.5);
        assert_eq!(words[0].line_num, 1);
        assert_eq!(words[0].word_num, 1);
        assert_eq!(words[1].text, "12.00");
        assert_eq!(words[1].left, 60);
    }

    #[test]
    fn parse_tsv_handles_empty_and_header_only_input() {
        assert!(parse_tsv("").is_empty());
        assert!(parse_tsv("level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n").is_empty());
    }

    #[test]
    fn parse_tsv_skips_malformed_rows_without_panicking() {
        let tsv = "header\n5\ttoo\tfew\tcols\n";
        assert!(parse_tsv(tsv).is_empty());
    }
}
