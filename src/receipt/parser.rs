//! Parsing pipeline: OCR output -> structured items (spec §1.3).
//!
//! Line reconstruction groups [`OcrWord`]s by Tesseract's own
//! `block_num`/`par_num`/`line_num` grouping (v1 simplification — the
//! spec's bounding-box-overlap fallback/cross-check for line grouping that
//! misfires on low-quality photos is **not** implemented; noted as a
//! tradeoff in the top-level summary, not silently dropped). Each
//! reconstructed line is then classified into `item`/`subtotal`/`tax`/
//! `tip_service`/`total`/`discount`/`noise` via a price-tail regex plus
//! keyword matching, and the receipt's total line is identified via the
//! bottom-up priority rules.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::receipt::ocr_engine::OcrWord;

// ---------------------------------------------------------------------
// Line reconstruction
// ---------------------------------------------------------------------

/// One reconstructed line of OCR text: the concatenation of a group of
/// words sharing Tesseract's `(block_num, par_num, line_num)`, in reading
/// order, plus aggregated confidence.
#[derive(Debug, Clone)]
pub struct RawLine {
    pub text: String,
    /// Position of this line among all reconstructed lines, top-to-bottom
    /// — used by [`identify_total`]'s bottom-up scan and "bottom third"
    /// fallback.
    pub line_index: usize,
    /// Mean of composing words' confidence (spec §1.7: "aggregate...up to
    /// per-item (mean or min of composing words)").
    pub mean_confidence: f32,
    pub min_confidence: f32,
}

/// Groups words by Tesseract's own line grouping and concatenates each
/// group into a [`RawLine`], preserving reading order (spec §1.3, step 1).
pub fn reconstruct_lines(words: &[OcrWord]) -> Vec<RawLine> {
    let mut order: Vec<(i32, i32, i32)> = Vec::new();
    let mut groups: HashMap<(i32, i32, i32), Vec<&OcrWord>> = HashMap::new();

    for w in words {
        let key = (w.block_num, w.par_num, w.line_num);
        groups
            .entry(key)
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(w);
    }

    order
        .into_iter()
        .enumerate()
        .map(|(idx, key)| {
            let mut ws = groups.remove(&key).expect("key was just inserted above");
            ws.sort_by_key(|w| w.word_num);
            let text = ws
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let confidences: Vec<f32> = ws.iter().map(|w| w.confidence).collect();
            let mean_confidence =
                confidences.iter().sum::<f32>() / confidences.len().max(1) as f32;
            let min_confidence = confidences
                .iter()
                .cloned()
                .fold(f32::INFINITY, f32::min);
            RawLine {
                text,
                line_index: idx,
                mean_confidence,
                min_confidence: if min_confidence.is_finite() {
                    min_confidence
                } else {
                    0.0
                },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// Price / quantity token extraction (spec §1.3)
// ---------------------------------------------------------------------

/// Price token regex: tail of line, since prices are right-aligned on
/// printed receipts (spec §1.3). Requires exactly two trailing decimal
/// digits (`.` or `,`); before that, the "whole" part is either plain
/// (ungrouped) digits of any length — e.g. `10440` in `10440.00`, a format
/// observed on a real Kazakhstani receipt with no thousands separator at
/// all — or standard thousands-grouped digits using `.`, `,`, **or a
/// space** as the grouping character (e.g. `1,234`, `1.234`, or `1 234` /
/// `12 037`, the last of which is the everyday Russian/Kazakh printed
/// convention: space-grouped thousands, comma decimal, as in `12 037,00`).
/// Optional currency symbol, optional leading `-`. Tolerates a short run of
/// trailing junk after the price — up to 3 non-digit characters, optionally
/// with one further stray digit amid them (`(?:[^\d]{0,3}\d)?[^\d]{0,3}$`
/// rather than a strict `\s*$`) — real photos of low-quality/low-light
/// printed receipts routinely produce a stray misrecognized character or
/// two right at the line's cut edge (observed on real Kazakhstani receipt
/// OCR output: a trailing stray `a,` after an otherwise-correct total
/// price; a trailing stray ` 1` — a lone extra digit — after an
/// otherwise-correct item value). A strict end-of-line anchor loses the
/// whole line's price (and, transitively, its keyword classification,
/// since keyword matching only runs on lines where a price was already
/// found) over 1-2 junk characters that clearly aren't part of the number
/// itself; the captured price group is unaffected either way; this only
/// changes whether such a line matches *at all*.
static PRICE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\$€£]?\s*(-?(?:\d{1,3}(?:[ .,]\d{3})+|\d+)[.,]\d{2})(?:[^\d]{0,3}\d)?[^\d]{0,3}$")
        .unwrap()
});

/// Quantity-prefix regex: head of line, e.g. `"2x Burger"` (spec §1.3).
static QTY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(\d{1,2})\s*[x×]\s*(.+)$").unwrap());

/// A parenthesized amount at the end of a line, e.g. `"(5.00)"` — a common
/// printed convention for a negative/discount amount that the plain
/// [`PRICE_RE`] tail-anchor won't match (trailing `)` breaks the anchor).
/// Same locale-aware whole-part grouping as [`PRICE_RE`] (see its doc
/// comment).
static PAREN_PRICE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\(\s*[\$€£]?\s*((?:\d{1,3}(?:[ .,]\d{3})+|\d+)[.,]\d{2})\s*\)\s*$").unwrap()
});

static PHONE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(?\d{3}\)?[\s.-]\d{3}[\s.-]\d{4}").unwrap());

static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b").unwrap());

static GRAND_TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(grand\s*total|amount\s*due|balance\s*due)\b").unwrap());

