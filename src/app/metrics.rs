//! Disk metrics collector + cache (#446).
//!
//! Background tokio task — independent of the web adapter, so disabling
//! `web:` does not disable metrics. Samples disk usage periodically and
//! publishes a `metrics.updated` bus event when a fresh snapshot is ready.
//!
//! ### Data model
//!
//! - [`VolumeSample`]: one entry per `metrics.disk.volumes` mountpoint.
//!   Populated from `df -BK --output=source,size,used,avail <mount>`.
//! - [`AgentDiskSample`]: per-agent home-directory bytes.
//!   Populated from `du -sh <agent_home>` (binary `du -sk` is the actual
//!   command — the `-h` flag is only used for the per-agent breakdown
//!   surface).
//! - [`AgentBreakdownEntry`]: one entry per top-level child of an agent
//!   home directory. Populated lazily for the `/agent/<name>` page via
//!   `du -k --max-depth=1`.
//!
//! ### Cache
//!
//! In-memory `Arc<RwLock<DiskSnapshot>>` for handlers + persisted to
//! `~/.deskd/cache/disk.json` so a restart shows stale-but-useful values
//! while the first fresh sample runs.
//!
//! ### Failure handling
//!
//! - Missing paths / permission-denied / non-zero exit → recorded as
//!   `null` in the snapshot. Logged once at WARN per path (see
//!   [`WarnedPaths`]) to avoid log spam from a misconfigured agent.
//! - The collector loop never panics. A failed sample is logged and the
//!   loop continues to the next tick.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

use crate::app::bus;
use crate::config::{DiskMetricsConfig, WorkspaceConfig};
use crate::ports::bus::MessageBus;

/// One sampled volume.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct VolumeSample {
    /// Configured mount point that was passed to `df`.
    pub mount: String,
    /// First column of `df -BK` (e.g. `/dev/vda1`). `None` when the sample
    /// failed.
    pub source: Option<String>,
    /// Total bytes (column 2). `None` on failure.
    pub size_bytes: Option<u64>,
    /// Used bytes (column 3). `None` on failure.
    pub used_bytes: Option<u64>,
    /// Available bytes (column 4). `None` on failure.
    pub avail_bytes: Option<u64>,
}

/// Per-agent home-directory sample.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct AgentDiskSample {
    pub agent: String,
    pub home_dir: String,
    /// `du -sk` reported size in bytes. `None` if the path is missing or
    /// `du` failed.
    pub home_dir_bytes: Option<u64>,
}

/// A single subdirectory size entry (used for `/agent/<name>` breakdown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentBreakdownEntry {
    /// Display name (last component of the path). The full path is kept
    /// out of the UI to avoid leaking absolute paths.
    pub name: String,
    pub bytes: u64,
}

/// Aggregate disk snapshot — everything the UI / API needs.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct DiskSnapshot {
    /// Timestamp when the snapshot was assembled.
    pub updated_at: Option<DateTime<Utc>>,
    pub volumes: Vec<VolumeSample>,
    pub agents: Vec<AgentDiskSample>,
}

impl DiskSnapshot {
    pub fn lookup_agent(&self, name: &str) -> Option<&AgentDiskSample> {
        self.agents.iter().find(|a| a.agent == name)
    }
}

/// In-process disk-metrics state. Cheap to clone (just `Arc`s).
#[derive(Clone)]
pub struct DiskMetrics {
    snapshot: Arc<RwLock<DiskSnapshot>>,
    last_refresh: Arc<Mutex<Option<std::time::Instant>>>,
    warned: Arc<Mutex<WarnedPaths>>,
    cache_path: PathBuf,
}

/// Per-path "warning emitted" set. Keeps the log readable when a path is
/// missing for several sample cycles in a row.
#[derive(Debug, Default)]
pub struct WarnedPaths {
    paths: HashSet<String>,
}

