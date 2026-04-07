//! `deskd usage` — aggregate token usage and cost across all agents.

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

use crate::app::tasklog;

/// Per-agent aggregated stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentStats {
    pub agent: String,
    pub tasks: usize,
    pub cost_usd: f64,
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

/// Aggregate stats across all agents.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageStats {
    pub period: String,
    pub total_tasks: usize,
    pub total_cost_usd: f64,
    pub total_turns: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_duration_ms: u64,
    pub by_agent: Vec<AgentStats>,
}

/// Parse a period string into a `since` cutoff timestamp.
fn parse_period(period: &str) -> Result<Option<DateTime<Utc>>> {
    let now = Utc::now();
    match period {
        "all" => Ok(None),
        "today" => {
            let start_of_day = now.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
            Ok(Some(start_of_day))
        }
        s if s.ends_with('d') => {
            let days: i64 = s.trim_end_matches('d').parse().map_err(|_| {
                anyhow::anyhow!("invalid period '{}' — expected e.g. 7d, 30d, today, all", s)
            })?;
            Ok(Some(now - Duration::days(days)))
        }
        s if s.ends_with('h') => {
            let hours: i64 = s.trim_end_matches('h').parse().map_err(|_| {
                anyhow::anyhow!("invalid period '{}' — expected e.g. 24h, 7d, today, all", s)
            })?;
            Ok(Some(now - Duration::hours(hours)))
        }
        _ => bail!(
            "unknown period '{}' — use today, 24h, 7d, 30d, or all",
            period
        ),
    }
}

/// Discover all agent names by scanning the tasklog directory.
fn discover_agents() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let logs_dir = std::path::PathBuf::from(home).join(".deskd").join("logs");

    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && path.join("tasks.jsonl").exists()
            {
                agents.push(name.to_string());
            }
        }
    }
    agents.sort();
    agents
}

/// Compute aggregate usage stats.
pub fn compute_stats(period: &str, agent_filter: Option<&str>) -> Result<UsageStats> {
    let since = parse_period(period)?;

    let agents = if let Some(name) = agent_filter {
        vec![name.to_string()]
    } else {
        discover_agents()
    };

    let mut by_agent: HashMap<String, AgentStats> = HashMap::new();

    for agent_name in &agents {
        let entries = tasklog::read_logs(agent_name, usize::MAX, None, since)?;
        if entries.is_empty() {
            continue;
        }

        let stats = by_agent
            .entry(agent_name.clone())
            .or_insert_with(|| AgentStats {
                agent: agent_name.clone(),
                tasks: 0,
                cost_usd: 0.0,
                turns: 0,
                input_tokens: 0,
                output_tokens: 0,
                duration_ms: 0,
            });

        for e in &entries {
            stats.tasks += 1;
            stats.cost_usd += e.cost;
            stats.turns += e.turns;
            stats.input_tokens += e.input_tokens.unwrap_or(0);
            stats.output_tokens += e.output_tokens.unwrap_or(0);
            stats.duration_ms += e.duration_ms;
        }
    }

    // Sort by cost descending.
    let mut agent_list: Vec<AgentStats> = by_agent.into_values().collect();
    agent_list.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_tasks = agent_list.iter().map(|a| a.tasks).sum();
    let total_cost_usd = agent_list.iter().map(|a| a.cost_usd).sum();
    let total_turns = agent_list.iter().map(|a| a.turns).sum();
    let total_input_tokens = agent_list.iter().map(|a| a.input_tokens).sum();
    let total_output_tokens = agent_list.iter().map(|a| a.output_tokens).sum();
    let total_duration_ms = agent_list.iter().map(|a| a.duration_ms).sum();

    Ok(UsageStats {
        period: period.to_string(),
        total_tasks,
        total_cost_usd,
        total_turns,
        total_input_tokens,
        total_output_tokens,
        total_duration_ms,
        by_agent: agent_list,
    })
}

