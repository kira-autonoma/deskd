//! Bus API — structured query/command handler for TUI and external clients.
//!
//! Connects to the agent's bus as `deskd:api`, subscribes to `deskd:query` and
//! `deskd:command`, and routes incoming requests to existing stores/services.
//! Responses are sent back to the requester's `source` address.
//!
//! # Message format
//!
//! Request (sent to `deskd:query` or `deskd:command`):
//! ```json
//! {"type":"message","target":"deskd:query","source":"tui","payload":{
//!   "method": "agent_list",
//!   "params": {},
//!   "request_id": "req-1"
//! }}
//! ```
//!
//! Response (sent back to `source`):
//! ```json
//! {"type":"message","target":"tui","source":"deskd:api","payload":{
//!   "method": "agent_list",
//!   "request_id": "req-1",
//!   "result": [...]
//! }}
//! ```

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::app::mcp_service;
use crate::config::UserConfig;
use crate::infra::unix_bus::UnixBus;
use crate::ports::bus::MessageBus;
use crate::ports::store::{StateMachineRepository, TaskRepository};

const API_CLIENT_NAME: &str = "deskd:api";

/// Run the bus API handler. Connects to the bus, subscribes to query/command
/// topics, and dispatches incoming requests to the appropriate handler.
///
/// This function runs forever (until the bus connection drops).
pub async fn run(
    bus_socket: &str,
    task_store: &(dyn TaskRepository + Send + Sync),
    sm_store: &(dyn StateMachineRepository + Send + Sync),
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<()> {
    let bus = UnixBus::connect(bus_socket).await?;
    bus.register(
        API_CLIENT_NAME,
        &["deskd:query".to_string(), "deskd:command".to_string()],
    )
    .await?;
    info!("bus_api registered on {}", bus_socket);

    loop {
        let msg = bus.recv().await?;

        let method = msg
            .payload
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = msg
            .payload
            .get("params")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let request_id = msg
            .payload
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reply_target = msg.source.clone();

        let result = dispatch(
            &method,
            &params,
            bus_socket,
            task_store,
            sm_store,
            user_config,
            agent_name,
        )
        .await;

        let response_payload = match result {
            Ok(value) => json!({
                "method": method,
                "request_id": request_id,
                "result": value,
            }),
            Err(e) => json!({
                "method": method,
                "request_id": request_id,
                "error": e.to_string(),
            }),
        };

        let response = crate::domain::message::Message {
            id: uuid::Uuid::new_v4().to_string(),
            source: API_CLIENT_NAME.to_string(),
            target: reply_target,
            payload: response_payload,
            reply_to: None,
            metadata: Default::default(),
        };

        if let Err(e) = bus.send(&response).await {
            warn!(error = %e, "bus_api: failed to send response");
        }
    }
}

/// Dispatch a method call to the appropriate handler.
async fn dispatch(
    method: &str,
    params: &Value,
    bus_socket: &str,
    task_store: &(dyn TaskRepository + Send + Sync),
    sm_store: &(dyn StateMachineRepository + Send + Sync),
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<Value> {
    match method {
        // ── Queries ─────────────────────────────────────────────────────
        "agent_list" => handle_agent_list(bus_socket).await,
        "agent_detail" => handle_agent_detail(params).await,
        "task_list" => handle_task_list(params, task_store),
        "task_detail" => handle_task_detail(params, task_store),
        "sm_list" => handle_sm_list(params, sm_store),
        "sm_detail" => handle_sm_detail(params, sm_store),
        "sm_models" => handle_sm_models(user_config),
        "usage_stats" => handle_usage_stats(params).await,
        "schedule_list" => handle_schedule_list(user_config),
        "inbox_list" => handle_inbox_list(),
        "inbox_read" => handle_inbox_read(params),
        "inbox_search" => handle_inbox_search(params),
        "bus_status" => handle_bus_status(bus_socket).await,
        "room_list" => handle_room_list().await,
        "room_children" => handle_room_children(params),
        "agent_requests" => handle_agent_requests(params, task_store),
        "agent_config_list" => handle_agent_config_list(),

        // ── Mutations ───────────────────────────────────────────────────
        "send_message" => handle_send_message(params, bus_socket, agent_name).await,
        "task_create" => handle_task_create(params, task_store, agent_name),
        "task_cancel" => handle_task_cancel(params, task_store),
        "sm_create" => {
            handle_sm_create(params, bus_socket, sm_store, user_config, agent_name).await
        }
        "sm_move" => handle_sm_move(params, bus_socket, sm_store, user_config, agent_name).await,
        "sm_cancel" => handle_sm_cancel(params, sm_store),
        "schedule_add" => handle_schedule_add(params, user_config, agent_name),
        "schedule_remove" => handle_schedule_remove(params, user_config, agent_name),

        _ => bail!("unknown method: {}", method),
    }
}

// ─── Query handlers ─────────────────────────────────────────────────────────

async fn handle_agent_list(bus_socket: &str) -> Result<Value> {
    let live = crate::app::serve::query_live_agents(bus_socket)
        .await
        .unwrap_or_default();
    let agents = crate::app::agent::list().await?;
    let summaries: Vec<Value> = agents
        .iter()
        .map(|state| {
            let is_finished = !live.contains(&state.config.name);
            mcp_service::build_agent_summary(&state.config.name, is_finished)
        })
        .collect();
    Ok(json!(summaries))
}

async fn handle_agent_detail(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'name' parameter"))?;
    let state = crate::app::agent::load_state(name)?;
    Ok(json!({
        "name": state.config.name,
        "model": state.config.model,
        "system_prompt": state.config.system_prompt,
        "work_dir": state.config.work_dir,
        "max_turns": state.config.max_turns,
        "pid": state.pid,
        "session_id": state.session_id,
        "total_turns": state.total_turns,
        "total_cost": state.total_cost,
        "created_at": state.created_at,
        "status": state.status,
        "current_task": state.current_task,
        "parent": state.parent,
    }))
}

fn handle_task_list(
    params: &Value,
    task_store: &(dyn TaskRepository + Send + Sync),
) -> Result<Value> {
    let status_filter = params.get("status_filter").and_then(|v| v.as_str());
    let tasks = mcp_service::task_list(status_filter, task_store)?;
    Ok(json!(tasks))
}

fn handle_task_detail(
    params: &Value,
    task_store: &(dyn TaskRepository + Send + Sync),
) -> Result<Value> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'id' parameter"))?;
    let task = task_store.load(id)?;
    Ok(json!({
        "id": task.id,
        "description": task.description,
        "status": task.status.to_string(),
        "assignee": task.assignee,
        "created_by": task.created_by,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "result": task.result,
        "error": task.error,
        "cost_usd": task.cost_usd,
        "turns": task.turns,
        "sm_instance_id": task.sm_instance_id,
        "metadata": task.metadata,
    }))
}

fn handle_sm_list(
    params: &Value,
    sm_store: &(dyn StateMachineRepository + Send + Sync),
) -> Result<Value> {
    let model_filter = params.get("model").and_then(|v| v.as_str());
    let state_filter = params.get("state").and_then(|v| v.as_str());
    mcp_service::sm_query(None, model_filter, state_filter, sm_store)
}

fn handle_sm_detail(
    params: &Value,
    sm_store: &(dyn StateMachineRepository + Send + Sync),
) -> Result<Value> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'id' parameter"))?;
    mcp_service::sm_query(Some(id), None, None, sm_store)
}

