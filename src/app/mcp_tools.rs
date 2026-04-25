//! MCP tool implementations — the `call_*` functions that back each tool.
//!
//! Split from `mcp.rs` (#293). Each function parses tool arguments, delegates
//! to `mcp_service` or performs the action directly, and returns a JSON result.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::app::mcp_service;
use crate::config::UserConfig;

// ─── Embedded bus for sub-agent orchestration ────────────────────────────────

/// Container-local bus for managing sub-agents spawned via `add_persistent_agent`.
/// Lazy-initialized on first use — zero overhead when sub-agents aren't needed.
pub(crate) struct InternalBus {
    /// Socket path for the internal bus.
    pub socket_path: String,
    /// Names of sub-agents registered on this bus.
    pub sub_agents: HashSet<String>,
    /// Handle to the bus server task (aborted on drop).
    pub _bus_handle: tokio::task::JoinHandle<()>,
    /// Worker handles for each sub-agent.
    pub worker_handles: Vec<(String, tokio::task::JoinHandle<()>)>,
}

impl InternalBus {
    /// Start a container-local bus on a temp socket.
    pub async fn start(agent_name: &str) -> Result<Self> {
        let socket_path = format!("/tmp/deskd-{}-internal.sock", agent_name);

        // Remove stale socket if exists.
        std::fs::remove_file(&socket_path).ok();

        let bus_path = socket_path.clone();
        let bus_handle = tokio::spawn(async move {
            if let Err(e) = crate::app::bus::serve(&bus_path).await {
                warn!(error = %e, "internal bus exited");
            }
        });

        // Give the bus a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify the bus is reachable.
        UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("internal bus failed to start at {}", socket_path))?;

        info!(agent = %agent_name, socket = %socket_path, "internal bus started");

        Ok(Self {
            socket_path,
            sub_agents: HashSet::new(),
            _bus_handle: bus_handle,
            worker_handles: Vec::new(),
        })
    }

    /// Check if a target is a sub-agent on this bus.
    pub fn is_sub_agent_target(&self, target: &str) -> bool {
        if let Some(name) = target.strip_prefix("agent:") {
            self.sub_agents.contains(name)
        } else {
            false
        }
    }
}

// ─── Utility functions ───────────────────────────────────────────────────────

/// Simple glob matching: `*` matches any sequence of characters except none.
/// `telegram.out:*` matches `telegram.out:-1234567`.
pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.splitn(2, '*').collect();
    match parts.as_slice() {
        [prefix, suffix] => value.starts_with(prefix) && value.ends_with(suffix),
        _ => pattern == value,
    }
}

/// Build the MCP tool description for `send_message` based on available
/// channels, sub-agents, and telegram routes defined in the user config.
pub(crate) fn build_send_message_description(cfg: &UserConfig, agent_name: &str) -> String {
    let mut lines = vec![
        "Send a message to a target on the bus.".to_string(),
        String::new(),
        "Available targets:".to_string(),
    ];

    for a in &cfg.agents {
        lines.push(format!(
            "  agent:{}  — {} ({}). {}",
            a.name,
            a.name,
            a.model,
            a.system_prompt.lines().next().unwrap_or("")
        ));
    }

    for ch in &cfg.channels {
        lines.push(format!("  {}  — {}", ch.name, ch.description));
    }

    if let Some(tg) = &cfg.telegram {
        for route in &tg.routes {
            lines.push(format!(
                "  telegram.out:{}  — Telegram chat {}",
                route.chat_id, route.chat_id
            ));
        }
    }

    lines.push(String::new());
    lines.push(format!("You are agent '{}'.", agent_name));

    lines.join("\n")
}

// ─── Tool implementations ────────────────────────────────────────────────────

pub(crate) async fn call_send_message(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    let target = args
        .get("target")
        .and_then(|t| t.as_str())
        .context("missing target")?;
    let text = args
        .get("text")
        .and_then(|t| t.as_str())
        .context("missing text")?;
    let fresh = args.get("fresh").and_then(|f| f.as_bool()).unwrap_or(false);

    // Enforce publish allow-list if the calling agent is a sub-agent in config.
    if let Some(cfg) = user_config
        && let Some(sub) = cfg.agents.iter().find(|a| a.name == agent_name)
        && let Some(ref allow) = sub.publish
    {
        let allowed = allow.iter().any(|pattern| glob_match(pattern, target));
        if !allowed {
            bail!("publish to '{}' not allowed by config", target);
        }
    }

    // Enforce can_message ACL if set on the calling agent's config.
    if let Some(cfg) = user_config
        && let Some(sub) = cfg.agents.iter().find(|a| a.name == agent_name)
        && let Some(ref allow) = sub.can_message
    {
        let allowed = allow.iter().any(|pattern| glob_match(pattern, target));
        if !allowed {
            bail!(
                "agent '{}' cannot message '{}' — not in can_message allow-list",
                agent_name,
                target
            );
        }
    }

    // Route to internal bus if target is a sub-agent, otherwise use parent bus.
    let effective_socket = {
        let ibus = internal_bus.lock().await;
        if let Some(ref ib) = *ibus {
            if ib.is_sub_agent_target(target) {
                ib.socket_path.clone()
            } else {
                bus_socket.to_string()
            }
        } else {
            bus_socket.to_string()
        }
    };

    // Connect to bus, register, send message, disconnect.
    let stream = UnixStream::connect(&effective_socket)
        .await
        .with_context(|| format!("failed to connect to bus at {}", effective_socket))?;

    let (read_half, mut write_half) = stream.into_split();

    // Register as the agent.
    let reg_name = format!("{}-mcp-send", agent_name);
    let reg = serde_json::json!({
        "type": "register",
        "name": reg_name,
        "subscriptions": [format!("agent:{}", reg_name)]
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;

    // For agent:* targets, verify the target is connected before sending.
    if let Some(target_agent) = target.strip_prefix("agent:") {
        let list_cmd = serde_json::json!({"type": "list"});
        let mut list_line = serde_json::to_string(&list_cmd)?;
        list_line.push('\n');
        write_half.write_all(list_line.as_bytes()).await?;
        write_half.flush().await?;

        // Read the list response with a short timeout.
        let reader = tokio::io::BufReader::new(read_half);
        let mut lines_reader = reader.lines();
        let list_result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            lines_reader.next_line(),
        )
        .await;

        let agent_connected = match list_result {
            Ok(Ok(Some(ref resp_line))) => {
                if let Ok(resp) = serde_json::from_str::<Value>(resp_line) {
                    resp.get("payload")
                        .and_then(|p| p.get("clients"))
                        .and_then(|c| c.as_array())
                        .map(|clients| {
                            clients
                                .iter()
                                .any(|c| c.as_str().map(|n| n == target_agent).unwrap_or(false))
                        })
                        .unwrap_or(false)
                } else {
                    true // can't parse, assume connected
                }
            }
            _ => true, // timeout or error, proceed optimistically
        };

        if !agent_connected {
            bail!(
                "agent '{}' is not connected to the bus — message would be lost. \
                 Check if the agent is running with `list_agents`.",
                target_agent
            );
        }
    }

    // Build and send the message (same for all target types).
    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": agent_name,
        "target": target,
        "payload": {"task": text},
        "metadata": {"priority": 5u8, "fresh": fresh}
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    write_half.write_all(msg_line.as_bytes()).await?;
    write_half.flush().await?;

    info!(agent = %agent_name, target = %target, bus = %effective_socket, "send_message via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!("Message sent to {}", target)}],
        "isError": false
    }))
}

