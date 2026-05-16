//! Axum router builder for the web adapter (#443).

use axum::{
    Router, middleware,
    routing::{get, post},
};

use super::middleware::headers::security_headers;
use super::routes::{dashboard, health, login, logout, sse, static_assets};
use super::state::WebState;

/// Build the axum router. Public so the integration tests can drive it
/// in-process via `tower::ServiceExt::oneshot`.
pub fn build(state: WebState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/", get(dashboard::dashboard))
        .route("/events", get(sse::events))
        .route("/login", get(login::login_form))
        .route("/login/request", post(login::login_request))
        .route("/login/consume", get(login::login_consume))
        .route("/logout", post(logout::logout))
        .route("/static/htmx.min.js", get(static_assets::htmx_js))
        .route("/static/htmx-sse.js", get(static_assets::htmx_sse_js))
        .route("/static/dashboard.css", get(static_assets::dashboard_css))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}
