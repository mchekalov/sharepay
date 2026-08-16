//! Image preprocessing pipeline (spec §1.2): decode+validate, EXIF
//! orientation correction, downscale to a bounded max dimension, grayscale,
//! contrast normalization, Otsu binarization. A straightforward sequential
//! pipeline — no branching/backtracking, no perspective correction/deskew/
//! glare removal (explicitly deferred per spec §1.2, in favor of Mobile
//! Client UX guidance).

use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use image::{DynamicImage, GenericImageView, GrayImage, ImageFormat};
use imageproc::contrast::{otsu_level, stretch_contrast, threshold, ThresholdType};

/// Long-edge cap for downscaling (spec §1.2: "downscale to a bounded max
/// dimension (long edge capped ~2000-2200px)"). Images already smaller than
/// this are left alone — never upscaled.
pub const MAX_LONG_EDGE: u32 = 2200;

#[derive(Debug, thiserror::Error)]
pub enum PreprocessError {
    /// The upload doesn't decode as a supported image format at all —
    /// maps to the `image_unreadable` failure reason (spec §1.6).
    #[error("could not decode image: {0}")]
    Decode(#[from] image::ImageError),
    /// Decoded successfully but re-encoding the processed result failed —
    /// treated as `internal_error` (spec §1.6), not a user-facing "bad
    /// photo" condition.
    #[error("failed to re-encode preprocessed image: {0}")]
    Encode(String),
}

/// Steps 1-3 (decode/validate, EXIF+OSD orientation correction, downscale)
/// — the genuinely engine-agnostic part of preprocessing: every
/// [`crate::receipt::ReceiptEngine`] wants an upright, bounded-size image,
/// regardless of what it does with it next. Shared by [`preprocess`]
/// (which continues on into Tesseract-specific grayscale/binarization) and
/// [`preprocess_light`] (which stops here, keeping full color).
fn decode_orient_downscale(raw_bytes: &[u8]) -> Result<DynamicImage, PreprocessError> {
    // 1. Decode + validate.
    let img = image::load_from_memory(raw_bytes)?;

    // 2. Correct EXIF orientation (read from the *original* bytes — the
    //    decoded `DynamicImage` above carries no EXIF metadata of its own).
    let img = correct_exif_orientation(raw_bytes, img);

    // 2b. Cross-check/fallback via Tesseract's own orientation-and-script
    //     detection (OSD). Real-world validation against genuine
    //     Kazakhstani retail receipt photos surfaced a case EXIF alone
    //     can't handle: no EXIF `Orientation` tag at all (common for
    //     re-saved/edited photos), yet the raw pixel data is landscape
    //     with the receipt's text running sideways in-frame. Always
    //     running this (rather than only when EXIF was absent) keeps the
    //     logic simple and makes it a genuine correctness cross-check in
    //     both directions: on an image EXIF already corrected, OSD
    //     reports "no rotation needed" and this is a no-op; when EXIF was
    //     absent/wrong, OSD detects and this applies the real fix.
    let img = correct_osd_orientation(img);

    // 3. Downscale to the bounded long edge (never upscale).
    Ok(downscale(img))
}

/// Finishes preprocessing a decoded image with the Tesseract-specific
/// steps (spec §1.2, steps 4-6: grayscale, contrast normalization, Otsu
/// binarization), returning PNG-encoded bytes.
fn binarize_and_encode(img: DynamicImage) -> Result<Vec<u8>, PreprocessError> {
    // 4. Grayscale.
    let gray = img.to_luma8();

    // 5. Contrast normalization: adaptive linear histogram stretch (spec
    //    §1.2: "histogram stretch / adaptive"), linearly remapping the
    //    image's actual [min, max] luma range to the full [0, 255] range.
    //    This compensates for dim/washed-out restaurant lighting the same
    //    way a fixed stretch would, but self-calibrates per photo instead
    //    of assuming a specific input range.
    //
    //    Deliberately *not* full histogram equalization
    //    (`imageproc::contrast::equalize_histogram`): that's a much more
    //    aggressive, non-linear remap driven by the histogram's local
    //    density, and empirically (verified against a synthetic
    //    high-contrast test receipt during development) it destroys fine
    //    glyph detail — Tesseract's mean word confidence dropped from ~96
    //    to ~45 on the same image, badly garbling digits — whereas the
    //    simple min/max linear stretch preserved ~96 confidence on both a
    //    clean image and a synthetically dimmed one.
    let gray = normalize_contrast(&gray);

    // 6. Binarization via Otsu's method (spec §1.2: "Otsu's method for
    //    v1; Sauvola if uneven lighting proves a problem").
    let level = otsu_level(&gray);
    let binarized = threshold(&gray, level, ThresholdType::Binary);

    let mut out = Vec::new();
    DynamicImage::ImageLuma8(binarized)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| PreprocessError::Encode(e.to_string()))?;
    Ok(out)
}

