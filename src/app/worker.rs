use anyhow::{Context, Result, bail};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::app::acp;
use crate::app::agent::{self, Executor, TokenUsage};
use crate::app::message::Message;
use crate::app::tasklog;
use crate::app::unified_inbox;
use crate::domain::agent::AgentRuntime;
use crate::domain::events::DomainEvent;
use crate::infra::dto::ConfigSessionMode;

/// Create an executor for the given runtime type.
///
/// Returns `Box<dyn Executor>` — the worker has no knowledge of the concrete
/// backend type. Adding a new executor (Gemini, Ollama) requires only a new
/// `Executor` impl and a match arm here.
async fn start_executor(
    name: &str,
    bus_socket: &str,
    runtime: &AgentRuntime,
) -> Result<Box<dyn Executor>> {
    match runtime {
        AgentRuntime::Claude => {
            let p = agent::AgentProcess::start(name, bus_socket).await?;
            Ok(Box::new(p))
        }
        AgentRuntime::Acp => {
            let p = acp::AcpProcess::start(name, bus_socket).await?;
            Ok(Box::new(p))
        }
    }
}

/// Create a fresh-session executor (no --resume).
async fn start_executor_fresh(
    name: &str,
    bus_socket: &str,
    runtime: &AgentRuntime,
) -> Result<Box<dyn Executor>> {
    match runtime {
        AgentRuntime::Claude => {
            let p = agent::AgentProcess::start_fresh(name, bus_socket).await?;
            Ok(Box::new(p))
        }
        AgentRuntime::Acp => {
            let p = acp::AcpProcess::start_fresh(name, bus_socket).await?;
            Ok(Box::new(p))
        }
    }
}