pub(crate) async fn call_add_persistent_agent(
    args: &Value,
    parent_name: &str,
    _parent_bus_socket: &str,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing name")?;
    let model = args
        .get("model")
        .and_then(|m| m.as_str())
        .context("missing model")?;
    let system_prompt = args
        .get("system_prompt")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let subscribe: Vec<String> = args
        .get("subscribe")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|| vec![format!("agent:{}", name)]);

    // Scope type: "inherit" (default) or "narrow".
    let scope_type = args
        .get("scope")
        .and_then(|s| s.as_str())
        .unwrap_or("inherit");
    let _scope = match scope_type {
        "narrow" => crate::config::ScopeType::Narrow,
        _ => crate::config::ScopeType::Inherit,
    };

    // Get the deskd binary path (we are running as a subprocess of claude, so $0 is deskd).
    let deskd_bin = std::env::var("DESKD_BIN").unwrap_or_else(|_| "deskd".to_string());

    // Work dir: caller-specified or same as parent.
    let parent_work_dir = std::env::var("PWD").unwrap_or_else(|_| "/tmp".to_string());
    let work_dir = args
        .get("work_dir")
        .and_then(|w| w.as_str())
        .map(String::from)
        .unwrap_or_else(|| parent_work_dir.clone());

    // Validate work_dir containment: child must be within parent's scope.
    {
        let child_path = std::path::Path::new(&work_dir)
            .canonicalize()
            .unwrap_or_else(|_| work_dir.clone().into());
        let parent_path = std::path::Path::new(&parent_work_dir)
            .canonicalize()
            .unwrap_or_else(|_| parent_work_dir.clone().into());
        if !child_path.starts_with(&parent_path) {
            bail!(
                "work_dir '{}' is outside parent scope '{}'",
                work_dir,
                parent_work_dir
            );
        }
    }

    // Environment variables: child gets only explicitly passed env, not parent's.
    let child_env: HashMap<String, String> = args
        .get("env")
        .and_then(|e| e.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // can_message: optional allow-list for outgoing messages.
    let can_message: Option<Vec<String>> =
        args.get("can_message")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            });

    // Lazy-init internal bus on first sub-agent creation.
    let bus_socket = {
        let mut ibus_guard = internal_bus.lock().await;
        if ibus_guard.is_none() {
            let ibus = InternalBus::start(parent_name)
                .await
                .with_context(|| "failed to start internal bus for sub-agent orchestration")?;
            *ibus_guard = Some(ibus);
        }
        let ibus = ibus_guard.as_ref().unwrap();
        ibus.socket_path.clone()
    };

    // Create agent state file via `deskd agent create`.
    let create_status = tokio::process::Command::new(&deskd_bin)
        .args([
            "agent",
            "create",
            name,
            "--model",
            model,
            "--prompt",
            system_prompt,
            "--workdir",
            &work_dir,
        ])
        .status()
        .await;

    match create_status {
        Err(e) => {
            anyhow::bail!("failed to create agent '{}': {}", name, e);
        }
        Ok(s) if !s.success() => {
            // Agent may already exist — that's OK for idempotent calls.
            info!(parent = %parent_name, agent = %name, "agent create returned non-zero (may already exist)");
        }
        Ok(_) => {
            info!(parent = %parent_name, agent = %name, model = %model, "agent state created");
        }
    }

    // Set parent + scope fields on the created agent state.
    if let Ok(mut state) = crate::app::agent::load_state(name) {
        state.parent = Some(parent_name.to_string());
        state.scope = Some(scope_type.to_string());
        if let Some(ref cm) = can_message {
            state.can_message = Some(cm.clone());
        }
        if !child_env.is_empty() {
            state.env_keys = Some(child_env.keys().cloned().collect());
        }
        crate::app::agent::save_state_pub(&state).ok();
    }

    // Fail-fast: verify the internal bus is reachable before spawning.
    UnixStream::connect(&bus_socket)
        .await
        .with_context(|| format!("internal bus at {} is not reachable", bus_socket))?;

    // Start the worker as a background process connected to the internal bus.
    let mut run_args = vec![
        "agent".to_string(),
        "run".to_string(),
        name.to_string(),
        "--socket".to_string(),
        bus_socket.clone(),
    ];
    for sub in &subscribe {
        run_args.push("--subscribe".to_string());
        run_args.push(sub.clone());
    }
    let mut cmd = tokio::process::Command::new(&deskd_bin);
    cmd.args(&run_args)
        .env("DESKD_BUS_SOCKET", &bus_socket)
        .env("DESKD_AGENT_NAME", name);
    // Inject child-specific env vars (isolated from parent).
    for (k, v) in &child_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn worker for agent '{}'", name))?;

    // Write the worker PID to the state file so `agent status` can detect it.
    let worker_pid = child.id().unwrap_or(0);
    if worker_pid > 0
        && let Ok(mut state) = crate::app::agent::load_state(name)
    {
        state.pid = worker_pid;
        crate::app::agent::save_state_pub(&state).ok();
        info!(parent = %parent_name, agent = %name, pid = worker_pid, "wrote sub-agent PID to state");
    }

    // Track the sub-agent in the internal bus for routing and cleanup.
    {
        let mut ibus_guard = internal_bus.lock().await;
        if let Some(ref mut ibus) = *ibus_guard {
            ibus.sub_agents.insert(name.to_string());
            let agent_name = name.to_string();
            let handle = tokio::spawn(async move {
                // Wait for the child process — just holds the handle for abort.
                let _ = child.wait().await;
            });
            ibus.worker_handles.push((agent_name, handle));
        }
    }

    info!(parent = %parent_name, agent = %name, bus = %bus_socket, "persistent sub-agent spawned on internal bus");

    // Poll until the worker registers on the bus (up to 30s), so callers can
    // safely send_message immediately after this call returns.
    let agent_name_owned = name.to_string();
    let bus_socket_poll = bus_socket.clone();
    let poll_timeout = std::time::Duration::from_secs(30);
    let poll_interval = tokio::time::Duration::from_millis(200);
    let registered = tokio::time::timeout(poll_timeout, async {
        loop {
            if let Ok(clients) = crate::app::serve::query_live_agents(&bus_socket_poll).await
                && clients.contains(&agent_name_owned)
            {
                break true;
            }
            tokio::time::sleep(poll_interval).await;
        }
    })
    .await
    .unwrap_or(false);

    if registered {
        info!(parent = %parent_name, agent = %name, "sub-agent registered on internal bus");
    } else {
        warn!(parent = %parent_name, agent = %name, "timed out waiting for sub-agent to register on bus (30s)");
    }

    let subscribe_display = subscribe.join(", ");
    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Agent '{}' started on internal bus {}. Subscriptions: {}",
                name, bus_socket, subscribe_display
            )
        }],
        "isError": false
    }))
}