impl WarnedPaths {
    /// Emit a WARN tracing event for `path` only the first time it's
    /// observed; subsequent calls for the same path are silent.
    pub fn warn_once(&mut self, path: &str, msg: &str) {
        if self.paths.insert(path.to_string()) {
            tracing::warn!(path = %path, "{}", msg);
        }
    }
}

impl DiskMetrics {
    pub fn new(cache_path: PathBuf) -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(DiskSnapshot::default())),
            last_refresh: Arc::new(Mutex::new(None)),
            warned: Arc::new(Mutex::new(WarnedPaths::default())),
            cache_path,
        }
    }

    /// Return a snapshot clone. Tests assert against this directly.
    pub async fn snapshot(&self) -> DiskSnapshot {
        self.snapshot.read().await.clone()
    }

    /// Replace the in-memory snapshot. Used by tests + on warm-start.
    pub async fn store(&self, snap: DiskSnapshot) {
        *self.snapshot.write().await = snap;
    }

    /// Cache file path (`~/.deskd/cache/disk.json` in production).
    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }
}

/// Default cache file under `$HOME/.deskd/cache/disk.json`.
pub fn default_cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".deskd")
        .join("cache")
        .join("disk.json")
}

/// Read a previously-persisted snapshot from disk. Missing / unreadable /
/// malformed → `Ok(None)` (warm-start is best effort).
pub fn load_cache(path: &Path) -> Result<Option<DiskSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<DiskSnapshot>(&s) {
            Ok(snap) => Ok(Some(snap)),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "disk cache parse failed; ignoring");
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Persist a snapshot atomically (write to `<path>.tmp` + rename).
pub fn save_cache(path: &Path, snap: &DiskSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(snap)?;
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Parse one row of `df -BK --output=source,size,used,avail`. Returns
/// `None` if the row is malformed.
///
/// `df -BK` emits sizes as `<n>K` (no space). We strip the trailing `K`
/// and multiply by 1024 to get bytes.
pub fn parse_df_row(line: &str) -> Option<(String, u64, u64, u64)> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    let source = parts[0].to_string();
    let size = parse_k_field(parts[1])?;
    let used = parse_k_field(parts[2])?;
    let avail = parse_k_field(parts[3])?;
    Some((source, size, used, avail))
}

fn parse_k_field(s: &str) -> Option<u64> {
    let digits = s.trim_end_matches('K');
    let n: u64 = digits.parse().ok()?;
    Some(n.saturating_mul(1024))
}

/// Parse `du -sk` output. Format: `<kbytes>\t<path>`. Returns bytes.
pub fn parse_du_sk(stdout: &str) -> Option<u64> {
    let first = stdout.lines().next()?;
    let mut iter = first.split_whitespace();
    let k: u64 = iter.next()?.parse().ok()?;
    Some(k.saturating_mul(1024))
}

/// Parse `du -k --max-depth=1 <dir>` output. Skips the summary line for
/// the dir itself (last line whose path equals `root`). Returns sorted
/// entries (largest first) capped at `limit`.
pub fn parse_du_breakdown(stdout: &str, root: &str, limit: usize) -> Vec<AgentBreakdownEntry> {
    let mut out = Vec::new();
    let root_trim = root.trim_end_matches('/');
    for line in stdout.lines() {
        let mut iter = line.split_whitespace();
        let Some(k_str) = iter.next() else { continue };
        let Some(path) = iter.next() else { continue };
        let trimmed = path.trim_end_matches('/');
        if trimmed == root_trim {
            continue;
        }
        let Ok(k) = k_str.parse::<u64>() else {
            continue;
        };
        // last path component as display name
        let name = std::path::Path::new(trimmed)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| trimmed.to_string());
        out.push(AgentBreakdownEntry {
            name,
            bytes: k.saturating_mul(1024),
        });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    out.truncate(limit);
    out
}

