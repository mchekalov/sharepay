//! Askama templates + small view-model builders for the Mobile Web Client
//! (spec §4). Route handlers (`src/routes/bill.rs`, `src/routes/receipt.rs`)
//! build these structs from the domain types in `pricing::api` and render
//! them to `Html<String>` responses.
//!
//! Money is always formatted here (never in a template expression) via
//! [`fmt_cents`], keeping the templates free of arithmetic.

use askama::Template;
use axum::response::Html;

use crate::config::Currency;
use crate::pricing::api::BillState;

/// Formats integer cents as a money string in `currency`, e.g. `1234` ->
/// `"$12.34"` (USD) or `"12.34 ₸"` (KZT). Never negative in practice (all
/// amounts in this app are non-negative), but handles the sign defensively
/// rather than panicking.
pub fn fmt_cents(cents: i64, currency: Currency) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    match currency {
        Currency::Usd => format!("{sign}${}.{:02}", abs / 100, abs % 100),
        Currency::Kzt => format!("{sign}{}.{:02} ₸", abs / 100, abs % 100),
    }
}

/// Formats integer cents as a bare decimal string suitable for a
/// `<input type="number">` `value` attribute, e.g. `1234` -> `"12.34"`.
pub fn cents_to_input_value(cents: i64) -> String {
    let abs = cents.max(0);
    format!("{}.{:02}", abs / 100, abs % 100)
}

/// Renders any Askama template to an `Html` response body. Template
/// rendering can only fail on a formatting bug (never on user input, since
/// all interpolated values are already-validated Rust data) — falling back
/// to a minimal error body rather than panicking keeps a broken template
/// from taking down the whole request.
pub fn render<T: Template>(tpl: T) -> Html<String> {
    match tpl.render() {
        Ok(body) => Html(body),
        Err(err) => Html(format!("<p>template error: {err}</p>")),
    }
}

// ---------------------------------------------------------------------
// Landing / host upload / host review / host QR
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "landing.html")]
pub struct LandingTemplate;

#[derive(Template)]
#[template(path = "host_upload.html")]
pub struct HostUploadTemplate {
    pub bill_id: String,
    /// `Some(message)` when this is a retry after a mismatch/OCR failure
    /// (spec §4.3.3); `None` for the first upload attempt.
    pub retry_message: Option<String>,
}

#[derive(Template)]
#[template(path = "totals_footer.html")]
pub struct TotalsFooterTemplate {
    pub items_subtotal_display: String,
    pub tax_tip_display: String,
    pub receipt_total_display: String,
    pub reconciled: bool,
    /// `true` when this is returned as an out-of-band swap fragment
    /// (add/delete item responses); `false` for the initial in-page render.
    pub oob: bool,
}

#[derive(Template)]
#[template(path = "review_item_row.html")]
pub struct ReviewItemRowTemplate {
    pub bill_id: String,
    pub id: i64,
    pub name: String,
    pub price_input_value: String,
}

#[derive(Template)]
#[template(path = "host_review.html")]
pub struct HostReviewTemplate {
    pub bill_id: String,
    /// Pre-rendered `<tr>` markup, one per item (built via
    /// [`ReviewItemRowTemplate`]) — keeps a single source of truth for row
    /// markup shared with the "add item" endpoint's response.
    pub item_rows: Vec<String>,
    /// Pre-rendered totals footer (via [`TotalsFooterTemplate`], `oob: false`).
    pub totals_footer: String,
}

/// Builds the review screen's totals footer from the bill's current items
/// + receipt_total/tax_tip_amount (spec §4.3.4's reconciliation display).
pub fn totals_footer(state: &BillState, oob: bool, currency: Currency) -> String {
    let items_subtotal: i64 = state.items.iter().map(|i| i.price_cents).sum();
    let receipt_total = state.receipt_total.unwrap_or(items_subtotal + state.tax_tip_amount);
    let reconciled = items_subtotal + state.tax_tip_amount == receipt_total;
    render(TotalsFooterTemplate {
        items_subtotal_display: fmt_cents(items_subtotal, currency),
        tax_tip_display: fmt_cents(state.tax_tip_amount, currency),
        receipt_total_display: fmt_cents(receipt_total, currency),
        reconciled,
        oob,
    })
    .0
}