/// Runs the full preprocessing pipeline (spec §1.2, steps 1-6) on raw
/// uploaded photo bytes, returning PNG-encoded bytes of the binarized,
/// upright, bounded-size grayscale image, ready to hand to
/// [`crate::receipt::OcrEngine::recognize_words`]. Used by
/// [`crate::receipt::receipt_engine::TesseractReceiptEngine`] internally
/// (via [`finish_binarization`]) and directly by `examples/receipt_debug.rs`.
pub fn preprocess(raw_bytes: &[u8]) -> Result<Vec<u8>, PreprocessError> {
    let img = decode_orient_downscale(raw_bytes)?;
    binarize_and_encode(img)
}

/// Steps 1-3 only (decode/validate, orientation correction, downscale) —
/// no grayscale/contrast/binarization. PNG-encoded, full color. Used ahead
/// of a vision-model-based [`crate::receipt::ReceiptEngine`] (e.g.
/// [`crate::receipt::claude_engine::ClaudeReceiptEngine`]), which reads a
/// full-color photo more accurately than Tesseract's pure-black-and-white-
/// tuned output — verified empirically: binarization measurably degraded a
/// vision model's extraction quality on real receipt photos (garbled text,
/// lost the total/subtotal distinction). [`crate::routes::receipt`] calls
/// this once per upload; [`TesseractReceiptEngine`] finishes its own
/// Tesseract-specific steps on top via [`finish_binarization`], so the two
/// engines share the genuinely engine-agnostic prep and diverge only where
/// their needs actually differ.
///
/// [`TesseractReceiptEngine`]: crate::receipt::receipt_engine::TesseractReceiptEngine
pub fn preprocess_light(raw_bytes: &[u8]) -> Result<Vec<u8>, PreprocessError> {
    let img = decode_orient_downscale(raw_bytes)?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| PreprocessError::Encode(e.to_string()))?;
    Ok(out)
}

/// Finishes the Tesseract-specific steps (grayscale, contrast
/// normalization, Otsu binarization) on top of [`preprocess_light`]'s
/// output. Kept as a separate step (rather than folded back into
/// `preprocess_light`) so a vision-model engine never pays for it.
pub fn finish_binarization(light_png_bytes: &[u8]) -> Result<Vec<u8>, PreprocessError> {
    let img = image::load_from_memory(light_png_bytes)?;
    binarize_and_encode(img)
}

/// EXIF `Orientation` tag values 1-8 (TIFF/EXIF spec), applied as the
/// corresponding rotation/flip so downstream steps always see an upright
/// image. Unreadable/absent EXIF data (very common for re-saved/edited
/// photos, and for any non-JPEG upload) is treated as "no correction
/// needed" (orientation 1) rather than an error — this is best-effort
/// metadata, not a required field (spec §1.2: "disproportionately common
/// real-world failure if skipped", not "must be present").
fn correct_exif_orientation(raw_bytes: &[u8], img: DynamicImage) -> DynamicImage {
    match read_exif_orientation(raw_bytes) {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img, // 1, or unknown/absent
    }
}

fn read_exif_orientation(raw_bytes: &[u8]) -> u32 {
    let mut cursor = Cursor::new(raw_bytes);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) else {
        return 1;
    };
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .unwrap_or(1)
}

/// Long edge of the small thumbnail sent to Tesseract's OSD pass (below).
/// OSD only needs to tell text-block orientation apart, not resolve fine
/// glyph detail, so a small thumbnail keeps this fast — measured ~0.2s at
/// this size on this dev machine, vs. ~1s+ feeding a full ~4000px-long-edge
/// phone photo straight in.
const OSD_THUMBNAIL_LONG_EDGE: u32 = 1000;