fn handle_sm_models(user_config: Option<&UserConfig>) -> Result<Value> {
    let config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let models: Vec<Value> = config
        .models
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "states": m.states,
                "transitions": m.transitions.iter().map(|t| {
                    json!({
                        "from": t.from,
                        "to": t.to,
                        "trigger": t.trigger,
                    })
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!(models))
}

async fn handle_usage_stats(params: &Value) -> Result<Value> {
    let period = params
        .get("period")
        .and_then(|v| v.as_str())
        .unwrap_or("7d");
    let agent = params.get("agent").and_then(|v| v.as_str());
    let stats = crate::app::commands::usage::compute_stats(period, agent)?;
    Ok(serde_json::to_value(stats)?)
}

fn handle_schedule_list(user_config: Option<&UserConfig>) -> Result<Value> {
    let config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let schedules: Vec<Value> = config
        .schedules
        .iter()
        .enumerate()
        .map(|(i, s)| {
            json!({
                "index": i,
                "cron": s.cron,
                "target": s.target,
                "action": format!("{:?}", s.action),
                "timezone": s.timezone,
            })
        })
        .collect();
    Ok(json!(schedules))
}

fn handle_inbox_list() -> Result<Value> {
    let inboxes = mcp_service::list_inboxes()?;
    Ok(json!(inboxes))
}

fn handle_inbox_read(params: &Value) -> Result<Value> {
    let inbox = params
        .get("inbox")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'inbox' parameter"))?;
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let since = params
        .get("since")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let messages = mcp_service::read_inbox(inbox, limit, since)?;
    Ok(json!(messages))
}

fn handle_inbox_search(params: &Value) -> Result<Value> {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'query' parameter"))?;
    let inbox = params.get("inbox").and_then(|v| v.as_str());
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let results = mcp_service::search_inbox(inbox, query, limit)?;
    Ok(json!(results))
}

async fn handle_bus_status(bus_socket: &str) -> Result<Value> {
    let clients = crate::app::serve::query_live_agents(bus_socket)
        .await
        .unwrap_or_default();
    let client_list: Vec<&str> = clients.iter().map(|s| s.as_str()).collect();
    Ok(json!({
        "socket": bus_socket,
        "clients": client_list,
    }))
}

async fn handle_room_list() -> Result<Value> {
    let serve_state = crate::config::ServeState::load()
        .ok_or_else(|| anyhow::anyhow!("no serve state found (deskd serve not running?)"))?;

    let mut rooms: Vec<Value> = Vec::new();
    for (name, agent_serve) in &serve_state.agents {
        let (status, cost_usd, _turns) = match crate::app::agent::load_state(name) {
            Ok(state) => (state.status, state.total_cost, state.total_turns),
            Err(_) => ("unknown".to_string(), 0.0, 0),
        };

        let agent_count = UserConfig::load(&agent_serve.config_path)
            .map(|cfg| cfg.agents.len())
            .unwrap_or(0);

        rooms.push(json!({
            "name": name,
            "work_dir": agent_serve.work_dir,
            "bus_socket": agent_serve.bus_socket,
            "status": status,
            "agent_count": agent_count,
            "cost_usd": cost_usd,
        }));
    }
    Ok(json!(rooms))
}

fn handle_room_children(params: &Value) -> Result<Value> {
    let room = params
        .get("room")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'room' parameter"))?;

    let serve_state = crate::config::ServeState::load()
        .ok_or_else(|| anyhow::anyhow!("no serve state found (deskd serve not running?)"))?;

    let agent_serve = serve_state
        .agent(room)
        .ok_or_else(|| anyhow::anyhow!("room '{}' not found in serve state", room))?;

    let user_config = UserConfig::load(&agent_serve.config_path)?;

    let children: Vec<Value> = user_config
        .agents
        .iter()
        .map(|sub| {
            let status = crate::app::agent::load_state(&sub.name)
                .map(|s| s.status)
                .unwrap_or_else(|_| "configured".to_string());
            json!({
                "name": sub.name,
                "model": sub.model,
                "status": status,
            })
        })
        .collect();

    Ok(json!(children))
}

fn handle_agent_requests(
    params: &Value,
    task_store: &(dyn TaskRepository + Send + Sync),
) -> Result<Value> {
    let agent = params
        .get("agent")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'agent' parameter"))?;

    let all_tasks = task_store.list(None)?;

    let matching: Vec<Value> = all_tasks
        .iter()
        .filter(|t| t.assignee.as_deref() == Some(agent) || t.created_by == agent)
        .map(|t| {
            json!({
                "id": t.id,
                "description": t.description,
                "turns": t.turns,
                "cost_usd": t.cost_usd,
                "status": t.status.to_string(),
                "created_at": t.created_at,
            })
        })
        .collect();

    Ok(json!(matching))
}

fn handle_agent_config_list() -> Result<Value> {
    let serve_state = crate::config::ServeState::load()
        .ok_or_else(|| anyhow::anyhow!("no serve state found (deskd serve not running?)"))?;

    let mut configs: Vec<Value> = Vec::new();
    for (name, agent_serve) in &serve_state.agents {
        let user_config = match UserConfig::load(&agent_serve.config_path) {
            Ok(cfg) => cfg,
            Err(_) => continue,
        };

        configs.push(json!({
            "name": name,
            "model": user_config.model,
            "system_prompt": user_config.system_prompt,
            "max_turns": user_config.max_turns,
        }));

        // Also include sub-agents from this room's config.
        for sub in &user_config.agents {
            configs.push(json!({
                "name": sub.name,
                "model": sub.model,
                "system_prompt": sub.system_prompt,
                "max_turns": null,
            }));
        }
    }

    Ok(json!(configs))
}

// ─── Mutation handlers ──────────────────────────────────────────────────────

async fn handle_send_message(params: &Value, bus_socket: &str, agent_name: &str) -> Result<Value> {
    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'target' parameter"))?;
    let payload = params
        .get("payload")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing 'payload' parameter"))?;
    let fresh = params
        .get("fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Use a one-shot bus connection to send the message.
    let sender = UnixBus::connect(bus_socket).await?;
    sender
        .register(&format!("{}-api-send", agent_name), &[])
        .await?;

    let msg = crate::domain::message::Message {
        id: uuid::Uuid::new_v4().to_string(),
        source: API_CLIENT_NAME.to_string(),
        target: target.to_string(),
        payload,
        reply_to: None,
        metadata: crate::domain::message::Metadata { priority: 5, fresh },
    };
    sender.send(&msg).await?;

    Ok(json!({"sent": true, "id": msg.id}))
}

fn handle_task_create(
    params: &Value,
    task_store: &(dyn TaskRepository + Send + Sync),
    agent_name: &str,
) -> Result<Value> {
    let description = params
        .get("description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'description' parameter"))?;
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let labels: Vec<String> = params
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let metadata = params.get("metadata").cloned().unwrap_or(Value::Null);

    let created =
        mcp_service::task_create(description, model, labels, metadata, agent_name, task_store)?;
    Ok(json!({
        "id": created.id,
        "description": created.description,
    }))
}

fn handle_task_cancel(
    params: &Value,
    task_store: &(dyn TaskRepository + Send + Sync),
) -> Result<Value> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'id' parameter"))?;
    let cancelled = mcp_service::task_cancel(id, task_store)?;
    Ok(json!({"id": cancelled.id, "cancelled": true}))
}

