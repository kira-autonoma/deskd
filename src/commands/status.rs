//! `deskd status` subcommand handler.

use std::str::FromStr;

use anyhow::Result;

use crate::{agent, config, task};

use super::{format_relative_time, truncate};

pub async fn handle(config_path: &str) -> Result<()> {
    let workspace = config::WorkspaceConfig::load(config_path)?;
    println!(
        "{:<12} {:<9} {:<6} {:<10} CURRENT TASK",
        "NAME", "STATUS", "TURNS", "COST"
    );
    println!("{}", "─".repeat(70));
    for def in &workspace.agents {
        let bus_socket = def.bus_socket();
        let online = std::path::Path::new(&bus_socket).exists();
        let state = agent::load_state(&def.name).ok();
        let (status, turns, cost, current_task) = match &state {
            Some(s) if online => (
                s.status.as_str(),
                s.total_turns,
                s.total_cost,
                if s.current_task.is_empty() {
                    "-".to_string()
                } else {
                    truncate(&s.current_task, 40)
                },
            ),
            Some(s) => ("offline", s.total_turns, s.total_cost, "-".to_string()),
            None => ("offline", 0, 0.0, "-".to_string()),
        };
        println!(
            "{:<12} {:<9} {:<6} ${:<9.4} {}",
            def.name, status, turns, cost, current_task
        );

        // Show registered schedules for this agent.
        let cfg_path = def.config_path();
        if let Ok(user_cfg) = config::UserConfig::load(&cfg_path)
            && !user_cfg.schedules.is_empty()
        {
            for sched in &user_cfg.schedules {
                let action_label = format!("{:?}", sched.action).to_lowercase();
                let next = cron::Schedule::from_str(&sched.cron)
                    .ok()
                    .and_then(|s| s.upcoming(chrono::Utc).next())
                    .map(|t| {
                        let dur = t - chrono::Utc::now();
                        format_relative_time(dur)
                    })
                    .unwrap_or_else(|| "?".to_string());
                println!(
                    "  {:<18} {:<14} → {:<24} next {}",
                    sched.cron, action_label, sched.target, next
                );
            }
        }
    }

    // Show task queue summary with per-worker active task info.
    let task_store = task::TaskStore::default_for_home();
    let qs = task_store.queue_summary();
    if qs.pending > 0 || qs.active > 0 || qs.done > 0 || qs.failed > 0 {
        println!();
        println!(
            "Task queue: {} pending, {} active, {} done, {} failed",
            qs.pending, qs.active, qs.done, qs.failed
        );

        if let Ok(active_tasks) = task_store.list(Some(task::TaskStatus::Active)) {
            for t in &active_tasks {
                if let Some(ref assignee) = t.assignee {
                    println!("  {} → {}", assignee, truncate(&t.description, 50));
                }
            }
        }
    }
    Ok(())
}