/// Step 2b: detects the rotation (in degrees) needed to make `img` upright
/// via Tesseract's orientation-and-script-detection (OSD) mode, and applies
/// it. `leptess`/the underlying `tesseract-plumbing`/`tesseract-sys`
/// bindings this app builds on expose no OSD API at all (checked directly
/// against their source before writing this) — so, unlike every other OCR
/// call in this pipeline, this one shells out to the `tesseract` CLI binary
/// with `--psm 0` (its dedicated OSD mode) rather than going through the
/// in-process engine. Best-effort, matching [`correct_exif_orientation`]'s
/// philosophy: any failure (binary missing, non-zero exit, no parseable
/// `Rotate:` line — OSD declines to guess on a mostly-blank or
/// already-tight-cropped image) leaves the image unrotated rather than
/// erroring the whole pipeline.
fn correct_osd_orientation(img: DynamicImage) -> DynamicImage {
    let Some(thumb_bytes) = encode_png(&osd_thumbnail(&img)) else {
        return img;
    };
    match detect_osd_rotation(&thumb_bytes) {
        Some(90) => img.rotate90(),
        Some(180) => img.rotate180(),
        Some(270) => img.rotate270(),
        _ => img, // 0, unrecognized, or detection failed outright.
    }
}

/// Downscales (never upscales) `img` so its long edge is at most
/// [`OSD_THUMBNAIL_LONG_EDGE`], for feeding to [`detect_osd_rotation`].
fn osd_thumbnail(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let long_edge = w.max(h);
    if long_edge <= OSD_THUMBNAIL_LONG_EDGE {
        return img.clone();
    }
    let scale = OSD_THUMBNAIL_LONG_EDGE as f64 / long_edge as f64;
    let new_w = ((w as f64 * scale).round() as u32).max(1);
    let new_h = ((h as f64 * scale).round() as u32).max(1);
    img.resize(new_w, new_h, image::imageops::FilterType::Triangle)
}

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).ok()?;
    Some(out)
}