async fn handle_sm_create(
    params: &Value,
    bus_socket: &str,
    sm_store: &(dyn StateMachineRepository + Send + Sync),
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<Value> {
    let config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'model' parameter"))?;
    let title = params
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'title' parameter"))?;
    let body = params.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let metadata = params.get("metadata").cloned().unwrap_or(Value::Null);

    let created = mcp_service::sm_create(
        model, title, body, metadata, agent_name, bus_socket, config, sm_store,
    )
    .await?;

    Ok(json!({
        "id": created.id,
        "model": created.model,
        "state": created.state,
        "assignee": created.assignee,
    }))
}

async fn handle_sm_move(
    params: &Value,
    bus_socket: &str,
    sm_store: &(dyn StateMachineRepository + Send + Sync),
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<Value> {
    let config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let instance_id = params
        .get("instance_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'instance_id' parameter"))?;
    let to_state = params
        .get("to_state")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'to_state' parameter"))?;
    let note = params.get("note").and_then(|v| v.as_str());

    let moved = mcp_service::sm_move(
        instance_id,
        to_state,
        note,
        agent_name,
        bus_socket,
        config,
        sm_store,
    )
    .await?;

    Ok(json!({
        "id": moved.id,
        "state": moved.state,
        "model": moved.model,
    }))
}

fn handle_sm_cancel(
    params: &Value,
    sm_store: &(dyn StateMachineRepository + Send + Sync),
) -> Result<Value> {
    let instance_id = params
        .get("instance_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'instance_id' parameter"))?;
    sm_store.delete(instance_id)?;
    Ok(json!({"id": instance_id, "cancelled": true}))
}