/// Sample one volume by invoking `df`. Errors are absorbed and recorded
/// as `None` in the result.
pub async fn sample_volume(mount: &str, warned: &Arc<Mutex<WarnedPaths>>) -> VolumeSample {
    let out = match Command::new("df")
        .args(["-BK", "--output=source,size,used,avail", mount])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            warned
                .lock()
                .await
                .warn_once(mount, &format!("df spawn failed: {}", e));
            return VolumeSample {
                mount: mount.to_string(),
                ..Default::default()
            };
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        warned
            .lock()
            .await
            .warn_once(mount, &format!("df failed: {}", stderr));
        return VolumeSample {
            mount: mount.to_string(),
            ..Default::default()
        };
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // First line is the header; second is the data.
    let row = stdout.lines().nth(1);
    let Some(row) = row else {
        warned
            .lock()
            .await
            .warn_once(mount, "df returned no data row");
        return VolumeSample {
            mount: mount.to_string(),
            ..Default::default()
        };
    };
    let Some((source, size, used, avail)) = parse_df_row(row) else {
        warned.lock().await.warn_once(mount, "df row malformed");
        return VolumeSample {
            mount: mount.to_string(),
            ..Default::default()
        };
    };
    VolumeSample {
        mount: mount.to_string(),
        source: Some(source),
        size_bytes: Some(size),
        used_bytes: Some(used),
        avail_bytes: Some(avail),
    }
}

/// Sample one agent's home directory via `du -sk`.
pub async fn sample_agent_home(
    agent: &str,
    home_dir: &str,
    warned: &Arc<Mutex<WarnedPaths>>,
) -> AgentDiskSample {
    if !Path::new(home_dir).exists() {
        warned
            .lock()
            .await
            .warn_once(home_dir, "agent home dir does not exist");
        return AgentDiskSample {
            agent: agent.to_string(),
            home_dir: home_dir.to_string(),
            home_dir_bytes: None,
        };
    }
    let out = match Command::new("du").args(["-sk", home_dir]).output().await {
        Ok(out) => out,
        Err(e) => {
            warned
                .lock()
                .await
                .warn_once(home_dir, &format!("du spawn failed: {}", e));
            return AgentDiskSample {
                agent: agent.to_string(),
                home_dir: home_dir.to_string(),
                home_dir_bytes: None,
            };
        }
    };
    // `du` exits non-zero when some entries are unreadable but still prints
    // a partial size for the rest. Treat any parseable first-line size as
    // success.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let bytes = parse_du_sk(&stdout);
    if bytes.is_none() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        warned.lock().await.warn_once(
            home_dir,
            &format!("du -sk produced no parsable output: {}", stderr),
        );
    }
    AgentDiskSample {
        agent: agent.to_string(),
        home_dir: home_dir.to_string(),
        home_dir_bytes: bytes,
    }
}

/// Sample the top-5 subdirectory breakdown for an agent home. Returns an
/// empty vec on any failure — the UI just shows "no data".
pub async fn sample_agent_breakdown(home_dir: &str, limit: usize) -> Vec<AgentBreakdownEntry> {
    if !Path::new(home_dir).exists() {
        return Vec::new();
    }
    let out = match Command::new("du")
        .args(["-k", "--max-depth=1", home_dir])
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(path = %home_dir, error = %e, "du --max-depth=1 spawn failed");
            return Vec::new();
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_du_breakdown(&stdout, home_dir, limit)
}

/// Configuration for the collector. `agents` is a flat list of
/// `(name, home_dir)` pairs — resolved once at startup so the collector
/// does not have to repeatedly read the workspace config.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub interval: Duration,
    pub volumes: Vec<String>,
    pub agents: Vec<(String, String)>,
}

impl CollectorConfig {
    /// Resolve from a parsed workspace config.
    pub fn from_workspace(ws: &WorkspaceConfig) -> Self {
        let disk = ws
            .metrics
            .as_ref()
            .map(|m| m.disk.clone())
            .unwrap_or_default();
        let agents: Vec<(String, String)> = ws
            .agents
            .iter()
            .map(|a| (a.name.clone(), a.work_dir.clone()))
            .collect();
        Self {
            interval: Duration::from_secs(disk.interval_seconds),
            volumes: disk.volumes,
            agents,
        }
    }
}