/// Builds one review-row's rendered `<tr>` markup.
pub fn review_row(bill_id: &str, id: i64, name: &str, price_cents: i64) -> String {
    render(ReviewItemRowTemplate {
        bill_id: bill_id.to_string(),
        id,
        name: name.to_string(),
        price_input_value: cents_to_input_value(price_cents),
    })
    .0
}

/// Builds the full host review/edit page from the bill's current state.
pub fn host_review_page(bill_id: &str, state: &BillState, currency: Currency) -> HostReviewTemplate {
    let item_rows = state
        .items
        .iter()
        .map(|i| review_row(bill_id, i.id, &i.name, i.price_cents))
        .collect();
    HostReviewTemplate {
        bill_id: bill_id.to_string(),
        item_rows,
        totals_footer: totals_footer(state, false, currency),
    }
}

#[derive(Template)]
#[template(path = "joined_fragment.html")]
pub struct JoinedFragmentTemplate {
    pub count: usize,
    pub names: Vec<String>,
}

#[derive(Template)]
#[template(path = "host_qr.html")]
pub struct HostQrTemplate {
    pub bill_id: String,
    pub join_url: String,
    pub qr_svg: String,
    pub joined_html: String,
}

// ---------------------------------------------------------------------
// Participant flow
// ---------------------------------------------------------------------

#[derive(Template)]
#[template(path = "name_entry.html")]
pub struct NameEntryTemplate {
    pub bill_id: String,
    pub error: Option<String>,
    pub name_value: String,
}

#[derive(Clone)]
pub struct FragmentItemRow {
    pub id: i64,
    pub name: String,
    pub price_display: String,
    pub marked: bool,
    pub markers_display: String,
}

#[derive(Template)]
#[template(path = "bill_fragment.html")]
pub struct BillFragmentTemplate {
    pub bill_id: String,
    pub is_closed: bool,
    pub items: Vec<FragmentItemRow>,
    pub my_total_display: String,
    pub unassigned_display: Option<String>,
    pub copy_text: String,
}

#[derive(Template)]
#[template(path = "participant_view.html")]
pub struct ParticipantViewTemplate {
    pub heading: String,
    pub fragment_html: String,
}

#[derive(Template)]
#[template(path = "not_ready.html")]
pub struct NotReadyTemplate;

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub title: String,
    pub message: String,
}

/// Builds the `#bill-fragment` partial (spec §4.4.2/§4.4.3) — the
/// authoritative item-list + your-total, shared by the initial full-page
/// render, the polling endpoint, and the mark/unmark responses.
/// `requesting_participant_id` scopes "You" labeling and `is_marked_by_me`;
/// pass `None` for the host's closed-bill view.
pub fn bill_fragment(
    bill_id: &str,
    state: &BillState,
    me: Option<&str>,
    currency: Currency,
) -> BillFragmentTemplate {
    let items: Vec<FragmentItemRow> = state
        .items
        .iter()
        .map(|item| {
            let names: Vec<String> = item
                .markers
                .iter()
                .map(|m| {
                    if Some(m.participant_id.as_str()) == me {
                        "You".to_string()
                    } else {
                        m.display_name.clone()
                    }
                })
                .collect();
            FragmentItemRow {
                id: item.id,
                name: item.name.clone(),
                price_display: fmt_cents(item.price_cents, currency),
                marked: item.is_marked_by_me,
                markers_display: names.join(", "),
            }
        })
        .collect();

    let unassigned_display = if state.unassigned_amount > 0 {
        Some(fmt_cents(state.unassigned_amount, currency))
    } else {
        None
    };

    let my_total_display = fmt_cents(state.my_total, currency);
    let copy_text = format!("You owe {my_total_display} for your SharePay bill");

    BillFragmentTemplate {
        bill_id: bill_id.to_string(),
        is_closed: state.status == "closed",
        items,
        my_total_display,
        unassigned_display,
        copy_text,
    }
}

/// Wraps [`bill_fragment`] in the full page shell, for `GET /b/{id}`'s
/// initial render (as opposed to a bare polling/toggle fragment response).
pub fn participant_page(
    bill_id: &str,
    state: &BillState,
    me: Option<&str>,
    currency: Currency,
) -> ParticipantViewTemplate {
    let heading = if state.status == "closed" {
        "Bill closed — final totals".to_string()
    } else {
        "Your bill".to_string()
    };
    let fragment_html = render(bill_fragment(bill_id, state, me, currency)).0;
    ParticipantViewTemplate {
        heading,
        fragment_html,
    }
}
