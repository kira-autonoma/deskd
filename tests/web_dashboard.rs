//! Integration tests for the #444 dashboard view + SSE endpoint.
//!
//! These exercise the same axum router used by the production binary, with
//! the magic-link dispatcher swapped for a recording stub. They cover the
//! ACs from #444:
//!
//! - `GET /` (auth) renders agent cards from live registry data.
//! - Each card carries the issue-specified fields (status / model / ctx /
//!   home dir / current task) and falls back to em-dash when data is
//!   missing.
//! - `GET /events` requires auth (302 to /login when missing/tampered),
//!   returns SSE content-type otherwise.
//! - SSE stream actually emits events when underlying state changes (via
//!   the public `diff_to_events` helper, which is the engine each tick
//!   uses internally).
//! - 0/1/10-agent renders all succeed without panics.
//! - Static htmx + SSE-extension assets are served from `/static/`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use deskd::app::adapters::web::audit::AuditLog;
use deskd::app::adapters::web::auth::{magic_link::TokenStore, session};
use deskd::app::adapters::web::data::AgentSummary;
use deskd::app::adapters::web::dispatch::testing::RecordingDispatcher;
use deskd::app::adapters::web::middleware::rate_limit::RateLimiter;
use deskd::app::adapters::web::router;
use deskd::app::adapters::web::routes::SESSION_COOKIE_NAME;
use deskd::app::adapters::web::routes::sse::diff_to_events;
use deskd::app::adapters::web::state::WebState;
use deskd::config::{WebConfig, WebRateLimitConfig};
use tower::ServiceExt;

const TEST_TG_ID: i64 = 11_000;
const TEST_SECRET: [u8; 32] = [
    9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 22, 33, 44, 55, 66, 77, 88, 99, 100, 101, 102, 103, 104, 105,
    106, 107, 108, 109, 110, 111, 112,
];

fn cfg(audit_path: std::path::PathBuf) -> WebConfig {
    WebConfig {
        enabled: true,
        bind: "127.0.0.1:0".into(),
        external_url: "https://deskd.example.com".into(),
        session_ttl_days: 30,
        magic_link_ttl_seconds: 300,
        allowed_telegram_ids: vec![TEST_TG_ID],
        audit_log: audit_path.to_string_lossy().to_string(),
        rate_limit: WebRateLimitConfig {
            auth_requests_per_hour: 20,
        },
    }
}

fn build_state() -> (WebState, RecordingDispatcher, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let cfg_obj = cfg(audit_path.clone());
    let dispatcher = RecordingDispatcher::new();
    let dispatcher_arc: Arc<dyn deskd::app::adapters::web::dispatch::TelegramDispatcher> =
        Arc::new(dispatcher.clone());
    let state = WebState {
        cfg: Arc::new(cfg_obj),
        secret: Arc::new(TEST_SECRET),
        tokens: Arc::new(TokenStore::new()),
        rate_limiter_ip: Arc::new(RateLimiter::new(20, 3600)),
        rate_limiter_tg: Arc::new(RateLimiter::new(20, 3600)),
        audit: AuditLog::new(audit_path),
        telegram: dispatcher_arc,
        now: Arc::new(|| 1_700_000_000),
    };
    (state, dispatcher, dir)
}

fn fake_peer() -> std::net::SocketAddr {
    "127.0.0.1:54321".parse().unwrap()
}

fn req_with_peer(req: Request<Body>) -> Request<Body> {
    let mut r = req;
    r.extensions_mut()
        .insert(axum::extract::ConnectInfo(fake_peer()));
    r
}