// Keyword lists are matched via case-insensitive substring containment
// (see `contains_any`), so a bilingual line like `ЖИЫНЫ / ИТОГ` or
// `БАРЛЫҒЫ/ИТОГО` matches on whichever language's word it contains — no
// separate language-detection step needed. English and Russian/Kazakh
// keywords are matched in parallel (not a language switch): this app
// keeps working on English receipts, and gains Cyrillic support alongside
// it, per real Kazakhstani retail/restaurant receipts used to validate
// this pipeline (Kazakhstan issues bilingual RU/KZ receipts).
const SUBTOTAL_KEYWORDS: &[&str] = &["subtotal", "sub total", "sub-total"];
// No clear, unambiguous Cyrillic *subtotal*-only term (distinct from
// `total`) turned up on any of the 4 real validation receipts — all of
// them print a single bottom-line total with no separate subtotal line at
// all. Left English-only; add Cyrillic subtotal keywords here if/when a
// real receipt surfaces one.
const TAX_KEYWORDS: &[&str] = &[
    "tax", "vat", "gst", "hst", // English
    "ндс", "налог", "ккс", "салық", // Russian/Kazakh
];
const TIP_KEYWORDS: &[&str] = &[
    "tip", "gratuity", "service charge", "service fee", // English
    "чаевые", "сервис", // Russian
];
const TOTAL_KEYWORDS: &[&str] = &[
    "grand total", "amount due", "balance due", "total", // English
    "итог", "итого", "барлығы", "барлыгы", "жиыны", // Russian/Kazakh
];
const DISCOUNT_KEYWORDS: &[&str] = &[
    "discount", "coupon", "promo", "off", // English
    "скидка", "жеңілдік", "шегерім", // Russian/Kazakh
];
/// A per-*item* "value/cost" label line — Russian `Стоимость` / Kazakh
/// `Құны` (OCR'd loosely as `Куны`) — seen on real receipts where one line
/// item's price is printed on its own follow-up line rather than trailing
/// the item name directly (spec: see [`merge_item_value_lines`]).
/// Deliberately **not** merged into [`TOTAL_KEYWORDS`]: despite "cost/
/// value" sounding total-adjacent, this labels a single item's price, not
/// the receipt-level total.
///
/// Matched via [`VALUE_LABEL_RE`] rather than plain substring containment
/// (like every other keyword list here) because this specific word gets
/// OCR-mangled unusually often in practice — real receipt scans in this
/// pipeline's validation set produced `Стоимость`, `Стоймость` (о/й typo),
/// and `Стоймасть` (а further garbled variant) for the *same* printed
/// word, all sharing a `сто...сть` shape but not a common literal
/// substring beyond the first three letters.
static VALUE_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)куны|құны|сто\w{2,6}сть|тоимость").unwrap());
const NOISE_KEYWORDS: &[&str] = &[
    // English
    "thank you",
    "server:",
    "table:",
    "cash",
    "change",
    "visa",
    "mastercard",
    "amex",
    "american express",
    "discover",
    // Russian/Kazakh receipt header/footer boilerplate observed on real
    // Kazakhstani retail/restaurant receipts: store registration numbers,
    // shift/cashier/register metadata, addresses, QR/fiscal-portal URLs,
    // card-payment lines. Not exhaustive by design (spec: "don't need to
    // be exhaustive, just enough that these don't get misclassified as
    // items") — real receipts will always carry some residual noise a
    // fixed keyword list can't fully anticipate; reconciliation tolerance
    // is the actual backstop for whatever slips through.
    "жсн",
    "бин",
    "ктн",
    "рнк",
    "фб/фп",
    "фискал",
    "смена",
    "ауысым",
    "касса",
    "кассир",
    "заказ",
    "тапсырыс",
    "серия",
    "уақыты",
    "время:",
    "адрес",
    "мекен",
    "сайт",
    "http",
    "www",
    "офд",
    "карт", // "банк картасы"/"банковская карта"/"платежной картой" etc.
    "сдача",
    "айтарым",
];

fn contains_any(haystack_lower: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack_lower.contains(n))
}

/// Normalizes a price-token match (e.g. `"1,234.50"`, `"12,50"`,
/// `"12 037,00"`, `"10440.00"`) to integer cents. Never a float (spec
/// §1.4: "all money as integer cents throughout — never floating point").
///
/// Locale-generalized rule (per real-world Russian/Kazakh-formatted
/// receipts observed alongside the original English/US ones): the decimal
/// separator is whichever of `.`/`,` appears **last** in the token — by
/// construction (see [`PRICE_RE`]) that's always the one immediately
/// followed by exactly two trailing digits, since any earlier `.`/`,`/` `
/// (space) occurrences are thousands-grouping separators followed by a
/// full group of three digits, never exactly two. Everything before that
/// separator has every non-digit character (any of `.`, `,`, or a
/// thousands-grouping space) stripped and the remaining digits read as the
/// whole-unit part — so `"12 037,00"` and `"12,037.00"` normalize
/// identically.
fn parse_price_to_cents(raw: &str) -> Option<i64> {
    let negative = raw.starts_with('-');
    let s = raw.trim_start_matches('-');

    let dec_pos = match (s.rfind('.'), s.rfind(',')) {
        (Some(d), Some(c)) => d.max(c),
        (Some(d), None) => d,
        (None, Some(c)) => c,
        (None, None) => return None,
    };
    let (whole_part, frac_part) = s.split_at(dec_pos);
    let frac_part = &frac_part[1..]; // drop the separator itself
    if frac_part.len() != 2 || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole_digits: String = whole_part.chars().filter(|c| c.is_ascii_digit()).collect();
    let whole: i64 = if whole_digits.is_empty() {
        0
    } else {
        whole_digits.parse().ok()?
    };
    let frac: i64 = frac_part.parse().ok()?;
    let cents = whole * 100 + frac;
    Some(if negative { -cents } else { cents })
}

/// Extracts the trailing price token from a line, if present. Returns
/// `(price_cents, text_before_the_price_token)`. Because the regex is
/// anchored to the end of the line, if two price-shaped tokens appear on
/// one line (e.g. a unit price and a line total), only the rightmost is
/// matched — satisfying spec §1.3's "take the rightmost as the line total"
/// without any extra logic.
pub fn extract_trailing_price(line: &str) -> Option<(i64, String)> {
    let caps = PRICE_RE.captures(line)?;
    let whole_match = caps.get(0)?;
    let cents = parse_price_to_cents(caps.get(1)?.as_str())?;
    let before = line[..whole_match.start()].trim().to_string();
    Some((cents, before))
}