/// Connect to the bus, register, and return the stream.
///
/// Retries up to 10 times with exponential backoff (100ms initial delay,
/// doubling each attempt) to handle the race where the worker starts
/// before the bus is listening on the socket.
pub async fn bus_connect(
    socket_path: &str,
    name: &str,
    subscriptions: Vec<String>,
) -> Result<UnixStream> {
    let max_retries = 10u32;
    let initial_delay = std::time::Duration::from_millis(100);

    let mut stream = None;
    let mut last_err = None;
    for attempt in 0..max_retries {
        match UnixStream::connect(socket_path).await {
            Ok(s) => {
                if attempt > 0 {
                    info!(agent = %name, attempt = attempt + 1, "connected to bus after retry");
                }
                stream = Some(s);
                break;
            }
            Err(e) => {
                if attempt + 1 < max_retries {
                    let delay = initial_delay * 2u32.saturating_pow(attempt);
                    warn!(
                        agent = %name,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "bus not ready, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
                last_err = Some(e);
            }
        }
    }

    let mut stream = match stream {
        Some(s) => s,
        None => {
            return Err(last_err.unwrap()).with_context(|| {
                format!(
                    "Failed to connect to bus at {} after {} attempts",
                    socket_path, max_retries
                )
            });
        }
    };

    let envelope = serde_json::json!({
        "type": "register",
        "name": name,
        "subscriptions": subscriptions,
    });
    let mut line = serde_json::to_string(&envelope)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    info!(agent = %name, "registered on bus");
    Ok(stream)
}

/// Helper: write an inbox entry for a completed task.
fn write_inbox(
    name: &str,
    msg: &Message,
    task: &str,
    result: Option<String>,
    error: Option<String>,
) {
    let text = if let Some(ref err) = error {
        format!("ERROR: {}", err)
    } else if let Some(ref res) = result {
        res.clone()
    } else {
        "(no output)".to_string()
    };

    let inbox_name = format!("replies/{}", msg.source);
    let inbox_msg = unified_inbox::InboxMessage {
        ts: chrono::Utc::now(),
        source: format!("agent:{}", name),
        from: Some(name.to_string()),
        text,
        metadata: serde_json::json!({
            "type": "task_result",
            "agent": name,
            "task": task,
            "message_id": msg.id,
            "in_reply_to": msg.id,
            "has_error": error.is_some(),
        }),
    };
    if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
        warn!(agent = %name, error = %e, "failed to write inbox entry");
    }
}

/// Run the agent worker loop: read messages from bus, execute tasks, post results.
/// `bus_socket`: the agent's bus socket path, injected as DESKD_BUS_SOCKET into
/// the claude subprocess so the MCP server can connect to the bus.
pub async fn run(
    name: &str,
    socket_path: &str,
    bus_socket: Option<String>,
    subscriptions: Option<Vec<String>>,
) -> Result<()> {
    let initial_state = agent::load_state(name)?;
    let budget_usd = initial_state.config.budget_usd;

    // Use custom subscriptions if provided, otherwise default.
    // Default subscriptions: Workers receive:
    //   agent:<name>     — direct messages (from MCP send_message, other agents, CLI)
    //   queue:tasks      — shared task queue
    //   telegram.in:*    — messages arriving from Telegram adapter
    let subscriptions = subscriptions.unwrap_or_else(|| {
        vec![
            format!("agent:{}", name),
            "queue:tasks".to_string(),
            "telegram.in:*".to_string(),
        ]
    });

    let stream = bus_connect(socket_path, name, subscriptions).await?;
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));

    // Start persistent agent process (reused across tasks).
    let effective_bus = bus_socket.as_deref().unwrap_or(socket_path).to_string();
    let agent_runtime: AgentRuntime = initial_state.config.runtime.clone().into();
    let mut process = start_executor(name, &effective_bus, &agent_runtime).await?;

    // Build task limits from agent config — enforced in real-time during tasks.
    let limits = agent::TaskLimits {
        max_turns: if initial_state.config.max_turns > 0 {
            Some(initial_state.config.max_turns)
        } else {
            None
        },
        budget_usd: Some(budget_usd),
    };

    info!(agent = %name, runtime = ?agent_runtime, "agent process ready, waiting for tasks");

    // Startup recovery: handle orphaned active tasks assigned to this agent.
    recover_orphaned_tasks(name);

    // Task queue polling interval (30 seconds).
    let mut queue_poll = tokio::time::interval(std::time::Duration::from_secs(30));
    queue_poll.tick().await; // skip first immediate tick

    let agent_model = initial_state.config.model.clone();
    let agent_labels: Vec<String> = Vec::new(); // TODO: add labels to AgentConfig

    loop {
        // Wait for either a bus message or a queue poll tick.
        let line = tokio::select! {
            result = lines.next_line() => {
                match result? {
                    Some(l) => l,
                    None => break, // bus closed
                }
            }
            _ = queue_poll.tick() => {
                // Poll the task queue for pending tasks matching this worker.
                let store = crate::app::task::TaskStore::default_for_home();
                if let Ok(Some(task)) = store.claim_next(name, &agent_model, &agent_labels) {
                    info!(
                        agent = %name,
                        task_id = %task.id,
                        description = %truncate(&task.description, 80),
                        "claimed task from queue"
                    );
                    // Synthesize a bus message from the claimed task.
                    let mut payload = serde_json::json!({
                        "task": task.description,
                        "task_queue_id": task.id,
                    });
                    if let Some(ref sm_id) = task.sm_instance_id {
                        payload["sm_instance_id"] = serde_json::json!(sm_id);
                    }
                    let synthetic = serde_json::json!({
                        "type": "message",
                        "id": format!("queue-{}", task.id),
                        "source": format!("task-queue:{}", task.created_by),
                        "target": format!("agent:{}", name),
                        "payload": payload,
                    });
                    serde_json::to_string(&synthetic).unwrap_or_default()
                } else {
                    continue; // no tasks available
                }
            }
        };

        if line.is_empty() {
            continue;
        }

        let msg: Message = match serde_json::from_str::<crate::infra::dto::BusMessage>(&line) {
            Ok(dto) => dto.into(),
            Err(e) => {
                warn!(agent = %name, error = %e, "invalid message from bus");
                continue;
            }
        };

        // Extract task context from the message.
        let ctx = match extract_task_context(&msg, name) {
            Some(c) => c,
            None => {
                debug!(agent = %name, "message has no task payload, skipping");
                // Log empty message with minimal context.
                let empty_log = tasklog::TaskLog {
                    ts: chrono::Utc::now().to_rfc3339(),
                    source: msg.source.clone(),
                    turns: 0,
                    cost: 0.0,
                    duration_ms: 0,
                    status: "empty".to_string(),
                    task: String::new(),
                    error: None,
                    msg_id: msg.id.clone(),
                    github_repo: msg
                        .payload
                        .get("github_repo")
                        .and_then(|r| r.as_str())
                        .map(str::to_string),
                    github_pr: msg.payload.get("github_pr").and_then(|p| p.as_u64()),
                    input_tokens: None,
                    output_tokens: None,
                };
                if let Err(e) = tasklog::log_task(name, &empty_log) {
                    warn!(agent = %name, error = %e, "failed to write task log");
                }
                continue;
            }
        };

        // Check budget.
        if let Some(budget_error) = check_budget(name, budget_usd) {
            log_skip(name, &msg, &ctx, "skip", Some("budget exceeded"));
            let reply_target = msg.reply_to.as_deref().unwrap_or(&msg.source);
            write_bus_envelope(
                &writer,
                name,
                reply_target,
                serde_json::json!({"error": budget_error, "in_reply_to": msg.id}),
            )
            .await;
            continue;
        }

        // Write to unified inbox (skip Telegram — adapter already writes those).
        if !msg.source.starts_with("telegram-") {
            let inbox_name = format!("agent/{}", name);
            let inbox_msg = unified_inbox::InboxMessage {
                ts: chrono::Utc::now(),
                source: msg.source.clone(),
                from: None,
                text: ctx.task_raw.clone(),
                metadata: serde_json::json!({
                    "target": msg.target,
                    "message_id": msg.id,
                }),
            };
            if let Err(e) = unified_inbox::write_message(&inbox_name, &inbox_msg) {
                warn!(agent = %name, error = %e, "failed to write to unified inbox");
            }
        }

        // Fresh session if requested or agent is ephemeral.
        let needs_fresh =
            msg.metadata.fresh || initial_state.config.session == ConfigSessionMode::Ephemeral;
        if needs_fresh {
            info!(agent = %name, fresh = msg.metadata.fresh, "fresh session requested, restarting process");
            process.stop().await;
            process = start_executor_fresh(name, &effective_bus, &agent_runtime).await?;
        }

        let task = &ctx.task_formatted;
        info!(agent = %name, source = %msg.source, task = %truncate(task, 80), "processing task");

        // Mark agent as working.
        if let Ok(mut st) = agent::load_state(name) {
            st.status = "working".to_string();
            st.current_task = truncate(task, 80).to_string();
            let _ = agent::save_state_pub(&st);
        }

        // Start Telegram typing/progress indicators if applicable.
        let telegram_progress = if let Some(chat_id) = ctx.telegram_chat_id {
            Some(TelegramProgress::start(chat_id, &writer, name).await)
        } else {
            None
        };

        // Stream progress blocks to the bus as they arrive.
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer_fwd = writer.clone();
        let name_owned = name.to_string();
        let reply_owned = ctx.reply_target.clone();
        let msg_id_owned = msg.id.clone();
        let fwd_chat_id = ctx.telegram_chat_id;
        let fwd_task = tokio::spawn(async move {
            let mut full_response = String::new();
            let mut typing_stopped = false;
            while let Some(text) = progress_rx.recv().await {
                if !typing_stopped {
                    if let Some(chat_id) = fwd_chat_id {
                        let ctrl_target = format!("telegram.ctrl:{}", chat_id);
                        write_bus_envelope(
                            &writer_fwd,
                            &name_owned,
                            &ctrl_target,
                            serde_json::json!({"progress_done": true}),
                        )
                        .await;
                        write_bus_envelope(
                            &writer_fwd,
                            &name_owned,
                            &ctrl_target,
                            serde_json::json!({"typing": false}),
                        )
                        .await;
                    }
                    typing_stopped = true;
                }
                full_response.push_str(&text);
                if !text.trim().is_empty() {
                    write_bus_envelope(
                        &writer_fwd,
                        &name_owned,
                        &reply_owned,
                        serde_json::json!({"result": text, "in_reply_to": msg_id_owned}),
                    )
                    .await;
                }
            }
            full_response
        });

        let image = ctx
            .image_base64
            .as_deref()
            .zip(ctx.image_media_type.as_deref());

        let task_start = std::time::Instant::now();

        // Use the persistent process. send_task writes to its stdin and reads
        // stdout events until the result event marks the turn complete.
        let mut task_fut = Box::pin(process.send_task(task, Some(&progress_tx), image, &limits));

        // Concurrently await task completion OR new bus messages for injection.
        let result = loop {
            tokio::select! {
                task_result = &mut task_fut => {
                    break task_result;
                }
                Ok(Some(bus_line)) = lines.next_line() => {
                    if bus_line.is_empty() {
                        continue;
                    }
                    if let Ok(dto) = serde_json::from_str::<crate::infra::dto::BusMessage>(&bus_line) {
                        let inject_msg: Message = dto.into();
                        let inject_task = inject_msg
                            .payload
                            .get("task")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default();
                        if !inject_task.is_empty() {
                            info!(
                                agent = %name,
                                source = %inject_msg.source,
                                task = %truncate(inject_task, 80),
                                "injecting mid-task message"
                            );
                            let _ = process.inject_message(inject_task);
                            // Restart typing indicator for the new message
                            if let Some(chat_id) = ctx.telegram_chat_id {
                                let ctrl_target = format!("telegram.ctrl:{}", chat_id);
                                write_bus_envelope(
                                    &writer,
                                    name,
                                    &ctrl_target,
                                    serde_json::json!({"typing": true}),
                                )
                                .await;
                                write_bus_envelope(
                                    &writer,
                                    name,
                                    &ctrl_target,
                                    serde_json::json!({"progress_start": true}),
                                )
                                .await;
                            }
                        }
                    } else {
                        warn!(agent = %name, "invalid message from bus during task, skipping");
                    }
                }
            }
        };

        // Drop the future to release borrows on process and progress_tx.
        drop(task_fut);
        drop(progress_tx);
        let full_response = fwd_task.await.unwrap_or_default();

        // Stop Telegram progress indicators.
        if let Some(tp) = telegram_progress {
            tp.stop(&writer, name).await;
        }

        set_idle(name);
        let task_duration_secs = task_start.elapsed().as_secs();
        let task_duration_ms = task_start.elapsed().as_millis() as u64;

        match result {
            Ok(ref turn) => {
                handle_task_success(
                    name,
                    &msg,
                    &ctx,
                    turn,
                    full_response,
                    task_duration_ms,
                    task_duration_secs,
                    &initial_state.config.work_dir,
                    &initial_state.config.model,
                    &writer,
                )
                .await;
            }
            Err(ref e) => {
                let needs_restart =
                    handle_task_failure(name, &msg, &ctx, e, task_duration_ms, &writer).await;
                if needs_restart {
                    warn!(agent = %name, "agent process crashed, restarting with fresh session");
                    match start_executor_fresh(name, &effective_bus, &agent_runtime).await {
                        Ok(new_proc) => {
                            process = new_proc;
                            info!(agent = %name, "agent process restarted (fresh session)");
                        }
                        Err(re) => {
                            warn!(agent = %name, error = %re, "failed to restart agent process");
                        }
                    }
                }
            }
        }
    }

    process.stop().await;
    info!(agent = %name, "disconnected from bus");
    Ok(())
}

