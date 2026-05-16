//! Vendored static assets (#444).
//!
//! Serves htmx, the htmx SSE extension, and the dashboard stylesheet from
//! the binary itself via `include_str!`. Vendoring (rather than CDN) keeps
//! the strict CSP from #443 (`script-src 'self'; style-src 'self'`) intact
//! — no third-party origins required, and no inline `<style>` element.
//!
//! Routes:
//! - `GET /static/htmx.min.js`   — htmx 2.0.3 minified
//! - `GET /static/htmx-sse.js`   — htmx-ext-sse 2.2.2 (the SSE extension)
//! - `GET /static/dashboard.css` — dashboard stylesheet

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

const HTMX_JS: &str = include_str!("../static/htmx.min.js");
const HTMX_SSE_JS: &str = include_str!("../static/htmx-sse.js");
/// Dashboard stylesheet. Public so the templates module can sanity-check
/// the bundled CSS without re-reading it from disk.
pub const DASHBOARD_CSS: &str = include_str!("../static/dashboard.css");

const JS_CONTENT_TYPE: &str = "application/javascript; charset=utf-8";
const CSS_CONTENT_TYPE: &str = "text/css; charset=utf-8";
/// Cache for an hour — we'll bump these by editing the file and rebuilding,
/// no fingerprinting needed for a single-user dashboard.
const STATIC_CACHE_CONTROL: &str = "public, max-age=3600";

pub async fn htmx_js() -> Response {
    static_response(HTMX_JS, JS_CONTENT_TYPE)
}

pub async fn htmx_sse_js() -> Response {
    static_response(HTMX_SSE_JS, JS_CONTENT_TYPE)
}

pub async fn dashboard_css() -> Response {
    static_response(DASHBOARD_CSS, CSS_CONTENT_TYPE)
}

fn static_response(body: &'static str, content_type: &'static str) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = content_type.parse() {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = STATIC_CACHE_CONTROL.parse() {
        h.insert(header::CACHE_CONTROL, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn htmx_js_served_with_js_content_type() {
        let resp = htmx_js().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("application/javascript"));
    }

    #[tokio::test]
    async fn sse_extension_starts_with_iife() {
        // Sanity-check that we vendored the right file, not an HTML 404 page.
        assert!(HTMX_SSE_JS.contains("Server Sent Events Extension"));
    }

    #[tokio::test]
    async fn htmx_main_starts_with_var_declaration() {
        assert!(HTMX_JS.starts_with("var htmx="));
    }

    #[tokio::test]
    async fn dashboard_css_served_with_css_content_type() {
        let resp = dashboard_css().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/css"));
    }

    #[tokio::test]
    async fn dashboard_css_includes_width_bucket_classes() {
        // The width-bucket scheme replaces inline `style="width: X%"` on
        // the progress bar so the strict CSP (no inline styles) holds.
        assert!(DASHBOARD_CSS.contains(".ctx-bar__fill--w-0"));
        assert!(DASHBOARD_CSS.contains(".ctx-bar__fill--w-50"));
        assert!(DASHBOARD_CSS.contains(".ctx-bar__fill--w-100"));
    }
}