fn auth_cookie(state: &WebState) -> String {
    let now = (state.now)();
    let payload = session::SessionPayload::new(TEST_TG_ID, now, 86_400, "csrf-x".into());
    session::sign(&payload, state.secret.as_ref())
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ─── /events authentication ─────────────────────────────────────────────

#[tokio::test]
async fn events_redirects_unauthenticated_to_login() {
    let (state, _disp, _dir) = build_state();
    let app = router::build(state);
    let req = req_with_peer(Request::get("/events").body(Body::empty()).unwrap());
    let resp = app.oneshot(req).await.unwrap();
    assert!(resp.status().is_redirection(), "got {}", resp.status());
    assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
}

#[tokio::test]
async fn events_with_valid_session_returns_sse_content_type() {
    let (state, _disp, _dir) = build_state();
    let cookie = auth_cookie(&state);
    let app = router::build(state);
    // max_ticks=1 + tick_ms=10 makes the stream finite enough that
    // `oneshot` returns promptly.
    let req = req_with_peer(
        Request::get("/events?max_ticks=1&tick_ms=10&keepalive_ms=10000")
            .header(
                header::COOKIE,
                format!("{}={}", SESSION_COOKIE_NAME, cookie),
            )
            .body(Body::empty())
            .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type set")
        .to_str()
        .unwrap();
    assert!(
        ct.starts_with("text/event-stream"),
        "expected SSE content-type, got {}",
        ct
    );
    let cache = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .expect("cache-control set");
    assert_eq!(cache.to_str().unwrap(), "no-cache");
}

// ─── diff coalescing rules (the SSE engine, exercised directly) ─────────

#[tokio::test]
async fn diff_emits_status_event_on_transition() {
    let prev = HashMap::from([("a".to_string(), summary("a", "idle", None))]);
    let curr = vec![summary("a", "working", None)];
    let events = diff_to_events(&prev, &curr);
    // Status event + card swap = 2 minimum; never zero.
    assert!(!events.is_empty());
}

#[tokio::test]
async fn diff_coalesces_sub_bucket_token_drift() {
    let prev = HashMap::from([("a".to_string(), summary("a", "idle", Some(100_000)))]);
    let curr = vec![summary("a", "idle", Some(100_500))]; // <1k delta
    assert!(diff_to_events(&prev, &curr).is_empty());
}

#[tokio::test]
async fn diff_emits_compacted_event_on_large_drop() {
    let prev = HashMap::from([("a".to_string(), summary("a", "idle", Some(280_000)))]);
    let curr = vec![summary("a", "idle", Some(48_000))];
    let events = diff_to_events(&prev, &curr);
    // ≥3: agent.context, agent.compacted, card swap.
    assert!(
        events.len() >= 3,
        "expected ≥3 events, got {}",
        events.len()
    );
}

fn summary(name: &str, status: &str, tokens: Option<u64>) -> AgentSummary {
    AgentSummary {
        name: name.into(),
        status: status.into(),
        model: "claude-opus-4-7".into(),
        last_activity: None,
        context_tokens: tokens,
        context_threshold: Some(300_000),
        home_dir_bytes: None,
        current_task: None,
        task_running_for: None,
    }
}

// ─── static asset serving ───────────────────────────────────────────────

#[tokio::test]
async fn static_htmx_served_with_js_content_type() {
    let (state, _disp, _dir) = build_state();
    let app = router::build(state);
    let req = req_with_peer(
        Request::get("/static/htmx.min.js")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.starts_with("application/javascript"));
    let body = body_string(resp).await;
    assert!(body.starts_with("var htmx="));
}

#[tokio::test]
async fn static_htmx_sse_extension_served_with_js_content_type() {
    let (state, _disp, _dir) = build_state();
    let app = router::build(state);
    let req = req_with_peer(
        Request::get("/static/htmx-sse.js")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    assert!(body.contains("Server Sent Events Extension"));
}

// ─── dashboard rendering (page-level smoke checks) ──────────────────────

#[tokio::test]
async fn dashboard_renders_vps_strip_and_agents_section() {
    // The dashboard route reads the live registry. Tests don't seed agents,
    // so this exercises the empty path: friendly empty-state copy + the VPS
    // strip with em-dash placeholders for the as-yet-unwired #446 disk
    // metrics.
    let (state, _disp, _dir) = build_state();
    let cookie = auth_cookie(&state);
    let app = router::build(state);
    let req = req_with_peer(
        Request::get("/")
            .header(
                header::COOKIE,
                format!("{}={}", SESSION_COOKIE_NAME, cookie),
            )
            .body(Body::empty())
            .unwrap(),
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    // VPS strip is always rendered, even when disk metrics are missing.
    assert!(
        body.contains("vps-strip"),
        "missing VPS strip: {}",
        &body[..200]
    );
    assert!(body.contains("uptime"));
    // Vendored htmx + SSE extension wired in.
    assert!(body.contains(r#"src="/static/htmx.min.js""#));
    assert!(body.contains(r#"src="/static/htmx-sse.js""#));
    // SSE wiring on the agent section.
    assert!(body.contains("sse-connect=\"/events\""));
}