/// Log token usage for a completed task to a JSONL file.
#[allow(clippy::too_many_arguments)]
fn log_token_usage(
    work_dir: &str,
    agent_name: &str,
    source: &str,
    task: &str,
    usage: &TokenUsage,
    duration_secs: u64,
    model: &str,
    github_repo: Option<&str>,
    github_pr: Option<u64>,
) {
    let deskd_dir = std::path::Path::new(work_dir).join(".deskd");
    if let Err(e) = std::fs::create_dir_all(&deskd_dir) {
        warn!(agent = %agent_name, error = %e, "failed to create .deskd dir for usage log");
        return;
    }

    let usage_path = deskd_dir.join("usage.jsonl");
    let truncated_task = if task.len() > 80 {
        let mut end = 80;
        while end > 0 && !task.is_char_boundary(end) {
            end -= 1;
        }
        &task[..end]
    } else {
        task
    };

    let mut entry = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "agent": agent_name,
        "source": source,
        "task": truncated_task,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read": usage.cache_read_input_tokens,
        "cache_creation": usage.cache_creation_input_tokens,
        "duration_secs": duration_secs,
        "model": model,
    });
    if let Some(repo) = github_repo {
        entry["github_repo"] = serde_json::json!(repo);
    }
    if let Some(pr) = github_pr {
        entry["github_pr"] = serde_json::json!(pr);
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&usage_path)
    {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", entry) {
                warn!(agent = %agent_name, error = %e, "failed to write usage log");
            }
        }
        Err(e) => {
            warn!(agent = %agent_name, error = %e, "failed to open usage log");
        }
    }

    info!(
        agent = %agent_name,
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cache_read = usage.cache_read_input_tokens,
        cache_creation = usage.cache_creation_input_tokens,
        duration_secs = duration_secs,
        "token usage"
    );
}