// ─── Reminder ────────────────────────────────────────────────────────────────

pub(crate) async fn call_create_reminder(args: &Value) -> Result<Value> {
    let target = args
        .get("target")
        .and_then(|t| t.as_str())
        .context("missing target")?;
    let message = args
        .get("message")
        .and_then(|m| m.as_str())
        .context("missing message")?;

    // Support three time specification modes (exactly one required):
    // 1. "at" — ISO 8601 timestamp or human-friendly time string
    // 2. "in" — duration string like "30m", "1h", "2h30m"
    // 3. "delay_minutes" — legacy numeric minutes (backwards compat)
    let at_str = args.get("at").and_then(|a| a.as_str());
    let in_str = args.get("in").and_then(|i| i.as_str());
    let delay_minutes = args.get("delay_minutes").and_then(|d| d.as_f64());

    let result = if let Some(at) = at_str {
        let fire_at = parse_at_time(at)?;
        mcp_service::create_reminder_at(target, message, fire_at)?
    } else if let Some(dur) = in_str {
        let secs = crate::app::commands::parse_duration_secs(dur)?;
        let delay = secs as f64 / 60.0;
        mcp_service::create_reminder(target, message, delay)?
    } else if let Some(mins) = delay_minutes {
        mcp_service::create_reminder(target, message, mins)?
    } else {
        bail!("create_reminder requires one of: 'at', 'in', or 'delay_minutes'");
    };

    Ok(json!({
        "content": [{
            "type": "text",
            "text": format!(
                "Reminder scheduled: target={} at={} (in {:.0} minutes)",
                result.target, result.fire_at, result.delay_minutes
            )
        }],
        "isError": false
    }))
}

/// Parse a human-friendly time string into a UTC DateTime.
///
/// Supported formats:
/// - ISO 8601: "2026-04-22T09:00:00Z", "2026-04-22T09:00:00+03:00"
/// - Date + time: "2026-04-22 09:00"
/// - Time only (today/tomorrow): "09:00", "9:00", "21:30"
/// - Relative: "tomorrow 09:00", "tomorrow 9:00"
fn parse_at_time(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveTime, TimeZone, Utc};
    use chrono_tz::Europe::Moscow;

    let s = s.trim();

    // Try ISO 8601 with timezone offset first.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // Try "YYYY-MM-DD HH:MM" or "YYYY-MM-DDTHH:MM" (naive, assume Moscow tz).
    for fmt in &["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt)
            && let Some(dt) = Moscow.from_local_datetime(&naive).earliest()
        {
            return Ok(dt.with_timezone(&Utc));
        }
    }

    // "tomorrow HH:MM" or "tomorrow H:MM"
    if let Some(time_part) = s.strip_prefix("tomorrow").map(str::trim)
        && let Ok(time) = NaiveTime::parse_from_str(time_part, "%H:%M")
    {
        let tomorrow_moscow =
            (Utc::now().with_timezone(&Moscow).date_naive()) + chrono::Duration::days(1);
        let naive = tomorrow_moscow.and_time(time);
        if let Some(dt) = Moscow.from_local_datetime(&naive).earliest() {
            return Ok(dt.with_timezone(&Utc));
        }
    }

    // Bare time "HH:MM" or "H:MM" — today if in the future, tomorrow if past.
    if let Ok(time) = NaiveTime::parse_from_str(s, "%H:%M") {
        let now_moscow = Utc::now().with_timezone(&Moscow);
        let today = now_moscow.date_naive().and_time(time);
        if let Some(dt) = Moscow.from_local_datetime(&today).earliest() {
            let dt_utc = dt.with_timezone(&Utc);
            if dt_utc > Utc::now() {
                return Ok(dt_utc);
            }
        }
        // Time is past today — schedule for tomorrow.
        let tomorrow = (now_moscow.date_naive() + chrono::Duration::days(1)).and_time(time);
        if let Some(dt) = Moscow.from_local_datetime(&tomorrow).earliest() {
            return Ok(dt.with_timezone(&Utc));
        }
    }

    bail!(
        "cannot parse time '{}'. Supported: ISO 8601 (2026-04-22T09:00:00Z), \
         datetime (2026-04-22 09:00), time (09:00), 'tomorrow 09:00', \
         or duration with 'in' param (30m, 1h, 2h30m)",
        s
    )
}

// ─── Unified inbox ───────────────────────────────────────────────────────────

/// Check if `agent_name` is allowed to access `inbox_name`.
///
/// Rules:
/// - An agent can always read its own inbox (name matches agent_name).
/// - If the agent is a sub-agent with `inbox_read` configured, check the
///   glob patterns in that allow-list.
/// - If `inbox_read` is None on a sub-agent, only own inbox is allowed.
/// - For top-level agents (not in `user_config.agents`), the per-agent
///   `inbox_acl` field on `UserConfig` is consulted: when `Some(list)`, only
///   the agent's own inbox plus listed glob patterns are readable. When
///   absent (`None`), behavior is unrestricted (current default — backward
///   compatible).
pub(crate) fn inbox_access_allowed(
    agent_name: &str,
    inbox_name: &str,
    user_config: Option<&UserConfig>,
) -> bool {
    // Own inbox is always allowed.
    if inbox_name == agent_name {
        return true;
    }
    // The "replies/<agent>" convention belongs to the agent.
    if inbox_name == format!("replies/{}", agent_name) {
        return true;
    }

    if let Some(cfg) = user_config {
        // If this agent is a sub-agent with inbox_read ACL, check it.
        if let Some(sub) = cfg.agents.iter().find(|a| a.name == agent_name) {
            return match &sub.inbox_read {
                Some(patterns) => patterns.iter().any(|p| glob_match(p, inbox_name)),
                None => false, // No inbox_read → own inbox only
            };
        }

        // Top-level agent: consult inbox_acl on the user config itself.
        if let Some(patterns) = &cfg.inbox_acl {
            return patterns.iter().any(|p| glob_match(p, inbox_name));
        }
    }

    // No config or no inbox_acl on top-level agent → unrestricted (default).
    true
}

pub(crate) async fn call_list_inboxes(
    agent_name: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let all = mcp_service::list_inboxes()?;
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|entry| {
            entry
                .get("inbox")
                .and_then(|v| v.as_str())
                .map(|name| inbox_access_allowed(agent_name, name, user_config))
                .unwrap_or(false)
        })
        .collect();

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&filtered)?}],
        "isError": false
    }))
}

pub(crate) async fn call_read_inbox(
    args: &Value,
    agent_name: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let inbox = args
        .get("inbox")
        .and_then(|i| i.as_str())
        .context("missing inbox")?;

    if !inbox_access_allowed(agent_name, inbox, user_config) {
        bail!(
            "inbox access denied: agent \"{}\" is not in allow-list for inbox \"{}\"",
            agent_name,
            inbox
        );
    }

    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
    let since = args
        .get("since")
        .and_then(|s| s.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));

    let output = mcp_service::read_inbox(inbox, limit, since)?;

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&output)?}],
        "isError": false
    }))
}

