//! `deskd task` subcommand handlers.

use anyhow::Result;

use crate::app::cli::TaskAction;
use crate::app::task;

use super::truncate;

pub fn handle(action: TaskAction) -> Result<()> {
    let store = task::TaskStore::default_for_home();
    match action {
        TaskAction::Add {
            description,
            model,
            labels,
            metadata,
        } => {
            let criteria = task::TaskCriteria { model, labels };
            let t = if let Some(ref meta_str) = metadata {
                let meta: serde_json::Value = serde_json::from_str(meta_str)
                    .map_err(|e| anyhow::anyhow!("invalid --metadata JSON: {}", e))?;
                store.create_with_metadata(&description, criteria, "cli", meta)?
            } else {
                store.create(&description, criteria, "cli")?
            };
            println!("Created task {} (pending)", t.id);
        }
        TaskAction::List {
            status,
            dead_letter,
        } => {
            let filter = if dead_letter {
                Some(task::TaskStatus::DeadLetter)
            } else {
                match status.as_deref() {
                    Some("pending") => Some(task::TaskStatus::Pending),
                    Some("active") => Some(task::TaskStatus::Active),
                    Some("done") => Some(task::TaskStatus::Done),
                    Some("failed") => Some(task::TaskStatus::Failed),
                    Some("cancelled") => Some(task::TaskStatus::Cancelled),
                    Some("dead_letter") => Some(task::TaskStatus::DeadLetter),
                    Some(other) => anyhow::bail!("Unknown status: {}", other),
                    None => None,
                }
            };
            let tasks = store.list(filter)?;
            if tasks.is_empty() {
                println!("No tasks.");
                return Ok(());
            }
            println!(
                "{:<14} {:<10} {:<10} {:<14} {:<14} DESCRIPTION",
                "ID", "STATUS", "COST", "ASSIGNEE", "SM"
            );
            for t in &tasks {
                let assignee = t.assignee.as_deref().unwrap_or("-");
                let sm = t.sm_instance_id.as_deref().unwrap_or("-");
                let cost = t
                    .cost_usd
                    .map(|c| format!("${:.4}", c))
                    .unwrap_or_else(|| "-".into());
                let desc = truncate(&t.description, 50);
                println!(
                    "{:<14} {:<10} {:<10} {:<14} {:<14} {}",
                    t.id, t.status, cost, assignee, sm, desc
                );
            }
        }
        TaskAction::Cancel { id } => {
            let t = store.cancel(&id)?;
            println!("Cancelled task {}", t.id);
        }
    }
    Ok(())
}