/// Mark agent as idle in its state file.
fn set_idle(name: &str) {
    if let Ok(mut st) = agent::load_state(name) {
        st.status = "idle".to_string();
        st.current_task = String::new();
        let _ = agent::save_state_pub(&st);
    }
}

// ─── Extracted helpers for worker decomposition (#141) ───────────────────────

/// Context for a single task extracted from a bus message.
struct TaskContext {
    task_raw: String,
    task_formatted: String,
    task_queue_id: Option<String>,
    sm_instance_id: Option<String>,
    reply_target: String,
    telegram_chat_id: Option<i64>,
    github_repo: Option<String>,
    github_pr: Option<u64>,
    image_base64: Option<String>,
    image_media_type: Option<String>,
}

/// Extract task context from a bus message. Returns None if the message has no task.
fn extract_task_context(msg: &Message, _name: &str) -> Option<TaskContext> {
    let github_repo = msg
        .payload
        .get("github_repo")
        .and_then(|r| r.as_str())
        .map(str::to_string);
    let github_pr = msg.payload.get("github_pr").and_then(|p| p.as_u64());

    let task_raw = msg
        .payload
        .get("task")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();

    if task_raw.is_empty() {
        return None;
    }

    let task_queue_id = msg
        .payload
        .get("task_queue_id")
        .and_then(|t| t.as_str())
        .map(str::to_string);

    // Format task with source context.
    let task_formatted =
        if let Some(chat_id) = msg.payload.get("telegram_chat_id").and_then(|v| v.as_i64()) {
            let label = msg
                .payload
                .get("telegram_chat_name")
                .and_then(|v| v.as_str())
                .map(|n| format!("{} ({})", n, chat_id))
                .unwrap_or_else(|| chat_id.to_string());
            let quoted = msg
                .payload
                .get("telegram_reply_to_text")
                .and_then(|v| v.as_str());
            if let Some(q) = quoted {
                format!("[Telegram: {}]\n> {}\n\n{}", label, q, task_raw)
            } else {
                format!("[Telegram: {}]\n{}", label, task_raw)
            }
        } else {
            format!("[source: {}]\n{}", msg.source, task_raw)
        };

    // Determine reply target.
    let reply_target =
        if let Some(sm_id) = msg.payload.get("sm_instance_id").and_then(|v| v.as_str()) {
            format!("sm:{}", sm_id)
        } else {
            msg.reply_to.as_deref().unwrap_or(&msg.source).to_string()
        };

    let telegram_chat_id = if reply_target.starts_with("telegram.out:") {
        reply_target
            .strip_prefix("telegram.out:")
            .and_then(|id| id.parse::<i64>().ok())
    } else {
        None
    };

    let image_base64 = msg
        .payload
        .get("image_base64")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let image_media_type = msg
        .payload
        .get("image_media_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let sm_instance_id = msg
        .payload
        .get("sm_instance_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    Some(TaskContext {
        task_raw,
        task_formatted,
        task_queue_id,
        sm_instance_id,
        reply_target,
        telegram_chat_id,
        github_repo,
        github_pr,
        image_base64,
        image_media_type,
    })
}

