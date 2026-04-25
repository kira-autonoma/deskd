//! Unified dashboard data model and renderers.
//!
//! Aggregates state from agents, sub-agent workers, state machine instances,
//! and the task queue into a single `Dashboard` snapshot. Used by
//! `deskd status` to provide a one-shot view of the running system.

use std::str::FromStr;

use anyhow::Result;
use serde::Serialize;

use crate::app::{agent, statemachine, task};
use crate::config;

use super::{format_relative_time, truncate};

/// A single agent (top-level, not a sub-agent worker) shown in the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardAgent {
    pub name: String,
    pub status: String,
    pub turns: u32,
    pub cost: f64,
    pub current_task: String,
    /// Workers spawned inside this agent (sub-agents).
    #[serde(default)]
    pub workers: Vec<DashboardWorker>,
    /// Registered cron schedules attached to this agent.
    #[serde(default)]
    pub schedules: Vec<DashboardSchedule>,
}

/// A sub-agent worker spawned inside a parent agent's bus scope.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardWorker {
    pub name: String,
    pub status: String,
    pub turns: u32,
    pub cost: f64,
    pub current_task: String,
}

/// A registered cron schedule.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSchedule {
    pub cron: String,
    pub action: String,
    pub target: String,
    pub next: String,
}

/// A non-terminal state machine instance.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSm {
    pub id: String,
    pub model: String,
    pub state: String,
    pub title: String,
    pub assignee: String,
    pub total_cost: f64,
}

/// Task queue summary numbers.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardTasks {
    pub pending: usize,
    pub active: usize,
    pub done: usize,
    pub failed: usize,
}

/// Full dashboard snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Dashboard {
    pub agents: Vec<DashboardAgent>,
    pub sm_instances: Vec<DashboardSm>,
    pub tasks: DashboardTasks,
}

/// Build the dashboard snapshot from on-disk state and the workspace config.
///
/// This is the pure aggregator — it does no IPC. Sub-agent workers are
/// discovered from the agent state directory (each sub-agent persists a state
/// file with `parent` set), so the dashboard works even when agents are not
/// reachable on the bus.
pub async fn build(config_path: &str) -> Result<Dashboard> {
    let workspace = config::WorkspaceConfig::load(config_path)?;

    // Load all known agent states; group sub-agents by parent name.
    let all_agents = agent::list().await.unwrap_or_default();
    let mut workers_by_parent: std::collections::HashMap<String, Vec<DashboardWorker>> =
        std::collections::HashMap::new();
    for s in &all_agents {
        if let Some(parent) = &s.parent {
            workers_by_parent
                .entry(parent.clone())
                .or_default()
                .push(DashboardWorker {
                    name: s.config.name.clone(),
                    status: s.status.clone(),
                    turns: s.total_turns,
                    cost: s.total_cost,
                    current_task: s.current_task.clone(),
                });
        }
    }

    let mut agents = Vec::new();
    for def in &workspace.agents {
        let bus_socket = def.bus_socket();
        let online = std::path::Path::new(&bus_socket).exists();
        let state = agent::load_state(&def.name).ok();
        let (status, turns, cost, current_task) = match &state {
            Some(s) if online => (
                s.status.clone(),
                s.total_turns,
                s.total_cost,
                s.current_task.clone(),
            ),
            Some(s) => (
                "offline".to_string(),
                s.total_turns,
                s.total_cost,
                String::new(),
            ),
            None => ("offline".to_string(), 0, 0.0, String::new()),
        };

        let workers = workers_by_parent.remove(&def.name).unwrap_or_default();

        // Schedules from per-agent deskd.yaml, if available.
        let mut schedules = Vec::new();
        let cfg_path = def.config_path();
        if let Ok(user_cfg) = config::UserConfig::load(&cfg_path) {
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
                schedules.push(DashboardSchedule {
                    cron: sched.cron.clone(),
                    action: action_label,
                    target: sched.target.clone(),
                    next,
                });
            }
        }

        agents.push(DashboardAgent {
            name: def.name.clone(),
            status,
            turns,
            cost,
            current_task,
            workers,
            schedules,
        });
    }

    // Active state machine instances. Filter to non-terminal states using the
    // first available agent's deskd.yaml (models are typically shared). If we
    // can't load a model, still include the instance — the dashboard prefers
    // showing too much over silently dropping.
    let sm_instances = collect_sm_instances();

    // Task queue summary.
    let task_store = task::TaskStore::default_for_home();
    let qs = task_store.queue_summary();
    let tasks = DashboardTasks {
        pending: qs.pending,
        active: qs.active,
        done: qs.done,
        failed: qs.failed,
    };

    Ok(Dashboard {
        agents,
        sm_instances,
        tasks,
    })
}

