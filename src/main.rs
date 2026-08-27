use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use stripe::Client;
use stripe_billing::subscription::{ListSubscription, ListSubscriptionStatus};
use stripe_billing::{Subscription, SubscriptionStatus};
use stripe_core::Customer;
use stripe_core::customer::ListCustomer;

secretspec_derive::declare_secrets!("secretspec.toml");

#[derive(Debug, Clone)]
struct AppState {
    stripe: Arc<Client>,
}

#[tokio::main]
async fn main() {
    let secrets = SecretSpec::builder()
        .with_provider("env://")
        .with_reason("stripe-ticket-printer boot")
        .load()
        .expect("failed to load secrets from secretspec");

    let state = AppState {
        stripe: Arc::new(Client::new(secrets.secrets.stripe_secret_key)),
    };

    let app = Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/report", get(report))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("failed to bind to 0.0.0.0:3000");
    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.expect("server error");
}

/// Health check for showing the app is running.
async fn healthz(State(_state): State<AppState>) -> Result<String, (StatusCode, String)> {
    Ok("OK".to_string())
}

/// Demonstrates the Stripe API key is wired up correctly by listing a
/// handful of customers from the connected Stripe account.
async fn root(State(state): State<AppState>) -> Result<String, (StatusCode, String)> {
    let customers = ListCustomer::new()
        .limit(3)
        .send(state.stripe.as_ref())
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Stripe API call failed: {err}"),
            )
        })?;

    Ok(format!(
        "Stripe API call succeeded: found {} customer(s).",
        customers.data.len()
    ))
}

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
async fn report(
    State(state): State<AppState>,
    Query(query): Query<ReportQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let start_date = NaiveDate::parse_from_str(&query.start_date, "%Y-%m-%d")
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid start_date {:?}: expected YYYY-MM-DD",
                    query.start_date
                ),
            )
        })?
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always a valid time")
        .and_utc()
        .timestamp();
    let now = Utc::now().timestamp();

    let customers =
        customers_with_subscriptions_active_between(state.stripe.as_ref(), start_date, now)
            .await
            .map_err(|err| {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("Stripe API call failed: {err}"),
                )
            })?;

    let latex = render_customer_report_latex(&customers);

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

/// Fetches every subscription (across all pages, all statuses) and returns
/// the distinct customers whose subscription overlapped `[start_date, now]`.
async fn customers_with_subscriptions_active_between(
    client: &Client,
    start_date: i64,
    now: i64,
) -> Result<Vec<Customer>, stripe::StripeError> {
    let mut customers = Vec::new();
    let mut seen_customer_ids = HashSet::new();
    let mut starting_after: Option<String> = None;

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

        for subscription in &page.data {
            if !subscription_active_between(subscription, start_date, now) {
                continue;
            }
            let Some(customer) = subscription.customer.as_object() else {
                continue;
            };
            if seen_customer_ids.insert(customer.id.clone()) {
                customers.push(customer.clone());
            }
        }

        if !page.has_more {
            break;
        }
        let Some(next_cursor) = next_cursor else {
            break;
        };
        starting_after = Some(next_cursor);
    }

    Ok(customers)
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