/// Check if budget is exceeded. Returns Some(error_message) if over budget.
fn check_budget(name: &str, budget_usd: f64) -> Option<String> {
    let current_state = agent::load_state(name).ok()?;
    if current_state.total_cost >= budget_usd {
        warn!(
            agent = %name,
            cost = current_state.total_cost,
            budget = budget_usd,
            "budget exceeded, rejecting task"
        );
        Some(format!(
            "Budget limit reached (${:.2} / ${:.2}). Task not processed.",
            current_state.total_cost, budget_usd,
        ))
    } else {
        None
    }
}

/// Log a skipped task (budget exceeded or empty payload).
fn log_skip(name: &str, msg: &Message, ctx: &TaskContext, status: &str, error: Option<&str>) {
    let log_entry = tasklog::TaskLog {
        ts: chrono::Utc::now().to_rfc3339(),
        source: msg.source.clone(),
        turns: 0,
        cost: 0.0,
        duration_ms: 0,
        status: status.to_string(),
        task: tasklog::truncate_task(&ctx.task_raw, 60),
        error: error.map(str::to_string),
        msg_id: msg.id.clone(),
        github_repo: ctx.github_repo.clone(),
        github_pr: ctx.github_pr,
        input_tokens: None,
        output_tokens: None,
    };
    if let Err(e) = tasklog::log_task(name, &log_entry) {
        warn!(agent = %name, error = %e, "failed to write task log");
    }
}

/// On startup, find active tasks assigned to this agent and handle them.
///
/// If deskd crashed while this agent was processing a task, the task is stuck in
/// Active status with no one working on it. Tasks with results are completed;
/// tasks without results are marked as Failed for visibility.
fn recover_orphaned_tasks(agent_name: &str) {
    let store = crate::app::task::TaskStore::default_for_home();
    let active_tasks = match store.list(Some(crate::domain::task::TaskStatus::Active)) {
        Ok(tasks) => tasks,
        Err(_) => return,
    };

    for task in active_tasks {
        if task.assignee.as_deref() != Some(agent_name) {
            continue;
        }

        // If the task already has a result, complete it.
        if task.result.as_ref().is_some_and(|r| !r.is_empty()) {
            info!(
                agent = %agent_name,
                task_id = %task.id,
                "recovering orphaned task with result: completing"
            );
            if let Err(e) =
                store.complete(&task.id, task.result.as_deref().unwrap_or(""), None, None)
            {
                warn!(agent = %agent_name, task_id = %task.id, error = %e, "failed to complete orphaned task");
            }
            continue;
        }

        // No result — mark as Failed. Manual retry or dead-letter will handle it.
        info!(
            agent = %agent_name,
            task_id = %task.id,
            "marking orphaned active task as Failed (no result, agent crashed)"
        );
        if let Err(e) = store.fail(
            &task.id,
            &format!("agent {} crashed — task requires manual retry", agent_name),
        ) {
            warn!(agent = %agent_name, task_id = %task.id, error = %e, "failed to mark orphaned task as failed");
        }
    }
}

