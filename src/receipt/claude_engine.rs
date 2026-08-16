//! [`ReceiptEngine`] implementation backed by the Anthropic Messages API
//! (Claude Sonnet 5), using forced tool-use for reliable structured JSON
//! output. Unlike [`crate::receipt::receipt_engine::TesseractReceiptEngine`],
//! this bypasses `parser`/the word-level [`crate::receipt::OcrEngine`]
//! entirely — Claude reads the receipt image directly and returns
//! structured items/total.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use serde::Deserialize;

use crate::receipt::ocr_engine::OcrError;
use crate::receipt::receipt_engine::{ReceiptEngine, RecognizedItem, RecognizedReceipt};

/// Hardcoded per an explicit product decision — not itself config-driven.
/// Switched from Haiku 4.5 to Sonnet 5 after Haiku proved unreliable at
/// distinguishing subtotal from total on dense, small-text Cyrillic
/// receipts (see local testing notes) — Sonnet 5 trades some cost for
/// meaningfully better vision/structured-extraction accuracy.
const CLAUDE_MODEL: &str = "claude-sonnet-5";
const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Independent of the outer `OCR_TIMEOUT` (20s) wrapper in
/// `src/routes/receipt.rs` — this bounds just the HTTP call itself.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TOOL_NAME: &str = "record_receipt";
const MAX_RESPONSE_TOKENS: u32 = 1024;

const PROMPT: &str = "This is a photo of a restaurant/cafe receipt. It may be \
    printed in English, Russian, or Kazakh (Kazakhstani receipts are commonly \
    bilingual RU/KZ). Read every line item, its price, and the receipt's \
    subtotal/tax-or-tip/total, then record them via the record_receipt tool. \
    All amounts must be in integer cents (e.g. $12.50 -> 1250), never \
    decimal/float. If a field isn't present on the receipt, omit it — never \
    guess.";

pub struct ClaudeReceiptEngine {
    client: reqwest::blocking::Client,
    api_key: String,
}

impl ClaudeReceiptEngine {
    pub fn new(api_key: String) -> Result<Self, OcrError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| OcrError::Init(e.to_string()))?;
        Ok(Self { client, api_key })
    }
}

impl ReceiptEngine for ClaudeReceiptEngine {
    fn recognize_receipt(&self, image_bytes: &[u8]) -> Result<RecognizedReceipt, OcrError> {
        let body = build_request_body(image_bytes);

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| OcrError::RequestFailed(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            return Err(OcrError::RequestFailed(format!(
                "Anthropic API returned {status}: {text}"
            )));
        }

        let response_body: MessagesResponse = response
            .json()
            .map_err(|e| OcrError::RequestFailed(format!("failed to parse response: {e}")))?;

        let tool_input = response_body
            .content
            .into_iter()
            .find_map(|block| match block {
                ContentBlock::ToolUse { name, input } if name == TOOL_NAME => Some(input),
                _ => None,
            })
            .ok_or_else(|| OcrError::RequestFailed("no tool_use block in Claude response".into()))?;

        let extracted: ExtractedReceipt =
            serde_json::from_value(tool_input).map_err(|e| OcrError::InvalidOutput(e.to_string()))?;

        Ok(extracted.into())
    }
}

// ---------------------------------------------------------------------
// Request construction
// ---------------------------------------------------------------------

fn build_request_body(image_bytes: &[u8]) -> serde_json::Value {
    let encoded = BASE64_STANDARD.encode(image_bytes);
    serde_json::json!({
        "model": CLAUDE_MODEL,
        "max_tokens": MAX_RESPONSE_TOKENS,
        "tool_choice": { "type": "tool", "name": TOOL_NAME },
        "tools": [tool_definition()],
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": encoded,
                    }
                },
                {
                    "type": "text",
                    "text": PROMPT,
                }
            ]
        }]
    })
}

fn tool_definition() -> serde_json::Value {
    serde_json::json!({
        "name": TOOL_NAME,
        "description": "Records the structured contents of a restaurant/cafe \
            receipt: its line items and the subtotal/tax/total amounts, all in \
            integer cents.",
        "input_schema": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "Every line item on the receipt.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "price_cents": {
                                "type": "integer",
                                "description": "The item's total price in integer cents."
                            },
                            "quantity": {
                                "type": "integer",
                                "description": "Quantity, if shown (e.g. \"2x\")."
                            }
                        },
                        "required": ["name", "price_cents"]
                    }
                },
                "subtotal_cents": {
                    "type": "integer",
                    "description": "The subtotal line, in cents, if the receipt prints one separately from the total."
                },
                "tax_or_tip_cents": {
                    "type": "integer",
                    "description": "An explicit tax and/or tip/service-charge line, in cents, if printed."
                },
                "total_cents": {
                    "type": "integer",
                    "description": "The receipt's final total, in cents."
                }
            },
            "required": ["items"]
        }
    })
}

// ---------------------------------------------------------------------
// Response deserialization
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        #[allow(dead_code)]
        text: String,
    },
    ToolUse {
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct ExtractedReceipt {
    items: Vec<ExtractedItem>,
    subtotal_cents: Option<i64>,
    tax_or_tip_cents: Option<i64>,
    total_cents: Option<i64>,
}

#[derive(Deserialize)]
struct ExtractedItem {
    name: String,
    price_cents: i64,
    quantity: Option<u32>,
}

impl From<ExtractedReceipt> for RecognizedReceipt {
    fn from(r: ExtractedReceipt) -> Self {
        RecognizedReceipt {
            items: r
                .items
                .into_iter()
                .map(|i| RecognizedItem {
                    name: i.name,
                    price_cents: i.price_cents,
                    quantity_hint: i.quantity,
                    confidence: None,
                })
                .collect(),
            subtotal_cents: r.subtotal_cents,
            tax_cents: r.tax_or_tip_cents,
            total_cents: r.total_cents,
        }
    }
}
