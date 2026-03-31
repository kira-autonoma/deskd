//! `deskd schedule` subcommand handlers.

use std::str::FromStr;

use anyhow::{Context, Result};

use crate::cli::ScheduleSubcommand;
use crate::config;

use super::format_relative_time;

pub fn handle(action: ScheduleSubcommand, config_path: &str) -> Result<()> {
    match action {
        ScheduleSubcommand::List => {
            let user_cfg = config::UserConfig::load(config_path)?;
            if user_cfg.schedules.is_empty() {
                println!("No schedules defined in {}", config_path);
                return Ok(());
            }
            println!(
                "{:<3} {:<20} {:<14} {:<26} NEXT",
                "#", "CRON", "ACTION", "TARGET"
            );
            println!("{}", "─".repeat(70));
            for (i, sched) in user_cfg.schedules.iter().enumerate() {
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
                    "{:<3} {:<20} {:<14} {:<26} next {}",
                    i, sched.cron, action_label, sched.target, next
                );
            }
        }
        ScheduleSubcommand::Add {
            cron: cron_expr,
            action,
            target,
            config_json,
        } => {
            cron::Schedule::from_str(&cron_expr)
                .map_err(|e| anyhow::anyhow!("invalid cron expression: {}", e))?;

            let schedule_action: config::ScheduleAction =
                serde_yaml::from_str(&format!("\"{}\"", action)).map_err(|_| {
                    anyhow::anyhow!(
                        "invalid action: {} (expected: github_poll, raw, shell)",
                        action
                    )
                })?;

            let config_val = match config_json {
                Some(json) => {
                    let v: serde_json::Value = serde_json::from_str(&json)
                        .map_err(|e| anyhow::anyhow!("invalid --config-json: {}", e))?;
                    let yaml_str = serde_json::to_string(&v)?;
                    Some(serde_yaml::from_str(&yaml_str)?)
                }
                None => None,
            };

            let new_sched = config::ScheduleDef {
                cron: cron_expr.clone(),
                target: target.clone(),
                action: schedule_action,
                config: config_val,
                timezone: None,
            };

            let raw = std::fs::read_to_string(config_path)
                .with_context(|| format!("failed to read {}", config_path))?;
            let mut user_cfg: config::UserConfig =
                serde_yaml::from_str(&raw).context("failed to parse config")?;
            user_cfg.schedules.push(new_sched);
            let out = serde_yaml::to_string(&user_cfg)?;
            std::fs::write(config_path, &out)
                .with_context(|| format!("failed to write {}", config_path))?;

            let idx = user_cfg.schedules.len() - 1;
            println!(
                "Added schedule #{}: {} {} → {}",
                idx, cron_expr, action, target
            );
        }
        ScheduleSubcommand::Rm { index } => {
            let raw = std::fs::read_to_string(config_path)
                .with_context(|| format!("failed to read {}", config_path))?;
            let mut user_cfg: config::UserConfig =
                serde_yaml::from_str(&raw).context("failed to parse config")?;

            if index >= user_cfg.schedules.len() {
                anyhow::bail!(
                    "index {} out of range (have {} schedules)",
                    index,
                    user_cfg.schedules.len()
                );
            }

            let removed = user_cfg.schedules.remove(index);
            let out = serde_yaml::to_string(&user_cfg)?;
            std::fs::write(config_path, &out)
                .with_context(|| format!("failed to write {}", config_path))?;

            let action_label = format!("{:?}", removed.action).to_lowercase();
            println!(
                "Removed schedule #{}: {} {} → {}",
                index, removed.cron, action_label, removed.target
            );
        }
    }
    Ok(())
}