/// Handle successful task completion: log, inbox, queue update, bus reply.
#[allow(clippy::too_many_arguments)]
async fn handle_task_success(
    name: &str,
    msg: &Message,
    ctx: &TaskContext,
    turn: &agent::TurnResult,
    full_response: String,
    task_duration_ms: u64,
    task_duration_secs: u64,
    work_dir: &str,
    model: &str,
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) {
    info!(
        agent = %name,
        cost = turn.cost_usd,
        turns = turn.num_turns,
        "task completed (persistent)"
    );

    log_token_usage(
        work_dir,
        name,
        &msg.source,
        &ctx.task_formatted,
        &turn.token_usage,
        task_duration_secs,
        model,
        ctx.github_repo.as_deref(),
        ctx.github_pr,
    );

    let log_entry = tasklog::TaskLog {
        ts: chrono::Utc::now().to_rfc3339(),
        source: msg.source.clone(),
        turns: turn.num_turns,
        cost: turn.cost_usd,
        duration_ms: task_duration_ms,
        status: "ok".to_string(),
        task: tasklog::truncate_task(&ctx.task_raw, 60),
        error: None,
        msg_id: msg.id.clone(),
        github_repo: ctx.github_repo.clone(),
        github_pr: ctx.github_pr,
        input_tokens: Some(turn.token_usage.input_tokens),
        output_tokens: Some(turn.token_usage.output_tokens),
    };
    if let Err(e) = tasklog::log_task(name, &log_entry) {
        warn!(agent = %name, error = %e, "failed to write task log");
    }

    let progress_was_streamed = !full_response.is_empty();
    let response = if full_response.is_empty() {
        turn.response_text.clone()
    } else {
        full_response
    };
    if !response.is_empty() {
        write_inbox(name, msg, &ctx.task_formatted, Some(response.clone()), None);
    }

    // Update task queue if this came from the queue.
    if let Some(ref tq_id) = ctx.task_queue_id {
        let store = crate::app::task::TaskStore::default_for_home();
        let result_text = if response.len() > 500 {
            let mut end = 500;
            while !response.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &response[..end])
        } else {
            response.clone()
        };
        if let Err(e) = store.complete(
            tq_id,
            &result_text,
            Some(turn.cost_usd),
            Some(turn.num_turns),
        ) {
            warn!(agent = %name, task_id = %tq_id, error = %e, "failed to mark queue task done");
        }
    }

    // Skip final bus write for telegram targets when progress was already streamed —
    // the progress forwarder already delivered the content to the adapter.
    let already_streamed = progress_was_streamed && ctx.reply_target.starts_with("telegram.out:");
    if !already_streamed {
        write_bus_envelope(
            writer,
            name,
            &ctx.reply_target,
            serde_json::json!({"result": response, "final": true, "in_reply_to": msg.id}),
        )
        .await;
    }

    // Emit domain event.
    let summary = truncate(&response, 200).to_string();
    emit_event(
        writer,
        name,
        &DomainEvent::TaskCompleted {
            task_id: ctx.task_queue_id.clone().unwrap_or_default(),
            instance_id: ctx.sm_instance_id.clone(),
            result_summary: summary,
        },
    )
    .await;
}

