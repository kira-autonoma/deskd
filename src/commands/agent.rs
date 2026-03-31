//! `deskd agent` subcommand handlers.

use anyhow::Result;
use tracing::info;

use crate::cli::AgentAction;
use crate::{agent, config, tasklog, unified_inbox, worker};

use super::{format_relative_time, parse_duration_secs, truncate};

pub async fn handle(action: AgentAction) -> Result<()> {
    match action {
        AgentAction::Create {
            name,
            prompt,
            model,
            workdir,
            max_turns,
            unix_user,
            budget_usd,
            command,
        } => {
            let cfg = agent::AgentConfig {
                name,
                model,
                system_prompt: prompt.unwrap_or_default(),
                work_dir: workdir.unwrap_or_else(|| ".".into()),
                max_turns,
                unix_user,
                budget_usd,
                command: if command.is_empty() {
                    vec!["claude".to_string()]
                } else {
                    command
                },
                config_path: None,
                container: None,
                session: config::SessionMode::default(),
                runtime: config::AgentRuntime::default(),
            };
            let state = agent::create(&cfg).await?;
            println!("Agent {} created", state.config.name);
        }
        AgentAction::Send {
            name,
            message,
            max_turns,
            socket,
        } => {
            let effective_socket = if let Some(ref s) = socket {
                if std::path::Path::new(s).exists() {
                    socket
                } else {
                    None
                }
            } else {
                agent::load_state(&name).ok().and_then(|s| {
                    let bus = config::agent_bus_socket(&s.config.work_dir);
                    if std::path::Path::new(&bus).exists() {
                        Some(bus)
                    } else {
                        None
                    }
                })
            };

            if let Some(sock) = effective_socket {
                let target = format!("agent:{}", name);
                worker::send_via_bus(&sock, "cli", &target, &message, max_turns).await?;
            } else {
                let response = agent::send(&name, &message, max_turns, None).await?;
                println!("{}", response);
            }
        }
        AgentAction::Run {
            name,
            socket,
            subscribe,
        } => {
            agent::load_state(&name)?;
            let subs = if subscribe.is_empty() {
                None
            } else {
                Some(subscribe)
            };
            info!(agent = %name, "starting worker");
            tokio::select! {
                result = worker::run(&name, &socket, Some(socket.clone()), subs) => { result?; }
                _ = tokio::signal::ctrl_c() => {
                    info!(agent = %name, "shutting down");
                }
            }
        }
        AgentAction::List { socket } => {
            let agents = agent::list().await?;
            let live = crate::serve::query_live_agents(&socket)
                .await
                .unwrap_or_default();

            if agents.is_empty() {
                println!("No agents registered");
            } else {
                println!(
                    "{:<15} {:<7} {:<8} {:<10} {:<12} MODEL",
                    "NAME", "STATUS", "TURNS", "COST", "USER"
                );
                for a in agents {
                    let status = if live.contains(&a.config.name) {
                        "live"
                    } else {
                        "idle"
                    };
                    println!(
                        "{:<15} {:<7} {:<8} ${:<9.2} {:<12} {}",
                        a.config.name,
                        status,
                        a.total_turns,
                        a.total_cost,
                        a.config.unix_user.as_deref().unwrap_or("-"),
                        a.config.model,
                    );
                }
            }
        }
        AgentAction::Stats { name } => {
            let s = agent::load_state(&name)?;
            println!("Agent:      {}", s.config.name);
            println!("Model:      {}", s.config.model);
            println!(
                "Unix user:  {}",
                s.config.unix_user.as_deref().unwrap_or("-")
            );
            println!("Work dir:   {}", s.config.work_dir);
            println!(
                "Bus:        {}",
                config::agent_bus_socket(&s.config.work_dir)
            );
            println!(
                "Config:     {}",
                s.config.config_path.as_deref().unwrap_or("-")
            );
            println!("Total turns:{}", s.total_turns);
            println!("Total cost: ${:.4}", s.total_cost);
            println!("Budget:     ${:.2}", s.config.budget_usd);
            println!(
                "Session:    {}",
                if s.session_id.is_empty() {
                    "-"
                } else {
                    &s.session_id
                }
            );
            println!("Created:    {}", s.created_at);
        }
        AgentAction::Read {
            name,
            clear: _,
            follow,
        } => {
            let inbox_name = format!("replies/{}", name);
            let entries = unified_inbox::read_messages(&inbox_name, 100, None)?;
            if entries.is_empty() && !follow {
                println!("No messages for {}", name);
            } else {
                for entry in &entries {
                    print_inbox_message(entry);
                }
            }
            if follow {
                let mut last_ts = entries.last().map(|m| m.ts);
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let newer = unified_inbox::read_messages(&inbox_name, 100, last_ts)?;
                    for entry in &newer {
                        print_inbox_message(entry);
                        last_ts = Some(entry.ts);
                    }
                }
            }
        }
        AgentAction::Tasks { name, limit } => {
            let show_all = name == "all";

            let mut filtered: Vec<unified_inbox::InboxMessage> = if show_all {
                let inboxes = unified_inbox::list_inboxes()?;
                let mut all = Vec::new();
                for (inbox_name, _) in &inboxes {
                    if inbox_name.starts_with("replies/") {
                        let msgs = unified_inbox::read_messages(inbox_name, 1000, None)?;
                        all.extend(msgs.into_iter().filter(|m| {
                            m.metadata.get("type").and_then(|v| v.as_str()) == Some("task_result")
                        }));
                    }
                }
                all
            } else {
                let inbox_name = format!("replies/{}", name);
                let msgs = unified_inbox::read_messages(&inbox_name, 1000, None)?;
                msgs.into_iter()
                    .filter(|m| {
                        m.metadata.get("type").and_then(|v| v.as_str()) == Some("task_result")
                    })
                    .collect()
            };

            filtered.sort_by(|a, b| b.ts.cmp(&a.ts));
            filtered.truncate(limit);

            if filtered.is_empty() {
                if show_all {
                    println!("No completed tasks found");
                } else {
                    println!("No completed tasks for {}", name);
                }
            } else {
                println!("COMPLETED ({}):", filtered.len());
                let now = chrono::Utc::now();
                for entry in &filtered {
                    let dur = now.signed_duration_since(entry.ts);
                    let age = format_relative_time(dur);
                    let agent = entry
                        .metadata
                        .get("agent")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let task_text = entry
                        .metadata
                        .get("task")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let task_excerpt = truncate(task_text, 36);
                    let has_error = entry
                        .metadata
                        .get("has_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let status = if has_error { "err" } else { "done" };
                    let msg_id = entry
                        .metadata
                        .get("message_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let id_short = if msg_id.len() > 6 {
                        &msg_id[..6]
                    } else {
                        msg_id
                    };
                    if show_all {
                        println!(
                            "  {:<8} {:<12} from:{:<6} {:38} {} {} ago",
                            id_short,
                            agent,
                            entry.source,
                            format!("\"{}\"", task_excerpt),
                            status,
                            age,
                        );
                    } else {
                        println!(
                            "  {:<8} from:{:<6} {:38} {} {} ago",
                            id_short,
                            entry.source,
                            format!("\"{}\"", task_excerpt),
                            status,
                            age,
                        );
                    }
                }
            }
        }
        AgentAction::Logs {
            name,
            limit,
            source,
            since,
            json,
            cost,
            by_pr,
        } => {
            let since_dt = if let Some(ref dur) = since {
                let secs = parse_duration_secs(dur)?;
                Some(chrono::Utc::now() - chrono::Duration::seconds(secs as i64))
            } else {
                None
            };

            let entries = tasklog::read_logs(
                &name,
                if cost || by_pr { usize::MAX } else { limit },
                source.as_deref(),
                since_dt,
            )?;

            if entries.is_empty() {
                println!("No task logs for {}", name);
            } else if by_pr {
                tasklog::print_pr_summary(&name, &entries, since.as_deref());
            } else if cost {
                tasklog::print_cost_summary(&name, &entries, since.as_deref());
            } else if json {
                tasklog::print_json(&entries);
            } else {
                tasklog::print_table(&entries);
            }
        }
        AgentAction::Rm { name } => {
            agent::remove(&name).await?;
            println!("Agent {} removed", name);
        }
        AgentAction::Spawn {
            name,
            task,
            socket,
            work_dir,
            model,
            max_turns,
        } => {
            let bus_socket = socket
                .or_else(|| std::env::var("DESKD_BUS_SOCKET").ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("No bus socket: pass --socket or set DESKD_BUS_SOCKET")
                })?;

            let parent = std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "unknown".into());
            let resolved_work_dir = work_dir.unwrap_or_else(|| ".".into());

            let response = agent::spawn_ephemeral(
                &name,
                &task,
                &model,
                &resolved_work_dir,
                max_turns,
                &bus_socket,
                &parent,
            )
            .await?;

            println!("{}", response);
        }
    }
    Ok(())
}

fn print_inbox_message(msg: &unified_inbox::InboxMessage) {
    let ts = msg.ts.format("%Y-%m-%dT%H:%M:%S").to_string();
    let from = msg.from.as_deref().unwrap_or(&msg.source);
    println!("─── {} [{}] ───", from, ts);
    println!("{}", msg.text);
    println!();
}