impl From<DiskMetricsConfig> for Duration {
    fn from(cfg: DiskMetricsConfig) -> Self {
        Duration::from_secs(cfg.interval_seconds)
    }
}

/// Run a single sample pass: sample every configured volume + every agent
/// home, then publish the result into the cache and emit a
/// `metrics.updated` bus event (if a bus socket is supplied).
pub async fn run_sample(
    metrics: &DiskMetrics,
    cfg: &CollectorConfig,
    bus_socket: Option<&str>,
) -> DiskSnapshot {
    let mut volumes = Vec::with_capacity(cfg.volumes.len());
    for v in &cfg.volumes {
        volumes.push(sample_volume(v, &metrics.warned).await);
    }
    let mut agents = Vec::with_capacity(cfg.agents.len());
    for (name, home) in &cfg.agents {
        agents.push(sample_agent_home(name, home, &metrics.warned).await);
    }
    let snap = DiskSnapshot {
        updated_at: Some(Utc::now()),
        volumes,
        agents,
    };
    metrics.store(snap.clone()).await;

    // Persist — failure is non-fatal.
    if let Err(e) = save_cache(metrics.cache_path(), &snap) {
        tracing::warn!(error = %e, "persist disk cache failed");
    }

    // Emit bus event.
    if let Some(socket) = bus_socket
        && let Err(e) = publish_metrics_updated(socket, &snap).await
    {
        tracing::warn!(error = %e, "publish metrics.updated failed");
    }

    snap
}

/// Publish a `metrics.updated` event to the bus. The payload carries the
/// fresh snapshot so subscribers (the web SSE pump) can swap UI without
/// re-reading the cache.
pub async fn publish_metrics_updated(socket: &str, snap: &DiskSnapshot) -> Result<()> {
    let bus = bus::connect_bus(socket).await?;
    bus.register("metrics", &[]).await?;
    let msg = crate::domain::message::Message {
        id: uuid::Uuid::new_v4().to_string(),
        source: "metrics".to_string(),
        target: "broadcast".to_string(),
        payload: serde_json::json!({
            "type": "metrics.updated",
            "snapshot": snap,
        }),
        reply_to: None,
        metadata: crate::domain::message::Metadata::default(),
    };
    bus.send(&msg).await?;
    Ok(())
}

/// Spawn the periodic sampler loop. Returns immediately; the loop runs
/// until the cancellation token fires.
pub fn spawn_collector(
    metrics: DiskMetrics,
    cfg: CollectorConfig,
    bus_socket: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        // Warm-start: load any cached snapshot before the first fresh
        // sample so handlers don't return an empty struct on a fresh
        // process.
        match load_cache(metrics.cache_path()) {
            Ok(Some(cached)) => {
                metrics.store(cached).await;
                tracing::info!(
                    path = %metrics.cache_path().display(),
                    "loaded disk metrics warm-start cache"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "warm-start cache load failed");
            }
        }

        // First sample fires immediately so we're not stuck on warm-start
        // data for up to `interval` seconds after boot.
        let _ = run_sample(&metrics, &cfg, bus_socket.as_deref()).await;

        let mut ticker = tokio::time::interval(cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately (we already sampled).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let _ = run_sample(&metrics, &cfg, bus_socket.as_deref()).await;
                }
                _ = cancel.cancelled() => {
                    tracing::info!("disk metrics collector shutting down");
                    return;
                }
            }
        }
    });
}

/// Outcome of a `/metrics/refresh` request as far as the rate-limiter is
/// concerned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RefreshGate {
    /// Caller may proceed; the lock is now held by them.
    Allowed,
    /// Too soon since the previous refresh. Body is in seconds.
    TooSoon { retry_after_secs: u64 },
}

