use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use chrono::{DateTime, Months, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use stripe::Client;
use stripe_billing::subscription::{ListSubscription, ListSubscriptionStatus};
use stripe_billing::{Subscription, SubscriptionStatus};
use stripe_core::Customer;
use stripe_shared::Address;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;
use tracing_subscriber::EnvFilter;
use typst::foundations::{Dict, IntoValue, Value};
use typst_as_lib::typst_kit_options::TypstKitFontOptions;
use typst_as_lib::{TypstEngine, TypstTemplateMainFile};
use typst_layout::PagedDocument;

secretspec_derive::declare_secrets!("secretspec.toml");

#[derive(Clone)]
struct AppState {
    stripe: Arc<Client>,
    typst_engine: Arc<TypstEngine<TypstTemplateMainFile>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let secrets = SecretSpec::builder()
        .with_reason("stripe-ticket-printer boot")
        .load()
        .expect("failed to load secrets from secretspec");

    let state = AppState {
        stripe: Arc::new(Client::new(secrets.secrets.stripe_secret_key)),
        typst_engine: Arc::new(build_typst_engine()),
    };

    // `/healthz` is polled constantly by orchestrators and never carries
    // useful information, so it's kept off the access-log router entirely.
    let logged_routes = Router::new()
        .route("/", get(root))
        .route("/report", get(report))
        .route("/report/csv", get(report_csv))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(DefaultOnRequest::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        );

    let app = Router::new()
        .route("/healthz", get(healthz))
        .merge(logged_routes)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind to 0.0.0.0:3000");
    tracing::info!(addr = %listener.local_addr().unwrap(), "listening");
    axum::serve(listener, app).await.expect("server error");
}

/// Health check for showing the app is running.
async fn healthz(State(_state): State<AppState>) -> Result<String, (StatusCode, String)> {
    Ok("OK".to_string())
}

/// Serves the index page: a Pico CSS form for choosing a report start date
/// (defaulting to exactly two months ago), a live PDF preview of the
/// shipping-label report for that date, and links to download the PDF or a
/// CSV export from `/report` and `/report/csv`.
async fn root() -> impl IntoResponse {
    let default_date = Utc::now()
        .date_naive()
        .checked_sub_months(Months::new(2))
        .expect("subtracting two months from today is always a valid date")
        .format("%Y-%m-%d")
        .to_string();
    let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();

    Html(
        INDEX_HTML_TEMPLATE
            .replace("%%DEFAULT_DATE%%", &default_date)
            .replace("%%TODAY%%", &today),
    )
}

/// The index page template, loaded from disk at compile time.
/// `%%DEFAULT_DATE%%` (two months before today) and `%%TODAY%%` are filled
/// in by [`root`].
const INDEX_HTML_TEMPLATE: &str = include_str!("./templates/index.html");

/// Query parameters for `/report`. `start_date` (`YYYY-MM-DD`) marks the
/// beginning of the reporting period; the period runs from that date until
/// now.
#[derive(Debug, Deserialize)]
struct ReportQuery {
    start_date: String,
}

/// Generates an A4 sheet of shipping labels (Avery L7163-compatible:
/// 99.09 x 38.1 mm, 2 columns x 7 rows) for Stripe customers who had a
/// subscription active at any point between `start_date` and now (i.e.
/// those eligible to be posted something), rendered from a Typst template.
#[tracing::instrument(skip(state), fields(start_date = %query.start_date))]
async fn report(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let start_date = parse_start_date(&query.start_date)?;
    let now = Utc::now().timestamp();
    tracing::trace!(start_date, now, "resolved report window");

    let subscriptions = subscriptions_active_between(state.stripe.as_ref(), start_date, now)
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Stripe API call failed: {err}"),
            )
        })?;
    let customers = distinct_customers(&subscriptions);
    tracing::debug!(
        customer_count = customers.len(),
        "fetched customers with active subscriptions"
    );

    let pages = render_customer_report_pages(&customers);
    tracing::trace!(page_count = pages.len(), "chunked label sheet pages");

    let engine = Arc::clone(&state.typst_engine);
    let pdf = tokio::task::spawn_blocking(move || render_label_sheet_pdf(&engine, pages))
        .await
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("PDF render task panicked: {err}"),
            )
        })?
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("PDF generation failed: {err}"),
            )
        })?;
    tracing::debug!(pdf_bytes = pdf.len(), "rendered pdf");

    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf"),
            (
                header::CONTENT_DISPOSITION,
                "inline; filename=\"shipping-labels.pdf\"",
            ),
        ],
        pdf,
    ))
}