/// Strips a leading `"2x "` / `"2 × "` quantity prefix, if present.
/// Returned quantity is **display metadata only** (spec §1.3's "Open
/// Risks": the parsed price is the line's already-computed total, never
/// multiplied by quantity).
pub fn strip_quantity_prefix(text: &str) -> (Option<u32>, String) {
    match QTY_RE.captures(text) {
        Some(caps) => {
            let qty = caps.get(1).and_then(|m| m.as_str().parse().ok());
            let name = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            (qty, name)
        }
        None => (None, text.trim().to_string()),
    }
}

// ---------------------------------------------------------------------
// Line classification (spec §1.3)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCategory {
    Item,
    Subtotal,
    Tax,
    TipService,
    Total,
    Discount,
    /// A per-item "value/cost" label line (Russian `Стоимость` / Kazakh
    /// `Құны`) — see [`VALUE_LABEL_RE`] and [`merge_item_value_lines`].
    /// Never reaches [`crate::routes::receipt`]'s item/total extraction
    /// directly: [`parse_lines`] merges it into the preceding item-name
    /// line before returning.
    ValueLabel,
    Noise,
}

#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub category: LineCategory,
    pub raw_text: String,
    pub line_index: usize,
    /// Item name (price/quantity-prefix stripped, trimmed) — `Some` only
    /// for [`LineCategory::Item`] lines.
    pub name: Option<String>,
    pub quantity_hint: Option<u32>,
    pub price_cents: Option<i64>,
    pub mean_confidence: f32,
    pub min_confidence: f32,
}

/// Classifies one reconstructed line per spec §1.3's rules: price-tail
/// regex + keyword matching. "A line is `item` only if it has a trailing
/// price match **and** matches none of the other categories."
pub fn classify_line(raw: &RawLine) -> ParsedLine {
    let base = |category: LineCategory, name: Option<String>, price_cents: Option<i64>| ParsedLine {
        category,
        raw_text: raw.text.clone(),
        line_index: raw.line_index,
        name,
        quantity_hint: None,
        price_cents,
        mean_confidence: raw.mean_confidence,
        min_confidence: raw.min_confidence,
    };

    // Parenthesized amount, e.g. "Loyalty discount (5.00)" — checked first
    // since it's a strong, unambiguous discount signal that the plain
    // trailing-price regex can't see (the trailing ')' breaks its anchor).
    if let Some(caps) = PAREN_PRICE_RE.captures(&raw.text)
        && let (Some(whole), Some(group)) = (caps.get(0), caps.get(1))
            && let Some(cents) = parse_price_to_cents(group.as_str()) {
                let name = raw.text[..whole.start()].trim().to_string();
                return base(LineCategory::Discount, Some(name), Some(-cents));
            }

    let Some((price_cents, before_price)) = extract_trailing_price(&raw.text) else {
        // No trailing price match at all. Usually noise (spec §1.3's
        // `noise` definition includes this case unconditionally) -- except
        // a bare `Куны/Стоимость` value-label line, printed on its own
        // line with the actual price following on a *later* line (see
        // `merge_item_value_lines`): flagging it `ValueLabel` here (rather
        // than losing it to `Noise`) is what lets that merge step find it.
        let lower = raw.text.to_lowercase();
        let category = if VALUE_LABEL_RE.is_match(&lower) {
            LineCategory::ValueLabel
        } else {
            LineCategory::Noise
        };
        return base(category, None, None);
    };

    let lower = raw.text.to_lowercase();

    let category = if VALUE_LABEL_RE.is_match(&lower) {
        LineCategory::ValueLabel
    } else if contains_any(&lower, SUBTOTAL_KEYWORDS) {
        LineCategory::Subtotal
    } else if contains_any(&lower, TAX_KEYWORDS) {
        LineCategory::Tax
    } else if contains_any(&lower, TIP_KEYWORDS) {
        LineCategory::TipService
    } else if contains_any(&lower, TOTAL_KEYWORDS) {
        LineCategory::Total
    } else if contains_any(&lower, DISCOUNT_KEYWORDS) || price_cents < 0 {
        LineCategory::Discount
    } else if contains_any(&lower, NOISE_KEYWORDS)
        || PHONE_RE.is_match(&raw.text)
        || DATE_RE.is_match(&raw.text)
    {
        LineCategory::Noise
    } else {
        LineCategory::Item
    };

    let (quantity_hint, name) = strip_quantity_prefix(&before_price);

    // A price-bearing line that would otherwise fall through to `Item` but
    // whose name (after quantity-prefix stripping) has fewer than 2
    // alphabetic characters isn't really a named line item -- it's an
    // artifact: a stray recap/duplicate price line (e.g. a receipt
    // printing a line total once as part of a `qty x price = total` line
    // and again on its own recap line a few lines later, OCR'd with no
    // real surrounding text) or pure OCR noise that happened to end in
    // something price-shaped. Downgrading these to `Noise` rather than
    // `Item` avoids inflating `items_sum` with near-nameless duplicate/
    // spurious price lines (observed on a real receipt where this exact
    // shape doubled the item total).
    let has_real_name = name.chars().filter(|c| c.is_alphabetic()).count() >= 2;
    let category = if category == LineCategory::Item && !has_real_name {
        LineCategory::Noise
    } else {
        category
    };

    let mut parsed = base(
        category,
        (category == LineCategory::Item).then_some(name),
        Some(price_cents),
    );
    parsed.quantity_hint = quantity_hint;
    parsed
}

/// Runs [`reconstruct_lines`] + [`classify_line`] + [`merge_item_value_lines`]
/// end to end.
pub fn parse_lines(words: &[OcrWord]) -> Vec<ParsedLine> {
    let classified: Vec<ParsedLine> = reconstruct_lines(words).iter().map(classify_line).collect();
    merge_item_value_lines(classified)
}

// ---------------------------------------------------------------------
// Multi-line item/value-label merging
// ---------------------------------------------------------------------

