//! `POST /webhooks/github` — GitHub webhook adapter (#457).
//!
//! Verifies the `X-Hub-Signature-256` HMAC against the configured shared
//! secret, dedupes by `X-GitHub-Delivery`, matches the payload against
//! configured per-repo subscriptions, and publishes a structured message to
//! each subscribed agent's bus inbox.
//!
//! Design notes:
//! - Signature verification is constant-time via `hmac::Mac::verify_slice`.
//! - Dedupe is in-memory with a 10-minute TTL; GitHub retries on transient
//!   failures so cold-start loss is acceptable.
//! - Unconfigured (repo, event) combinations return 200 quietly so GitHub
//!   does not back off the delivery — we just have nothing to do.
//! - Event-type matching: `X-GitHub-Event` alone (bare event, e.g. `ping`)
//!   matches subscriptions written as `ping`; deliveries with an `action`
//!   field additionally match `event.action` (e.g. `pull_request.closed`).

use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::GitHubWebhookConfig;

use super::super::state::WebState;

/// Dedupe TTL for `X-GitHub-Delivery` ids. GitHub assigns one UUID per
/// delivery and retries up to ~8h on failure; 10 minutes catches the common
/// "we 5xx'd and they immediately retried" duplicate without unbounded growth.
pub const DELIVERY_TTL_SECS: i64 = 600;

/// In-memory `delivery_id → first_seen_unix` map with TTL eviction. Behind a
/// `Mutex` because webhook traffic is low (a few per minute) and the lock is
/// held for microseconds.
#[derive(Default)]
pub struct DeliveryDedupe {
    seen: HashMap<String, i64>,
}

impl DeliveryDedupe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if `id` is fresh (not seen recently). Records it as
    /// seen at `now`. Evicts entries older than `DELIVERY_TTL_SECS`.
    pub fn check_and_record(&mut self, id: &str, now: i64) -> bool {
        // Evict expired entries.
        self.seen
            .retain(|_, ts| now.saturating_sub(*ts) < DELIVERY_TTL_SECS);
        if let Some(&seen_at) = self.seen.get(id)
            && now.saturating_sub(seen_at) < DELIVERY_TTL_SECS
        {
            return false;
        }
        self.seen.insert(id.to_string(), now);
        true
    }
}

fn make_dedupe() -> Mutex<DeliveryDedupe> {
    Mutex::new(DeliveryDedupe::new())
}

/// Build a fresh shared dedupe store for production wiring.
pub fn shared_dedupe() -> std::sync::Arc<Mutex<DeliveryDedupe>> {
    std::sync::Arc::new(make_dedupe())
}

/// Verify the GitHub-style `X-Hub-Signature-256` header. The header value is
/// `sha256=<hex>` where the hex is HMAC-SHA256(secret, body). Returns `true`
/// only when the prefix is present, the hex parses, and the MAC matches in
/// constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], header_value: &str) -> bool {
    let hex_part = match header_value.strip_prefix("sha256=") {
        Some(s) => s,
        None => return false,
    };
    let provided = match hex::decode(hex_part) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

/// Resolve a delivery to the bus targets that should receive it.
///
/// `event_header` is the value of `X-GitHub-Event` (e.g. `pull_request`).
/// `action` is the JSON `action` field if present. Matching rules:
///   - subscription event `pull_request.closed` matches when
///     `event_header == "pull_request"` and `action == Some("closed")`.
///   - subscription event `pull_request` (no dot) matches any action.
fn matching_targets(
    cfg: &GitHubWebhookConfig,
    repo_full_name: &str,
    event_header: &str,
    action: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    for sub in &cfg.subscriptions {
        if sub.repo != repo_full_name {
            continue;
        }
        for ev in &sub.events {
            let matches = match ev.split_once('.') {
                Some((base, sub_action)) => base == event_header && action == Some(sub_action),
                None => ev == event_header,
            };
            if matches {
                out.push(sub.deliver_to.clone());
                break;
            }
        }
    }
    out
}