/// Exports the same customers as [`report`], one row per subscription that
/// overlapped `[start_date, now]`, as CSV with columns
/// `name,address,sub_start_date,sub_amount`. `sub_amount` is the
/// subscription's per-cycle total (sum of `unit_amount * quantity` across
/// its items) in major currency units (e.g. dollars, not cents).
#[tracing::instrument(skip(state), fields(start_date = %query.start_date))]
async fn report_csv(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let start_date = parse_start_date(&query.start_date)?;
    let now = Utc::now().timestamp();
    tracing::trace!(start_date, now, "resolved report window");

    let subscriptions = subscriptions_active_between(state.stripe.as_ref(), start_date, now)
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Stripe API call failed: {err}"),
            )
        })?;
    tracing::debug!(
        subscription_count = subscriptions.len(),
        "fetched subscriptions active in period"
    );

    let csv = render_subscriptions_csv(&subscriptions);

    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"subscriptions.csv\"",
            ),
        ],
        csv,
    ))
}

/// Parses a `YYYY-MM-DD` `start_date` query value into a Unix timestamp at
/// midnight UTC on that date.
fn parse_start_date(raw: &str) -> Result<i64, (StatusCode, String)> {
    Ok(NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!("invalid start_date {raw:?}: expected YYYY-MM-DD"),
            )
        })?
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always a valid time")
        .and_utc()
        .timestamp())
}

/// Fetches every subscription (across all pages, all statuses) that
/// overlapped `[start_date, now]`.
#[tracing::instrument(skip(client))]
async fn subscriptions_active_between(
    client: &Client,
    start_date: i64,
    now: i64,
) -> Result<Vec<Subscription>, stripe::StripeError> {
    let mut subscriptions = Vec::new();
    let mut starting_after: Option<String> = None;
    let mut page_number = 0u32;

    loop {
        let mut request = ListSubscription::new()
            .status(ListSubscriptionStatus::All)
            .expand(vec!["data.customer".to_string()])
            .limit(100);
        if let Some(cursor) = starting_after.take() {
            request = request.starting_after(cursor);
        }

        let page = request.send(client).await?;
        let next_cursor = page
            .data
            .last()
            .map(|subscription| subscription.id.to_string());
        page_number += 1;
        tracing::trace!(
            page_number,
            page_size = page.data.len(),
            has_more = page.has_more,
            next_cursor = ?next_cursor,
            "fetched subscription page"
        );

        subscriptions.extend(
            page.data
                .into_iter()
                .filter(|subscription| subscription_active_between(subscription, start_date, now)),
        );

        if !page.has_more {
            break;
        }
        let Some(next_cursor) = next_cursor else {
            break;
        };
        starting_after = Some(next_cursor);
    }

    tracing::trace!(
        subscription_count = subscriptions.len(),
        pages_fetched = page_number,
        "collected subscriptions overlapping reporting period"
    );
    Ok(subscriptions)
}

/// Extracts the distinct customers referenced by `subscriptions` (in
/// first-seen order), skipping any subscription whose customer failed to
/// expand.
fn distinct_customers(subscriptions: &[Subscription]) -> Vec<Customer> {
    let mut seen_customer_ids = HashSet::new();
    let mut customers = Vec::new();
    for subscription in subscriptions {
        let Some(customer) = subscription.customer.as_object() else {
            continue;
        };
        if seen_customer_ids.insert(customer.id.clone()) {
            customers.push(customer.clone());
        }
    }
    customers
}

/// A subscription counts as active at some point during `[period_start,
/// now]` if it actually started (excluding `incomplete`/`incomplete_expired`,
/// which never had a paying period) and either hasn't ended yet or ended on
/// or after `period_start`.
fn subscription_active_between(subscription: &Subscription, period_start: i64, now: i64) -> bool {
    subscription_overlaps_period(
        &subscription.status,
        subscription.start_date,
        subscription.ended_at,
        period_start,
        now,
    )
}