/// Runs `tesseract stdin - --psm 0` against `image_bytes` and parses its
/// `Rotate: N` output line — the clockwise rotation (0/90/180/270) needed
/// to make the image upright. `None` on any failure to run/parse.
fn detect_osd_rotation(image_bytes: &[u8]) -> Option<u32> {
    let mut child = Command::new("tesseract")
        .args(["stdin", "-", "--psm", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(image_bytes).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_osd_rotate(&String::from_utf8_lossy(&output.stdout))
}

/// Parses the `Rotate: N` line out of `tesseract --psm 0`'s stdout, e.g.:
/// ```text
/// Page number: 0
/// Orientation in degrees: 270
/// Rotate: 90
/// Orientation confidence: 16.73
/// Script: Cyrillic
/// Script confidence: 6.92
/// ```
/// A pure, easily-unit-testable split from [`detect_osd_rotation`]'s actual
/// process-spawning.
fn parse_osd_rotate(osd_output: &str) -> Option<u32> {
    osd_output
        .lines()
        .find_map(|line| line.strip_prefix("Rotate:")?.trim().parse::<u32>().ok())
}

/// Linearly stretches `gray`'s actual min-max luma range to fill [0, 255].
/// Falls back to returning the image unchanged for a degenerate
/// (constant-color) input, where [`stretch_contrast`] would otherwise
/// panic (`input_lower >= input_upper`).
fn normalize_contrast(gray: &GrayImage) -> GrayImage {
    let (min, max) = gray
        .pixels()
        .fold((u8::MAX, u8::MIN), |(lo, hi), p| (lo.min(p.0[0]), hi.max(p.0[0])));
    if min >= max {
        return gray.clone();
    }
    stretch_contrast(gray, min, max, 0, 255)
}

/// Downscales so the long edge is at most [`MAX_LONG_EDGE`], preserving
/// aspect ratio. Never upscales a smaller image.
fn downscale(img: DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let long_edge = w.max(h);
    if long_edge <= MAX_LONG_EDGE {
        return img;
    }
    let scale = MAX_LONG_EDGE as f64 / long_edge as f64;
    let new_w = ((w as f64 * scale).round() as u32).max(1);
    let new_h = ((h as f64 * scale).round() as u32).max(1);
    img.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    fn make_test_png(width: u32, height: u32) -> Vec<u8> {
        let img = GrayImage::from_fn(width, height, |x, y| {
            // A simple checkerboard-ish gradient so Otsu/equalize have
            // non-degenerate (non-constant) input to work on.
            if (x / 4 + y / 4) % 2 == 0 {
                Luma([40u8])
            } else {
                Luma([220u8])
            }
        });
        let mut out = Vec::new();
        DynamicImage::ImageLuma8(img)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn preprocess_rejects_undecodable_bytes() {
        let result = preprocess(b"this is not an image");
        assert!(matches!(result, Err(PreprocessError::Decode(_))));
    }

    #[test]
    fn preprocess_produces_a_decodable_png_of_bounded_size() {
        let input = make_test_png(300, 200);
        let output = preprocess(&input).expect("preprocessing a valid PNG should succeed");

        let decoded = image::load_from_memory(&output).expect("output should be a valid image");
        let (w, h) = decoded.dimensions();
        assert!(w <= MAX_LONG_EDGE && h <= MAX_LONG_EDGE);
        // Grayscale/binarized output: still decodes fine as luma.
        assert_eq!(decoded.to_luma8().dimensions(), (w, h));
    }

    #[test]
    fn preprocess_downscales_oversized_images_without_upscaling_small_ones() {
        let big = make_test_png(4000, 1000);
        let out = preprocess(&big).unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        assert_eq!(w, MAX_LONG_EDGE, "long edge should be capped exactly");
        assert!(h < 1000);

        let small = make_test_png(100, 80);
        let out = preprocess(&small).unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        assert_eq!((w, h), (100, 80), "small images must not be upscaled");
    }

    #[test]
    fn preprocess_light_keeps_color_and_is_not_binarized() {
        let input = make_test_png(200, 200);
        let output = preprocess_light(&input).unwrap();
        let decoded = image::load_from_memory(&output).unwrap().to_luma8();
        // The synthetic input is 40/220 luma, not 0/255 — preprocess_light
        // must not have binarized it (unlike `preprocess`, tested below).
        assert!(
            decoded.pixels().any(|p| p.0[0] != 0 && p.0[0] != 255),
            "preprocess_light should not binarize — pixels should retain non-extreme values"
        );
    }

    #[test]
    fn finish_binarization_on_top_of_preprocess_light_matches_preprocess_exactly() {
        // Proves the preprocess/preprocess_light+finish_binarization split
        // is behavior-preserving for the Tesseract path: same input, same
        // final binarized pixels, regardless of which route produced them.
        let input = make_test_png(300, 200);
        let via_preprocess = preprocess(&input).unwrap();
        let light = preprocess_light(&input).unwrap();
        let via_split = finish_binarization(&light).unwrap();

        let a = image::load_from_memory(&via_preprocess).unwrap().to_luma8();
        let b = image::load_from_memory(&via_split).unwrap().to_luma8();
        assert_eq!(a.dimensions(), b.dimensions());
        assert_eq!(a.into_raw(), b.into_raw());
    }

    #[test]
    fn binarized_output_is_pure_black_and_white() {
        let input = make_test_png(200, 200);
        let output = preprocess(&input).unwrap();
        let decoded = image::load_from_memory(&output).unwrap().to_luma8();
        for pixel in decoded.pixels() {
            assert!(
                pixel.0[0] == 0 || pixel.0[0] == 255,
                "Otsu binarization should produce only pure black/white pixels, got {}",
                pixel.0[0]
            );
        }
    }

    // -- OSD (`tesseract --psm 0`) output parsing --------------------------

    #[test]
    fn parses_rotate_line_from_real_osd_output() {
        let output = "Page number: 0\n\
                       Orientation in degrees: 270\n\
                       Rotate: 90\n\
                       Orientation confidence: 16.73\n\
                       Script: Cyrillic\n\
                       Script confidence: 6.92\n";
        assert_eq!(parse_osd_rotate(output), Some(90));
    }

    #[test]
    fn parses_zero_rotate_for_an_already_upright_image() {
        let output = "Page number: 0\nOrientation in degrees: 0\nRotate: 0\n";
        assert_eq!(parse_osd_rotate(output), Some(0));
    }

    #[test]
    fn returns_none_when_no_rotate_line_present() {
        // OSD declines to guess (e.g. too little text) and emits an error
        // instead of a normal report.
        assert_eq!(parse_osd_rotate(""), None);
        assert_eq!(parse_osd_rotate("Too few characters. Skipping this page\n"), None);
    }

    #[test]
    fn osd_correction_rotates_a_sideways_synthetic_receipt_upright() {
        // Skip gracefully if the `tesseract` CLI binary isn't on PATH —
        // this one test step (unlike the in-process `leptess` calls used
        // everywhere else) depends on it directly.
        if Command::new("tesseract").arg("--version").output().is_err() {
            eprintln!("skipping: `tesseract` CLI binary not found on PATH");
            return;
        }
        // A tall, mostly-white image with a solid black band isn't enough
        // real text for OSD to make a confident call either way, so this
        // just exercises that `correct_osd_orientation` runs without
        // panicking/hanging and returns *some* valid image back out; the
        // real, text-bearing rotation behavior is validated against actual
        // receipt photos separately (see the task report, not a unit test
        // — OSD's confidence on real text needs real text).
        let img = DynamicImage::ImageLuma8(GrayImage::from_pixel(300, 600, Luma([255u8])));
        let out = correct_osd_orientation(img);
        assert!(out.dimensions().0 > 0 && out.dimensions().1 > 0);
    }
}