/// Check + claim the manual-refresh slot. The rate-limit is 1 request per
/// `min_interval` (default 30s) — a single global slot, not per-IP. v1 is
/// single-user.
pub async fn check_refresh_gate(
    metrics: &DiskMetrics,
    min_interval: Duration,
    now: std::time::Instant,
) -> RefreshGate {
    let mut g = metrics.last_refresh.lock().await;
    if let Some(prev) = *g {
        let elapsed = now.saturating_duration_since(prev);
        if elapsed < min_interval {
            let remaining = min_interval.saturating_sub(elapsed);
            return RefreshGate::TooSoon {
                retry_after_secs: remaining.as_secs().max(1),
            };
        }
    }
    *g = Some(now);
    RefreshGate::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_df_row_extracts_four_columns() {
        // Real `df -BK --output=source,size,used,avail /` line.
        let line = "/dev/vda1  41943040K  20971520K  20971520K";
        let (source, size, used, avail) = parse_df_row(line).unwrap();
        assert_eq!(source, "/dev/vda1");
        assert_eq!(size, 41943040u64 * 1024);
        assert_eq!(used, 20971520u64 * 1024);
        assert_eq!(avail, 20971520u64 * 1024);
    }

    #[test]
    fn parse_df_row_rejects_short_row() {
        assert!(parse_df_row("only two").is_none());
        assert!(parse_df_row("").is_none());
    }

    #[test]
    fn parse_df_row_rejects_non_numeric_field() {
        assert!(parse_df_row("/dev/vda1 not-a-number 100K 200K").is_none());
    }

    #[test]
    fn parse_du_sk_takes_first_line_first_field() {
        let s = "412345\t/home/kira\n";
        assert_eq!(parse_du_sk(s), Some(412345u64 * 1024));
    }

    #[test]
    fn parse_du_sk_handles_empty() {
        assert_eq!(parse_du_sk(""), None);
    }

    #[test]
    fn parse_du_breakdown_sorts_and_caps() {
        let stdout = "\
100\t/home/kira/.cargo
500\t/home/kira/.deskd
50\t/home/kira/notes
20\t/home/kira/bin
9000\t/home/kira/projects
9670\t/home/kira
";
        let out = parse_du_breakdown(stdout, "/home/kira", 3);
        assert_eq!(out.len(), 3);
        // Largest first.
        assert_eq!(out[0].name, "projects");
        assert_eq!(out[0].bytes, 9000 * 1024);
        assert_eq!(out[1].name, ".deskd");
        assert_eq!(out[2].name, ".cargo");
    }

    #[test]
    fn parse_du_breakdown_skips_root_entry() {
        let stdout = "\
500\t/home/kira/foo
9000\t/home/kira
";
        let out = parse_du_breakdown(stdout, "/home/kira", 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "foo");
    }

    #[test]
    fn parse_du_breakdown_handles_trailing_slash_root() {
        let stdout = "\
500\t/home/kira/foo
9000\t/home/kira/
";
        let out = parse_du_breakdown(stdout, "/home/kira/", 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "foo");
    }

    #[tokio::test]
    async fn cache_roundtrip_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.json");
        let snap = DiskSnapshot {
            updated_at: Some(Utc::now()),
            volumes: vec![VolumeSample {
                mount: "/".into(),
                source: Some("/dev/vda1".into()),
                size_bytes: Some(100),
                used_bytes: Some(40),
                avail_bytes: Some(60),
            }],
            agents: vec![AgentDiskSample {
                agent: "kira".into(),
                home_dir: "/home/kira".into(),
                home_dir_bytes: Some(1024 * 1024 * 412),
            }],
        };

        save_cache(&path, &snap).unwrap();
        let loaded = load_cache(&path).unwrap().unwrap();
        assert_eq!(loaded.volumes, snap.volumes);
        assert_eq!(loaded.agents, snap.agents);
        assert_eq!(loaded.updated_at, snap.updated_at);
    }

    #[tokio::test]
    async fn cache_load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.json");
        let loaded = load_cache(&path).unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn cache_load_malformed_returns_none_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.json");
        std::fs::write(&path, "this is not json").unwrap();
        // Per docs: malformed is logged-and-swallowed.
        let loaded = load_cache(&path).unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn refresh_gate_allows_first_call_blocks_immediate_second() {
        let dir = tempfile::tempdir().unwrap();
        let m = DiskMetrics::new(dir.path().join("disk.json"));
        let now = std::time::Instant::now();
        assert_eq!(
            check_refresh_gate(&m, Duration::from_secs(30), now).await,
            RefreshGate::Allowed
        );
        match check_refresh_gate(&m, Duration::from_secs(30), now + Duration::from_secs(1)).await {
            RefreshGate::TooSoon { retry_after_secs } => {
                assert!(retry_after_secs > 0 && retry_after_secs <= 30);
            }
            other => panic!("expected TooSoon, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn refresh_gate_resets_after_min_interval() {
        let dir = tempfile::tempdir().unwrap();
        let m = DiskMetrics::new(dir.path().join("disk.json"));
        let start = std::time::Instant::now();
        let _ = check_refresh_gate(&m, Duration::from_secs(30), start).await;
        let later = start + Duration::from_secs(31);
        assert_eq!(
            check_refresh_gate(&m, Duration::from_secs(30), later).await,
            RefreshGate::Allowed
        );
    }

    #[tokio::test]
    async fn warned_paths_emits_once() {
        let warned = Arc::new(Mutex::new(WarnedPaths::default()));
        // No panic; emits the WARN only on the first call. We can't capture
        // the log output here without a subscriber harness, but exercising
        // both calls verifies the HashSet path.
        warned.lock().await.warn_once("/missing", "msg");
        warned.lock().await.warn_once("/missing", "msg");
        assert!(warned.lock().await.paths.contains("/missing"));
        assert_eq!(warned.lock().await.paths.len(), 1);
    }

    #[tokio::test]
    async fn sample_agent_home_missing_path_records_null() {
        let dir = tempfile::tempdir().unwrap();
        let m = DiskMetrics::new(dir.path().join("disk.json"));
        // Implausible nested path that definitely does not exist.
        let s = sample_agent_home("ghost", "/nope/does-not-exist-xyz", &m.warned).await;
        assert_eq!(s.agent, "ghost");
        assert_eq!(s.home_dir_bytes, None);
    }

    #[tokio::test]
    async fn sample_agent_home_existing_path_returns_some() {
        let dir = tempfile::tempdir().unwrap();
        // Write a small file so `du` reports a non-zero size.
        std::fs::write(dir.path().join("a.txt"), b"hello world").unwrap();
        let m = DiskMetrics::new(dir.path().join(".disk.json"));
        let s = sample_agent_home("k", dir.path().to_str().unwrap(), &m.warned).await;
        // We can't predict the exact bytes (block size + tmp dir overhead),
        // but it must be Some and non-zero.
        assert!(
            s.home_dir_bytes.unwrap_or(0) > 0,
            "expected some du size, got {:?}",
            s
        );
    }

    #[tokio::test]
    async fn run_sample_persists_and_updates_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("disk.json");
        let m = DiskMetrics::new(cache.clone());
        // Empty config: zero volumes, zero agents. Still records a
        // snapshot with updated_at and persists the empty result.
        let cfg = CollectorConfig {
            interval: Duration::from_secs(60),
            volumes: vec![],
            agents: vec![],
        };
        let snap = run_sample(&m, &cfg, None).await;
        assert!(snap.updated_at.is_some());
        assert!(cache.exists(), "cache file written");
        let cur = m.snapshot().await;
        assert_eq!(cur.updated_at, snap.updated_at);
    }
}