/// Pure overlap check between a subscription's `[sub_start, sub_ended_at]`
/// window (open-ended if `sub_ended_at` is `None`) and the reporting period
/// `[period_start, now]`. Split out from [`subscription_active_between`] so
/// it can be unit tested without constructing a full `Subscription`.
fn subscription_overlaps_period(
    status: &SubscriptionStatus,
    sub_start: i64,
    sub_ended_at: Option<i64>,
    period_start: i64,
    now: i64,
) -> bool {
    if matches!(
        status,
        SubscriptionStatus::Incomplete | SubscriptionStatus::IncompleteExpired
    ) {
        return false;
    }

    sub_start <= now && sub_ended_at.is_none_or(|ended_at| ended_at >= period_start)
}

/// Number of labels per A4 sheet for the Avery L7163-compatible layout
/// (2 columns x 7 rows of 99.09 x 38.1 mm labels).
const LABELS_PER_PAGE: usize = 14;

/// The label sheet Typst template, loaded from disk at compile time. It
/// defines the Avery L7163-compatible page geometry and a `shipping-label`
/// function, and lays out `sys.inputs.pages` (see [`ReportInput`]).
const LABEL_SHEET_TEMPLATE: &str = include_str!("./templates/shipping_labels.typ");

/// Builds a [`TypstEngine`] for rendering the label sheet template, with
/// Typst's default fonts (New Computer Modern) embedded at compile time via
/// `typst-assets` so rendering needs no network or system font access at
/// runtime.
fn build_typst_engine() -> TypstEngine<TypstTemplateMainFile> {
    TypstEngine::builder()
        .main_file(LABEL_SHEET_TEMPLATE)
        .search_fonts_with(
            TypstKitFontOptions::default()
                .include_system_fonts(false)
                .include_embedded_fonts(true),
        )
        .build()
}

/// Compiles the label sheet template with `pages` injected as `sys.inputs`
/// and exports the result to PDF bytes.
fn render_label_sheet_pdf(
    engine: &TypstEngine<TypstTemplateMainFile>,
    pages: Vec<Vec<LabelCell>>,
) -> Result<Vec<u8>, String> {
    let doc: PagedDocument = engine
        .compile_with_input(ReportInput { pages })
        .output
        .map_err(|err| format!("typst compilation failed: {err}"))?;
    typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default())
        .map_err(|err| format!("typst PDF export failed: {err:?}"))
}

/// One label sheet cell: either a standalone placeholder message (e.g. "no
/// customers found") or a customer's name and address, injected into the
/// Typst template as a dict (see `shipping-label` in
/// `templates/shipping_labels.typ`).
#[derive(Debug, Clone)]
enum LabelCell {
    Placeholder(String),
    Customer {
        name: String,
        lines: Vec<String>,
        no_address: bool,
    },
}

impl IntoValue for LabelCell {
    fn into_value(self) -> Value {
        let mut dict = Dict::new();
        match self {
            LabelCell::Placeholder(text) => {
                dict.insert("kind".into(), "placeholder".into_value());
                dict.insert("text".into(), text.into_value());
            }
            LabelCell::Customer {
                name,
                lines,
                no_address,
            } => {
                dict.insert("kind".into(), "customer".into_value());
                dict.insert("name".into(), name.into_value());
                dict.insert("lines".into(), lines.into_value());
                dict.insert("no_address".into(), no_address.into_value());
            }
        }
        Value::Dict(dict)
    }
}

/// Input injected as `sys.inputs` for the label sheet template: pages of up
/// to [`LABELS_PER_PAGE`] label cells each.
struct ReportInput {
    pages: Vec<Vec<LabelCell>>,
}

impl From<ReportInput> for Dict {
    fn from(value: ReportInput) -> Self {
        let mut dict = Dict::new();
        dict.insert("pages".into(), value.pages.into_value());
        dict
    }
}

/// Chunks `customers` into shipping-label sheet pages (A4, Avery
/// L7163-compatible: 99.09 x 38.1 mm, 2 columns x 7 rows, 14 labels per
/// sheet), one [`LabelCell`] per customer. Produces a single page with a
/// placeholder message if there are no customers.
fn render_customer_report_pages(customers: &[Customer]) -> Vec<Vec<LabelCell>> {
    if customers.is_empty() {
        return vec![vec![LabelCell::Placeholder(
            "No customers found.".to_string(),
        )]];
    }

    customers
        .chunks(LABELS_PER_PAGE)
        .map(|page| page.iter().map(label_cell).collect())
        .collect()
}

