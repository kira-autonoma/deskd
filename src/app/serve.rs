//! `deskd serve` — start per-agent buses, workers, adapters, and schedules.

use anyhow::Result;
use tracing::info;

use crate::app::{adapters, agent, bus, schedule, worker, workflow};
use crate::config;

/// Start per-agent buses and workers for all agents in workspace config.
/// Each agent has its own isolated bus at {work_dir}/.deskd/bus.sock.
pub async fn serve(config_path: String) -> Result<()> {
    let workspace = config::WorkspaceConfig::load(&config_path)?;
    info!(path = %config_path, agents = workspace.agents.len(), "loaded workspace config");

    if workspace.agents.is_empty() {
        tracing::warn!("No agents defined in workspace config");
    }

    for def in &workspace.agents {
        let cfg_path = def.config_path();
        let user_cfg = config::UserConfig::load(&cfg_path).ok();
        if user_cfg.is_some() {
            info!(agent = %def.name, config = %cfg_path, "loaded user config");
        } else {
            info!(agent = %def.name, "no user config at {}, using defaults", cfg_path);
        }

        let state = agent::create_or_recover(def, user_cfg.as_ref()).await?;
        let name = state.config.name.clone();
        let bus_socket = def.bus_socket();

        // Ensure {work_dir}/.deskd/ exists.
        let bus_dir = std::path::Path::new(&def.work_dir).join(".deskd");
        std::fs::create_dir_all(&bus_dir)?;

        // Start the agent's isolated bus.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            tokio::spawn(async move {
                if let Err(e) = bus::serve(&bus).await {
                    tracing::error!(agent = %agent_name, socket = %bus, error = %e, "bus failed");
                }
            });
        }
        info!(agent = %name, bus = %bus_socket, "started agent bus");

        // Start configured adapters (Telegram, Discord, etc.).
        for adapter in
            adapters::build_adapters(def, user_cfg.as_ref(), &workspace.admin_telegram_ids)
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            let adapter_name = adapter.name().to_string();
            tokio::spawn(async move {
                if let Err(e) = adapter.run(bus, agent_name).await {
                    tracing::error!(adapter = %adapter_name, error = %e, "adapter failed");
                }
            });
        }

        // Start schedule watcher — handles initial load + hot-reload on config changes.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            let config = cfg_path.clone();
            let home = def.work_dir.clone();
            tokio::spawn(async move {
                schedule::watch_and_reload(config, bus, agent_name, home).await;
            });
            info!(agent = %name, "started schedule watcher");
        }

        // Start reminder runner — fires one-shot reminders from ~/.deskd/reminders/.
        {
            let bus = bus_socket.clone();
            let agent_name = name.clone();
            tokio::spawn(async move {
                schedule::run_reminders(bus, agent_name).await;
            });
            info!(agent = %name, "started reminder runner");
        }

        // Start worker on the agent's bus.
        let bus = bus_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = worker::run(&name, &bus, Some(bus.clone()), None).await {
                tracing::error!(agent = %name, error = %e, "worker exited with error");
            }
        });

        // Start sub-agent workers defined in the agent's deskd.yaml.
        if let Some(ref ucfg) = user_cfg {
            for sub in &ucfg.agents {
                let mcp_json = serde_json::json!({
                    "mcpServers": {
                        "deskd": {
                            "command": "deskd",
                            "args": ["mcp", "--agent", &sub.name]
                        }
                    }
                })
                .to_string();

                let sub_cfg = agent::AgentConfig {
                    name: sub.name.clone(),
                    model: sub.model.clone(),
                    system_prompt: sub.system_prompt.clone(),
                    work_dir: def.work_dir.clone(),
                    max_turns: ucfg.max_turns,
                    unix_user: def.unix_user.clone(),
                    budget_usd: def.budget_usd,
                    command: vec![
                        "claude".into(),
                        "--output-format".into(),
                        "stream-json".into(),
                        "--verbose".into(),
                        "--dangerously-skip-permissions".into(),
                        "--model".into(),
                        sub.model.clone(),
                        "--max-turns".into(),
                        ucfg.max_turns.to_string(),
                        "--mcp-config".into(),
                        mcp_json,
                    ],
                    config_path: Some(cfg_path.clone()),
                    container: def.container.clone(),
                    session: sub.session.clone(),
                    runtime: sub.runtime.clone(),
                };
                agent::create_or_update_from_config(&sub_cfg).await?;

                let sub_name = sub.name.clone();
                let bus = bus_socket.clone();
                let subs = sub.subscribe.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        worker::run(&sub_name, &bus, Some(bus.clone()), Some(subs)).await
                    {
                        tracing::error!(agent = %sub_name, error = %e, "sub-agent worker exited");
                    }
                });
                info!(agent = %def.name, sub_agent = %sub.name, "started sub-agent worker");
            }
        }

        // Start workflow engine if models are defined.
        if let Some(ref ucfg) = user_cfg
            && !ucfg.models.is_empty()
        {
            let bus = bus_socket.clone();
            let models: Vec<crate::domain::statemachine::ModelDef> =
                ucfg.models.iter().cloned().map(Into::into).collect();
            let agent_name = def.name.clone();
            tokio::spawn(async move {
                if let Err(e) = workflow::run(&bus, models).await {
                    tracing::error!(agent = %agent_name, error = %e, "workflow engine exited");
                }
            });
            info!(agent = %def.name, models = ucfg.models.len(), "started workflow engine");
        }
    }

    info!("all agents started — press Ctrl-C to stop");
    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    Ok(())
}

/// Query which agents are currently connected to a bus socket.
pub async fn query_live_agents(socket: &str) -> anyhow::Result<std::collections::HashSet<String>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    if !std::path::Path::new(socket).exists() {
        return Ok(Default::default());
    }

    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("connect: {}", e))?;

    let reg =
        serde_json::json!({"type": "register", "name": "deskd-cli-list", "subscriptions": []});
    let mut line = serde_json::to_string(&reg)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;

    let list_req = serde_json::json!({"type": "list"});
    let mut req_line = serde_json::to_string(&list_req)?;
    req_line.push('\n');
    stream.write_all(req_line.as_bytes()).await?;

    let (reader, _) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let timeout = tokio::time::Duration::from_secs(2);
    let result = tokio::time::timeout(timeout, async {
        while let Some(l) = lines.next_line().await? {
            let v: serde_json::Value = serde_json::from_str(&l)?;
            if v.get("type").and_then(|t| t.as_str()) == Some("list_response")
                && let Some(arr) = v.get("clients").and_then(|c| c.as_array())
            {
                return Ok::<_, anyhow::Error>(
                    arr.iter()
                        .filter_map(|n| n.as_str())
                        .map(|s| s.to_string())
                        .collect(),
                );
            }
        }
        Ok(Default::default())
    })
    .await;

    result.unwrap_or(Ok(Default::default()))
}
