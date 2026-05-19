//! Append-only JSONL audit log for the web adapter (#443).
//!
//! One line per auth event:
//!
//! ```json
//! {"ts":"2026-05-09T12:34:56Z","event":"login_request",
//!  "telegram_id":123,"ip":"127.0.0.1","ua":"Mozilla/5.0","ok":true}
//! ```
//!
//! Writes are best-effort: a write failure is logged via diag but never
//! propagates back to the auth handler — losing an audit line should not
//! cause user-facing 500s.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// All event kinds the web adapter records to the audit log.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// Magic link requested (will only succeed if `ok=true`).
    LoginRequest,
    /// Magic link consumed successfully.
    LoginConsume,
    /// Login failed (rate-limit, non-whitelisted id, expired/invalid token).
    LoginFailed,
    /// Session cookie cleared.
    Logout,
    /// Operator hit the per-agent Restart button (#445). Audited regardless
    /// of whether the runtime acted on it; the request is the auditable
    /// event.
    AgentRestart,
    /// Operator hit the per-agent Force compact button (#445).
    AgentCompact,
    /// Operator hit the per-agent Stop button (#445).
    AgentStop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub ts: String,
    pub event: Event,
    pub telegram_id: Option<i64>,
    pub ip: String,
    pub ua: String,
    pub ok: bool,
    /// Free-form reason on failure ("rate_limited", "not_whitelisted",
    /// "token_expired", etc.). `None` for successful events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Agent the action targeted (#445). `None` for auth events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// Async-safe audit log writer. Cheap to clone; multiple handlers share the
/// same underlying mutex, so concurrent writes are serialized into a single
/// `append` syscall per JSONL line — never interleaved.
#[derive(Clone)]
pub struct AuditLog {
    path: Arc<PathBuf>,
    lock: Arc<Mutex<()>>,
}

impl AuditLog {
    /// Create a writer that will append to `path`. The parent directory is
    /// created on first write; we do not pre-create it so an unconfigured
    /// install never touches the filesystem until an event actually fires.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Path the writer appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a single JSONL line, fsyncing before returning so the line is
    /// durable across crashes. Uses a tokio mutex to serialize writes, so
    /// concurrent calls do not interleave bytes.
    pub async fn append(&self, entry: AuditEntry) -> std::io::Result<()> {
        let _g = self.lock.lock().await;

        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut line = serde_json::to_string(&entry).unwrap_or_else(|_| "{}".into());
        line.push('\n');

        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*self.path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.flush().await?;
        // Best-effort fsync — a missing fsync syscall on some filesystems is
        // not a hard failure for our durability goals.
        let _ = f.sync_all().await;
        Ok(())
    }
}

/// Resolve `~` in audit log paths against `$HOME`. Mirrors the convention
/// used elsewhere in the codebase for ergonomic config paths.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn append_writes_one_jsonl_line_per_event() {
        let dir = tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));

        log.append(AuditEntry {
            ts: "2026-05-09T00:00:00Z".into(),
            event: Event::LoginRequest,
            telegram_id: Some(1),
            ip: "127.0.0.1".into(),
            ua: "test".into(),
            ok: true,
            reason: None,
            agent: None,
        })
        .await
        .unwrap();

        log.append(AuditEntry {
            ts: "2026-05-09T00:00:01Z".into(),
            event: Event::LoginConsume,
            telegram_id: Some(1),
            ip: "127.0.0.1".into(),
            ua: "test".into(),
            ok: true,
            reason: None,
            agent: None,
        })
        .await
        .unwrap();

        let contents = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        let l0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(l0["event"], "login_request");
        assert_eq!(l0["telegram_id"], 1);
        assert_eq!(l0["ok"], true);
        let l1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l1["event"], "login_consume");
    }

    #[tokio::test]
    async fn append_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("nested").join("logs").join("audit.jsonl");
        let log = AuditLog::new(log_path.clone());
        log.append(AuditEntry {
            ts: "2026".into(),
            event: Event::Logout,
            telegram_id: None,
            ip: "::1".into(),
            ua: "".into(),
            ok: true,
            reason: None,
            agent: None,
        })
        .await
        .unwrap();
        assert!(log_path.exists());
    }

    #[test]
    fn expand_home_resolves_tilde() {
        // Serialize env mutation; setenv is not thread-safe on POSIX.
        let _g = crate::test_support::env_lock().blocking_lock();
        let prev = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "/tmp/fake-home-test");
        }
        let p = expand_home("~/.deskd/logs/web-audit.jsonl");
        // Restore HOME so concurrent tests that also rely on it see the
        // original process value.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert_eq!(
            p.to_string_lossy(),
            "/tmp/fake-home-test/.deskd/logs/web-audit.jsonl"
        );
    }

    #[test]
    fn expand_home_passes_through_absolute() {
        let p = expand_home("/var/log/deskd/audit.jsonl");
        assert_eq!(p.to_string_lossy(), "/var/log/deskd/audit.jsonl");
    }

    #[tokio::test]
    async fn failed_event_serializes_reason() {
        let dir = tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        log.append(AuditEntry {
            ts: "2026".into(),
            event: Event::LoginFailed,
            telegram_id: Some(99),
            ip: "127.0.0.1".into(),
            ua: "x".into(),
            ok: false,
            reason: Some("not_whitelisted".into()),
            agent: None,
        })
        .await
        .unwrap();
        let l = std::fs::read_to_string(log.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(l.trim()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["reason"], "not_whitelisted");
    }
}