/// Handle task failure: log, queue update, crash recovery, bus reply.
/// Returns true if the process crashed and needs restart.
async fn handle_task_failure(
    name: &str,
    msg: &Message,
    ctx: &TaskContext,
    error: &anyhow::Error,
    task_duration_ms: u64,
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> bool {
    let err_str = format!("{}", error);
    warn!(agent = %name, error = %err_str, "task failed");

    let log_entry = tasklog::TaskLog {
        ts: chrono::Utc::now().to_rfc3339(),
        source: msg.source.clone(),
        turns: 0,
        cost: 0.0,
        duration_ms: task_duration_ms,
        status: "error".to_string(),
        task: tasklog::truncate_task(&ctx.task_raw, 60),
        error: Some(err_str.clone()),
        msg_id: msg.id.clone(),
        github_repo: ctx.github_repo.clone(),
        github_pr: ctx.github_pr,
        input_tokens: None,
        output_tokens: None,
    };
    if let Err(le) = tasklog::log_task(name, &log_entry) {
        warn!(agent = %name, error = %le, "failed to write task log");
    }

    if let Some(ref tq_id) = ctx.task_queue_id {
        let store = crate::app::task::TaskStore::default_for_home();
        if let Err(e) = store.fail(tq_id, &err_str) {
            warn!(agent = %name, task_id = %tq_id, error = %e, "failed to mark queue task failed");
        }
    }

    let needs_restart = err_str.contains("process exited") || err_str.contains("stdin closed");

    write_inbox(name, msg, &ctx.task_formatted, None, Some(err_str.clone()));
    write_bus_envelope(
        writer,
        name,
        &ctx.reply_target,
        serde_json::json!({"error": err_str, "in_reply_to": msg.id}),
    )
    .await;

    // Emit domain event.
    emit_event(
        writer,
        name,
        &DomainEvent::TaskFailed {
            task_id: ctx.task_queue_id.clone().unwrap_or_default(),
            instance_id: ctx.sm_instance_id.clone(),
            error: err_str,
        },
    )
    .await;

    needs_restart
}

/// Telegram typing indicator and progress message management.
struct TelegramProgress {
    cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
    chat_id: i64,
}

impl TelegramProgress {
    /// Start typing indicator and progress timer for a Telegram-targeted task.
    async fn start(
        chat_id: i64,
        writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
        name: &str,
    ) -> Self {
        let ctrl_target = format!("telegram.ctrl:{}", chat_id);

        write_bus_envelope(
            writer,
            name,
            &ctrl_target,
            serde_json::json!({"typing": true}),
        )
        .await;
        write_bus_envelope(
            writer,
            name,
            &ctrl_target,
            serde_json::json!({"progress_start": true}),
        )
        .await;

        let start_time = std::time::Instant::now();
        let progress_writer = writer.clone();
        let progress_name = name.to_string();
        let progress_ctrl = ctrl_target;
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let elapsed = start_time.elapsed().as_secs();
                        let text = format!("⏳ Working... {}s", elapsed);
                        write_bus_envelope(
                            &progress_writer,
                            &progress_name,
                            &progress_ctrl,
                            serde_json::json!({"progress_update": text}),
                        )
                        .await;
                    }
                    _ = &mut cancel_rx => break,
                }
            }
        });

        Self {
            cancel_tx: Some(cancel_tx),
            chat_id,
        }
    }

    /// Stop typing and progress indicators.
    async fn stop(
        mut self,
        writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
        name: &str,
    ) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
        let ctrl_target = format!("telegram.ctrl:{}", self.chat_id);
        write_bus_envelope(
            writer,
            name,
            &ctrl_target,
            serde_json::json!({"progress_done": true}),
        )
        .await;
        write_bus_envelope(
            writer,
            name,
            &ctrl_target,
            serde_json::json!({"typing": false}),
        )
        .await;
    }
}

/// Send a message via the bus (connect, send, wait for one reply, disconnect).
pub async fn send_via_bus(
    socket_path: &str,
    source: &str,
    target: &str,
    task: &str,
    max_turns: Option<u32>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("Failed to connect to bus at {}", socket_path))?;

    let reg = serde_json::json!({"type": "register", "name": source, "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let mut payload = serde_json::json!({"task": task});
    if let Some(turns) = max_turns {
        payload["max_turns"] = serde_json::json!(turns);
    }

    let msg = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": source,
        "target": target,
        "payload": payload,
        "reply_to": format!("agent:{}", source),
        "metadata": {"priority": 5u8},
    });
    let mut msg_line = serde_json::to_string(&msg)?;
    msg_line.push('\n');
    stream.write_all(msg_line.as_bytes()).await?;

    debug!(source = %source, target = %target, "task posted to bus");

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    if let Some(response_line) = lines.next_line().await? {
        let resp: serde_json::Value = serde_json::from_str(&response_line)?;
        if let Some(result) = resp
            .get("payload")
            .and_then(|p| p.get("result"))
            .and_then(|r| r.as_str())
        {
            println!("{}", result);
        } else if let Some(err) = resp
            .get("payload")
            .and_then(|p| p.get("error"))
            .and_then(|e| e.as_str())
        {
            bail!("Agent error: {}", err);
        } else {
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
    }

    Ok(())
}

