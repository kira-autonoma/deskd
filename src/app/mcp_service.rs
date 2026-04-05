//! Business logic extracted from MCP handlers.
//!
//! Each public function here performs the core operation — argument validation,
//! store access, side-effects — returning a domain result. The MCP layer
//! (`mcp.rs`) stays thin: parse JSON args, call into this module, format the
//! JSON-RPC response.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::app::statemachine;
use crate::app::unified_inbox;
use crate::config::UserConfig;

// ─── Reminder ────────────────────────────────────────────────────────────────

/// Result of creating a reminder.
pub struct ReminderCreated {
    pub target: String,
    pub fire_at: String,
    pub delay_minutes: f64,
}

/// Create a one-shot reminder persisted to disk.
pub fn create_reminder(target: &str, message: &str, delay_minutes: f64) -> Result<ReminderCreated> {
    let fire_at =
        chrono::Utc::now() + chrono::Duration::seconds((delay_minutes * 60.0).round() as i64);

    let remind = crate::config::RemindDef {
        at: fire_at.to_rfc3339(),
        target: target.to_string(),
        message: message.to_string(),
    };

    let dir = crate::config::reminders_dir();
    let filename = format!("{}.json", uuid::Uuid::new_v4());
    let path = dir.join(&filename);

    let json = serde_json::to_string_pretty(&remind).context("failed to serialize reminder")?;
    std::fs::write(&path, json)
        .with_context(|| format!("failed to write reminder file: {}", path.display()))?;

    info!(target = %target, at = %fire_at, "create_reminder");

    Ok(ReminderCreated {
        target: target.to_string(),
        fire_at: fire_at.to_rfc3339(),
        delay_minutes,
    })
}

// ─── Unified inbox ───────────────────────────────────────────────────────────

/// List all inboxes with message counts.
pub fn list_inboxes() -> Result<Vec<Value>> {
    let inboxes = unified_inbox::list_inboxes().context("failed to list inboxes")?;
    Ok(inboxes
        .iter()
        .map(|(name, count)| {
            json!({
                "inbox": name,
                "messages": count,
            })
        })
        .collect())
}

/// Read messages from a unified inbox, returning JSON-ready values.
pub fn read_inbox(
    inbox: &str,
    limit: usize,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<Value>> {
    let messages =
        unified_inbox::read_messages(inbox, limit, since).context("failed to read inbox")?;
    Ok(messages
        .iter()
        .map(|m| {
            json!({
                "ts": m.ts.to_rfc3339(),
                "source": m.source,
                "from": m.from,
                "text": m.text,
                "metadata": m.metadata,
            })
        })
        .collect())
}

/// Search messages across inboxes, returning JSON-ready values.
pub fn search_inbox(inbox: Option<&str>, query: &str, limit: usize) -> Result<Vec<Value>> {
    let results =
        unified_inbox::search_messages(inbox, query, limit).context("failed to search inbox")?;
    Ok(results
        .iter()
        .map(|m| {
            json!({
                "ts": m.ts.to_rfc3339(),
                "source": m.source,
                "from": m.from,
                "text": m.text,
                "metadata": m.metadata,
            })
        })
        .collect())
}

// ─── Graph execution ─────────────────────────────────────────────────────────

/// Resolve graph file path to absolute, load YAML, execute, and return summary.
pub async fn run_graph(file: &str, work_dir: Option<&str>, vars: Option<&Value>) -> Result<String> {
    let file_path = Path::new(file);
    let abs_path = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        let cwd = std::env::var("PWD").unwrap_or_else(|_| ".".to_string());
        Path::new(&cwd).join(file_path)
    };

    let effective_work_dir = if let Some(wd) = work_dir {
        PathBuf::from(wd)
    } else {
        abs_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };

    let yaml = std::fs::read_to_string(&abs_path)
        .with_context(|| format!("failed to read graph file: {}", abs_path.display()))?;
    let graph_def: crate::app::graph::GraphDef = serde_yaml::from_str(&yaml)
        .with_context(|| format!("failed to parse graph YAML: {}", abs_path.display()))?;

    let inputs: Option<HashMap<String, String>> = vars.and_then(|v| v.as_object()).map(|obj| {
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });

    info!(graph = %graph_def.graph, steps = graph_def.steps.len(), "run_graph");

    let ctx = crate::app::graph::execute(&graph_def, &effective_work_dir, None, inputs)
        .await
        .with_context(|| format!("graph execution failed: {}", graph_def.graph))?;

    // Build summary.
    let mut summary = format!(
        "Graph '{}' completed: {} steps executed\n",
        graph_def.graph,
        ctx.results.len()
    );

    for step in &graph_def.steps {
        if let Some(result) = ctx.results.get(&step.id) {
            let status = if result.skipped { "SKIP" } else { "DONE" };
            summary.push_str(&format!(
                "  [{status}] {} ({}ms)\n",
                result.id, result.duration_ms
            ));

            for tr in &result.tool_results {
                if !tr.stdout.is_empty() && !tr.skipped {
                    let output = if tr.stdout.len() > 500 {
                        format!("{}...", &tr.stdout[..500])
                    } else {
                        tr.stdout.clone()
                    };
                    summary.push_str(&format!("    {}: {}\n", tr.tool, output.trim()));
                }
                if !tr.stderr.is_empty() && tr.exit_code != 0 {
                    let err_output = if tr.stderr.len() > 200 {
                        format!("{}...", &tr.stderr[..200])
                    } else {
                        tr.stderr.clone()
                    };
                    summary.push_str(&format!("    {} stderr: {}\n", tr.tool, err_output.trim()));
                }
            }

            if let Some(ref llm_out) = result.llm_output {
                let display = if llm_out.len() > 500 {
                    format!("{}...", &llm_out[..500])
                } else {
                    llm_out.clone()
                };
                summary.push_str(&format!("    LLM: {}\n", display.trim()));
            }
        }
    }

    if !ctx.variables.is_empty() {
        summary.push_str("\nVariables:\n");
        for (k, v) in &ctx.variables {
            let display = if v.len() > 200 {
                format!("{}...", &v[..200])
            } else {
                v.clone()
            };
            summary.push_str(&format!("  {}: {}\n", k, display));
        }
    }

    Ok(summary)
}

