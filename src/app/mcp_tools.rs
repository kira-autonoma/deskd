//! MCP tool implementations — the `call_*` functions that back each tool.
//!
//! Split from `mcp.rs` (#293). Each function parses tool arguments, delegates
//! to `mcp_service` or performs the action directly, and returns a JSON result.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashSet;
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

    // Get the deskd binary path (we are running as a subprocess of claude, so $0 is deskd).
    let deskd_bin = std::env::var("DESKD_BIN").unwrap_or_else(|_| "deskd".to_string());

    // Work dir: same as parent (best effort from env or cwd).
    let work_dir = std::env::var("PWD").unwrap_or_else(|_| "/tmp".to_string());

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

    // Set parent field on the created agent state.
    if let Ok(mut state) = crate::app::agent::load_state(name) {
        state.parent = Some(parent_name.to_string());
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
    let mut child = tokio::process::Command::new(&deskd_bin)
        .args(&run_args)
        .env("DESKD_BUS_SOCKET", &bus_socket)
        .env("DESKD_AGENT_NAME", name)
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
    let delay_minutes = args
        .get("delay_minutes")
        .and_then(|d| d.as_f64())
        .context("missing delay_minutes")?;

    let result = mcp_service::create_reminder(target, message, delay_minutes)?;

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

// ─── Unified inbox ───────────────────────────────────────────────────────────

pub(crate) async fn call_list_inboxes() -> Result<Value> {
    let summary = mcp_service::list_inboxes()?;

    Ok(json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&summary)?}],
        "isError": false
    }))
}

pub(crate) async fn call_read_inbox(args: &Value) -> Result<Value> {
    let inbox = args
        .get("inbox")
        .and_then(|i| i.as_str())
        .context("missing inbox")?;
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

pub(crate) async fn call_search_inbox(args: &Value) -> Result<Value> {
    let inbox = args.get("inbox").and_then(|i| i.as_str());
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .context("missing query")?;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;

    let output = mcp_service::search_inbox(inbox, query, limit)?;

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
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
) -> Result<Value> {
    let ibus_guard = internal_bus.lock().await;
    let agents: Vec<Value> = match &*ibus_guard {
        None => Vec::new(),
        Some(ibus) => {
            let mut list = Vec::new();
            for name in &ibus.sub_agents {
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

/// Send an A2A task to a remote agent via HTTP.
///
/// MCP tool: `a2a_send(url, skill, message, api_key?)`
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
    let api_key = args.get("api_key").and_then(|v| v.as_str());

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
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }

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
}