pub(crate) async fn call_search_inbox(
    args: &Value,
    agent_name: &str,
    user_config: Option<&UserConfig>,
) -> Result<Value> {
    let inbox = args.get("inbox").and_then(|i| i.as_str());
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing query")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;

    // If a specific inbox is requested, check access.
    if let Some(name) = inbox
        && !inbox_access_allowed(agent_name, name, user_config)
    {
        bail!(
            "inbox access denied: agent \"{}\" is not in allow-list for inbox \"{}\"",
            agent_name,
            name
        );
    }

    // When searching all inboxes (inbox=None), filter results to allowed inboxes.
    // The mcp_service returns results tagged with inbox names, but the current
    // unified_inbox search doesn't expose per-result inbox names. For cross-inbox
    // search without ACL filtering, we restrict to the agent's own inbox.
    let effective_inbox = if inbox.is_some() {
        inbox
    } else if let Some(cfg) = user_config {
        // Sub-agent: cross-inbox search restricted to own inbox.
        if cfg.agents.iter().any(|a| a.name == agent_name) {
            Some(agent_name)
        } else if cfg.inbox_acl.is_some() {
            // Top-level agent with ACL: cross-inbox search restricted to own
            // inbox to avoid leaking content from inboxes outside the ACL.
            Some(agent_name)
        } else {
            None // Top-level agent without ACL — search all (default)
        }
    } else {
        None
    };

    let output = mcp_service::search_inbox(effective_inbox, query, limit)?;

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&output)?}],
        "isError": false
    }))
}

// ─── Graph execution ─────────────────────────────────────────────────────────

pub(crate) async fn call_run_graph(args: &Value) -> Result<Value> {
    let file = args
        .get("file")
        .and_then(|f| f.as_str())
        .context("missing file")?;
    let work_dir = args.get("work_dir").and_then(|w| w.as_str());
    let vars = args.get("vars");

    let summary = mcp_service::run_graph(file, work_dir, vars).await?;

    Ok(json!({
        "content": [{"type": "text", "text": summary}],
        "isError": false
    }))
}

// ─── Task queue ──────────────────────────────────────────────────────────────

pub(crate) async fn call_task_create(
    args: &Value,
    agent_name: &str,
    task_store: &dyn crate::ports::store::TaskRepository,
) -> Result<Value> {
    let description = args
        .get("description")
        .and_then(|d| d.as_str())
        .context("missing description")?;
    let model = args.get("model").and_then(|m| m.as_str()).map(String::from);
    let labels: Vec<String> = args
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let metadata = args
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let result =
        mcp_service::task_create(description, model, labels, metadata, agent_name, task_store)?;

    Ok(json!({
        "content": [{"type": "text", "text": format!(
            "Task created: {} (status=pending, id={})", result.description, result.id
        )}],
        "isError": false
    }))
}

pub(crate) async fn call_task_list(
    args: &Value,
    task_store: &dyn crate::ports::store::TaskRepository,
) -> Result<Value> {
    let status_filter = args.get("status").and_then(|s| s.as_str());

    let summary = mcp_service::task_list(status_filter, task_store)?;

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&summary)?}],
        "isError": false
    }))
}

pub(crate) async fn call_task_cancel(
    args: &Value,
    task_store: &dyn crate::ports::store::TaskRepository,
) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .context("missing id")?;

    let result = mcp_service::task_cancel(id, task_store)?;

    Ok(json!({
        "content": [{"type": "text", "text": format!("Task {} cancelled", result.id)}],
        "isError": false
    }))
}

// ─── Sub-agent management ────────────────────────────────────────────────────

pub(crate) async fn call_list_agents(
    caller: &str,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    let ibus_guard = internal_bus.lock().await;
    let agents: Vec<Value> = match &*ibus_guard {
        None => Vec::new(),
        Some(ibus) => {
            let mut list = Vec::new();
            for name in &ibus.sub_agents {
                // Scoped visibility: only show agents that the caller is parent of.
                if let Ok(state) = crate::app::agent::load_state(name) {
                    let is_visible = match &state.parent {
                        Some(parent) => parent == caller,
                        None => true,
                    };
                    if !is_visible {
                        continue;
                    }
                }

                let is_finished = ibus
                    .worker_handles
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, handle)| handle.is_finished())
                    .unwrap_or(true);

                list.push(mcp_service::build_agent_summary(name, is_finished));
            }
            list
        }
    };

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&agents)?}],
        "isError": false
    }))
}

/// Read recent log lines captured from a sub-agent's stderr or stream-json
/// stdout. Access is restricted to sub-agents of the caller (or the caller's
/// own logs) so an agent cannot peek at a sibling's diagnostics.
pub(crate) async fn call_agent_logs(args: &Value, caller: &str) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .context("agent_logs: missing required 'name'")?;

    let tail = args.get("tail").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
    if tail == 0 || tail > 1000 {
        bail!("agent_logs: 'tail' must be in 1..=1000 (got {})", tail);
    }

    let source = args
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("stderr");
    if source != "stderr" && source != "stream" {
        bail!(
            "agent_logs: 'source' must be 'stderr' or 'stream' (got {:?})",
            source
        );
    }

    // Access control: caller may read its own logs or its sub-agents' logs.
    // Matches the visibility rules of list_agents/remove_agent.
    if name != caller {
        let state = crate::app::agent::load_state(name)
            .with_context(|| format!("agent '{}' not found", name))?;
        match &state.parent {
            Some(p) if p == caller => {}
            _ => bail!(
                "agent '{}' is not a sub-agent of '{}' — log access denied",
                name,
                caller
            ),
        }
    }

    let path = match source {
        "stream" => crate::app::agent::stream_log_path(name),
        _ => crate::app::agent::stderr_log_path(name),
    };

    if !path.exists() {
        return Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&json!({
                    "name": name,
                    "source": source,
                    "path": path.display().to_string(),
                    "exists": false,
                    "lines": Vec::<String>::new(),
                }))?
            }],
            "isError": false
        }));
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("agent_logs: failed to read {}", path.display()))?;
    let all_lines: Vec<&str> = content.lines().collect();
    let start = all_lines.len().saturating_sub(tail);
    let returned: Vec<String> = all_lines[start..].iter().map(|s| s.to_string()).collect();

    Ok(json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&json!({
                "name": name,
                "source": source,
                "path": path.display().to_string(),
                "exists": true,
                "total_lines": all_lines.len(),
                "returned_lines": returned.len(),
                "lines": returned,
            }))?
        }],
        "isError": false
    }))
}

