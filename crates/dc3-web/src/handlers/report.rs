//! `GET /report/{key}` (the form) and `POST /report/{key}` (submission).
//!
//! A submission goes through the CSRF check, then its fields are cleaned and
//! checked, then `submit_report` stores it. The thank-you page is the same
//! whether or not the report hid anything. No address is stored.

use std::sync::Arc;

use axum::RequestExt;
use axum::extract::rejection::{FormRejection, PathRejection};
use axum::extract::{Form, Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use dc3_core::AnyKey;
use dc3_core::text::is_bidi_control;
use dc3_store::{NewReport, REPORT_CONTACT_MAX_CHARS, REPORT_MESSAGE_MAX_CHARS, ReportReason};
use serde::Deserialize;

use super::torrent::BAD_KEY_MESSAGE;
use super::{key_hex, log_backend_error, parse_key};
use crate::app::{AppState, Site, routes};
use crate::render::{Flavor, error_response, html};
use crate::telemetry::metric_names;
use crate::templates::{ReasonChoice, ReportDonePage, ReportPage};
use crate::{Backend, BackendError, csrf};

/// Report reasons in form order, with their labels.
const REASONS: [(ReportReason, &str); 4] = [
    (ReportReason::Csam, "Child sexual abuse material"),
    (ReportReason::Copyright, "Copyright infringement"),
    (ReportReason::Malware, "Malware or a harmful file"),
    (ReportReason::Other, "Something else"),
];

const UNAVAILABLE_MESSAGE: &str = "Reports are temporarily unavailable. Please try again later.";

/// The submitted form fields.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReportForm {
    reason: Option<String>,
    message: Option<String>,
    contact: Option<String>,
}

/// `GET /report/{key}`.
pub(crate) async fn report_form<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    key: Result<Path<String>, PathRejection>,
) -> Response {
    let Some(key) = parse_key(key) else {
        return error_response(
            &st.site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            BAD_KEY_MESSAGE,
        );
    };
    form_page(&st.site, StatusCode::OK, &key, None, "", "", None)
}

/// `POST /report/{key}` (`application/x-www-form-urlencoded`).
pub(crate) async fn report_submit<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    key: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let site = &st.site;
    // Checked before the body is read.
    if !csrf::post_allowed(request.headers(), &site.base_url) {
        return error_response(
            site,
            Flavor::Html,
            StatusCode::FORBIDDEN,
            "Reports can only be sent from the report form on this site.",
        );
    }
    let Some(key) = parse_key(key) else {
        return error_response(site, Flavor::Html, StatusCode::BAD_REQUEST, BAD_KEY_MESSAGE);
    };
    let form: Result<Form<ReportForm>, FormRejection> = request.extract().await;
    let form = match form {
        Ok(Form(form)) => form,
        Err(rejection) => return rejected(site, &rejection),
    };

    let reason = form
        .reason
        .as_deref()
        .and_then(|r| r.parse::<ReportReason>().ok());
    let message = clean_message(form.message.as_deref().unwrap_or_default());
    let contact = clean_line(form.contact.as_deref().unwrap_or_default());
    let problem = if reason.is_none() {
        Some("Please choose a reason.".to_owned())
    } else if message.chars().count() > REPORT_MESSAGE_MAX_CHARS {
        Some(format!(
            "The details are too long. Use at most {REPORT_MESSAGE_MAX_CHARS} characters."
        ))
    } else if contact.chars().count() > REPORT_CONTACT_MAX_CHARS {
        Some(format!(
            "The contact address is too long. Use at most {REPORT_CONTACT_MAX_CHARS} characters."
        ))
    } else {
        None
    };
    let (Some(reason), None) = (reason, problem.as_deref()) else {
        return form_page(
            site,
            StatusCode::BAD_REQUEST,
            &key,
            reason,
            &message,
            &contact,
            problem.as_deref(),
        );
    };

    let report = NewReport {
        key,
        reason,
        message,
        contact: (!contact.is_empty()).then_some(contact),
    };
    match st.backend.submit_report(&report).await {
        Ok(outcome) => {
            metrics::counter!(metric_names::REPORTS, "reason" => reason.as_str()).increment(1);
            if outcome.hidden {
                metrics::counter!(metric_names::AUTOHIDE).increment(1);
            }
            if outcome.budget_exhausted {
                metrics::counter!(metric_names::AUTOHIDE_BUDGET_EXHAUSTED).increment(1);
            }
            let page = ReportDonePage {
                page: site.page("Report received", "", true),
                csam: reason == ReportReason::Csam,
            };
            html(StatusCode::OK, &page)
        }
        Err(BackendError::Invalid(_)) => form_page(
            site,
            StatusCode::BAD_REQUEST,
            &key,
            Some(reason),
            &report.message,
            report.contact.as_deref().unwrap_or_default(),
            Some("The report could not be accepted. Please check the fields and try again."),
        ),
        Err(BackendError::ReportsFull) => error_response(
            site,
            Flavor::Html,
            StatusCode::SERVICE_UNAVAILABLE,
            UNAVAILABLE_MESSAGE,
        ),
        Err(e) => {
            log_backend_error(&e, "report submission");
            error_response(
                site,
                Flavor::Html,
                StatusCode::SERVICE_UNAVAILABLE,
                UNAVAILABLE_MESSAGE,
            )
        }
    }
}

