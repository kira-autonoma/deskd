/// MCP (Model Context Protocol) server for deskd.
///
/// Run as: `deskd mcp --agent <name>`
/// Claude invokes this as a subprocess via `--mcp-server "deskd mcp --agent <name>"`.
///
/// Split into three modules (#293):
/// - `mcp_protocol` — JSON-RPC types, I/O helpers, bus channel listener
/// - `mcp_tools` — tool implementations (call_* functions)
/// - `mcp` (this file) — orchestration: event loop, request routing, tool registry
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::app::mcp_protocol::{
    Request, Response, connect_bus_listener, emit_channel_notification, handle_initialize,
    write_response,
};
use crate::app::mcp_tools::{self, InternalBus, build_send_message_description};
use crate::config::UserConfig;
use crate::ports::bus_wire::BusMessage;

// ─── Main entry point ────────────────────────────────────────────────────────

/// Run the MCP server for the given agent. Reads JSON-RPC from stdin, writes to stdout.
/// Terminates when stdin closes (i.e. when Claude exits).
pub async fn run(agent_name: &str) -> Result<()> {
    let bus_socket = std::env::var("DESKD_BUS_SOCKET")
        .with_context(|| "DESKD_BUS_SOCKET not set — was this started by deskd?")?;

    let config_path = std::env::var("DESKD_AGENT_CONFIG").ok();
    let user_config = config_path
        .as_deref()
        .and_then(|p| UserConfig::load(p).ok());

    // Lazy-initialized internal bus for sub-agent orchestration.
    let internal_bus: Arc<Mutex<Option<InternalBus>>> = Arc::new(Mutex::new(None));

    // Create stores once at startup — passed as trait references to all handlers.
    let task_store = crate::app::task::TaskStore::default_for_home();
    let sm_store = crate::app::statemachine::StateMachineStore::default_for_home();

    info!(agent = %agent_name, bus = %bus_socket, "MCP server started");

    let stdin = tokio::io::stdin();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let reader = tokio::io::BufReader::new(stdin);

    // Detect framing mode from first line of input.
    // Claude Code uses newline-delimited JSON (no Content-Length headers).
    // Older MCP clients use Content-Length framed messages.
    let mut lines = reader.lines();

    // Establish persistent bus subscription to receive incoming messages.
    // The MCP server registers as `<agent>-mcp-channel` and subscribes to
    // messages targeted at this agent so they can be forwarded as channel
    // notifications into the Claude session.
    let mut bus_rx = connect_bus_listener(agent_name, &bus_socket).await;

    loop {
        // Select between stdin (JSON-RPC requests from Claude) and bus
        // messages (events to push as channel notifications).
        tokio::select! {
            stdin_line = lines.next_line() => {
                let line = match stdin_line {
                    Ok(Some(l)) => l,
                    Ok(None) => break, // stdin closed
                    Err(e) => {
                        warn!(error = %e, "failed to read line");
                        break;
                    }
                };

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Skip Content-Length headers (compat with framed clients).
                if trimmed.starts_with("Content-Length:") {
                    continue;
                }

                debug!(msg = %trimmed, "received MCP message");

                let req: Request = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, line = %trimmed, "failed to parse JSON-RPC request");
                        let resp = Response::err(None, -32700, "Parse error");
                        let mut out = stdout.lock().await;
                        write_response(&mut *out, &resp).await?;
                        continue;
                    }
                };

                // Notifications (no id) — acknowledge and continue
                if req.id.is_none() && req.method.starts_with("notifications/") {
                    debug!(method = %req.method, "received notification");
                    continue;
                }

                let resp = handle_request(
                    &req,
                    agent_name,
                    &bus_socket,
                    user_config.as_ref(),
                    &internal_bus,
                    &task_store,
                    &sm_store,
                )
                .await;
                let mut out = stdout.lock().await;
                write_response(&mut *out, &resp).await?;
            }
            bus_msg = async {
                match &bus_rx {
                    Some(rx) => {
                        let mut rx = rx.lock().await;
                        rx.next_line().await
                    }
                    None => std::future::pending().await,
                }
            } => {
                match bus_msg {
                    Ok(Some(line)) if !line.is_empty() => {
                        if let Ok(msg) = serde_json::from_str::<BusMessage>(&line) {
                            debug!(source = %msg.source, target = %msg.target, "bus event → channel notification");
                            let mut out = stdout.lock().await;
                            emit_channel_notification(&mut *out, &msg).await?;
                        }
                    }
                    Ok(None) => {
                        warn!("bus connection closed — channel notifications disabled");
                        bus_rx = None;
                    }
                    Err(e) => {
                        warn!(error = %e, "bus read error");
                    }
                    _ => {} // empty line, skip
                }
            }
        }
    }

    // Cleanup: stop all sub-agents on the internal bus.
    if let Some(ibus) = internal_bus.lock().await.take() {
        info!(agent = %agent_name, "stopping internal bus and sub-agents");
        for (name, handle) in &ibus.worker_handles {
            info!(agent = %name, "aborting sub-agent worker");
            handle.abort();
        }
        ibus._bus_handle.abort();
        std::fs::remove_file(&ibus.socket_path).ok();
    }

    info!(agent = %agent_name, "MCP server stopped (stdin closed)");
    Ok(())
}