pub(crate) async fn call_remove_agent(
    args: &Value,
    caller: &str,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    let name = args
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing name")?;

    // Validate ownership and get agent state.
    let state = mcp_service::validate_remove_agent(name, caller)?;

    // Graceful shutdown of the agent process.
    mcp_service::stop_agent_process(name, state.pid).await;

    // Clean up internal bus state.
    {
        let mut ibus_guard = internal_bus.lock().await;
        if let Some(ref mut ibus) = *ibus_guard {
            ibus.sub_agents.remove(name);
            ibus.worker_handles.retain(|(n, handle)| {
                if n == name {
                    handle.abort();
                    false
                } else {
                    true
                }
            });
        }
    }

    crate::app::agent::remove(name).await?;

    info!(agent = %name, caller = %caller, "remove_agent via MCP");

    Ok(json!({
        "content": [{"type": "text", "text": format!("Agent '{}' removed", name)}],
        "isError": false
    }))
}

// ─── Scope introspection ────────────────────────────────────────────────────

pub(crate) async fn call_get_scope(
    agent_name: &str,
    user_config: Option<&UserConfig>,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    // Load agent state for scope info.
    let state = crate::app::agent::load_state(agent_name).ok();

    let scope = state
        .as_ref()
        .and_then(|s| s.scope.clone())
        .unwrap_or_else(|| "inherit".into());
    let parent = state.as_ref().and_then(|s| s.parent.clone());
    let work_dir = state
        .as_ref()
        .map(|s| s.config.work_dir.clone())
        .unwrap_or_else(|| std::env::var("PWD").unwrap_or_default());
    let env_keys = state
        .as_ref()
        .and_then(|s| s.env_keys.clone())
        .unwrap_or_default();
    let can_message_state = state.as_ref().and_then(|s| s.can_message.clone());

    // Merge can_message from config if available.
    let can_message = can_message_state.or_else(|| {
        user_config
            .and_then(|cfg| cfg.agents.iter().find(|a| a.name == agent_name))
            .and_then(|sub| sub.can_message.clone())
    });

    // Collect children from internal bus.
    let children: Vec<String> = {
        let ibus_guard = internal_bus.lock().await;
        match &*ibus_guard {
            None => Vec::new(),
            Some(ibus) => ibus.sub_agents.iter().cloned().collect(),
        }
    };

    // Get bus topics from config.
    let bus_topics: Vec<String> = user_config
        .and_then(|cfg| cfg.agents.iter().find(|a| a.name == agent_name))
        .map(|sub| sub.subscribe.clone())
        .unwrap_or_default();

    let scope_info = json!({
        "agent": agent_name,
        "scope": scope,
        "parent": parent,
        "work_dir": work_dir,
        "can_message": can_message,
        "bus_topics": bus_topics,
        "env_keys": env_keys,
        "children": children,
    });

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&scope_info)?}],
        "isError": false
    }))
}

// ─── State machines ──────────────────────────────────────────────────────────

pub(crate) async fn call_sm_create(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
    sm_store: &dyn crate::ports::store::StateMachineRepository,
) -> Result<Value> {
    let model_name = args
        .get("model")
        .and_then(|m| m.as_str())
        .context("missing model")?;
    let title = args
        .get("title")
        .and_then(|t| t.as_str())
        .context("missing title")?;
    let body = args.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let metadata = args
        .get("metadata")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let cfg = user_config.context("no user config loaded — models not available")?;
    let result = mcp_service::sm_create(
        model_name, title, body, metadata, agent_name, bus_socket, cfg, sm_store,
    )
    .await?;

    Ok(json!({
        "content": [{"type": "text", "text": format!(
            "Created instance {} (model={}, state={}, assignee={})",
            result.id, result.model, result.state, result.assignee
        )}],
        "isError": false
    }))
}

pub(crate) async fn call_sm_move(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
    sm_store: &dyn crate::ports::store::StateMachineRepository,
) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .context("missing id")?;
    let state = args
        .get("state")
        .and_then(|s| s.as_str())
        .context("missing state")?;
    let note = args.get("note").and_then(|n| n.as_str());

    let cfg = user_config.context("no user config loaded — models not available")?;
    let result =
        mcp_service::sm_move(id, state, note, agent_name, bus_socket, cfg, sm_store).await?;

    Ok(json!({
        "content": [{"type": "text", "text": format!("{} → {} (model={})", result.id, result.state, result.model)}],
        "isError": false
    }))
}

pub(crate) async fn call_sm_query(
    args: &Value,
    sm_store: &dyn crate::ports::store::StateMachineRepository,
) -> Result<Value> {
    let id = args.get("id").and_then(|i| i.as_str());
    let model_filter = args.get("model").and_then(|m| m.as_str());
    let state_filter = args.get("state").and_then(|s| s.as_str());

    let result = mcp_service::sm_query(id, model_filter, state_filter, sm_store)?;

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&result)?}],
        "isError": false
    }))
}

// ─── Usage stats ─────────────────────────────────────────────────────────────

pub(crate) async fn call_usage_stats(args: &Value) -> Result<Value> {
    let period = args.get("period").and_then(|v| v.as_str()).unwrap_or("7d");
    let agent = args.get("agent").and_then(|v| v.as_str());

    let stats = crate::app::commands::usage::compute_stats(period, agent)?;
    Ok(serde_json::to_value(stats)?)
}

/// Apply authentication to an outgoing A2A HTTP request.
///
/// Supports two modes:
/// - `api_key` param → `x-api-key` header
/// - `jwt_key` param (path to private key PEM) → `Authorization: Bearer <jwt>`
fn apply_a2a_auth(
    mut req: reqwest::RequestBuilder,
    args: &serde_json::Value,
    url: &str,
) -> reqwest::RequestBuilder {
    if let Some(jwt_key_path) = args.get("jwt_key").and_then(|v| v.as_str()) {
        let path = std::path::Path::new(jwt_key_path);
        match crate::app::a2a_jwt::KeyPair::load(path) {
            Ok(kp) => match kp.sign_jwt(url, 60) {
                Ok(token) => {
                    req = req.header("authorization", format!("Bearer {}", token));
                }
                Err(e) => {
                    tracing::warn!("failed to sign JWT: {}", e);
                }
            },
            Err(e) => {
                tracing::warn!("failed to load JWT key from {}: {}", jwt_key_path, e);
            }
        }
    } else if let Some(key) = args.get("api_key").and_then(|v| v.as_str()) {
        req = req.header("x-api-key", key);
    }
    req
}

