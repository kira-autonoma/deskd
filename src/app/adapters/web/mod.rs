//! Web adapter — axum HTTP server with Telegram magic-link auth (#443).
//!
//! Foundation for the #442 web control panel epic. Exposes a small surface:
//!
//! - `GET  /healthz`        — unauthenticated liveness probe
//! - `GET  /`               — placeholder dashboard (auth required)
//! - `GET  /login`          — magic-link request form
//! - `POST /login/request`  — issue a token and dispatch via Telegram
//! - `GET  /login/consume`  — consume a one-shot token, set session cookie
//! - `POST /logout`         — clear session cookie
//!
//! The web adapter is workspace-level (one instance per `deskd serve`,
//! shared across all agents) because the issue spec defines a single
//! `web:` block at the top of `workspace.yaml` and the magic-link flow
//! routes through whichever agent's bus we connect to. Future child PRs
//! of #442 may add per-agent dashboards on top of this foundation.

pub mod audit;
pub mod auth;
pub mod data;
pub mod dispatch;
pub mod middleware;
pub mod router;
pub mod routes;
pub mod state;
pub mod templates;
pub mod view;

use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::config::{GitHubWebhookConfig, WebConfig};

use audit::AuditLog;
use auth::{magic_link::TokenStore, secret};
use dispatch::{BusDispatcher, BusSender, TelegramDispatcher};
use middleware::rate_limit::RateLimiter;
use routes::github_webhook;
use state::{WebState, system_now};

/// Construct a `WebState` with the production dispatcher pointed at the
/// supplied bus socket. `github_webhooks` is forwarded onto state so the
/// `POST /webhooks/github` route knows which subscriptions are active.
pub fn build_state(
    cfg: WebConfig,
    bus_socket: String,
    github_webhooks: Option<GitHubWebhookConfig>,
) -> Result<WebState> {
    let secret_bytes = secret::load_or_create()?;
    let bus_dispatcher = Arc::new(BusDispatcher::new(bus_socket, "web".to_string()));
    let telegram: Arc<dyn TelegramDispatcher> = bus_dispatcher.clone();
    let bus: Arc<dyn BusSender> = bus_dispatcher;
    Ok(build_state_with_dispatcher(
        cfg,
        secret_bytes,
        telegram,
        bus,
        github_webhooks,
    ))
}

/// Like [`build_state`] but with injected dispatchers (used by tests).
pub fn build_state_with_dispatcher(
    cfg: WebConfig,
    secret_bytes: [u8; 32],
    dispatcher: Arc<dyn TelegramDispatcher>,
    bus: Arc<dyn BusSender>,
    github_webhooks: Option<GitHubWebhookConfig>,
) -> WebState {
    let audit_path = audit::expand_home(&cfg.audit_log);
    let limit = cfg.rate_limit.auth_requests_per_hour;

    WebState {
        cfg: Arc::new(cfg),
        secret: Arc::new(secret_bytes),
        tokens: Arc::new(TokenStore::new()),
        rate_limiter_ip: Arc::new(RateLimiter::new(limit, 3600)),
        rate_limiter_tg: Arc::new(RateLimiter::new(limit, 3600)),
        audit: AuditLog::new(audit_path),
        telegram: dispatcher,
        github_webhooks: github_webhooks.map(Arc::new),
        bus,
        github_deliveries: github_webhook::shared_dedupe(),
        now: system_now(),
    }
}

/// Start the web adapter HTTP server. Returns when `cancel` is triggered or
/// `axum::serve` exits with an error. Bound to `cfg.bind`.
pub async fn run(
    cfg: WebConfig,
    bus_socket: String,
    github_webhooks: Option<GitHubWebhookConfig>,
    cancel: CancellationToken,
) -> Result<()> {
    let bind = cfg.bind.clone();
    let state = build_state(cfg, bus_socket, github_webhooks)?;
    run_with_state(state, bind, cancel).await
}

/// Start the server with a pre-built state. Public so tests can spin up the
/// adapter against a recording dispatcher.
pub async fn run_with_state(
    state: WebState,
    bind: String,
    cancel: CancellationToken,
) -> Result<()> {
    let app = router::build(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, "web adapter listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        cancel.cancelled().await;
    })
    .await?;
    Ok(())
}
