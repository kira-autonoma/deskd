//! `deskd remind` subcommand handler.

use anyhow::{Context, Result};

use crate::config;

use super::parse_duration_secs;

pub fn handle(
    name: String,
    duration_str: Option<String>,
    at: Option<String>,
    target_override: Option<String>,
    message: String,
) -> Result<()> {
    let fire_at: chrono::DateTime<chrono::Utc> = if let Some(ref dur) = duration_str {
        let secs = parse_duration_secs(dur)?;
        chrono::Utc::now() + chrono::Duration::seconds(secs as i64)
    } else if let Some(ref ts) = at {
        chrono::DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("invalid --at timestamp '{}' (expected ISO 8601)", ts))?
            .with_timezone(&chrono::Utc)
    } else {
        anyhow::bail!("either --in <duration> or --at <timestamp> is required");
    };

    let target = target_override.unwrap_or_else(|| format!("agent:{}", name));

    let remind = config::RemindDef {
        at: fire_at.to_rfc3339(),
        target: target.clone(),
        message,
    };

    let dir = config::reminders_dir();
    let filename = format!("{}.json", uuid::Uuid::new_v4());
    let path = dir.join(&filename);

    let json = serde_json::to_string_pretty(&remind).context("failed to serialize reminder")?;
    std::fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;

    println!(
        "Reminder scheduled: target={} at={} file={}",
        target,
        fire_at.to_rfc3339(),
        path.display()
    );

    Ok(())
}