/// The report form, optionally with the visitor's input and a problem.
fn form_page(
    site: &Site,
    status: StatusCode,
    key: &AnyKey,
    reason: Option<ReportReason>,
    message: &str,
    contact: &str,
    error: Option<&str>,
) -> Response {
    let hex = key_hex(key);
    let reasons = REASONS
        .iter()
        .map(|(value, label)| ReasonChoice {
            value: value.as_str(),
            label,
            checked: reason == Some(*value),
        })
        .collect();
    let page = ReportPage {
        page: site.page("Report a torrent", "", true),
        action: format!("{}{hex}", routes::REPORT_PREFIX),
        torrent_href: format!("{}{hex}", routes::TORRENT_PREFIX),
        key: hex,
        reasons,
        message,
        contact,
        error,
        message_max: REPORT_MESSAGE_MAX_CHARS,
        contact_max: REPORT_CONTACT_MAX_CHARS,
    };
    html(status, &page)
}

/// The response to a body the form extractor refused.
fn rejected(site: &Site, rejection: &FormRejection) -> Response {
    let (status, message) = match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => {
            (StatusCode::PAYLOAD_TOO_LARGE, "The report is too large.")
        }
        StatusCode::UNSUPPORTED_MEDIA_TYPE => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Reports must be sent with the report form on this site.",
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "The report could not be read. Please use the report form.",
        ),
    };
    error_response(site, Flavor::Html, status, message)
}

/// A multi-line message: line breaks become `\n`, other control and bidi
/// characters are removed (reports are read in a terminal), and the text is
/// trimmed.
fn clean_message(raw: &str) -> String {
    raw.replace("\r\n", "\n")
        .chars()
        .map(|c| if c == '\r' { '\n' } else { c })
        .filter(|c| *c == '\n' || *c == '\t' || !(c.is_control() || is_bidi_control(*c)))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A single line: control and bidi characters removed, trimmed.
fn clean_line(raw: &str) -> String {
    raw.chars()
        .filter(|c| !(c.is_control() || is_bidi_control(*c)))
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_cleaned() {
        assert_eq!(
            clean_message("  line one\r\nline two\rthree\u{1b}[2J\u{202e}\t!  "),
            "line one\nline two\nthree[2J\t!"
        );
        assert_eq!(clean_message("\u{0}"), "");
    }

    #[test]
    fn lines_are_cleaned() {
        assert_eq!(
            clean_line(" me@example.org\r\nBcc: x "),
            "me@example.orgBcc: x"
        );
        assert_eq!(clean_line("\u{2066}a\u{2069}"), "a");
    }

    #[test]
    fn reasons_cover_every_report_reason() {
        assert_eq!(REASONS.len(), ReportReason::ALL.len());
        for reason in ReportReason::ALL {
            assert!(REASONS.iter().any(|(r, _)| r == reason));
        }
    }
}