/// Write a bus message envelope to the shared writer.
async fn write_bus_envelope(
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    source: &str,
    target: &str,
    payload: serde_json::Value,
) {
    let envelope = serde_json::json!({
        "type": "message",
        "id": Uuid::new_v4().to_string(),
        "source": source,
        "target": target,
        "payload": payload,
        "metadata": {"priority": 5u8},
    });
    let Ok(mut line) = serde_json::to_string(&envelope) else {
        return;
    };
    line.push('\n');
    let mut w = writer.lock().await;
    let _ = w.write_all(line.as_bytes()).await;
}

/// Publish a domain event to the message bus via the worker's bus connection.
async fn emit_event(
    writer: &std::sync::Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    source: &str,
    event: &DomainEvent,
) {
    write_bus_envelope(
        writer,
        source,
        &format!("events:{}", event.event_type()),
        event.to_json(),
    )
    .await;
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── truncate tests ──────────────────────────────────────────────────────

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long_string() {
        assert_eq!(truncate("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate("", 10), "");
    }

    #[test]
    fn test_truncate_unicode_boundary() {
        // '€' is 3 bytes in UTF-8; truncating at byte 1 should snap back to 0.
        let s = "€abc";
        let result = truncate(s, 1);
        assert!(result.is_empty());
    }

    #[test]
    fn test_truncate_multibyte_safe() {
        let s = "a€b"; // a=1byte, €=3bytes, b=1byte → total 5
        assert_eq!(truncate(s, 4), "a€");
        assert_eq!(truncate(s, 3), "a"); // byte 3 is mid-€, snaps back
    }

    // ─── extract_task_context tests ──────────────────────────────────────────

    fn make_message(payload: serde_json::Value) -> Message {
        Message {
            id: "msg-1".into(),
            source: "cli".into(),
            target: "agent:test".into(),
            payload,
            reply_to: None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn test_extract_task_context_empty_payload() {
        let msg = make_message(json!({}));
        assert!(extract_task_context(&msg, "test").is_none());
    }

    #[test]
    fn test_extract_task_context_empty_task() {
        let msg = make_message(json!({"task": ""}));
        assert!(extract_task_context(&msg, "test").is_none());
    }

    #[test]
    fn test_extract_task_context_basic() {
        let msg = make_message(json!({"task": "do something"}));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.task_raw, "do something");
        assert!(ctx.task_formatted.contains("[source: cli]"));
        assert!(ctx.task_formatted.contains("do something"));
        assert_eq!(ctx.reply_target, "cli");
        assert!(ctx.telegram_chat_id.is_none());
        assert!(ctx.github_repo.is_none());
    }

    #[test]
    fn test_extract_task_context_with_reply_to() {
        let mut msg = make_message(json!({"task": "work"}));
        msg.reply_to = Some("agent:dev".into());
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.reply_target, "agent:dev");
    }

    #[test]
    fn test_extract_task_context_telegram() {
        let msg = make_message(json!({
            "task": "hello",
            "telegram_chat_id": -1234567890_i64,
            "telegram_chat_name": "Dev Chat"
        }));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert!(
            ctx.task_formatted
                .contains("[Telegram: Dev Chat (-1234567890)]")
        );
        assert!(ctx.task_formatted.contains("hello"));
    }

    #[test]
    fn test_extract_task_context_telegram_with_quote() {
        let msg = make_message(json!({
            "task": "reply to this",
            "telegram_chat_id": -100,
            "telegram_reply_to_text": "original message"
        }));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert!(ctx.task_formatted.contains("> original message"));
    }

    #[test]
    fn test_extract_task_context_github() {
        let msg = make_message(json!({
            "task": "review PR",
            "github_repo": "kgatilin/deskd",
            "github_pr": 42
        }));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.github_repo.as_deref(), Some("kgatilin/deskd"));
        assert_eq!(ctx.github_pr, Some(42));
    }

    #[test]
    fn test_extract_task_context_sm_reply() {
        let msg = make_message(json!({
            "task": "handle this",
            "sm_instance_id": "sm-abc-123"
        }));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.reply_target, "sm:sm-abc-123");
    }

    #[test]
    fn test_extract_task_context_telegram_out_reply() {
        let mut msg = make_message(json!({"task": "respond"}));
        msg.reply_to = Some("telegram.out:-1234".into());
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.reply_target, "telegram.out:-1234");
        assert_eq!(ctx.telegram_chat_id, Some(-1234));
    }

    #[test]
    fn test_extract_task_context_task_queue_id() {
        let msg = make_message(json!({
            "task": "queued work",
            "task_queue_id": "tq-456"
        }));
        let ctx = extract_task_context(&msg, "test").unwrap();
        assert_eq!(ctx.task_queue_id.as_deref(), Some("tq-456"));
    }
}