// ─── Request routing ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    req: &Request,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
    task_store: &dyn crate::ports::store::TaskRepository,
    sm_store: &dyn crate::ports::store::StateMachineRepository,
) -> Response {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => handle_initialize(id),
        "tools/list" => handle_tools_list(id, agent_name, user_config),
        "tools/call" => {
            let params = req.params.as_ref().unwrap_or(&Value::Null);
            match handle_tools_call(
                params,
                agent_name,
                bus_socket,
                user_config,
                internal_bus,
                task_store,
                sm_store,
            )
            .await
            {
                Ok(resp) => Response::ok(id, resp),
                Err(e) => Response::err(id, -32603, &format!("{:#}", e)),
            }
        }
        other => {
            debug!(method = %other, "unknown MCP method");
            Response::err(id, -32601, "Method not found")
        }
    }
}

// ─── Tool registry ───────────────────────────────────────────────────────────

fn handle_tools_list(
    id: Option<Value>,
    agent_name: &str,
    user_config: Option<&UserConfig>,
) -> Response {
    let send_message_desc = user_config
        .map(|c| build_send_message_description(c, agent_name))
        .unwrap_or_else(|| "Send a message to a target on the bus.".to_string());

    // Build sm_create description dynamically listing available models.
    let sm_create_desc = if let Some(cfg) = user_config
        && !cfg.models.is_empty()
    {
        let model_list: Vec<String> = cfg
            .models
            .iter()
            .map(|m| {
                if m.description.is_empty() {
                    format!("  {} ({} states)", m.name, m.states.len())
                } else {
                    format!("  {} — {}", m.name, m.description)
                }
            })
            .collect();
        format!(
            "Create a new state machine instance. Available models:\n{}",
            model_list.join("\n")
        )
    } else {
        "Create a new state machine instance.".to_string()
    };

    let mut tools = vec![
        json!({
            "name": "send_message",
            "description": send_message_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Bus target (e.g. agent:dev, telegram.out:-1234, news:ecosystem)"
                    },
                    "text": {
                        "type": "string",
                        "description": "Message content"
                    },
                    "fresh": {
                        "type": "boolean",
                        "description": "When true, the target agent starts a fresh session for this task (no --resume), regardless of its default session config."
                    }
                },
                "required": ["target", "text"]
            }
        }),
        json!({
            "name": "add_persistent_agent",
            "description": "Launch a new persistent sub-agent on this agent's bus. The agent starts immediately and remains connected until the bus shuts down.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Unique name for the new agent"
                    },
                    "model": {
                        "type": "string",
                        "description": "Claude model to use (e.g. claude-sonnet-4-6, claude-haiku-4-5)"
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "System prompt for the new agent"
                    },
                    "subscribe": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Bus targets to subscribe to (e.g. [\"agent:helper\", \"queue:tasks\"])"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["inherit", "narrow"],
                        "description": "Scope type: 'inherit' = shares parent scope (default), 'narrow' = isolated sub-scope"
                    },
                    "work_dir": {
                        "type": "string",
                        "description": "Working directory for the agent. Must be under parent's work_dir."
                    },
                    "env": {
                        "type": "object",
                        "description": "Environment variables for the agent. Isolated from parent — child only gets what is explicitly passed here."
                    },
                    "can_message": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Allow-list of bus targets this agent can send to. Supports glob patterns. If omitted, unrestricted."
                    }
                },
                "required": ["name", "model", "system_prompt", "subscribe"]
            }
        }),
    ];

    // Scope introspection
    tools.push(json!({
        "name": "get_scope",
        "description": "Introspect own agent scope: parent, children, work_dir, can_message ACL, env keys, bus topics.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    }));

    tools.push(json!({
        "name": "create_reminder",
        "description": "Schedule a one-shot reminder. Specify time with 'at' (datetime/time string), 'in' (duration like 30m/1h/2h30m), or 'delay_minutes' (number). Exactly one time param required.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Bus target to deliver the reminder to (e.g. agent:kira, telegram.out:-1234)"
                },
                "message": {
                    "type": "string",
                    "description": "Message payload to deliver when the reminder fires"
                },
                "at": {
                    "type": "string",
                    "description": "When to fire: ISO 8601 (2026-04-22T09:00:00Z), datetime (2026-04-22 09:00), time today/tomorrow (09:00), or 'tomorrow 09:00'. Moscow timezone assumed for naive times."
                },
                "in": {
                    "type": "string",
                    "description": "Duration from now: 30m, 1h, 2h30m, 90s, 1d"
                },
                "delay_minutes": {
                    "type": "number",
                    "description": "Minutes from now (legacy, prefer 'at' or 'in')"
                }
            },
            "required": ["target", "message"]
        }
    }));

    // Unified inbox tools
    tools.push(json!({
        "name": "list_inboxes",
        "description": "List all unified inboxes with message counts. Returns inbox names (e.g. telegram/12345, github/owner-repo, agent/dev) and how many messages each contains.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    }));
    tools.push(json!({
        "name": "read_inbox",
        "description": "Read messages from a unified inbox. Messages are returned in chronological order (oldest first). Use list_inboxes to discover available inbox names.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "inbox": {
                    "type": "string",
                    "description": "Inbox name (e.g. telegram/12345, github/owner-repo, agent/dev)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of messages to return (default: 50)"
                },
                "since": {
                    "type": "string",
                    "description": "Only return messages after this RFC3339 timestamp (e.g. 2026-03-29T10:00:00Z)"
                }
            },
            "required": ["inbox"]
        }
    }));
    tools.push(json!({
        "name": "search_inbox",
        "description": "Search messages across one or all unified inboxes. Case-insensitive substring match on text, source, and from fields.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "inbox": {
                    "type": "string",
                    "description": "Inbox name to search (omit to search all inboxes)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (substring match)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of results (default: 50)"
                }
            },
            "required": ["query"]
        }
    }));

    // Graph execution tool
    tools.push(json!({
        "name": "run_graph",
        "description": "Execute an executable skill graph from a YAML file. The graph is a DAG of tool call groups and LLM decision nodes. Returns step results and extracted variables.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "Path to the graph YAML file (absolute, or relative to agent work dir)"
                },
                "work_dir": {
                    "type": "string",
                    "description": "Working directory for tool execution (defaults to graph file's parent directory)"
                },
                "vars": {
                    "type": "object",
                    "description": "Input variables as key-value pairs (e.g. {\"pr_number\": \"42\", \"repo\": \"owner/repo\"})"
                }
            },
            "required": ["file"]
        }
    }));

    // Task queue tools
    tools.push(json!({
        "name": "task_create",
        "description": "Create a task in the pull-based task queue. Idle workers matching the criteria will pick it up automatically.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Task description — what the worker should do"
                },
                "model": {
                    "type": "string",
                    "description": "Required model (e.g. claude-sonnet-4-6). Only workers with this model will pick up the task."
                },
                "labels": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Required labels. Only workers with ALL listed labels will pick up the task."
                },
                "metadata": {
                    "type": "object",
                    "description": "Structured metadata (e.g. {\"worktree\": \"/path\", \"branch\": \"feat/x\"})"
                }
            },
            "required": ["description"]
        }
    }));
    tools.push(json!({
        "name": "task_list",
        "description": "List tasks in the task queue. Returns task ID, status, assignee, and description.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: pending, active, done, failed, cancelled. Omit to list all."
                }
            }
        }
    }));
    tools.push(json!({
        "name": "task_cancel",
        "description": "Cancel a pending task in the task queue. Only pending tasks can be cancelled.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Task ID (e.g. task-a1b2c3d4)"
                }
            },
            "required": ["id"]
        }
    }));
    tools.push(json!({
        "name": "list_agents",
        "description": "List all sub-agents spawned by this agent on its internal bus. Returns name, model, status (running/finished), turns, and cost for each agent.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        }
    }));
    tools.push(json!({
        "name": "remove_agent",
        "description": "Stop and remove a sub-agent. The agent's worker process is terminated and its state file is deleted.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the agent to remove"
                }
            },
            "required": ["name"]
        }
    }));

    tools.push(json!({
        "name": "usage_stats",
        "description": "Get aggregate token usage and cost statistics across agents. Returns total tasks, cost, tokens, and per-agent breakdown.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "Time period: 'today', '24h', '7d' (default), '30d', 'all'"
                },
                "agent": {
                    "type": "string",
                    "description": "Filter to a specific agent name"
                }
            }
        }
    }));

    // Add state machine tools if models are defined.
    if user_config.map(|c| !c.models.is_empty()).unwrap_or(false) {
        tools.push(json!({
            "name": "sm_create",
            "description": sm_create_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "Model name (e.g. 'feature', 'bugfix')"},
                    "title": {"type": "string", "description": "Instance title"},
                    "body": {"type": "string", "description": "Instance body/description"},
                    "metadata": {"type": "object", "description": "Structured metadata (e.g. {\"worktree\": \"/path\", \"branch\": \"feat/x\"})"}
                },
                "required": ["model", "title"]
            }
        }));
        tools.push(json!({
            "name": "sm_move",
            "description": "Move a state machine instance to a new state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Instance ID (e.g. sm-a1b2c3d4)"},
                    "state": {"type": "string", "description": "Target state name"},
                    "note": {"type": "string", "description": "Optional note/reason"}
                },
                "required": ["id", "state"]
            }
        }));
        tools.push(json!({
            "name": "sm_query",
            "description": "Query state machine instances. Returns JSON list of matching instances.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string", "description": "Filter by model name"},
                    "state": {"type": "string", "description": "Filter by current state"},
                    "id": {"type": "string", "description": "Get specific instance by ID"}
                }
            }
        }));
    }

    // A2A tools
    tools.push(json!({
        "name": "a2a_send",
        "description": "Send an A2A task to a remote agent. Requires the agent's base URL and skill ID.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Remote agent's base URL (e.g. https://archi.agent.example.com)"},
                "skill": {"type": "string", "description": "Skill ID to invoke (e.g. agent_name/skill_id)"},
                "message": {"type": "string", "description": "Task description / message to send"},
                "api_key": {"type": "string", "description": "API key for the remote agent (optional)"}
            },
            "required": ["url", "skill", "message"]
        }
    }));
    tools.push(json!({
        "name": "a2a_discover",
        "description": "Fetch a remote agent's Agent Card (skills, needs, capabilities). Use to discover what a remote agent can do before sending tasks.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Remote agent's base URL (e.g. https://archi.agent.example.com)"}
            },
            "required": ["url"]
        }
    }));
    tools.push(json!({
        "name": "publish_need",
        "description": "Dynamically publish a need to this agent's Agent Card. The need is persisted in deskd.yaml and visible to other agents via A2A discovery.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "What you need done (e.g. 'Architecture review of bus module')"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags for discovery (e.g. ['architecture', 'review'])"},
                "priority": {"type": "string", "enum": ["low", "medium", "high"], "description": "Priority level (default: medium)"}
            },
            "required": ["description"]
        }
    }));
    tools.push(json!({
        "name": "browse_needs",
        "description": "Browse a remote agent's published needs. Returns the needs section from their Agent Card.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Remote agent's base URL (e.g. https://archi.agent.example.com)"}
            },
            "required": ["url"]
        }
    }));
    tools.push(json!({
        "name": "propose_for_need",
        "description": "Send a proposal to fulfill a remote agent's published need via A2A. The proposal is delivered as a task to the remote agent.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Remote agent's base URL"},
                "need_id": {"type": "string", "description": "Need ID from the remote agent's Agent Card (e.g. 'kira/want-restart')"},
                "proposal": {"type": "string", "description": "Your proposal to fulfill the need"},
                "api_key": {"type": "string", "description": "API key for the remote agent (optional)"}
            },
            "required": ["url", "need_id", "proposal"]
        }
    }));

    tools.push(json!({
        "name": "query_agent",
        "description": "Synchronously query another agent and wait for the response. Sends a question to the target agent and blocks until the answer arrives or timeout is reached.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target": {"type": "string", "description": "Bus target (e.g. agent:memory-arch, agent:dev)"},
                "question": {"type": "string", "description": "Question to ask the target agent"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default: 30)", "default": 30}
            },
            "required": ["target", "question"]
        }
    }));

    // telegram_history — read locally accumulated Telegram messages.
    tools.push(json!({
        "name": "telegram_history",
        "description": "Fetch recent messages from a Telegram chat. Reads from the local message store accumulated by the Telegram bot adapter.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "chat_id": {"type": "integer", "description": "Telegram chat id (positive for users/groups, negative for channels/supergroups)"},
                "limit": {"type": "integer", "description": "How many messages to fetch (1..=200, default 50)", "default": 50},
                "offset_id": {"type": "integer", "description": "Return only messages older than this message id (pagination)"}
            },
            "required": ["chat_id"]
        }
    }));

    Response::ok(id, json!({ "tools": tools }))
}

