//! `deskd sm` subcommand handlers.

use anyhow::Result;
use tracing::warn;

use crate::app::cli::SmAction;
use crate::app::{statemachine, workflow};
use crate::config;

use super::truncate;

pub async fn handle(
    action: SmAction,
    user_cfg: &config::UserConfig,
    config_path: &str,
) -> Result<()> {
    let store = statemachine::StateMachineStore::default_for_home();
    let models: Vec<statemachine::ModelDef> =
        user_cfg.models.iter().cloned().map(Into::into).collect();
    match action {
        SmAction::Models => {
            if models.is_empty() {
                println!("No models defined");
            } else {
                println!("{:<20} {:<8} {:<8} DESCRIPTION", "NAME", "STATES", "TRANS");
                for m in &models {
                    println!(
                        "{:<20} {:<8} {:<8} {}",
                        m.name,
                        m.states.len(),
                        m.transitions.len(),
                        m.description,
                    );
                }
            }
        }
        SmAction::Show { model } => {
            let m = models
                .iter()
                .find(|m| m.name == model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model))?;
            println!("Model:    {}", m.name);
            if !m.description.is_empty() {
                println!("Desc:     {}", m.description);
            }
            println!("States:   {}", m.states.join(", "));
            println!("Initial:  {}", m.initial);
            println!(
                "Terminal: {}",
                if m.terminal.is_empty() {
                    "-".to_string()
                } else {
                    m.terminal.join(", ")
                }
            );
            println!();
            println!(
                "{:<15} {:<15} {:<12} {:<12} ASSIGNEE",
                "FROM", "TO", "TRIGGER", "ON"
            );
            for t in &m.transitions {
                println!(
                    "{:<15} {:<15} {:<12} {:<12} {}",
                    t.from,
                    t.to,
                    t.trigger.as_deref().unwrap_or("-"),
                    t.on.as_deref().unwrap_or("-"),
                    t.assignee.as_deref().unwrap_or("-"),
                );
            }
        }
        SmAction::Create { model, title, body } => {
            let m = models
                .iter()
                .find(|m| m.name == model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model))?;
            let creator = std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "cli".to_string());
            let inst = store.create(m, &title, body.as_deref().unwrap_or(""), &creator)?;
            println!(
                "Created {} (model={}, state={})",
                inst.id, inst.model, inst.state
            );
        }
        SmAction::Move { id, state, note } => {
            let mut inst = store.load(&id)?;
            let m = models
                .iter()
                .find(|m| m.name == inst.model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found in config", inst.model))?;
            let trigger =
                std::env::var("DESKD_AGENT_NAME").unwrap_or_else(|_| "manual".to_string());
            store.move_to(&mut inst, m, &state, &trigger, note.as_deref(), None, None)?;
            println!("{} -> {} ({})", id, inst.state, inst.model);

            // Notify workflow engine if the new state has an assignee.
            if !inst.assignee.is_empty() && !statemachine::is_terminal(m, &inst) {
                let bus_socket = std::env::var("DESKD_BUS_SOCKET").unwrap_or_else(|_| {
                    let work_dir = std::path::Path::new(config_path)
                        .parent()
                        .unwrap_or(std::path::Path::new("."));
                    config::agent_bus_socket(&work_dir.to_string_lossy())
                });
                if std::path::Path::new(&bus_socket).exists()
                    && let Err(e) = workflow::notify_moved(&bus_socket, &id, "cli").await
                {
                    warn!(instance = %id, error = %e, "failed to notify workflow engine");
                }
            }
        }
        SmAction::Status { id } => {
            let inst = store.load(&id)?;
            println!("ID:        {}", inst.id);
            println!("Model:     {}", inst.model);
            println!("Title:     {}", inst.title);
            if !inst.body.is_empty() {
                println!("Body:      {}", inst.body);
            }
            println!("State:     {}", inst.state);
            println!("Assignee:  {}", inst.assignee);
            if let Some(ref r) = inst.result {
                println!("Result:    {}", r);
            }
            if let Some(ref e) = inst.error {
                println!("Error:     {}", e);
            }
            println!("Created:   {} by {}", inst.created_at, inst.created_by);
            println!("Updated:   {}", inst.updated_at);
            if inst.total_cost > 0.0 || inst.total_turns > 0 {
                println!(
                    "Total:     ${:.4} / {} turns",
                    inst.total_cost, inst.total_turns
                );
            }
            if !inst.history.is_empty() {
                println!();
                println!(
                    "{:<15} {:<15} {:<20} {:<10} {:<8} TIMESTAMP",
                    "FROM", "TO", "TRIGGER", "COST", "TURNS"
                );
                for h in &inst.history {
                    let cost = h
                        .cost_usd
                        .map(|c| format!("${:.4}", c))
                        .unwrap_or_else(|| "-".into());
                    let turns = h.turns.map(|t| t.to_string()).unwrap_or_else(|| "-".into());
                    println!(
                        "{:<15} {:<15} {:<20} {:<10} {:<8} {}",
                        h.from, h.to, h.trigger, cost, turns, h.timestamp,
                    );
                }
            }
        }
        SmAction::List {
            model,
            state,
            limit,
        } => {
            let mut instances = store.list_all()?;
            if let Some(ref m) = model {
                instances.retain(|i| i.model == *m);
            }
            if let Some(ref s) = state {
                instances.retain(|i| i.state == *s);
            }
            instances.truncate(limit);
            if instances.is_empty() {
                println!("No instances found");
            } else {
                println!(
                    "{:<12} {:<15} {:<12} {:<12} TITLE",
                    "ID", "MODEL", "STATE", "ASSIGNEE"
                );
                for inst in &instances {
                    println!(
                        "{:<12} {:<15} {:<12} {:<12} {}",
                        inst.id,
                        inst.model,
                        inst.state,
                        inst.assignee,
                        truncate(&inst.title, 40),
                    );
                }
            }
        }
        SmAction::Cancel { id } => {
            let mut inst = store.load(&id)?;
            let m = models
                .iter()
                .find(|m| m.name == inst.model)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found in config", inst.model))?;
            if statemachine::is_terminal(m, &inst) {
                println!("{} is already in terminal state '{}'", id, inst.state);
                return Ok(());
            }
            let valid = statemachine::valid_transitions(m, &inst.state);
            let cancel_target = valid
                .iter()
                .find(|t| m.terminal.contains(&t.to))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No transition to a terminal state from '{}' in model '{}'",
                        inst.state,
                        m.name
                    )
                })?;
            let target = cancel_target.to.clone();
            store.move_to(
                &mut inst,
                m,
                &target,
                "cancel",
                Some("Cancelled via CLI"),
                None,
                None,
            )?;
            println!("{} cancelled -> {}", id, inst.state);
        }
    }
    Ok(())
}
