//! Data collection for the dashboard (#444).
//!
//! Aggregates an [`AgentSummary`] for every known agent from the same
//! sources the rest of deskd uses:
//!
//! - `crate::app::agent::list()` — all known agents (top-level + sub-agents).
//! - `crate::app::context_size::gather()` — current session context tokens
//!   and the resolved auto-compact threshold per live agent.
//! - `crate::app::tasklog::read_logs(name, …)` — last task entry's `ts`,
//!   used as a "last activity" approximation.
//!
//! Anything that isn't available today renders as `None` and the view layer
//! draws an em-dash. In particular, home-directory disk metrics belong to
//! the as-yet-unmerged #446 collector; we leave `home_dir_bytes = None` and
//! cite it in the PR body.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::app::agent;
use crate::app::context_size;
use crate::app::tasklog::{self, TaskLog};

/// Snapshot of a single agent rendered as one card on the dashboard.
///
/// Field semantics mirror the issue spec. `Option<_>` is used wherever a
/// value can be genuinely missing (no log entry yet, sub-agent without
/// context tokens, missing #446 disk cache) so the renderer can show an
/// em-dash instead of a misleading zero.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentSummary {
    pub name: String,
    /// Coarse status string: "idle", "working", or "offline".
    pub status: String,
    /// Model id (e.g. `claude-sonnet-4-6`).
    pub model: String,
    /// RFC 3339 timestamp of the most recent task log entry, if any.
    pub last_activity: Option<DateTime<Utc>>,
    /// Tokens currently pinned in the session context window.
    pub context_tokens: Option<u64>,
    /// Auto-compact threshold (per the resolved agent config).
    pub context_threshold: Option<u64>,
    /// Home-directory size from the #446 disk cache. `None` until that lands.
    pub home_dir_bytes: Option<u64>,
    /// Truncated text of the task currently being processed, if any.
    pub current_task: Option<String>,
    /// Duration the current task has been running, if `status == working`
    /// and a session start time is recorded.
    pub task_running_for: Option<Duration>,
}

/// VPS-level overview shown above the agent list.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VpsOverview {
    /// `CARGO_PKG_VERSION` of this binary.
    pub deskd_version: String,
    /// Process uptime since the dashboard handler first ran. The web adapter
    /// doesn't track its own start time today — we display deskd's uptime as
    /// best-effort from `/proc/self/stat` on Linux, otherwise `None`.
    pub uptime: Option<Duration>,
    /// Total disk bytes (#446 cache); `None` until that lands.
    pub disk_total_bytes: Option<u64>,
    /// Free disk bytes (#446 cache); `None` until that lands.
    pub disk_free_bytes: Option<u64>,
}

/// Per-agent detail page payload (#445).
///
/// Strictly an additive wrapper around [`AgentSummary`]: every dashboard
/// field is preserved, plus the extra fields the issue spec demands
/// (session id, home directory path, compaction strategy label, recent
/// tasklog entries).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AgentDetail {
    pub summary: AgentSummary,
    /// Session id (short — first 8 chars). Empty when no session has started.
    pub session_id: String,
    /// Absolute path to the agent's working directory.
    pub home_dir: String,
    /// Human-friendly compaction strategy label (e.g. "auto @ 80%", "manual").
    pub compaction_strategy: String,
    /// Up-to-N most recent tasklog entries, newest first.
    pub recent_tasks: Vec<TaskLogRow>,
    /// True when the agent has an in-flight task — used to block destructive
    /// actions (`restart` / `stop` / `compact`) per the «Agent busy» AC.
    pub busy: bool,
}

/// One row in the «Last tasks» list.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskLogRow {
    /// Status string from the tasklog ("ok", "error", "skip", "empty").
    pub status: String,
    /// RFC 3339 timestamp.
    pub ts: String,
    /// Truncated task summary text (first 60 chars per tasklog convention).
    pub summary: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Default maximum number of tasklog rows surfaced on the detail page.
pub const DETAIL_TASKLOG_LIMIT: usize = 10;

/// Look up a single agent's detail payload by name. Returns `None` when no
/// agent state file exists for that name (the caller turns this into a 404).
pub async fn collect_agent_detail(name: &str) -> Option<AgentDetail> {
    let states = crate::app::agent::list().await.unwrap_or_default();
    let state = states.into_iter().find(|s| s.config.name == name)?;

    let context_by_agent: HashMap<String, context_size::SessionContext> = context_size::gather()
        .await
        .map(snapshot_to_map)
        .unwrap_or_default();
    let ctx = context_by_agent.get(&state.config.name);

    let summary = summarise(&state, ctx);
    let busy = summary.status == "working";
    let session_short = if state.session_id.is_empty() {
        String::new()
    } else {
        state.session_id.chars().take(8).collect::<String>()
    };
    let compaction_strategy = compaction_strategy_label(&state.config);
    let recent_tasks = recent_tasklog_rows(&state.config.name, DETAIL_TASKLOG_LIMIT);

    Some(AgentDetail {
        summary,
        session_id: session_short,
        home_dir: state.config.work_dir.clone(),
        compaction_strategy,
        recent_tasks,
        busy,
    })
}