/// How many reconstructed lines *before* a [`LineCategory::ValueLabel`]
/// line to search backward for the item-name line it belongs to. Wider
/// than the "previous 1-3 lines" a minimal version of this receipt layout
/// would need, because real receipts interleave extra noise lines here
/// too — a product serial/barcode line and/or a per-item VAT breakdown
/// line commonly land *between* an item's name and its value label (both
/// observed on real Kazakhstani grocery receipts used to validate this
/// pipeline).
const VALUE_LABEL_BACKWARD_WINDOW: usize = 6;
/// How many lines *after* a *bare* (price-less) value-label line to search
/// for the price it labels.
const VALUE_LOOKAHEAD_FROM_LABEL: usize = 2;

/// Merges the multi-line item layout seen on some real receipts (spec
/// §1.3 extension) — a bare item-name line, optionally a `qty x
/// unit_price` line, then a `Куны/Стоимость` ("Cost/Value") label line
/// carrying (or immediately preceding) the actual line-total price —
/// into a single logical [`LineCategory::Item`] (name + that price),
/// rather than parsing the name line as unpriced noise and any
/// intervening qty/unit-price line as a bogus separate item.
///
/// Example (real OCR output, reconstructed lines):
/// ```text
/// 1.Брестское угощение Брест      <- bare name, no trailing price -> Noise
/// 0,435 кг/кг х 9 850,00          <- qty x unit price (has *a* price, but
///                                     it's the unit price, not the line
///                                     total -- would otherwise misparse
///                                     as its own bogus, wrongly-priced item)
/// Куны/Стоимость                  <- value-label line, no price on it
/// 4 284,75                        <- the actual line-total price
/// ```
/// merges to one item: name `"1.Брестское угощение Брест"`, price
/// `4 284,75`. Runs after [`classify_line`], on the whole reconstructed
/// line sequence (spec §1.3, step 3 extension) — a v1 heuristic, not
/// guaranteed against every receipt layout variant.
///
/// Deliberately resolves **backward from each value-label line** to the
/// *nearest* unclaimed preceding name candidate, rather than scanning
/// forward from each name candidate to the nearest label: on a receipt
/// with several short boilerplate lines between an item's true name and
/// its value label (a wide-enough backward window to reach across them is
/// needed — see [`VALUE_LABEL_BACKWARD_WINDOW`]), scanning forward from
/// *every* plausible-looking name candidate in top-down order let an
/// unrelated, earlier noise line that happened to also look name-shaped
/// (e.g. a boilerplate "Чек №8" receipt-number line) wrongly claim a value
/// label meant for the real item a few lines below it, since forward
/// search only cares whether *a* label is reachable, not whether it's the
/// *closest* one. Resolving backward-from-the-label instead guarantees
/// each label pairs with whichever name line is actually adjacent to it.
fn merge_item_value_lines(lines: Vec<ParsedLine>) -> Vec<ParsedLine> {
    let n = lines.len();
    let mut consumed = vec![false; n];
    let mut replacement: HashMap<usize, ParsedLine> = HashMap::new();

    for (j, label_line) in lines.iter().enumerate() {
        if label_line.category != LineCategory::ValueLabel {
            continue;
        }

        // Resolve the price this label carries (either on its own line, or
        // on a nearby following line — see the doc comment above).
        let resolved = label_line.price_cents.map(|p| (j, p)).or_else(|| {
            let search_end = (j + VALUE_LOOKAHEAD_FROM_LABEL).min(n.saturating_sub(1));
            ((j + 1)..=search_end).find_map(|k| lines[k].price_cents.map(|p| (k, p)))
        });
        let Some((price_idx, price_cents)) = resolved else {
            continue;
        };

        // Nearest unclaimed plausible item-name line strictly before this
        // label, within the backward window.
        let search_start = j.saturating_sub(VALUE_LABEL_BACKWARD_WINDOW);
        let name_idx = (search_start..j).rev().find(|&k| {
            !consumed[k]
                && lines[k].category == LineCategory::Noise
                && lines[k].price_cents.is_none()
                && is_plausible_item_name(&lines[k].raw_text)
        });
        let Some(name_idx) = name_idx else {
            continue;
        };

        for consumed_flag in consumed.iter_mut().take(price_idx + 1).skip(name_idx) {
            *consumed_flag = true;
        }
        let mut item = lines[name_idx].clone();
        item.category = LineCategory::Item;
        item.name = Some(clean_item_name(&lines[name_idx].raw_text));
        item.price_cents = Some(price_cents);
        replacement.insert(name_idx, item);
    }

    (0..n)
        .filter_map(|i| {
            replacement
                .remove(&i)
                .or_else(|| (!consumed[i]).then(|| lines[i].clone()))
        })
        .collect()
}

/// A loose plausibility filter for "is this bare, price-less line an item
/// name awaiting its price on a later line, or just unrelated boilerplate
/// noise" — used by [`merge_item_value_lines`]. Rejects lines that are too
/// short, mostly non-alphabetic (store registration/serial numbers are
/// digit-heavy runs, not names), or that already match a known noise
/// keyword/phone/date pattern.
fn is_plausible_item_name(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < 3 {
        return false;
    }
    let letters = trimmed.chars().filter(|c| c.is_alphabetic()).count();
    let digits = trimmed.chars().filter(|c| c.is_ascii_digit()).count();
    if letters < 3 || digits > letters {
        return false;
    }
    let lower = trimmed.to_lowercase();
    !contains_any(&lower, NOISE_KEYWORDS) && !PHONE_RE.is_match(trimmed) && !DATE_RE.is_match(trimmed)
}

/// Strips a leading item-ordinal prefix (e.g. `"1."`, `"12)"`) some
/// receipts number their line items with, then trims. Best-effort
/// cosmetic cleanup, not load-bearing for parsing correctness.
static ITEM_ORDINAL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d{1,2}[.)]\s*").unwrap());

fn clean_item_name(text: &str) -> String {
    ITEM_ORDINAL_RE.replace(text.trim(), "").trim().to_string()
}

// ---------------------------------------------------------------------
// Total-line identification (spec §1.3, bottom-up priority rules)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TotalCandidate {
    pub price_cents: i64,
    pub line_index: usize,
}