/// Send an A2A task to a remote agent via HTTP.
///
/// MCP tool: `a2a_send(url, skill, message, api_key?, jwt_key?)`
pub(crate) async fn call_a2a_send(args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'url' parameter"))?;
    let skill = args
        .get("skill")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'skill' parameter"))?;
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'message' parameter"))?;

    let rpc_body = json!({
        "jsonrpc": "2.0",
        "id": uuid::Uuid::new_v4().to_string(),
        "method": "tasks/send",
        "params": {
            "skill": skill,
            "message": message,
        }
    });

    let a2a_url = format!("{}/a2a", url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let mut req = client.post(&a2a_url).json(&rpc_body);
    req = apply_a2a_auth(req, args, url);

    let resp = req.send().await.context("A2A request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("A2A request returned {}: {}", status, body);
    }

    let body: Value = resp.json().await.context("failed to parse A2A response")?;

    if let Some(error) = body.get("error") {
        bail!(
            "A2A error {}: {}",
            error.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
            error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
        );
    }

    Ok(body.get("result").cloned().unwrap_or(body))
}

/// Fetch an Agent Card from a remote A2A agent (discovery).
///
/// MCP tool: `a2a_discover(url)`
pub(crate) async fn call_a2a_discover(args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'url' parameter"))?;

    let card_url = format!("{}/.well-known/agent-card.json", url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let resp = client
        .get(&card_url)
        .send()
        .await
        .context("failed to fetch Agent Card")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Agent Card request returned {}: {}", status, body);
    }

    let card: Value = resp
        .json()
        .await
        .context("failed to parse Agent Card JSON")?;

    Ok(card)
}

/// Dynamically add a need to the agent's own deskd.yaml.
///
/// MCP tool: `publish_need(description, tags?, priority?)`
pub(crate) async fn call_publish_need(args: &Value) -> Result<Value> {
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'description' parameter"))?;

    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("medium");

    // Generate a stable ID from the description.
    let id = description
        .to_lowercase()
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join("-");

    let config_path = std::env::var("DESKD_AGENT_CONFIG")
        .context("DESKD_AGENT_CONFIG not set — cannot publish need")?;

    // Load existing config, add need, write back.
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config: {}", config_path))?;

    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&raw).context("failed to parse config YAML")?;

    let need_val = serde_yaml::to_value(&crate::config::NeedDef {
        id: id.clone(),
        description: description.to_string(),
        tags: tags.clone(),
        priority: priority.to_string(),
    })?;

    // Ensure needs: array exists and append.
    let needs = doc
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("config is not a YAML mapping"))?
        .entry(serde_yaml::Value::String("needs".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(vec![]));

    needs
        .as_sequence_mut()
        .ok_or_else(|| anyhow::anyhow!("needs: is not a sequence"))?
        .push(need_val);

    let updated = serde_yaml::to_string(&doc)?;
    std::fs::write(&config_path, updated)
        .with_context(|| format!("failed to write config: {}", config_path))?;

    Ok(json!({
        "status": "published",
        "need": {
            "id": id,
            "description": description,
            "tags": tags,
            "priority": priority,
        }
    }))
}

/// Fetch a remote agent's needs from their Agent Card.
///
/// MCP tool: `browse_needs(url)`
pub(crate) async fn call_browse_needs(args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'url' parameter"))?;

    let card_url = format!("{}/.well-known/agent-card.json", url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let resp = client
        .get(&card_url)
        .send()
        .await
        .context("failed to fetch Agent Card")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Agent Card request returned {}: {}", status, body);
    }

    let card: Value = resp
        .json()
        .await
        .context("failed to parse Agent Card JSON")?;

    let needs = card.get("needs").cloned().unwrap_or(json!([]));
    let agent_name = card
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    Ok(json!({
        "agent": agent_name,
        "url": url,
        "needs": needs,
    }))
}

/// Send a proposal to fulfill a remote agent's need via A2A.
///
/// MCP tool: `propose_for_need(url, need_id, proposal, api_key?)`
pub(crate) async fn call_propose_for_need(args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'url' parameter"))?;
    let need_id = args
        .get("need_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'need_id' parameter"))?;
    let proposal = args
        .get("proposal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'proposal' parameter"))?;

    let rpc_body = json!({
        "jsonrpc": "2.0",
        "id": uuid::Uuid::new_v4().to_string(),
        "method": "tasks/send",
        "params": {
            "skill": need_id,
            "message": format!("Proposal for need '{}': {}", need_id, proposal),
            "metadata": {
                "type": "need_proposal",
                "need_id": need_id,
            }
        }
    });

    let a2a_url = format!("{}/a2a", url.trim_end_matches('/'));

    let client = reqwest::Client::new();
    let mut req = client.post(&a2a_url).json(&rpc_body);
    req = apply_a2a_auth(req, args, url);

    let resp = req.send().await.context("A2A proposal request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("A2A proposal returned {}: {}", status, body);
    }

    let body: Value = resp.json().await.context("failed to parse A2A response")?;

    if let Some(error) = body.get("error") {
        bail!(
            "A2A error {}: {}",
            error.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
            error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
        );
    }

    Ok(json!({
        "status": "proposed",
        "need_id": need_id,
        "result": body.get("result").cloned().unwrap_or(Value::Null),
    }))
}

/// Synchronously query another agent and wait for the response.
///
/// MCP tool: `query_agent(target, question, timeout_secs?)`
///
/// Creates a temporary bus connection, sends the question with `reply_to`
/// pointing to the temporary name, and waits for the response via direct
/// name routing.
pub(crate) async fn call_query_agent(
    args: &Value,
    agent_name: &str,
    bus_socket: &str,
) -> Result<Value> {
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'target' parameter"))?;
    let question = args
        .get("question")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'question' parameter"))?;
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    let query_id = Uuid::new_v4().to_string();
    let temp_name = format!("{}-query-{}", agent_name, &query_id[..8]);

    // Connect to bus with a temporary identity for receiving the response.
    use crate::infra::unix_bus::UnixBus;
    use crate::ports::bus::MessageBus;
    let bus = UnixBus::connect(bus_socket).await?;
    bus.register(&temp_name, &[]).await?;

    // Send query to target with reply_to pointing to our temporary name.
    let msg = crate::domain::message::Message {
        id: query_id.clone(),
        source: temp_name.clone(),
        target: target.to_string(),
        payload: json!({"task": question}),
        reply_to: Some(temp_name.clone()),
        metadata: crate::domain::message::Metadata::default(),
    };
    bus.send(&msg).await?;

    info!(
        agent = %agent_name,
        target = %target,
        query_id = %query_id,
        timeout = timeout_secs,
        "query_agent: waiting for response"
    );

    // Wait for response with timeout.
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let response = tokio::time::timeout(timeout, async {
        // Collect all response chunks until we get a "final" one.
        let mut full_response = String::new();
        loop {
            let resp = bus.recv().await?;
            // Extract text from payload.
            if let Some(text) = resp.payload.get("text").and_then(|v| v.as_str()) {
                full_response.push_str(text);
            } else if let Some(result) = resp.payload.get("result").and_then(|v| v.as_str()) {
                full_response.push_str(result);
            } else if let Some(task) = resp.payload.get("task").and_then(|v| v.as_str()) {
                full_response.push_str(task);
            }

            // Check if this is the final response.
            let is_final = resp
                .payload
                .get("final")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_final || !full_response.is_empty() {
                return Ok::<String, anyhow::Error>(full_response);
            }
        }
    })
    .await;

    match response {
        Ok(Ok(text)) => {
            info!(
                agent = %agent_name,
                target = %target,
                len = text.len(),
                "query_agent: received response"
            );
            Ok(json!({
                "status": "ok",
                "source": target,
                "response": text,
            }))
        }
        Ok(Err(e)) => {
            bail!("query_agent: bus error while waiting for response: {}", e);
        }
        Err(_) => {
            bail!(
                "query_agent: timed out after {}s waiting for response from {}",
                timeout_secs,
                target
            );
        }
    }
}

// ─── telegram_history ────────────────────────────────────────────────────────