/// Build a plain-text summary the agent can read directly. Stays short and
/// includes the upstream URL so the agent can drill in without re-fetching.
fn build_summary(
    repo: &str,
    event: &str,
    action: Option<&str>,
    payload: &serde_json::Value,
) -> String {
    let header = match action {
        Some(a) => format!("[github] {repo} {event}.{a}"),
        None => format!("[github] {repo} {event}"),
    };
    let detail = match event {
        "pull_request" => {
            let pr = &payload["pull_request"];
            let number = pr["number"].as_i64();
            let title = pr["title"].as_str().unwrap_or("");
            let url = pr["html_url"].as_str().unwrap_or("");
            match number {
                Some(n) => format!("PR #{n}: {title} ({url})"),
                None => format!("{title} ({url})"),
            }
        }
        "pull_request_review" => {
            let pr = &payload["pull_request"];
            let review = &payload["review"];
            let number = pr["number"].as_i64();
            let state = review["state"].as_str().unwrap_or("");
            let url = review["html_url"].as_str().unwrap_or("");
            match number {
                Some(n) => format!("PR #{n} review {state} ({url})"),
                None => format!("review {state} ({url})"),
            }
        }
        "issues" => {
            let issue = &payload["issue"];
            let number = issue["number"].as_i64();
            let title = issue["title"].as_str().unwrap_or("");
            let url = issue["html_url"].as_str().unwrap_or("");
            match number {
                Some(n) => format!("Issue #{n}: {title} ({url})"),
                None => format!("{title} ({url})"),
            }
        }
        "issue_comment" => {
            let issue = &payload["issue"];
            let number = issue["number"].as_i64();
            let url = payload["comment"]["html_url"].as_str().unwrap_or("");
            match number {
                Some(n) => format!("comment on #{n} ({url})"),
                None => format!("comment ({url})"),
            }
        }
        _ => String::new(),
    };
    if detail.is_empty() {
        header
    } else {
        format!("{header}\n{detail}")
    }
}

/// Compact structured metadata included alongside the summary so the agent
/// can pivot on action/repo/PR number without re-fetching.
fn build_metadata(
    repo: &str,
    event: &str,
    action: Option<&str>,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "delivery_id": delivery_id,
        "event": event,
        "action": action,
        "repo": repo,
        "sender": payload["sender"]["login"].as_str(),
    });
    if let serde_json::Value::Object(ref mut m) = meta {
        if let Some(pr) = payload.get("pull_request") {
            m.insert(
                "pull_request".to_string(),
                serde_json::json!({
                    "number": pr["number"].as_i64(),
                    "title": pr["title"].as_str(),
                    "state": pr["state"].as_str(),
                    "merged": pr["merged"].as_bool(),
                    "html_url": pr["html_url"].as_str(),
                    "user": pr["user"]["login"].as_str(),
                }),
            );
        }
        if let Some(issue) = payload.get("issue") {
            m.insert(
                "issue".to_string(),
                serde_json::json!({
                    "number": issue["number"].as_i64(),
                    "title": issue["title"].as_str(),
                    "state": issue["state"].as_str(),
                    "html_url": issue["html_url"].as_str(),
                    "labels": issue["labels"].as_array().map(|arr| {
                        arr.iter()
                            .filter_map(|l| l["name"].as_str())
                            .collect::<Vec<_>>()
                    }),
                }),
            );
        }
        if let Some(review) = payload.get("review") {
            m.insert(
                "review".to_string(),
                serde_json::json!({
                    "state": review["state"].as_str(),
                    "html_url": review["html_url"].as_str(),
                    "user": review["user"]["login"].as_str(),
                }),
            );
        }
        if let Some(label) = payload.get("label") {
            m.insert(
                "label".to_string(),
                serde_json::json!({"name": label["name"].as_str()}),
            );
        }
    }
    meta
}