/// Identifies the receipt's total line, scanning bottom-up per spec §1.3:
/// 1. First line matching `grand total`/`amount due`/`balance due` with a
///    valid price.
/// 2. Else, first non-subtotal line classified [`LineCategory::Total`].
/// 3. Else, the numerically largest price among the bottom third of lines.
/// 4. Else `None` — the caller must never guess silently.
///
/// At every step, a candidate smaller than the largest single item line is
/// rejected as a likely misparse and the scan falls through to the next
/// priority level.
pub fn identify_total(lines: &[ParsedLine]) -> Option<TotalCandidate> {
    let largest_item_price = lines
        .iter()
        .filter(|l| l.category == LineCategory::Item)
        .filter_map(|l| l.price_cents)
        .max();
    let is_plausible_total = |price: i64| largest_item_price.is_none_or(|max| price >= max);

    // Step 1: grand total / amount due / balance due.
    for line in lines.iter().rev() {
        if let Some(price) = line.price_cents
            && GRAND_TOTAL_RE.is_match(&line.raw_text) && is_plausible_total(price) {
                return Some(TotalCandidate {
                    price_cents: price,
                    line_index: line.line_index,
                });
            }
    }

    // Step 2: any other non-subtotal "total" line.
    for line in lines.iter().rev() {
        if line.category == LineCategory::Total
            && let Some(price) = line.price_cents
                && is_plausible_total(price) {
                    return Some(TotalCandidate {
                        price_cents: price,
                        line_index: line.line_index,
                    });
                }
    }

    // Step 3: fallback — largest price among the bottom third of lines.
    if lines.is_empty() {
        return None;
    }
    let n = lines.len();
    let bottom_third_start = n - n.div_ceil(3);
    lines
        .iter()
        .filter(|l| l.line_index >= bottom_third_start)
        .filter_map(|l| l.price_cents.map(|p| (p, l.line_index)))
        .filter(|&(p, _)| is_plausible_total(p))
        .max_by_key(|&(p, _)| p)
        .map(|(price_cents, line_index)| TotalCandidate {
            price_cents,
            line_index,
        })
}

// ---------------------------------------------------------------------
// Confidence bucketing (spec §1.7)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfidenceBucket {
    Low,
    High,
}

/// Buckets a raw 0-100 confidence score per spec §1.7: `"low"` below ~70,
/// otherwise `"high"`. Never fails validation on its own — purely a UI hint
/// for the host-confirmation step.
pub const LOW_CONFIDENCE_THRESHOLD: f32 = 70.0;