/// Collect non-terminal SM instances. Tries to load the first available
/// agent's user config to get model definitions for terminal-state filtering;
/// falls back to including all instances if no model defs are available.
fn collect_sm_instances() -> Vec<DashboardSm> {
    let store = statemachine::StateMachineStore::default_for_home();
    let instances = store.list_all().unwrap_or_default();

    // Try to resolve a user config that contains model definitions, so we can
    // distinguish terminal from active states.
    let models: Vec<statemachine::ModelDef> = config::ServeState::load()
        .as_ref()
        .and_then(|s| s.find_agent_config())
        .and_then(|a| config::UserConfig::load(&a.config_path).ok())
        .map(|cfg| {
            cfg.models
                .into_iter()
                .filter_map(|m| TryInto::<statemachine::ModelDef>::try_into(m).ok())
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    for inst in instances {
        let is_terminal = models
            .iter()
            .find(|m| m.name == inst.model)
            .is_some_and(|m| statemachine::is_terminal(m, &inst));
        if is_terminal {
            continue;
        }
        out.push(DashboardSm {
            id: inst.id,
            model: inst.model,
            state: inst.state,
            title: inst.title,
            assignee: inst.assignee,
            total_cost: inst.total_cost,
        });
    }
    out
}

/// Render the dashboard as plain text matching the format described in #213.
pub fn render_text(dash: &Dashboard) -> String {
    let mut out = String::new();

    // AGENTS
    out.push_str("AGENTS\n");
    if dash.agents.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for a in &dash.agents {
            let task_field = if a.current_task.is_empty() {
                "-".to_string()
            } else {
                format!("\"{}\"", truncate(&a.current_task, 60))
            };
            out.push_str(&format!(
                "  {:<10} {:<9} ${:<6.2} {}\n",
                a.name, a.status, a.cost, task_field
            ));
            // Schedules nested under agent.
            for s in &a.schedules {
                out.push_str(&format!(
                    "    {:<18} {:<14} -> {:<24} next {}\n",
                    s.cron, s.action, s.target, s.next
                ));
            }
        }
    }

    // WORKERS — one section per parent that has workers.
    for a in &dash.agents {
        if a.workers.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&format!("WORKERS (inside {})\n", a.name));
        for w in &a.workers {
            let task_field = if w.current_task.is_empty() {
                "-".to_string()
            } else {
                format!("\"{}\"", truncate(&w.current_task, 60))
            };
            out.push_str(&format!(
                "  {:<22} {:<9} {:>3} turns  ${:<6.2} {}\n",
                w.name, w.status, w.turns, w.cost, task_field
            ));
        }
    }

    // SM INSTANCES
    out.push('\n');
    out.push_str("SM INSTANCES\n");
    if dash.sm_instances.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for sm in &dash.sm_instances {
            out.push_str(&format!(
                "  {:<12} {:<10} {:<10} {:<40} ${:.2}\n",
                sm.id,
                sm.model,
                sm.state,
                truncate(&sm.title, 40),
                sm.total_cost
            ));
        }
    }

    // TASKS
    out.push('\n');
    out.push_str("TASKS\n");
    out.push_str(&format!(
        "  {} pending, {} active, {} done, {} failed\n",
        dash.tasks.pending, dash.tasks.active, dash.tasks.done, dash.tasks.failed
    ));

    out
}

/// Render the dashboard as a pretty-printed JSON document.
pub fn render_json(dash: &Dashboard) -> Result<String> {
    Ok(serde_json::to_string_pretty(dash)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dashboard() -> Dashboard {
        Dashboard {
            agents: vec![DashboardAgent {
                name: "uagent".into(),
                status: "working".into(),
                turns: 12,
                cost: 4.20,
                current_task: "Delegating implement for UAGENT-456...".into(),
                workers: vec![
                    DashboardWorker {
                        name: "worker-UAGENT-456".into(),
                        status: "idle".into(),
                        turns: 13,
                        cost: 3.08,
                        current_task: String::new(),
                    },
                    DashboardWorker {
                        name: "worker-UAGENT-465".into(),
                        status: "working".into(),
                        turns: 25,
                        cost: 0.52,
                        current_task: "Implementing memory hooks...".into(),
                    },
                ],
                schedules: vec![],
            }],
            sm_instances: vec![DashboardSm {
                id: "sm-9d696f10".into(),
                model: "ticket".into(),
                state: "implement".into(),
                title: "UAGENT-456: Global System Hooks".into(),
                assignee: "agent:uagent".into(),
                total_cost: 2.50,
            }],
            tasks: DashboardTasks {
                pending: 0,
                active: 0,
                done: 3,
                failed: 0,
            },
        }
    }

    #[test]
    fn render_text_includes_all_sections() {
        let dash = sample_dashboard();
        let text = render_text(&dash);
        assert!(text.contains("AGENTS"), "missing AGENTS header");
        assert!(text.contains("uagent"), "missing agent name");
        assert!(text.contains("WORKERS (inside uagent)"));
        assert!(text.contains("worker-UAGENT-456"));
        assert!(text.contains("worker-UAGENT-465"));
        assert!(text.contains("SM INSTANCES"));
        assert!(text.contains("sm-9d696f10"));
        assert!(text.contains("UAGENT-456: Global System Hooks"));
        assert!(text.contains("TASKS"));
        assert!(text.contains("0 pending"));
        assert!(text.contains("3 done"));
    }

    #[test]
    fn render_text_empty_sections_show_none_marker() {
        let dash = Dashboard {
            agents: vec![],
            sm_instances: vec![],
            tasks: DashboardTasks {
                pending: 0,
                active: 0,
                done: 0,
                failed: 0,
            },
        };
        let text = render_text(&dash);
        assert!(text.contains("AGENTS"));
        assert!(text.contains("(none)"));
        assert!(text.contains("SM INSTANCES"));
        assert!(text.contains("TASKS"));
        // No "WORKERS" section when there are no agents/workers.
        assert!(!text.contains("WORKERS"));
    }

    #[test]
    fn render_text_omits_workers_section_when_empty() {
        let mut dash = sample_dashboard();
        // Strip workers from the only agent.
        dash.agents[0].workers.clear();
        let text = render_text(&dash);
        assert!(
            !text.contains("WORKERS"),
            "workers section should be omitted"
        );
    }

    #[test]
    fn render_json_roundtrips() {
        let dash = sample_dashboard();
        let json = render_json(&dash).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["agents"][0]["name"], "uagent");
        assert_eq!(parsed["agents"][0]["workers"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["sm_instances"][0]["id"], "sm-9d696f10");
        assert_eq!(parsed["tasks"]["done"], 3);
    }
}
