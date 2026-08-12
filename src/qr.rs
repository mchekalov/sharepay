//! QR code generation (spec §2.2).
//!
//! Renders an inline SVG (not PNG) from the join URL, at error-correction
//! level M, generated on demand at request time — never precomputed or
//! cached (spec §2.2: cheap, no cache-invalidation to manage).

use qrcode::render::svg;
use qrcode::{EcLevel, QrCode};

#[derive(Debug, thiserror::Error)]
pub enum QrError {
    #[error("failed to encode QR code: {0}")]
    Encode(#[from] qrcode::types::QrError),
}

/// Builds the join URL for a bill: `<base_url>/b/<bill_id>` (spec §2.1 —
/// single path segment, no query params).
pub fn join_url(base_url: &str, bill_id: &str) -> String {
    format!("{}/b/{}", base_url.trim_end_matches('/'), bill_id)
}

/// Renders `data` (the join URL) as an inline SVG string at error
/// correction level M — spec §2.2 calls for "M or Q" since restaurant
/// lighting and arm's-length phone-camera scanning benefit from the
/// redundancy; M is chosen over Q as the lower of the two acceptable
/// levels, keeping the rendered code slightly smaller/less dense for a
/// token-length payload that's already small.
pub fn render_svg(data: &str) -> Result<String, QrError> {
    let code = QrCode::with_error_correction_level(data, EcLevel::M)?;
    let svg_xml = code
        .render()
        .min_dimensions(200, 200)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Ok(svg_xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_url_has_single_clean_path_segment() {
        assert_eq!(
            join_url("https://sharepay.app", "abc123"),
            "https://sharepay.app/b/abc123"
        );
        // Trailing slash on base_url is tolerated.
        assert_eq!(
            join_url("https://sharepay.app/", "abc123"),
            "https://sharepay.app/b/abc123"
        );
    }

    #[test]
    fn render_svg_produces_svg_markup() {
        let svg_xml = render_svg("https://sharepay.app/b/abc123").unwrap();
        assert!(svg_xml.contains("<svg"));
        assert!(svg_xml.contains("</svg>"));
    }
}