/// `POST /webhooks/github` handler.
pub async fn github_webhook(
    State(state): State<WebState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let cfg = match state.github_webhooks.as_ref() {
        Some(c) => c.clone(),
        None => return StatusCode::NOT_FOUND,
    };

    // Reject early if the operator left the secret blank — never trust an
    // unsigned payload. Same status as a real signature mismatch so we don't
    // leak config state.
    if cfg.secret.is_empty() {
        return StatusCode::UNAUTHORIZED;
    }

    let sig_header = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(cfg.secret.as_bytes(), &body, sig_header) {
        return StatusCode::UNAUTHORIZED;
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if event.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if delivery_id.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // Dedupe before doing any work so a retried delivery is a cheap 200.
    {
        let now = (state.now)();
        let mut guard = state.github_deliveries.lock().unwrap();
        if !guard.check_and_record(&delivery_id, now) {
            tracing::debug!(delivery_id, "duplicate github webhook delivery — deduped");
            return StatusCode::OK;
        }
    }

    // GitHub's `ping` event has no `repository` field in some org-level
    // configurations; treat the absence as "nothing to do" rather than 400.
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if repo.is_empty() {
        return StatusCode::OK;
    }

    let action = payload.get("action").and_then(|v| v.as_str());
    let targets = matching_targets(&cfg, repo, &event, action);
    if targets.is_empty() {
        return StatusCode::OK;
    }

    let summary = build_summary(repo, &event, action, &payload);
    let metadata = build_metadata(repo, &event, action, &delivery_id, &payload);

    for target in targets {
        if let Err(e) = state
            .bus
            .send_bus(&target, &summary, metadata.clone())
            .await
        {
            tracing::warn!(target = %target, error = %e, "failed to publish github webhook to bus");
        }
    }

    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitHubWebhookSubscription;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn verify_signature_accepts_matching() {
        let secret = b"abc";
        let body = b"hello";
        let header = sign(secret, body);
        assert!(verify_signature(secret, body, &header));
    }

    #[test]
    fn verify_signature_rejects_wrong_secret() {
        assert!(!verify_signature(b"a", b"hello", &sign(b"b", b"hello")));
    }

    #[test]
    fn verify_signature_rejects_missing_prefix() {
        assert!(!verify_signature(b"a", b"hello", "deadbeef"));
    }

    #[test]
    fn verify_signature_rejects_garbage_hex() {
        assert!(!verify_signature(b"a", b"hello", "sha256=not-hex!"));
    }

    #[test]
    fn matching_targets_with_action() {
        let cfg = GitHubWebhookConfig {
            secret: "s".into(),
            subscriptions: vec![GitHubWebhookSubscription {
                repo: "kgatilin/archai".into(),
                events: vec!["pull_request.closed".into()],
                deliver_to: "agent:kira".into(),
            }],
        };
        assert_eq!(
            matching_targets(&cfg, "kgatilin/archai", "pull_request", Some("closed")),
            vec!["agent:kira"]
        );
        assert!(
            matching_targets(&cfg, "kgatilin/archai", "pull_request", Some("opened")).is_empty()
        );
        assert!(matching_targets(&cfg, "other/repo", "pull_request", Some("closed")).is_empty());
    }

    #[test]
    fn matching_targets_bare_event() {
        let cfg = GitHubWebhookConfig {
            secret: "s".into(),
            subscriptions: vec![GitHubWebhookSubscription {
                repo: "kgatilin/archai".into(),
                events: vec!["ping".into()],
                deliver_to: "agent:kira".into(),
            }],
        };
        assert_eq!(
            matching_targets(&cfg, "kgatilin/archai", "ping", None),
            vec!["agent:kira"]
        );
    }

    #[test]
    fn matching_targets_fan_out() {
        let cfg = GitHubWebhookConfig {
            secret: "s".into(),
            subscriptions: vec![
                GitHubWebhookSubscription {
                    repo: "kgatilin/archai".into(),
                    events: vec!["pull_request.closed".into()],
                    deliver_to: "agent:kira".into(),
                },
                GitHubWebhookSubscription {
                    repo: "kgatilin/archai".into(),
                    events: vec!["pull_request.closed".into()],
                    deliver_to: "agent:dev".into(),
                },
            ],
        };
        assert_eq!(
            matching_targets(&cfg, "kgatilin/archai", "pull_request", Some("closed")),
            vec!["agent:kira", "agent:dev"]
        );
    }

    #[test]
    fn dedupe_blocks_repeat_within_ttl() {
        let mut d = DeliveryDedupe::new();
        assert!(d.check_and_record("abc", 1000));
        assert!(!d.check_and_record("abc", 1100));
    }

    #[test]
    fn dedupe_permits_after_ttl() {
        let mut d = DeliveryDedupe::new();
        assert!(d.check_and_record("abc", 1000));
        assert!(d.check_and_record("abc", 1000 + DELIVERY_TTL_SECS + 1));
    }

    #[test]
    fn build_summary_pr_closed() {
        let payload = serde_json::json!({
            "pull_request": {
                "number": 42,
                "title": "fix: thing",
                "html_url": "https://github.com/x/y/pull/42",
            }
        });
        let s = build_summary("x/y", "pull_request", Some("closed"), &payload);
        assert!(s.contains("pull_request.closed"));
        assert!(s.contains("#42"));
        assert!(s.contains("fix: thing"));
    }

    #[test]
    fn build_metadata_extracts_fields() {
        let payload = serde_json::json!({
            "sender": {"login": "alice"},
            "pull_request": {
                "number": 7,
                "title": "t",
                "state": "closed",
                "merged": true,
                "html_url": "u",
                "user": {"login": "bob"},
            },
        });
        let m = build_metadata("x/y", "pull_request", Some("closed"), "d-1", &payload);
        assert_eq!(m["event"], "pull_request");
        assert_eq!(m["action"], "closed");
        assert_eq!(m["repo"], "x/y");
        assert_eq!(m["delivery_id"], "d-1");
        assert_eq!(m["pull_request"]["number"], 7);
        assert_eq!(m["pull_request"]["merged"], true);
        assert_eq!(m["sender"], "alice");
    }
}