/// Human-readable compaction strategy label. Falls back to "auto" when no
/// explicit override is configured — matches the runtime default.
fn compaction_strategy_label(cfg: &crate::app::agent::AgentConfig) -> String {
    if let Some(threshold) = cfg.auto_compact_threshold_tokens {
        return format!("auto @ {}k tokens", threshold / 1_000);
    }
    if let Some(fraction) = cfg.compact_threshold {
        let pct = (fraction * 100.0).round() as u64;
        return format!("auto @ {}%", pct);
    }
    "auto (default)".to_string()
}

fn recent_tasklog_rows(agent_name: &str, limit: usize) -> Vec<TaskLogRow> {
    let mut entries = match tasklog::read_logs(agent_name, limit, None, None) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // tasklog::read_logs returns oldest→newest; flip so the most recent
    // row is rendered first.
    entries.reverse();
    entries
        .into_iter()
        .map(|t: TaskLog| TaskLogRow {
            status: t.status,
            ts: t.ts,
            summary: t.task,
            duration_ms: t.duration_ms,
        })
        .collect()
}

/// Build [`AgentSummary`] entries for every known agent. Errors from the
/// underlying data sources are logged-and-swallowed so a single broken file
/// can't take down the dashboard; the failed entries fall back to defaults.
pub async fn collect_agent_summaries() -> Vec<AgentSummary> {
    let states = agent::list().await.unwrap_or_default();
    let context_by_agent: HashMap<String, context_size::SessionContext> = context_size::gather()
        .await
        .map(snapshot_to_map)
        .unwrap_or_default();

    let mut out = Vec::with_capacity(states.len());
    for state in &states {
        out.push(summarise(state, context_by_agent.get(&state.config.name)));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn snapshot_to_map(
    snapshot: Vec<context_size::SessionContext>,
) -> HashMap<String, context_size::SessionContext> {
    let mut m = HashMap::new();
    for s in snapshot {
        m.insert(s.agent.clone(), s);
    }
    m
}

fn summarise(
    state: &agent::AgentState,
    ctx: Option<&context_size::SessionContext>,
) -> AgentSummary {
    let last_activity = latest_task_ts(&state.config.name);
    let (context_tokens, context_threshold) = match ctx {
        Some(c) => (c.context_tokens, Some(c.auto_compact_threshold)),
        None => (None, None),
    };
    let current_task = if state.current_task.is_empty() {
        None
    } else {
        Some(state.current_task.clone())
    };
    let task_running_for = if state.status == "working" {
        state
            .session_start
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .and_then(|start| (Utc::now() - start).to_std().ok())
    } else {
        None
    };

    AgentSummary {
        name: state.config.name.clone(),
        status: status_string(state),
        model: state.config.model.clone(),
        last_activity,
        context_tokens,
        context_threshold,
        home_dir_bytes: None, // #446 disk cache not yet wired.
        current_task,
        task_running_for,
    }
}

/// Coarse status: top-level agents with `pid == 0` (`deskd serve` doesn't
/// record a PID for them) are reported by `state.status`. Sub-agents with a
/// PID are reported as offline when the process is gone, otherwise pass the
/// stored status through.
fn status_string(state: &agent::AgentState) -> String {
    if state.pid > 0 && !std::path::Path::new(&format!("/proc/{}", state.pid)).exists() {
        return "offline".to_string();
    }
    state.status.clone()
}

fn latest_task_ts(agent_name: &str) -> Option<DateTime<Utc>> {
    let entries = tasklog::read_logs(agent_name, 1, None, None).ok()?;
    let last = entries.into_iter().next_back()?;
    DateTime::parse_from_rfc3339(&last.ts)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Build the VPS overview strip. Only deskd version is universally known
/// today; uptime is best-effort and disk metrics defer to #446.
pub fn collect_vps_overview() -> VpsOverview {
    VpsOverview {
        deskd_version: env!("CARGO_PKG_VERSION").to_string(),
        uptime: process_uptime(),
        disk_total_bytes: None,
        disk_free_bytes: None,
    }
}

/// Best-effort process uptime from `/proc/self/stat` (Linux only). Returns
/// `None` on other platforms or any parse failure.
fn process_uptime() -> Option<Duration> {
    #[cfg(target_os = "linux")]
    {
        // /proc/self/stat field 22 is starttime in clock ticks since boot.
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // The 2nd field can contain spaces inside parentheses; skip past it.
        let close = stat.rfind(')')?;
        let after = &stat[close + 1..];
        let fields: Vec<&str> = after.split_whitespace().collect();
        // After the closing paren, field 3 in the doc is at index 0 here.
        // starttime is doc-field 22 → index 19 here.
        let starttime: u64 = fields.get(19)?.parse().ok()?;

        let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
        let uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

        // Clock ticks per second; assume 100 (USER_HZ) — the canonical Linux
        // value. Reading SC_CLK_TCK at runtime would require libc; the
        // approximation is fine for a dashboard.
        let ticks_per_sec = 100u64;
        let start_secs = starttime as f64 / ticks_per_sec as f64;
        let elapsed = uptime_secs - start_secs;
        if elapsed < 0.0 {
            return None;
        }
        Some(Duration::from_secs(elapsed as u64))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent_registry::{AgentConfig, AgentState};
    use crate::domain::config_types::{ConfigAgentKind, ConfigAgentRuntime, ConfigSessionMode};

    fn mk_state(name: &str, status: &str, pid: u32) -> AgentState {
        AgentState {
            config: AgentConfig {
                name: name.into(),
                model: "claude-opus-4-7".into(),
                system_prompt: String::new(),
                work_dir: "/tmp".into(),
                max_turns: 100,
                unix_user: None,
                budget_usd: 50.0,
                command: vec!["claude".into()],
                config_path: None,
                container: None,
                session: ConfigSessionMode::default(),
                runtime: ConfigAgentRuntime::default(),
                kind: ConfigAgentKind::default(),
                context: None,
                compact_threshold: None,
                auto_compact_threshold_tokens: None,
                empty_completion_threshold: None,
                empty_completion_restart_min_secs: None,
            },
            pid,
            session_id: "abcdef0123".into(),
            total_turns: 0,
            total_cost: 0.0,
            created_at: Utc::now().to_rfc3339(),
            status: status.into(),
            current_task: String::new(),
            parent: None,
            scope: None,
            can_message: None,
            env_keys: None,
            session_start: None,
            session_cost: 0.0,
            session_turns: 0,
            consecutive_empty_completions: 0,
            last_empty_restart_at: None,
            total_empty_restarts: 0,
        }
    }

    #[test]
    fn status_string_reports_offline_when_pid_dead() {
        // u32::MAX-1 is implausibly high — guaranteed not to exist.
        let state = mk_state("a", "idle", u32::MAX - 1);
        assert_eq!(status_string(&state), "offline");
    }

    #[test]
    fn status_string_passes_through_for_top_level_agents() {
        // Top-level agents have pid=0 → use stored status verbatim.
        let mut state = mk_state("a", "working", 0);
        state.current_task = "review PR".into();
        assert_eq!(status_string(&state), "working");
    }

    #[test]
    fn summarise_fills_running_for_when_working() {
        let mut state = mk_state("a", "working", 0);
        state.session_start = Some((Utc::now() - chrono::Duration::seconds(30)).to_rfc3339());
        state.current_task = "review PR #441".into();
        let s = summarise(&state, None);
        assert_eq!(s.status, "working");
        assert_eq!(s.current_task.as_deref(), Some("review PR #441"));
        let dur = s.task_running_for.unwrap();
        assert!(dur.as_secs() >= 25 && dur.as_secs() <= 60);
    }

    #[test]
    fn summarise_omits_running_for_when_idle() {
        let state = mk_state("a", "idle", 0);
        let s = summarise(&state, None);
        assert!(s.task_running_for.is_none());
        assert!(s.current_task.is_none());
    }

    #[test]
    fn summarise_pulls_context_from_provided_snapshot() {
        let state = mk_state("a", "idle", 0);
        let ctx = context_size::SessionContext {
            agent: "a".into(),
            model: "claude-opus-4-7".into(),
            session_id: "abcdef01".into(),
            context_tokens: Some(120_000),
            context_limit: 1_000_000,
            auto_compact_threshold: 300_000,
            stale: false,
        };
        let s = summarise(&state, Some(&ctx));
        assert_eq!(s.context_tokens, Some(120_000));
        assert_eq!(s.context_threshold, Some(300_000));
    }

    #[test]
    fn vps_overview_reports_pkg_version() {
        let v = collect_vps_overview();
        assert_eq!(v.deskd_version, env!("CARGO_PKG_VERSION"));
    }
}