/// Handle the `telegram_history` MCP tool call.
///
/// Reads from the locally accumulated unified inbox at
/// `~/.deskd/inboxes/telegram/{chat_id}.jsonl`. Messages are written
/// there by the teloxide adapter on every incoming Telegram message.
pub(crate) async fn call_telegram_history(args: &Value) -> Result<Value> {
    let chat_id = args
        .get("chat_id")
        .and_then(|v| v.as_i64())
        .context("telegram_history: missing required integer 'chat_id'")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    if limit == 0 || limit > 200 {
        bail!(
            "telegram_history: 'limit' must be in 1..=200 (got {})",
            limit
        );
    }

    let inbox_name = format!("telegram/{}", chat_id);
    let messages = mcp_service::read_inbox(&inbox_name, limit, None)?;

    Ok(json!({
        "chat_id": chat_id,
        "count": messages.len(),
        "messages": messages,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("agent:*", "agent:dev"));
        assert!(glob_match("telegram.out:*", "telegram.out:-1234567"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("agent:dev", "agent:dev"));
        assert!(!glob_match("agent:dev", "agent:researcher"));
        assert!(!glob_match("telegram.out:*", "telegram.in:-1234"));
    }

    #[tokio::test]
    async fn agent_logs_rejects_missing_name() {
        let err = call_agent_logs(&json!({}), "caller").await.unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("name"), "expected name error, got: {}", msg);
    }

    #[tokio::test]
    async fn agent_logs_rejects_bad_tail() {
        for bad in [0u64, 5000] {
            let err = call_agent_logs(&json!({"name": "x", "tail": bad}), "x")
                .await
                .unwrap_err();
            let msg = format!("{:#}", err);
            assert!(msg.contains("tail"), "expected tail error, got: {}", msg);
        }
    }

    #[tokio::test]
    async fn agent_logs_rejects_bad_source() {
        let err = call_agent_logs(&json!({"name": "x", "source": "garbage"}), "x")
            .await
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("source"),
            "expected source error, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn agent_logs_self_read_nonexistent_returns_exists_false() {
        // Requesting own logs when the log file does not exist must not error —
        // it returns `exists: false` so callers can distinguish "no logs yet"
        // from "access denied" or "bad arg".
        let unique = format!("__agent_logs_test_{}", Uuid::new_v4());
        let resp = call_agent_logs(&json!({"name": unique, "tail": 10}), &unique)
            .await
            .expect("self-read must not error on missing file");
        let text = resp["content"][0]["text"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed["exists"], false);
        assert_eq!(parsed["source"], "stderr");
        assert!(parsed["lines"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn telegram_history_rejects_missing_chat_id() {
        let err = call_telegram_history(&json!({})).await.unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("chat_id"),
            "expected chat_id error, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn telegram_history_rejects_bad_limit() {
        let err = call_telegram_history(&json!({"chat_id": 1, "limit": 500}))
            .await
            .unwrap_err();
        let msg = format!("{:#}", err);
        assert!(msg.contains("limit"), "expected limit error, got: {}", msg);
    }

    // ─── inbox ACL tests ────────────────────────────────────────────────

    fn make_config_with_sub_agent(name: &str, inbox_read: Option<Vec<String>>) -> UserConfig {
        let inbox_yaml = match &inbox_read {
            Some(patterns) => {
                let items: Vec<String> = patterns.iter().map(|p| format!("\"{}\"", p)).collect();
                format!("    inbox_read: [{}]\n", items.join(", "))
            }
            None => String::new(),
        };
        let yaml = format!(
            "agents:\n  - name: {}\n    model: haiku\n    subscribe: []\n{}",
            name, inbox_yaml
        );
        serde_yaml::from_str(&yaml).expect("test config should parse")
    }

    #[test]
    fn inbox_acl_allows_own_inbox() {
        // Sub-agent can always read its own inbox, even without inbox_read.
        let cfg = make_config_with_sub_agent("reader", None);
        assert!(inbox_access_allowed("reader", "reader", Some(&cfg)));
    }

    #[test]
    fn inbox_acl_denies_other_inbox_by_default() {
        // Sub-agent with no inbox_read cannot read other inboxes.
        let cfg = make_config_with_sub_agent("reader", None);
        assert!(!inbox_access_allowed("reader", "secret", Some(&cfg)));
    }

    #[test]
    fn inbox_acl_allows_listed_inbox() {
        let cfg =
            make_config_with_sub_agent("reader", Some(vec!["shared".into(), "collab-*".into()]));
        assert!(inbox_access_allowed("reader", "shared", Some(&cfg)));
        assert!(inbox_access_allowed("reader", "collab-daily", Some(&cfg)));
        assert!(inbox_access_allowed("reader", "reader", Some(&cfg))); // own always ok
    }

    #[test]
    fn inbox_acl_denies_unlisted_inbox() {
        let cfg = make_config_with_sub_agent("reader", Some(vec!["shared".into()]));
        assert!(!inbox_access_allowed("reader", "secret", Some(&cfg)));
    }

    #[test]
    fn inbox_acl_top_level_agent_unrestricted() {
        // Agent not in user_config.agents → top-level → unrestricted.
        let cfg = make_config_with_sub_agent("other-agent", None);
        assert!(inbox_access_allowed("top-level", "anything", Some(&cfg)));
    }

    #[test]
    fn inbox_acl_no_config_unrestricted() {
        // No user_config at all → unrestricted.
        assert!(inbox_access_allowed("agent", "any-inbox", None));
    }

    // ─── Top-level inbox_acl on UserConfig ─────────────────────────────────

    fn make_top_level_config(inbox_acl: Option<Vec<String>>) -> UserConfig {
        let yaml = match inbox_acl {
            Some(patterns) => {
                let items: Vec<String> = patterns.iter().map(|p| format!("\"{}\"", p)).collect();
                format!("inbox_acl: [{}]\n", items.join(", "))
            }
            None => String::new(),
        };
        // Empty yaml is valid for a default UserConfig.
        if yaml.is_empty() {
            UserConfig::default()
        } else {
            serde_yaml::from_str(&yaml).expect("test config should parse")
        }
    }

    #[test]
    fn top_level_inbox_acl_absent_is_unrestricted() {
        // Backward compat: no inbox_acl key → top-level agent reads any inbox.
        let cfg = make_top_level_config(None);
        assert!(inbox_access_allowed("kira", "anything", Some(&cfg)));
        assert!(inbox_access_allowed("kira", "dev", Some(&cfg)));
    }

    #[test]
    fn top_level_inbox_acl_allows_own_inbox() {
        // Even with an empty allow-list, the agent's own inbox is readable.
        let cfg = make_top_level_config(Some(vec![]));
        assert!(inbox_access_allowed("kira", "kira", Some(&cfg)));
        assert!(inbox_access_allowed("kira", "replies/kira", Some(&cfg)));
    }

    #[test]
    fn top_level_inbox_acl_denies_unlisted() {
        let cfg = make_top_level_config(Some(vec!["dev".into()]));
        assert!(!inbox_access_allowed("kira", "secret", Some(&cfg)));
        assert!(!inbox_access_allowed("kira", "vasya", Some(&cfg)));
    }

    #[test]
    fn top_level_inbox_acl_allows_listed_with_glob() {
        let cfg = make_top_level_config(Some(vec!["dev".into(), "collab-*".into()]));
        assert!(inbox_access_allowed("kira", "dev", Some(&cfg)));
        assert!(inbox_access_allowed("kira", "collab-daily", Some(&cfg)));
        assert!(!inbox_access_allowed("kira", "secret", Some(&cfg)));
    }

    #[test]
    fn top_level_inbox_acl_yaml_round_trip() {
        let yaml = "inbox_acl:\n  - dev\n  - kira\n";
        let cfg: UserConfig = serde_yaml::from_str(yaml).unwrap();
        let acl = cfg.inbox_acl.expect("inbox_acl should parse");
        assert_eq!(acl, vec!["dev".to_string(), "kira".to_string()]);
    }

    #[test]
    fn read_inbox_error_message_format() {
        // The denial error should match the documented "inbox access denied"
        // wording so callers can match on it.
        let cfg = make_top_level_config(Some(vec!["dev".into()]));
        let allowed = inbox_access_allowed("kira", "secret", Some(&cfg));
        assert!(!allowed);
        // Spot-check the error format used in call_read_inbox / bus_api by
        // formatting with the agreed template.
        let msg = format!(
            "inbox access denied: agent \"{}\" is not in allow-list for inbox \"{}\"",
            "kira", "secret"
        );
        assert!(msg.starts_with("inbox access denied:"));
        assert!(msg.contains("\"kira\""));
        assert!(msg.contains("\"secret\""));
    }

    // ─── can_message ACL tests ──────────────────────────────────────────────

    fn make_config_with_can_message(name: &str, can_message: Option<Vec<String>>) -> UserConfig {
        let cm_yaml = match &can_message {
            Some(targets) => {
                let items: Vec<String> = targets.iter().map(|t| format!("\"{}\"", t)).collect();
                format!("    can_message: [{}]\n", items.join(", "))
            }
            None => String::new(),
        };
        let yaml = format!(
            "agents:\n  - name: {}\n    model: haiku\n    subscribe: []\n{}",
            name, cm_yaml
        );
        serde_yaml::from_str(&yaml).expect("test config should parse")
    }

    #[test]
    fn can_message_unrestricted_by_default() {
        // No can_message → agent can message anyone (publish ACL may still apply).
        let cfg = make_config_with_can_message("worker", None);
        // The can_message check in call_send_message only fires if can_message is Some.
        let sub = cfg.agents.iter().find(|a| a.name == "worker").unwrap();
        assert!(sub.can_message.is_none());
    }

    #[test]
    fn can_message_allows_listed_target() {
        let cfg = make_config_with_can_message(
            "worker",
            Some(vec!["agent:parent".into(), "telegram.out:*".into()]),
        );
        let sub = cfg.agents.iter().find(|a| a.name == "worker").unwrap();
        let allow = sub.can_message.as_ref().unwrap();
        assert!(allow.iter().any(|p| glob_match(p, "agent:parent")));
        assert!(allow.iter().any(|p| glob_match(p, "telegram.out:-123")));
        assert!(!allow.iter().any(|p| glob_match(p, "agent:sibling")));
    }

    #[test]
    fn can_message_denies_unlisted_target() {
        let cfg = make_config_with_can_message("worker", Some(vec!["agent:parent".into()]));
        let sub = cfg.agents.iter().find(|a| a.name == "worker").unwrap();
        let allow = sub.can_message.as_ref().unwrap();
        assert!(!allow.iter().any(|p| glob_match(p, "agent:sibling")));
        assert!(!allow.iter().any(|p| glob_match(p, "telegram.out:-123")));
    }

    // ─── scope config tests ─────────────────────────────────────────────────

    #[test]
    fn scope_config_parses_narrow() {
        let yaml = r#"
agents:
  - name: worker
    model: haiku
    subscribe: []
    scope: narrow
    can_message: ["agent:parent"]
    work_dir: /home/dev/tasks
    env:
      API_KEY: sk-test
"#;
        let cfg: UserConfig = serde_yaml::from_str(yaml).expect("parse");
        let sub = &cfg.agents[0];
        assert_eq!(sub.scope, crate::config::ScopeType::Narrow);
        assert_eq!(sub.can_message.as_ref().unwrap(), &["agent:parent"]);
        assert_eq!(sub.work_dir.as_deref(), Some("/home/dev/tasks"));
        assert_eq!(sub.env.as_ref().unwrap().get("API_KEY").unwrap(), "sk-test");
    }

    #[test]
    fn scope_config_defaults() {
        let yaml = "agents:\n  - name: w\n    model: h\n    subscribe: []\n";
        let cfg: UserConfig = serde_yaml::from_str(yaml).expect("parse");
        let sub = &cfg.agents[0];
        assert_eq!(sub.scope, crate::config::ScopeType::Inherit);
        assert!(sub.can_message.is_none());
        assert!(sub.work_dir.is_none());
        assert!(sub.env.is_none());
    }

    // ─── parse_at_time tests ────────────────────────────────────────────────

    #[test]
    fn parse_at_iso8601() {
        let dt = parse_at_time("2026-04-22T09:00:00Z").unwrap();
        assert_eq!(dt.hour(), 9);
        assert_eq!(dt.day(), 22);
    }

    #[test]
    fn parse_at_iso8601_with_offset() {
        let dt = parse_at_time("2026-04-22T12:00:00+03:00").unwrap();
        // 12:00 MSK = 09:00 UTC
        assert_eq!(dt.hour(), 9);
    }

    #[test]
    fn parse_at_datetime_naive() {
        let dt = parse_at_time("2026-04-22 09:00").unwrap();
        // 09:00 Moscow = 06:00 UTC
        assert_eq!(dt.hour(), 6);
        assert_eq!(dt.day(), 22);
    }

    #[test]
    fn parse_at_datetime_naive_with_t() {
        let dt = parse_at_time("2026-04-22T09:00").unwrap();
        assert_eq!(dt.hour(), 6); // 09:00 MSK = 06:00 UTC
    }

    #[test]
    fn parse_at_tomorrow() {
        let dt = parse_at_time("tomorrow 09:00").unwrap();
        let now = chrono::Utc::now();
        // Should be in the future.
        assert!(dt > now);
        // Hour in UTC: 09:00 Moscow = 06:00 UTC.
        assert_eq!(dt.hour(), 6);
    }

    #[test]
    fn parse_at_bare_time_future() {
        // Use a time that's definitely in the future (23:59 Moscow).
        let dt = parse_at_time("23:59").unwrap();
        // 23:59 Moscow = 20:59 UTC
        assert_eq!(dt.hour(), 20);
        assert_eq!(dt.minute(), 59);
    }

    #[test]
    fn parse_at_invalid() {
        assert!(parse_at_time("not a time").is_err());
        assert!(parse_at_time("").is_err());
    }

    use chrono::{Datelike, Timelike};
}