/// Builds a single customer's label cell: name, followed by their address
/// (street lines, city/state/postal line, country) if present, or an italic
/// "No address on file" placeholder if not.
fn label_cell(customer: &Customer) -> LabelCell {
    let name = customer
        .name
        .as_deref()
        .or(customer.email.as_deref())
        .unwrap_or(customer.id.as_str())
        .to_string();

    match &customer.address {
        Some(address) => LabelCell::Customer {
            name,
            lines: address_lines(address),
            no_address: false,
        },
        None => LabelCell::Customer {
            name,
            lines: Vec::new(),
            no_address: true,
        },
    }
}

/// Formats a customer address as separate lines: street lines, then
/// city/state/postal code, then country, omitting any that are absent or
/// blank.
fn address_lines(address: &Address) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(line1) = non_empty(address.line1.as_deref()) {
        lines.push(line1.to_string());
    }
    if let Some(line2) = non_empty(address.line2.as_deref()) {
        lines.push(line2.to_string());
    }

    let city_state_zip = [
        address.city.as_deref(),
        address.state.as_deref(),
        address.postal_code.as_deref(),
    ]
    .into_iter()
    .filter_map(non_empty)
    .collect::<Vec<_>>()
    .join(" ");
    if !city_state_zip.is_empty() {
        lines.push(city_state_zip);
    }

    if let Some(country) = non_empty(address.country.as_deref()) {
        lines.push(country.to_string());
    }

    lines
}

/// Treats blank strings the same as absent values.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.trim().is_empty())
}

/// Renders `subscriptions` as CSV with columns
/// `name,address,sub_start_date,sub_amount`, one row per subscription (so a
/// customer with multiple overlapping subscriptions gets multiple rows).
/// Subscriptions whose customer failed to expand are skipped.
fn render_subscriptions_csv(subscriptions: &[Subscription]) -> String {
    let mut csv = String::from("name,address,sub_start_date,sub_amount\n");
    for subscription in subscriptions {
        let Some(customer) = subscription.customer.as_object() else {
            continue;
        };
        let name = customer
            .name
            .as_deref()
            .or(customer.email.as_deref())
            .unwrap_or(customer.id.as_str());
        let address = customer
            .address
            .as_ref()
            .map(format_address_single_line)
            .unwrap_or_default();

        let _ = writeln!(
            csv,
            "{},{},{},{}",
            csv_field(name),
            csv_field(&address),
            csv_field(&format_date(subscription.start_date)),
            csv_field(&subscription_amount(subscription)),
        );
    }
    csv
}

