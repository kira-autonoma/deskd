//! Shared state for the web adapter (#443).
//!
//! `WebState` is a single struct shared across every axum handler via
//! `axum::extract::State`. It bundles config, the secret, the magic-link
//! token store, the rate limiter, the audit log writer, and the Telegram
//! dispatcher (a trait object so tests can stub it out).

use std::sync::{Arc, Mutex};

use crate::config::{GitHubWebhookConfig, WebConfig};

use super::audit::AuditLog;
use super::auth::magic_link::TokenStore;
use super::dispatch::{BusSender, TelegramDispatcher};
use super::middleware::rate_limit::RateLimiter;
use super::routes::github_webhook::DeliveryDedupe;

/// Aggregate state passed through axum's `State` extractor.
#[derive(Clone)]
pub struct WebState {
    pub cfg: Arc<WebConfig>,
    pub secret: Arc<[u8; 32]>,
    pub tokens: Arc<TokenStore>,
    pub rate_limiter_ip: Arc<RateLimiter>,
    pub rate_limiter_tg: Arc<RateLimiter>,
    pub audit: AuditLog,
    pub telegram: Arc<dyn TelegramDispatcher>,
    /// GitHub webhook adapter config (#457). When `Some`, the
    /// `POST /webhooks/github` route is mounted and incoming deliveries are
    /// routed to subscribed agents via `bus`. `None` → route returns 404.
    pub github_webhooks: Option<Arc<GitHubWebhookConfig>>,
    /// Bus sender used by the GitHub webhook adapter (#457). Kept on state so
    /// integration tests can inject a recording sender. Production wiring
    /// shares one `BusDispatcher` for both `telegram` and `bus`.
    pub bus: Arc<dyn BusSender>,
    /// In-memory dedupe store for `X-GitHub-Delivery` ids — 10 minute TTL.
    /// Restart-safe loss is acceptable (GitHub retries deliveries on its own).
    pub github_deliveries: Arc<Mutex<DeliveryDedupe>>,
    /// Cached "now" provider — defaults to system time. Tests substitute a
    /// closure that returns a fixed timestamp so cookie/expiry semantics are
    /// deterministic.
    pub now: NowFn,
}

/// Returns a unix timestamp in seconds. Boxed so we can stub it out in tests.
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Default "now" implementation backed by `chrono::Utc::now()`.
pub fn system_now() -> NowFn {
    Arc::new(|| chrono::Utc::now().timestamp())
}