// ─── Task queue ──────────────────────────────────────────────────────────────

/// Result of creating a task.
pub struct TaskCreated {
    pub id: String,
    pub description: String,
}

/// Create a task in the pull-based queue.
pub fn task_create(
    description: &str,
    model: Option<String>,
    labels: Vec<String>,
    metadata: Value,
    created_by: &str,
) -> Result<TaskCreated> {
    let store = crate::app::task::TaskStore::default_for_home();
    let criteria = crate::app::task::TaskCriteria { model, labels };
    let task = if metadata.is_null() {
        store.create(description, criteria, created_by)?
    } else {
        store.create_with_metadata(description, criteria, created_by, metadata)?
    };

    info!(agent = %created_by, task_id = %task.id, "task_create");

    Ok(TaskCreated {
        id: task.id,
        description: task.description,
    })
}

/// Parse a task status string into the domain enum.
fn parse_task_status(s: &str) -> Option<crate::app::task::TaskStatus> {
    match s {
        "pending" => Some(crate::app::task::TaskStatus::Pending),
        "active" => Some(crate::app::task::TaskStatus::Active),
        "done" => Some(crate::app::task::TaskStatus::Done),
        "failed" => Some(crate::app::task::TaskStatus::Failed),
        "cancelled" => Some(crate::app::task::TaskStatus::Cancelled),
        "dead_letter" => Some(crate::app::task::TaskStatus::DeadLetter),
        _ => None,
    }
}

/// List tasks, optionally filtered by status string.
pub fn task_list(status_filter: Option<&str>) -> Result<Vec<Value>> {
    let filter = status_filter.and_then(parse_task_status);
    let store = crate::app::task::TaskStore::default_for_home();
    let tasks = store.list(filter)?;

    Ok(tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "description": t.description,
                "status": t.status.to_string(),
                "assignee": t.assignee,
                "created_by": t.created_by,
                "created_at": t.created_at,
                "sm_instance_id": t.sm_instance_id,
            })
        })
        .collect())
}

/// Cancel a pending task by ID.
pub struct TaskCancelled {
    pub id: String,
}

pub fn task_cancel(id: &str) -> Result<TaskCancelled> {
    let store = crate::app::task::TaskStore::default_for_home();
    let task = store.cancel(id)?;
    info!(task_id = %task.id, "task_cancel");
    Ok(TaskCancelled { id: task.id })
}

// ─── State machine ───────────────────────────────────────────────────────────

/// Result of creating an SM instance.
pub struct SmCreated {
    pub id: String,
    pub model: String,
    pub state: String,
    pub assignee: String,
}