/// Renders a customer's address as a single comma-separated line (street
/// lines, city, state, postal code, country), for the CSV export.
fn format_address_single_line(address: &Address) -> String {
    [
        non_empty(address.line1.as_deref()),
        non_empty(address.line2.as_deref()),
        non_empty(address.city.as_deref()),
        non_empty(address.state.as_deref()),
        non_empty(address.postal_code.as_deref()),
        non_empty(address.country.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

/// A subscription's per-cycle total: the sum of `unit_amount * quantity`
/// across its line items, converted from the minor currency unit (e.g.
/// cents) to major units (e.g. dollars) and formatted with two decimal
/// places.
#[allow(
    clippy::cast_precision_loss,
    reason = "currency amounts are always far below 2^52 cents"
)]
fn subscription_amount(subscription: &Subscription) -> String {
    let minor_units: i64 = subscription
        .items
        .data
        .iter()
        .map(|item| {
            let quantity = i64::try_from(item.quantity.unwrap_or(1)).unwrap_or(i64::MAX);
            item.price.unit_amount.unwrap_or(0).saturating_mul(quantity)
        })
        .sum();
    format!("{:.2}", minor_units as f64 / 100.0)
}

/// Formats a Unix timestamp as a `YYYY-MM-DD` UTC date, for the CSV export.
fn format_date(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|datetime| datetime.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Escapes a value for embedding as a single CSV field: wraps it in quotes
/// (doubling any embedded quotes) if it contains a comma, quote, or
/// newline.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stripe_shared::Address;

    fn fake_customer(index: usize, name: Option<&str>, address: Option<Address>) -> Customer {
        Customer {
            address,
            balance: None,
            business_name: None,
            cash_balance: None,
            created: 0,
            currency: None,
            customer_account: None,
            default_source: None,
            delinquent: None,
            description: None,
            discount: None,
            email: Some(format!("customer{index}@example.com")),
            id: stripe_core::CustomerId::from(format!("cus_test{index}").as_str()),
            individual_name: None,
            invoice_credit_balance: None,
            invoice_prefix: None,
            invoice_settings: None,
            livemode: false,
            metadata: None,
            name: name.map(str::to_owned),
            next_invoice_sequence: None,
            phone: None,
            preferred_locales: None,
            shipping: None,
            sources: None,
            subscriptions: None,
            tax: None,
            tax_exempt: None,
            tax_ids: None,
            test_clock: None,
        }
    }

    /// Generates a label sheet for 17 customers (forcing a second page),
    /// including one with no address and one with special characters in the
    /// name, and verifies Typst compiles it into a valid, multi-page PDF.
    #[test]
    fn label_sheet_compiles_to_multi_page_pdf() {
        let mut customers: Vec<Customer> = (0..16)
            .map(|i| {
                fake_customer(
                    i,
                    Some("Jane Doe"),
                    Some(Address {
                        city: Some("Springfield".to_string()),
                        country: Some("US".to_string()),
                        line1: Some("123 Main St".to_string()),
                        line2: Some("Apt 4B".to_string()),
                        postal_code: Some("62704".to_string()),
                        state: Some("IL".to_string()),
                    }),
                )
            })
            .collect();
        customers.push(fake_customer(16, Some("No Address & Co. 50%"), None));

        let pages = render_customer_report_pages(&customers);
        assert_eq!(pages.len(), 2, "expected 17 customers to span 2 pages");
        assert_eq!(pages[0].len(), 14);
        assert_eq!(pages[1].len(), 3);
        assert!(matches!(
            &pages[1][2],
            LabelCell::Customer { name, no_address: true, .. } if name == "No Address & Co. 50%"
        ));

        let engine = build_typst_engine();
        let pdf =
            render_label_sheet_pdf(&engine, pages).expect("typst should compile the label sheet");
        assert!(pdf.starts_with(b"%PDF"), "output is not a valid PDF");

        let document =
            lopdf::Document::load_mem(&pdf).expect("typst output should be a parseable PDF");
        assert_eq!(
            document.get_pages().len(),
            2,
            "expected exactly 2 pages for 17 customers"
        );
    }

    #[test]
    fn empty_customer_list_still_produces_a_valid_pdf() {
        let pages = render_customer_report_pages(&[]);
        assert!(matches!(
            &pages[..],
            [page] if matches!(&page[..], [LabelCell::Placeholder(text)] if text == "No customers found.")
        ));

        let engine = build_typst_engine();
        let pdf =
            render_label_sheet_pdf(&engine, pages).expect("typst should compile an empty sheet");
        assert!(pdf.starts_with(b"%PDF"));
    }

    /// A day count of seconds, for building readable Unix timestamps in tests.
    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn overlaps_when_subscription_is_still_active_and_started_before_the_period() {
        let period_start = 100 * DAY;
        let now = 200 * DAY;
        assert!(subscription_overlaps_period(
            &SubscriptionStatus::Active,
            50 * DAY,
            None,
            period_start,
            now,
        ));
    }

    #[test]
    fn excludes_subscription_canceled_before_the_period_started() {
        let period_start = 100 * DAY;
        let now = 200 * DAY;
        assert!(!subscription_overlaps_period(
            &SubscriptionStatus::Canceled,
            10 * DAY,
            Some(50 * DAY),
            period_start,
            now,
        ));
    }

    #[test]
    fn includes_subscription_canceled_during_the_period() {
        let period_start = 100 * DAY;
        let now = 200 * DAY;
        assert!(subscription_overlaps_period(
            &SubscriptionStatus::Canceled,
            10 * DAY,
            Some(150 * DAY),
            period_start,
            now,
        ));
    }

    #[test]
    fn excludes_subscriptions_that_never_left_incomplete_status() {
        let period_start = 0;
        let now = 200 * DAY;
        assert!(!subscription_overlaps_period(
            &SubscriptionStatus::Incomplete,
            10 * DAY,
            None,
            period_start,
            now,
        ));
        assert!(!subscription_overlaps_period(
            &SubscriptionStatus::IncompleteExpired,
            10 * DAY,
            Some(11 * DAY),
            period_start,
            now,
        ));
    }

    #[test]
    fn excludes_subscription_that_has_not_started_yet() {
        let period_start = 0;
        let now = 100 * DAY;
        assert!(!subscription_overlaps_period(
            &SubscriptionStatus::Trialing,
            200 * DAY,
            None,
            period_start,
            now,
        ));
    }
}
