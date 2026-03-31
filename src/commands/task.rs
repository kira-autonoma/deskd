//! `deskd task` subcommand handlers.

use anyhow::Result;

use crate::cli::TaskAction;
use crate::task;

use super::truncate;

pub fn handle(action: TaskAction) -> Result<()> {
    let store = task::TaskStore::default_for_home();
    match action {
        TaskAction::Add {
            description,
            model,
            labels,
        } => {
            let criteria = task::TaskCriteria { model, labels };
            let t = store.create(&description, criteria, "cli")?;
            println!("Created task {} (pending)", t.id);
        }
        TaskAction::List { status } => {
            let filter = match status.as_deref() {
                Some("pending") => Some(task::TaskStatus::Pending),
                Some("active") => Some(task::TaskStatus::Active),
                Some("done") => Some(task::TaskStatus::Done),
                Some("failed") => Some(task::TaskStatus::Failed),
                Some("cancelled") => Some(task::TaskStatus::Cancelled),
                Some(other) => anyhow::bail!("Unknown status: {}", other),
                None => None,
            };
            let tasks = store.list(filter)?;
            if tasks.is_empty() {
                println!("No tasks.");
                return Ok(());
            }
            println!(
                "{:<14} {:<10} {:<14} {:<14} DESCRIPTION",
                "ID", "STATUS", "ASSIGNEE", "SM"
            );
            for t in &tasks {
                let assignee = t.assignee.as_deref().unwrap_or("-");
                let sm = t.sm_instance_id.as_deref().unwrap_or("-");
                let desc = truncate(&t.description, 50);
                println!(
                    "{:<14} {:<10} {:<14} {:<14} {}",
                    t.id, t.status, assignee, sm, desc
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
