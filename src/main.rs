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

secretspec_derive::declare_secrets!("secretspec.toml");

#[derive(Debug, Clone)]
struct AppState {
    stripe: Arc<Client>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let secrets = SecretSpec::builder()
        .with_reason("stripe-ticket-printer boot")
        .load()
        .expect("failed to load secrets from secretspec");

    let state = AppState {
        stripe: Arc::new(Client::new(secrets.secrets.stripe_secret_key)),
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
const INDEX_HTML_TEMPLATE: &str = include_str!("../templates/index.html");

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
/// those eligible to be posted something), rendered from a LaTeX template
/// via `tectonic`.
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

    let latex = render_customer_report_latex(&customers);
    tracing::trace!(latex_len = latex.len(), "rendered label sheet latex");

    let pdf = tokio::task::spawn_blocking(move || tectonic::latex_to_pdf(latex))
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
const LABEL_COLUMNS: usize = 2;

/// The label sheet LaTeX template, loaded from disk at compile time. It
/// defines the Avery L7163-compatible page geometry and a `\ShippingLabel`
/// macro; `%%CONTENT%%` is replaced with the generated label grid.
const LABEL_SHEET_TEMPLATE: &str = include_str!("../templates/shipping_labels.tex");

/// Builds a shipping-label sheet (A4, Avery L7163-compatible: 99.09 x 38.1 mm,
/// 2 columns x 7 rows, 14 labels per sheet) listing each customer's name and
/// address, adding as many pages as needed.
fn render_customer_report_latex(customers: &[Customer]) -> String {
    let mut content = String::new();

    if customers.is_empty() {
        content.push_str(
            "\\begin{tabular}{@{}p{\\LabelWidth}@{\\hspace{\\LabelGap}}p{\\LabelWidth}@{}}\n\
             \\ShippingLabel{\\textit{No customers found.}} & \\ShippingLabel{}\\\\\n\
             \\end{tabular}\n",
        );
    } else {
        for (page_index, page) in customers.chunks(LABELS_PER_PAGE).enumerate() {
            if page_index > 0 {
                content.push_str("\\newpage\n");
            }
            content.push_str(
                "\\begin{tabular}{@{}p{\\LabelWidth}@{\\hspace{\\LabelGap}}p{\\LabelWidth}@{}}\n",
            );
            for row in page.chunks(LABEL_COLUMNS) {
                let left = label_body(&row[0]);
                let right = row.get(1).map(label_body).unwrap_or_default();
                let _ = writeln!(
                    content,
                    "\\ShippingLabel{{{left}}} & \\ShippingLabel{{{right}}}\\\\"
                );
            }
            content.push_str("\\end{tabular}\n");
        }
    }

    LABEL_SHEET_TEMPLATE.replace("%%CONTENT%%", &content)
}

/// Renders a single shipping label's body: customer name in bold, followed by
/// their address (street lines, city/state/postal line, country), each
/// separated by a LaTeX line break.
fn label_body(customer: &Customer) -> String {
    let name = customer
        .name
        .as_deref()
        .or(customer.email.as_deref())
        .unwrap_or(customer.id.as_str());

    let mut lines = vec![format!("\\textbf{{{}}}", escape_latex(name))];

    match &customer.address {
        Some(address) => {
            if let Some(line1) = non_empty(address.line1.as_deref()) {
                lines.push(escape_latex(line1));
            }
            if let Some(line2) = non_empty(address.line2.as_deref()) {
                lines.push(escape_latex(line2));
            }

            let city_state_zip = [
                address.city.as_deref(),
                address.state.as_deref(),
                address.postal_code.as_deref(),
            ]
            .into_iter()
            .filter_map(non_empty)
            .map(escape_latex)
            .collect::<Vec<_>>()
            .join(" ");
            if !city_state_zip.is_empty() {
                lines.push(city_state_zip);
            }

            if let Some(country) = non_empty(address.country.as_deref()) {
                lines.push(escape_latex(country));
            }
        }
        None => lines.push("\\textit{No address on file}".to_string()),
    }

    lines.join("\\\\\n      ")
}

/// Treats blank strings the same as absent values.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|s| !s.trim().is_empty())
}

/// Escapes LaTeX special characters so arbitrary customer-provided strings
/// can be embedded safely in the generated document.
fn escape_latex(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\textbackslash{}"),
            '&' | '%' | '$' | '#' | '_' | '{' | '}' => {
                out.push('\\');
                out.push(ch);
            }
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            _ => out.push(ch),
        }
    }
    out
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
            item.price
                .unit_amount
                .unwrap_or(0)
                .saturating_mul(quantity)
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
    use std::sync::Mutex;
    use stripe_shared::Address;

    /// Tectonic writes to a shared on-disk cache and is not safe to call from
    /// multiple threads at once.  All tests that invoke `tectonic::latex_to_pdf`
    /// must hold this lock for the duration of the call.
    static TECTONIC_LOCK: Mutex<()> = Mutex::new(());

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
    /// including one with no address and one with special LaTeX characters
    /// in the name, and verifies `tectonic` compiles it into a valid,
    /// multi-page PDF.
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

        let latex = render_customer_report_latex(&customers);
        assert!(latex.contains("\\ShippingLabel"));
        assert!(latex.contains("\\newpage"));
        assert!(latex.contains("No Address \\& Co. 50\\%"));

        let _lock = TECTONIC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pdf = tectonic::latex_to_pdf(latex).expect("tectonic should compile the label sheet");
        assert!(pdf.starts_with(b"%PDF"), "output is not a valid PDF");

        let document =
            lopdf::Document::load_mem(&pdf).expect("tectonic output should be a parseable PDF");
        assert_eq!(
            document.get_pages().len(),
            2,
            "expected exactly 2 pages for 17 customers"
        );
    }

    #[test]
    fn empty_customer_list_still_produces_a_valid_pdf() {
        let latex = render_customer_report_latex(&[]);
        assert!(latex.contains("No customers found."));
        let _lock = TECTONIC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pdf = tectonic::latex_to_pdf(latex).expect("tectonic should compile an empty sheet");
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
