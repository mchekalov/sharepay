//! Standalone debugging tool for the Claude receipt engine: dumps the full
//! raw Anthropic API response for a given image, both preprocessed
//! (Tesseract-oriented binarization) and raw, to compare. Not part of the
//! app; local-testing-only.
//!
//! Usage:
//!   SHAREPAY_ANTHROPIC_API_KEY=sk-ant-... cargo run --example claude_debug -- <path-to-image>

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use sharepay::receipt::preprocess;

const CLAUDE_MODEL: &str = "claude-sonnet-5";

fn call_claude(api_key: &str, image_bytes: &[u8], label: &str) {
    println!("\n=== {label} ({} bytes) ===", image_bytes.len());
    let encoded = BASE64_STANDARD.encode(image_bytes);
    let body = serde_json::json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 1024,
        "tool_choice": { "type": "tool", "name": "record_receipt" },
        "tools": [{
            "name": "record_receipt",
            "description": "Records the structured contents of a restaurant/cafe receipt: its line items and the subtotal/tax/total amounts, all in integer cents.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "price_cents": { "type": "integer" },
                                "quantity": { "type": "integer" }
                            },
                            "required": ["name", "price_cents"]
                        }
                    },
                    "subtotal_cents": { "type": "integer" },
                    "tax_or_tip_cents": { "type": "integer" },
                    "total_cents": { "type": "integer" }
                },
                "required": ["items"]
            }
        }],
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/png", "data": encoded }
                },
                {
                    "type": "text",
                    "text": "This is a photo of a restaurant/cafe receipt. It may be printed in English, Russian, or Kazakh (Kazakhstani receipts are commonly bilingual RU/KZ). Read every line item, its price, and the receipt's subtotal/tax-or-tip/total, then record them via the record_receipt tool. All amounts must be in integer cents (e.g. $12.50 -> 1250), never decimal/float. If a field isn't present on the receipt, omit it — never guess."
                }
            ]
        }]
    });

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .expect("request failed");
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    println!("status: {status}");
    println!("{text}");
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: claude_debug <path-to-image>");
    let api_key = std::env::var("SHAREPAY_ANTHROPIC_API_KEY").expect("SHAREPAY_ANTHROPIC_API_KEY must be set");
    let raw = std::fs::read(&path).expect("failed to read image");

    // Original bytes, re-encoded as-is (JPEG media type).
    println!("\n### Testing against ORIGINAL (unpreprocessed) JPEG ###");
    call_claude_jpeg(&api_key, &raw);

    // Preprocessed (Tesseract-oriented binarized PNG).
    let preprocessed = preprocess::preprocess(&raw).expect("preprocess failed");
    call_claude(&api_key, &preprocessed, "PREPROCESSED (binarized PNG, what the real pipeline sends)");
}

fn call_claude_jpeg(api_key: &str, image_bytes: &[u8]) {
    let encoded = BASE64_STANDARD.encode(image_bytes);
    let body = serde_json::json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 1024,
        "tool_choice": { "type": "tool", "name": "record_receipt" },
        "tools": [{
            "name": "record_receipt",
            "description": "Records the structured contents of a restaurant/cafe receipt: its line items and the subtotal/tax/total amounts, all in integer cents.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "price_cents": { "type": "integer" },
                                "quantity": { "type": "integer" }
                            },
                            "required": ["name", "price_cents"]
                        }
                    },
                    "subtotal_cents": { "type": "integer" },
                    "tax_or_tip_cents": { "type": "integer" },
                    "total_cents": { "type": "integer" }
                },
                "required": ["items"]
            }
        }],
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/jpeg", "data": encoded }
                },
                {
                    "type": "text",
                    "text": "This is a photo of a restaurant/cafe receipt. It may be printed in English, Russian, or Kazakh (Kazakhstani receipts are commonly bilingual RU/KZ). Read every line item, its price, and the receipt's subtotal/tax-or-tip/total, then record them via the record_receipt tool. All amounts must be in integer cents (e.g. $12.50 -> 1250), never decimal/float. If a field isn't present on the receipt, omit it — never guess."
                }
            ]
        }]
    });

    println!("({} bytes)", image_bytes.len());
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .expect("request failed");
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    println!("status: {status}");
    println!("{text}");
}