fn handle_schedule_add(
    params: &Value,
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<Value> {
    let _config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let cron = params
        .get("cron")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'cron' parameter"))?;
    let action = params
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'action' parameter"))?;
    let target = params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing 'target' parameter"))?;

    // Validate cron expression.
    use std::str::FromStr;
    let _schedule = cron::Schedule::from_str(cron)
        .map_err(|e| anyhow::anyhow!("invalid cron expression: {}", e))?;

    // Persist by appending to deskd.yaml.
    let config_path = std::env::var("DESKD_AGENT_CONFIG")
        .unwrap_or_else(|_| format!("/home/{}/deskd.yaml", agent_name));

    let schedule_action = match action {
        "raw" => crate::config::ScheduleAction::Raw,
        "github_poll" => crate::config::ScheduleAction::GithubPoll,
        "shell" => crate::config::ScheduleAction::Shell,
        other => bail!("unknown schedule action: {}", other),
    };

    let schedule_config = params
        .get("config")
        .and_then(|v| serde_json::from_value::<serde_yaml::Value>(v.clone()).ok());

    let new_schedule = crate::config::ScheduleDef {
        cron: cron.to_string(),
        target: target.to_string(),
        action: schedule_action,
        config: schedule_config,
        timezone: None,
    };

    // Load, append, and save config.
    let mut cfg = UserConfig::load(&config_path).unwrap_or_default();
    let index = cfg.schedules.len();
    cfg.schedules.push(new_schedule);
    let yaml = serde_yaml::to_string(&cfg)?;
    std::fs::write(&config_path, yaml)?;

    info!(agent = %agent_name, index = index, cron = %cron, "schedule_add");

    Ok(json!({
        "index": index,
        "cron": cron,
        "target": target,
        "action": action,
    }))
}

fn handle_schedule_remove(
    params: &Value,
    user_config: Option<&UserConfig>,
    agent_name: &str,
) -> Result<Value> {
    let _config = user_config.ok_or_else(|| anyhow::anyhow!("no user config loaded"))?;
    let index = params
        .get("index")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("missing 'index' parameter"))? as usize;

    let config_path = std::env::var("DESKD_AGENT_CONFIG")
        .unwrap_or_else(|_| format!("/home/{}/deskd.yaml", agent_name));

    let mut cfg = UserConfig::load(&config_path).unwrap_or_default();

    if index >= cfg.schedules.len() {
        bail!(
            "schedule index {} out of range (have {} schedules)",
            index,
            cfg.schedules.len()
        );
    }

    let removed = cfg.schedules.remove(index);
    let yaml = serde_yaml::to_string(&cfg)?;
    std::fs::write(&config_path, yaml)?;

    info!(agent = %agent_name, index = index, cron = %removed.cron, "schedule_remove");

    Ok(json!({
        "index": index,
        "removed": true,
        "cron": removed.cron,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatch_unknown_method() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let task_store =
            crate::app::task::TaskStore::new(std::path::PathBuf::from("/tmp/bus_api_test_tasks"));
        let sm_store = crate::app::statemachine::StateMachineStore::new(std::path::PathBuf::from(
            "/tmp/bus_api_test_sm",
        ));

        let result = rt.block_on(dispatch(
            "nonexistent_method",
            &json!({}),
            "/tmp/nonexistent.sock",
            &task_store,
            &sm_store,
            None,
            "test",
        ));

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown method"));
    }

    #[test]
    fn test_task_list_empty() {
        let dir = std::path::PathBuf::from("/tmp/deskd_bus_api_test_tasks_empty");
        let _ = std::fs::remove_dir_all(&dir);
        let task_store = crate::app::task::TaskStore::new(dir);
        let result = handle_task_list(&json!({}), &task_store);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), json!([]));
    }

    #[test]
    fn test_task_detail_missing_id() {
        let dir = std::path::PathBuf::from("/tmp/deskd_bus_api_test_tasks_detail");
        let _ = std::fs::remove_dir_all(&dir);
        let task_store = crate::app::task::TaskStore::new(dir);
        let result = handle_task_detail(&json!({}), &task_store);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'id'"));
    }

    #[test]
    fn test_sm_models_no_config() {
        let result = handle_sm_models(None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no user config"));
    }

    #[test]
    fn test_schedule_list_no_config() {
        let result = handle_schedule_list(None);
        assert!(result.is_err());
    }

    #[test]
    fn test_inbox_read_missing_inbox() {
        let result = handle_inbox_read(&json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'inbox'"));
    }

    #[test]
    fn test_inbox_search_missing_query() {
        let result = handle_inbox_search(&json!({}));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'query'"));
    }
}