/// Format token count as human-readable.
fn format_tokens(n: u64) -> String {
    if n == 0 {
        "-".to_string()
    } else if n < 1_000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Print usage stats as a table.
pub fn print_table(stats: &UsageStats) {
    println!("Token Usage ({})", stats.period);
    println!("{}", "─".repeat(75));
    println!(
        "{:<20} {:>5}  {:>7}  {:>8}  {:>8}  {:>9}",
        "Agent", "Tasks", "Cost", "Input", "Output", "Duration"
    );
    println!("{}", "─".repeat(75));

    for a in &stats.by_agent {
        println!(
            "{:<20} {:>5}  ${:>6.2}  {:>8}  {:>8}  {:>9}",
            a.agent,
            a.tasks,
            a.cost_usd,
            format_tokens(a.input_tokens),
            format_tokens(a.output_tokens),
            tasklog::format_duration(a.duration_ms),
        );
    }

    println!("{}", "─".repeat(75));
    println!(
        "{:<20} {:>5}  ${:>6.2}  {:>8}  {:>8}  {:>9}",
        "TOTAL",
        stats.total_tasks,
        stats.total_cost_usd,
        format_tokens(stats.total_input_tokens),
        format_tokens(stats.total_output_tokens),
        tasklog::format_duration(stats.total_duration_ms),
    );

    if stats.total_tasks > 0 {
        println!();
        println!(
            "Avg per task: {} input, {} output, ${:.2}",
            format_tokens(stats.total_input_tokens / stats.total_tasks as u64),
            format_tokens(stats.total_output_tokens / stats.total_tasks as u64),
            stats.total_cost_usd / stats.total_tasks as f64,
        );
    }
}

/// Print usage stats as JSON.
pub fn print_json(stats: &UsageStats) {
    if let Ok(json) = serde_json::to_string_pretty(stats) {
        println!("{}", json);
    }
}

/// Entry point for `deskd usage`.
pub async fn run(period: &str, agent: Option<&str>, format: &str) -> Result<()> {
    let stats = compute_stats(period, agent)?;

    match format {
        "json" => print_json(&stats),
        _ => print_table(&stats),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_period_7d() {
        let since = parse_period("7d").unwrap();
        assert!(since.is_some());
        let cutoff = since.unwrap();
        let now = Utc::now();
        let diff = now - cutoff;
        // Should be approximately 7 days (within a second tolerance).
        assert!((diff.num_days() - 7).abs() <= 1);
    }

    #[test]
    fn test_parse_period_today() {
        let since = parse_period("today").unwrap();
        assert!(since.is_some());
        let cutoff = since.unwrap();
        assert_eq!(cutoff.date_naive(), Utc::now().date_naive());
    }

    #[test]
    fn test_parse_period_24h() {
        let since = parse_period("24h").unwrap();
        assert!(since.is_some());
    }

    #[test]
    fn test_parse_period_all() {
        let since = parse_period("all").unwrap();
        assert!(since.is_none());
    }

    #[test]
    fn test_parse_period_invalid() {
        assert!(parse_period("xyz").is_err());
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "-");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5K");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_compute_stats_empty() {
        // Use a non-existent agent to get empty results.
        let stats = compute_stats("7d", Some("nonexistent-agent-xyz")).unwrap();
        assert_eq!(stats.total_tasks, 0);
        assert_eq!(stats.total_cost_usd, 0.0);
        assert!(stats.by_agent.is_empty());
    }

    #[test]
    fn test_usage_stats_serializable() {
        let stats = UsageStats {
            period: "7d".to_string(),
            total_tasks: 10,
            total_cost_usd: 5.50,
            total_turns: 42,
            total_input_tokens: 100_000,
            total_output_tokens: 25_000,
            total_duration_ms: 300_000,
            by_agent: vec![AgentStats {
                agent: "test".to_string(),
                tasks: 10,
                cost_usd: 5.50,
                turns: 42,
                input_tokens: 100_000,
                output_tokens: 25_000,
                duration_ms: 300_000,
            }],
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"total_tasks\":10"));
        assert!(json.contains("\"total_cost_usd\":5.5"));
    }
}