// ─── Tool dispatch ───────────────────────────────────────────────────────────

async fn handle_tools_call(
    params: &Value,
    agent_name: &str,
    bus_socket: &str,
    user_config: Option<&UserConfig>,
    internal_bus: &Arc<Mutex<Option<InternalBus>>>,
    task_store: &dyn crate::ports::store::TaskRepository,
    sm_store: &dyn crate::ports::store::StateMachineRepository,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .context("missing tool name")?;

    let args = params.get("arguments").unwrap_or(&Value::Null);

    match name {
        "send_message" => {
            mcp_tools::call_send_message(args, agent_name, bus_socket, user_config, internal_bus)
                .await
        }
        "add_persistent_agent" => {
            mcp_tools::call_add_persistent_agent(args, agent_name, bus_socket, internal_bus).await
        }
        "create_reminder" => mcp_tools::call_create_reminder(args).await,
        "list_inboxes" => mcp_tools::call_list_inboxes(agent_name, user_config).await,
        "read_inbox" => mcp_tools::call_read_inbox(args, agent_name, user_config).await,
        "search_inbox" => mcp_tools::call_search_inbox(args, agent_name, user_config).await,
        "run_graph" => mcp_tools::call_run_graph(args).await,
        "task_create" => mcp_tools::call_task_create(args, agent_name, task_store).await,
        "task_list" => mcp_tools::call_task_list(args, task_store).await,
        "task_cancel" => mcp_tools::call_task_cancel(args, task_store).await,
        "list_agents" => mcp_tools::call_list_agents(agent_name, internal_bus).await,
        "remove_agent" => mcp_tools::call_remove_agent(args, agent_name, internal_bus).await,
        "get_scope" => mcp_tools::call_get_scope(agent_name, user_config, internal_bus).await,
        "sm_create" => {
            mcp_tools::call_sm_create(args, agent_name, bus_socket, user_config, sm_store).await
        }
        "sm_move" => {
            mcp_tools::call_sm_move(args, agent_name, bus_socket, user_config, sm_store).await
        }
        "sm_query" => mcp_tools::call_sm_query(args, sm_store).await,
        "usage_stats" => mcp_tools::call_usage_stats(args).await,
        "a2a_send" => mcp_tools::call_a2a_send(args).await,
        "a2a_discover" => mcp_tools::call_a2a_discover(args).await,
        "publish_need" => mcp_tools::call_publish_need(args).await,
        "browse_needs" => mcp_tools::call_browse_needs(args).await,
        "propose_for_need" => mcp_tools::call_propose_for_need(args).await,
        "query_agent" => mcp_tools::call_query_agent(args, agent_name, bus_socket).await,
        "telegram_history" => mcp_tools::call_telegram_history(args).await,
        other => anyhow::bail!("Unknown tool: {}", other),
    }
}