pub fn confidence_bucket(score: f32) -> ConfidenceBucket {
    if score < LOW_CONFIDENCE_THRESHOLD {
        ConfidenceBucket::Low
    } else {
        ConfidenceBucket::High
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, conf: f32, block: i32, par: i32, line: i32, word_num: i32, left: i32) -> OcrWord {
        OcrWord {
            text: text.to_string(),
            confidence: conf,
            left,
            top: 0,
            width: 10,
            height: 10,
            block_num: block,
            par_num: par,
            line_num: line,
            word_num,
        }
    }

    fn raw_line(text: &str, line_index: usize) -> RawLine {
        RawLine {
            text: text.to_string(),
            line_index,
            mean_confidence: 90.0,
            min_confidence: 85.0,
        }
    }

    // -- Line reconstruction --------------------------------------------

    #[test]
    fn reconstruct_lines_groups_by_tesseract_line_and_orders_words() {
        let words = vec![
            word("Burger", 90.0, 1, 1, 1, 2, 60), // out of word_num order on purpose
            word("Cheese", 88.0, 1, 1, 1, 1, 10),
            word("12.00", 95.0, 1, 1, 1, 3, 110),
            word("Fries", 91.0, 1, 1, 2, 1, 10),
        ];
        let lines = reconstruct_lines(&words);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Cheese Burger 12.00");
        assert_eq!(lines[0].line_index, 0);
        assert_eq!(lines[1].text, "Fries");
        // mean confidence of first line = (90+88+95)/3
        assert!((lines[0].mean_confidence - (90.0 + 88.0 + 95.0) / 3.0).abs() < 1e-4);
    }

    // -- Price extraction --------------------------------------------

    #[test]
    fn extracts_simple_price() {
        let (cents, name) = extract_trailing_price("Margherita Pizza 14.00").unwrap();
        assert_eq!(cents, 1400);
        assert_eq!(name, "Margherita Pizza");
    }

    #[test]
    fn extracts_price_with_currency_symbol() {
        let (cents, name) = extract_trailing_price("Caesar Salad $9.50").unwrap();
        assert_eq!(cents, 950);
        assert_eq!(name, "Caesar Salad");
    }

    #[test]
    fn extracts_price_with_thousands_separator() {
        let (cents, _) = extract_trailing_price("Catering package 1,234.56").unwrap();
        assert_eq!(cents, 123456);
    }

    #[test]
    fn extracts_price_with_comma_decimal_separator() {
        // European-style: comma as decimal separator, no thousands sep.
        let (cents, _) = extract_trailing_price("Kaffee 3,50").unwrap();
        assert_eq!(cents, 350);
    }

    #[test]
    fn extracts_negative_price() {
        let (cents, _) = extract_trailing_price("Loyalty discount -5.00").unwrap();
        assert_eq!(cents, -500);
    }

    #[test]
    fn rightmost_price_wins_when_two_are_present() {
        // Unit price 12.00, line total 24.00 -> only the rightmost (line
        // total) should be captured; the "2x" quantity is handled
        // separately as display metadata.
        let (cents, before) = extract_trailing_price("2x Burger 12.00 24.00").unwrap();
        assert_eq!(cents, 2400);
        assert_eq!(before, "2x Burger 12.00");
    }

    #[test]
    fn no_price_token_returns_none() {
        assert!(extract_trailing_price("Thank you for dining with us").is_none());
        assert!(extract_trailing_price("Table: 5").is_none());
    }

    // -- Locale-aware price formats (real Kazakhstani receipts) ---------

    #[test]
    fn extracts_price_with_space_thousands_and_comma_decimal() {
        // Russian/Kazakh printed convention: space-grouped thousands,
        // comma decimal -- e.g. a receipt total "Барлығы/Итого: 12 037,00".
        let (cents, _) = extract_trailing_price("Барлығы/Итого: 12 037,00").unwrap();
        assert_eq!(cents, 1203700);
    }

    #[test]
    fn extracts_price_with_space_thousands_larger_amount() {
        let (cents, _) = extract_trailing_price("БАРЛЫҒЫ/ИТОГО: 17 469,88").unwrap();
        assert_eq!(cents, 1746988);
    }

    #[test]
    fn extracts_price_plain_dot_decimal_no_thousands_separator() {
        // Observed on a real receipt: no thousands separator at all, dot
        // decimal, e.g. "=10440.00".
        let (cents, _) = extract_trailing_price("=10440.00").unwrap();
        assert_eq!(cents, 1044000);
    }

    #[test]
    fn extracts_price_with_four_digit_space_group() {
        let (cents, _) = extract_trailing_price("1. Брестское угощение 4 284,75").unwrap();
        assert_eq!(cents, 428475);
    }

    #[test]
    fn tolerates_a_couple_of_trailing_ocr_garbage_characters() {
        // Real OCR output occasionally tacks 1-2 stray misrecognized
        // characters onto the very end of an otherwise-correct price.
        let (cents, _) = extract_trailing_price("Барлығы/Того: 17 469,88 a,").unwrap();
        assert_eq!(cents, 1746988);
        let (cents, _) = extract_trailing_price("Куны/Стоимость 4200,00 1").unwrap();
        assert_eq!(cents, 420000);
    }

    #[test]
    fn parenthesized_amount_is_recognized_by_classification() {
        let line = raw_line("Member discount (5.00)", 0);
        let parsed = classify_line(&line);
        assert_eq!(parsed.category, LineCategory::Discount);
        assert_eq!(parsed.price_cents, Some(-500));
    }

    // -- Quantity prefix --------------------------------------------

    #[test]
    fn strips_quantity_prefix() {
        let (qty, name) = strip_quantity_prefix("2x Burger");
        assert_eq!(qty, Some(2));
        assert_eq!(name, "Burger");
    }

    #[test]
    fn strips_quantity_prefix_with_multiplication_sign_and_spaces() {
        let (qty, name) = strip_quantity_prefix("3 × Sparkling Water");
        assert_eq!(qty, Some(3));
        assert_eq!(name, "Sparkling Water");
    }

    #[test]
    fn no_quantity_prefix_leaves_name_untouched() {
        let (qty, name) = strip_quantity_prefix("Tiramisu");
        assert_eq!(qty, None);
        assert_eq!(name, "Tiramisu");
    }

    #[test]
    fn quantity_is_display_metadata_price_not_multiplied() {
        // "2x Burger 24.00" -> price stays 24.00 (the line's printed
        // total), quantity is only ever surfaced as a hint.
        let line = raw_line("2x Burger 24.00", 0);
        let parsed = classify_line(&line);
        assert_eq!(parsed.category, LineCategory::Item);
        assert_eq!(parsed.price_cents, Some(2400));
        assert_eq!(parsed.quantity_hint, Some(2));
        assert_eq!(parsed.name.as_deref(), Some("Burger"));
    }

    // -- Line classification --------------------------------------------

    #[test]
    fn classifies_item_line() {
        let parsed = classify_line(&raw_line("Margherita Pizza 14.00", 0));
        assert_eq!(parsed.category, LineCategory::Item);
        assert_eq!(parsed.name.as_deref(), Some("Margherita Pizza"));
        assert_eq!(parsed.price_cents, Some(1400));
    }

    #[test]
    fn classifies_subtotal_line() {
        let parsed = classify_line(&raw_line("Subtotal 38.00", 0));
        assert_eq!(parsed.category, LineCategory::Subtotal);
    }

    #[test]
    fn classifies_sub_total_variants() {
        assert_eq!(classify_line(&raw_line("Sub Total 38.00", 0)).category, LineCategory::Subtotal);
        assert_eq!(classify_line(&raw_line("Sub-Total 38.00", 0)).category, LineCategory::Subtotal);
    }

    #[test]
    fn classifies_tax_line() {
        let parsed = classify_line(&raw_line("Sales Tax 3.20", 0));
        assert_eq!(parsed.category, LineCategory::Tax);
    }

    #[test]
    fn classifies_tip_line() {
        assert_eq!(classify_line(&raw_line("Gratuity 6.50", 0)).category, LineCategory::TipService);
        assert_eq!(classify_line(&raw_line("Service Charge 4.00", 0)).category, LineCategory::TipService);
    }

    #[test]
    fn classifies_total_line_but_not_subtotal() {
        let total = classify_line(&raw_line("Total 47.85", 0));
        assert_eq!(total.category, LineCategory::Total);
        let subtotal = classify_line(&raw_line("Subtotal 38.00", 0));
        assert_ne!(subtotal.category, LineCategory::Total);
    }

    #[test]
    fn classifies_grand_total_amount_due_balance_due() {
        assert_eq!(classify_line(&raw_line("Grand Total 47.85", 0)).category, LineCategory::Total);
        assert_eq!(classify_line(&raw_line("Amount Due 47.85", 0)).category, LineCategory::Total);
        assert_eq!(classify_line(&raw_line("Balance Due 47.85", 0)).category, LineCategory::Total);
    }

    #[test]
    fn classifies_discount_line_by_keyword() {
        assert_eq!(classify_line(&raw_line("10% off 2.00", 0)).category, LineCategory::Discount);
        assert_eq!(classify_line(&raw_line("Coupon -3.00", 0)).category, LineCategory::Discount);
    }

    #[test]
    fn classifies_noise_lines() {
        assert_eq!(classify_line(&raw_line("Thank you for visiting!", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Server: Jamie", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Table: 12", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Cash tendered 50.00", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Change 2.15", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("VISA **** 1234 47.85", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Call us at 555-123-4567", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("08/12/2026 47.85", 0)).category, LineCategory::Noise);
    }

    #[test]
    fn line_with_no_price_is_noise() {
        let parsed = classify_line(&raw_line("Bon appetit", 0));
        assert_eq!(parsed.category, LineCategory::Noise);
        assert_eq!(parsed.price_cents, None);
    }

    // -- Cyrillic (Russian/Kazakh) keyword classification ----------------
    // English keyword matching stays intact and is exercised in parallel
    // above -- this app supports both, not a language switch.

    #[test]
    fn classifies_cyrillic_total_lines() {
        assert_eq!(classify_line(&raw_line("Итого: 12 037,00", 0)).category, LineCategory::Total);
        assert_eq!(classify_line(&raw_line("Барлығы/Итого: 24 000,00", 0)).category, LineCategory::Total);
        assert_eq!(classify_line(&raw_line("ЖИЫНЫ/ИТОГ =10440.00", 0)).category, LineCategory::Total);
    }

    #[test]
    fn classifies_cyrillic_tax_lines() {
        assert_eq!(classify_line(&raw_line("ҚҚС/НДС 16% 135,17", 0)).category, LineCategory::Tax);
        assert_eq!(classify_line(&raw_line("Салық 16.00% 1440.00", 0)).category, LineCategory::Tax);
    }

    #[test]
    fn classifies_cyrillic_tip_service_lines() {
        assert_eq!(classify_line(&raw_line("Чаевые 500,00", 0)).category, LineCategory::TipService);
        assert_eq!(classify_line(&raw_line("Сервис 10% 300,00", 0)).category, LineCategory::TipService);
    }

    #[test]
    fn classifies_cyrillic_discount_lines() {
        assert_eq!(classify_line(&raw_line("Жеңілдік/Скидка: 0,00", 0)).category, LineCategory::Discount);
        assert_eq!(classify_line(&raw_line("Шегерім = 5550.00", 0)).category, LineCategory::Discount);
    }

    #[test]
    fn classifies_cyrillic_noise_boilerplate() {
        assert_eq!(classify_line(&raw_line("ЖСН/БИН 940505300351", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("Банк картасы/Банковская карта: 24 000,00", 0)).category, LineCategory::Noise);
        assert_eq!(classify_line(&raw_line("чека зайдите на сайт: consumer.oofd.kz", 0)).category, LineCategory::Noise);
    }

    #[test]
    fn classifies_value_label_lines_as_a_distinct_category_not_total() {
        // Correct spelling, and the OCR-mangled variants actually observed
        // on real receipts (о/й confusion, dropped leading letter) --
        // all must land on `ValueLabel`, and specifically *not* `Total`,
        // despite "value/cost" sounding total-adjacent.
        assert_eq!(classify_line(&raw_line("Куны/Стоимость 24 000,00", 0)).category, LineCategory::ValueLabel);
        assert_eq!(classify_line(&raw_line("Құны/Стоймость 980.00", 0)).category, LineCategory::ValueLabel);
        assert_eq!(classify_line(&raw_line("2 Тоимость 294,73", 0)).category, LineCategory::ValueLabel);
        // Bare label line, no price on it at all -- still `ValueLabel`,
        // not lost to `Noise` (this is what lets the merge step find it).
        let bare = classify_line(&raw_line("Куны/Стоимость", 0));
        assert_eq!(bare.category, LineCategory::ValueLabel);
        assert_eq!(bare.price_cents, None);
    }

    #[test]
    fn near_nameless_item_price_line_is_downgraded_to_noise() {
        // A price-bearing line whose "name" (after stripping) has fewer
        // than 2 alphabetic characters is a stray recap/duplicate price
        // artifact, not a real named item (spec: avoids double-counting
        // seen on a real receipt where a `qty x price` line and a
        // separate `=price` recap line both independently looked
        // item-shaped).
        let parsed = classify_line(&raw_line("Я! =10440.00", 0));
        assert_eq!(parsed.category, LineCategory::Noise);
        // But a line with a real (if short/garbled) name still counts.
        let real_item = classify_line(&raw_line("Ab 12.00", 0));
        assert_eq!(real_item.category, LineCategory::Item);
    }

    // -- Multi-line item/value-label merging (spec extension) -----------

    #[test]
    fn merges_bare_name_line_with_a_later_split_value_label_and_price() {
        // The exact real-world shape from a butcher-shop receipt: name,
        // then an (unrelated-looking) qty-x-unit-price line, then a bare
        // value-label line, then the actual price on its own line.
        // (line words are 1-indexed to match Tesseract's own numbering.)
        let ocr_words = vec![
            word("1.Брестское", 90.0, 0, 0, 0, 1, 0),
            word("угощение", 90.0, 0, 0, 0, 2, 60),
            word("Брест", 90.0, 0, 0, 0, 3, 140),
            word("0,435", 90.0, 0, 0, 1, 1, 0),
            word("кг/кг", 90.0, 0, 0, 1, 2, 60),
            word("х", 90.0, 0, 0, 1, 3, 120),
            word("9", 90.0, 0, 0, 1, 4, 140),
            word("850,00", 90.0, 0, 0, 1, 5, 160),
            word("Куны/Стоимость", 90.0, 0, 0, 2, 1, 0),
            word("4", 90.0, 0, 0, 3, 1, 0),
            word("284,75", 90.0, 0, 0, 3, 2, 20),
        ];
        let lines = parse_lines(&ocr_words);
        let items: Vec<_> = lines.iter().filter(|l| l.category == LineCategory::Item).collect();
        assert_eq!(items.len(), 1, "expected exactly one merged item, got: {lines:#?}");
        assert_eq!(items[0].price_cents, Some(428475));
        assert!(items[0].name.as_deref().unwrap().contains("Брестское"));
        // The intervening qty/label lines must not survive as their own
        // spurious items.
        assert!(lines.iter().all(|l| l.category != LineCategory::ValueLabel));
    }

    #[test]
    fn merges_name_with_same_line_label_and_price() {
        // Simpler real-world shape: label + price share one reconstructed
        // OCR line, right after the bare name line.
        let ocr_words = vec![
            word("1.", 90.0, 0, 0, 0, 1, 0),
            word("Сет", 90.0, 0, 0, 0, 2, 30),
            word("Айтпаевы", 90.0, 0, 0, 0, 3, 70),
            word("Куны/Стоимость", 90.0, 0, 0, 1, 1, 0),
            word("24", 90.0, 0, 0, 1, 2, 100),
            word("000,00", 90.0, 0, 0, 1, 3, 130),
        ];
        let lines = parse_lines(&ocr_words);
        let items: Vec<_> = lines.iter().filter(|l| l.category == LineCategory::Item).collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].price_cents, Some(2400000));
    }

    #[test]
    fn does_not_merge_across_an_unrelated_noise_line_beyond_the_backward_window() {
        // A name-shaped noise line that is *not* actually adjacent to the
        // upcoming value label (further back than the backward window)
        // must not get merged with it -- guards against the false-merge
        // regression this heuristic originally had (an early boilerplate
        // line stealing a later item's value label meant for the name
        // line actually sitting right next to it).
        let mut lines = vec![
            raw_line("Чек/Чек Кей и", 0), // name-shaped boilerplate, too far away
        ];
        for i in 1..=(VALUE_LABEL_BACKWARD_WINDOW + 2) {
            lines.push(raw_line(&format!("filler line {i}"), i));
        }
        let label_idx = VALUE_LABEL_BACKWARD_WINDOW + 3;
        lines.push(raw_line("Куны/Стоимость 500,00", label_idx));
        let classified: Vec<ParsedLine> = lines.iter().map(classify_line).collect();
        let merged = merge_item_value_lines(classified);

        // The far-away boilerplate line must survive untouched -- it must
        // NOT be the one that absorbed the value label's price.
        let boilerplate = merged
            .iter()
            .find(|l| l.raw_text == "Чек/Чек Кей и")
            .expect("the too-far-away boilerplate line must survive as its own line");
        assert_eq!(boilerplate.category, LineCategory::Noise);
        assert_eq!(boilerplate.price_cents, None);

        // The label's price is still claimed by *some* in-window
        // candidate (merging isn't broken outright) -- just not that one.
        assert!(
            merged
                .iter()
                .any(|l| l.category == LineCategory::Item && l.price_cents == Some(50000)),
            "expected the value label to still merge with an in-window candidate: {merged:#?}"
        );
    }

    // -- Total identification --------------------------------------------

    fn parsed(category: LineCategory, raw_text: &str, price_cents: Option<i64>, idx: usize) -> ParsedLine {
        ParsedLine {
            category,
            raw_text: raw_text.to_string(),
            line_index: idx,
            name: None,
            quantity_hint: None,
            price_cents,
            mean_confidence: 90.0,
            min_confidence: 85.0,
        }
    }

    #[test]
    fn identifies_grand_total_with_highest_priority() {
        let lines = vec![
            parsed(LineCategory::Item, "Burger 12.00", Some(1200), 0),
            parsed(LineCategory::Subtotal, "Subtotal 12.00", Some(1200), 1),
            parsed(LineCategory::Tax, "Tax 1.00", Some(100), 2),
            parsed(LineCategory::Total, "Total 13.00", Some(1300), 3),
            parsed(LineCategory::Total, "Grand Total 13.00", Some(1300), 4),
        ];
        let total = identify_total(&lines).unwrap();
        assert_eq!(total.price_cents, 1300);
        assert_eq!(total.line_index, 4);
    }

    #[test]
    fn falls_back_to_plain_total_when_no_grand_total_present() {
        let lines = vec![
            parsed(LineCategory::Item, "Burger 12.00", Some(1200), 0),
            parsed(LineCategory::Subtotal, "Subtotal 12.00", Some(1200), 1),
            parsed(LineCategory::Total, "Total 13.00", Some(1300), 2),
        ];
        let total = identify_total(&lines).unwrap();
        assert_eq!(total.price_cents, 1300);
        assert_eq!(total.line_index, 2);
    }

    #[test]
    fn scans_bottom_up_taking_the_last_matching_total_line() {
        // Two "total"-ish lines (misparse duplicate); bottom-up scan should
        // pick the later (lower on the receipt) one.
        let lines = vec![
            parsed(LineCategory::Total, "Total 13.00", Some(1300), 0),
            parsed(LineCategory::Total, "Total 13.05", Some(1305), 1),
        ];
        let total = identify_total(&lines).unwrap();
        assert_eq!(total.price_cents, 1305);
        assert_eq!(total.line_index, 1);
    }

    #[test]
    fn falls_back_to_largest_price_in_bottom_third_when_no_total_keyword() {
        let lines = vec![
            parsed(LineCategory::Item, "Burger 12.00", Some(1200), 0),
            parsed(LineCategory::Item, "Fries 5.00", Some(500), 1),
            parsed(LineCategory::Item, "Salad 9.00", Some(900), 2),
            // No "total"/"subtotal" keyword anywhere -- bottom-third
            // fallback should pick the largest price near the bottom.
            parsed(LineCategory::Noise, "26.00", Some(2600), 3),
        ];
        let total = identify_total(&lines).unwrap();
        assert_eq!(total.price_cents, 2600);
    }

    #[test]
    fn rejects_total_candidate_smaller_than_largest_item_and_falls_through() {
        let lines = vec![
            parsed(LineCategory::Item, "Steak 42.00", Some(4200), 0),
            // Misparsed "total" line with an implausibly small price --
            // smaller than the largest item -- must be rejected.
            parsed(LineCategory::Total, "Total 4.00", Some(400), 1),
            parsed(LineCategory::Noise, "46.00", Some(4600), 2),
        ];
        let total = identify_total(&lines).unwrap();
        assert_eq!(total.price_cents, 4600, "should fall through to the plausible bottom-third candidate");
    }

    #[test]
    fn returns_none_when_nothing_qualifies() {
        let lines = vec![parsed(LineCategory::Noise, "Thank you", None, 0)];
        assert!(identify_total(&lines).is_none());
        assert!(identify_total(&[]).is_none());
    }

    // -- Confidence bucketing --------------------------------------------

    #[test]
    fn confidence_bucketing_threshold() {
        assert_eq!(confidence_bucket(69.9), ConfidenceBucket::Low);
        assert_eq!(confidence_bucket(70.0), ConfidenceBucket::High);
        assert_eq!(confidence_bucket(95.0), ConfidenceBucket::High);
    }
}
