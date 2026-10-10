//! `POST /theme`: storing the dark/light theme preference.
//!
//! The header switch is a plain form with the wanted `theme` and the local
//! `next` path to return to. The choice is stored in a `theme` cookie and
//! the visitor is redirected back, so the preference works without
//! JavaScript and survives navigation. There is no session or account to
//! forge, so a wrong choice at worst annoys; `next` is still kept local.

use std::sync::Arc;

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, State};
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde::Deserialize;

use crate::Backend;
use crate::app::AppState;
use crate::render::{Flavor, error_response};
use crate::theme::Theme;

/// The theme switch form fields.
#[derive(Debug, Deserialize)]
pub(crate) struct ThemeForm {
    pub theme: String,
    pub next: Option<String>,
}

/// `POST /theme`.
pub(crate) async fn set_theme<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    form: Result<Form<ThemeForm>, FormRejection>,
) -> Response {
    let current = Theme::from_headers(&headers);
    let Ok(Form(input)) = form else {
        return error_response(
            &st.site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            "The theme choice could not be read.",
            current,
            "/",
        );
    };
    let Some(theme) = Theme::from_value(input.theme.trim()) else {
        return error_response(
            &st.site,
            Flavor::Html,
            StatusCode::BAD_REQUEST,
            "The theme must be dark or light.",
            current,
            "/",
        );
    };
    let next = Theme::sanitize_next(input.next.as_deref().unwrap_or("/"));
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    let location = HeaderValue::from_str(&next).unwrap_or_else(|_| HeaderValue::from_static("/"));
    response.headers_mut().insert(LOCATION, location);
    response
        .headers_mut()
        .insert(SET_COOKIE, theme.set_cookie());
    response
}