/// Create a new state machine instance. If the initial state has an assignee,
/// dispatches the first task to the bus.
pub async fn sm_create(
    model_name: &str,
    title: &str,
    body: &str,
    metadata: Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: &UserConfig,
) -> Result<SmCreated> {
    let model: statemachine::ModelDef = user_config
        .models
        .iter()
        .find(|m| m.name == model_name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found", model_name))?
        .into();

    let store = statemachine::StateMachineStore::default_for_home();
    let mut inst = store.create(&model, title, body, agent_name)?;
    if !metadata.is_null() {
        inst.metadata = metadata;
        store.save(&inst)?;
    }
    info!(agent = %agent_name, instance = %inst.id, model = %model_name, "sm_create");

    // If the initial state has an assignee, dispatch the first task via the bus.
    if !inst.assignee.is_empty() {
        dispatch_sm_task(bus_socket, agent_name, &inst).await?;
    }

    Ok(SmCreated {
        id: inst.id,
        model: inst.model,
        state: inst.state,
        assignee: inst.assignee,
    })
}

/// Dispatch an SM task to the bus for the current assignee.
async fn dispatch_sm_task(
    bus_socket: &str,
    agent_name: &str,
    inst: &statemachine::Instance,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use uuid::Uuid;

    let task_text = format!(
        "---\n## Task: {}\n\n{}\n\n---\n## Metadata\ninstance_id: {}\nmodel: {}\nstate: {}",
        inst.title, inst.body, inst.id, inst.model, inst.state
    );

    let mut stream = UnixStream::connect(bus_socket)
        .await
        .with_context(|| format!("failed to connect to bus at {}", bus_socket))?;

    let reg = serde_json::json!({
        "type": "register",
        "name": format!("{}-mcp-sm", agent_name),
        "subscriptions": []
    });
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": "workflow-engine",
        "target": &inst.assignee,
        "payload": {
            "task": task_text,
            "sm_instance_id": inst.id,
        },
        "reply_to": format!("sm:{}", inst.id),
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;
    stream.flush().await?;
    info!(instance = %inst.id, assignee = %inst.assignee, "dispatched initial task");

    Ok(())
}

/// Result of moving an SM instance.
pub struct SmMoved {
    pub id: String,
    pub state: String,
    pub model: String,
}

/// Move a state machine instance to a new state and notify the workflow engine.
pub async fn sm_move(
    id: &str,
    state: &str,
    note: Option<&str>,
    agent_name: &str,
    bus_socket: &str,
    user_config: &UserConfig,
) -> Result<SmMoved> {
    let store = statemachine::StateMachineStore::default_for_home();
    let mut inst = store.load(id)?;
    let model: statemachine::ModelDef = user_config
        .models
        .iter()
        .find(|m| m.name == inst.model)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("model '{}' not found in config", inst.model))?
        .into();

    let from = inst.state.clone();
    store.move_to(&mut inst, &model, state, agent_name, note, None, None)?;
    info!(agent = %agent_name, instance = %id, from = %from, to = %state, "sm_move");

    // Notify workflow engine to dispatch if the new state has an assignee.
    if !inst.assignee.is_empty()
        && !statemachine::is_terminal(&model, &inst)
        && let Err(e) = crate::app::workflow::notify_moved(bus_socket, id, agent_name).await
    {
        tracing::warn!(agent = %agent_name, instance = %id, error = %e, "failed to notify workflow engine after sm_move");
    }

    Ok(SmMoved {
        id: id.to_string(),
        state: inst.state,
        model: inst.model,
    })
}

/// Query SM instances — either by ID or with optional model/state filters.
pub fn sm_query(
    id: Option<&str>,
    model_filter: Option<&str>,
    state_filter: Option<&str>,
) -> Result<Value> {
    let store = statemachine::StateMachineStore::default_for_home();

    if let Some(id) = id {
        let inst = store.load(id)?;
        let dto: crate::infra::dto::StoredInstance = (&inst).into();
        return Ok(serde_json::to_value(&dto)?);
    }

    let mut instances = store.list_all()?;
    if let Some(mf) = model_filter {
        instances.retain(|i| i.model == mf);
    }
    if let Some(sf) = state_filter {
        instances.retain(|i| i.state == sf);
    }

    let summary: Vec<Value> = instances
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "model": i.model,
                "title": i.title,
                "state": i.state,
                "assignee": i.assignee,
                "updated_at": i.updated_at,
            })
        })
        .collect();

    Ok(json!(summary))
}

// ─── Agent listing ───────────────────────────────────────────────────────────

/// Build a JSON summary for a sub-agent, given its name and worker handle status.
pub fn build_agent_summary(name: &str, is_finished: bool) -> Value {
    let (model, turns, cost_usd) = match crate::app::agent::load_state(name) {
        Ok(state) => (
            state.config.model.clone(),
            state.total_turns,
            state.total_cost,
        ),
        Err(_) => ("unknown".to_string(), 0, 0.0),
    };

    let status = if is_finished { "finished" } else { "running" };

    json!({
        "name": name,
        "model": model,
        "status": status,
        "turns": turns,
        "cost_usd": cost_usd,
    })
}

/// Validate that a target agent is a sub-agent of the caller before removal.
pub fn validate_remove_agent(name: &str, caller: &str) -> Result<crate::app::agent::AgentState> {
    let state = crate::app::agent::load_state(name)
        .with_context(|| format!("agent '{}' not found", name))?;

    match &state.parent {
        Some(parent) if parent == caller => Ok(state),
        _ => bail!(
            "agent '{}' is not a sub-agent of '{}' — removal denied",
            name,
            caller
        ),
    }
}

/// Gracefully stop a sub-agent process (SIGTERM, then SIGKILL after timeout).
pub async fn stop_agent_process(name: &str, pid: u32) {
    if pid == 0 {
        return;
    }

    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    info!(agent = %name, pid = pid, "sent SIGTERM, waiting for exit");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(agent = %name, pid = pid, "process did not exit in 30s, force killing");
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
