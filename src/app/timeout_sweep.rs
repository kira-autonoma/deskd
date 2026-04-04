//! Periodic sweep loop that enforces `TransitionDef.timeout` / `timeout_goto`.
//!
//! Background task — started once per `deskd serve`. Every `sweep_interval`
//! it scans all active tasks that have a linked SM instance, checks whether
//! `updated_at + timeout` has elapsed, and if so:
//!
//! 1. Marks the task as `Failed` with a timeout error and sets `timed_out_at`.
//! 2. Force-transitions the SM instance to the `timeout_goto` state.
//!
//! The sweep interval defaults to 30 s and is configurable via the caller.

use std::time::Duration;

use tracing::{info, warn};

use crate::app::statemachine::StateMachineStore;
use crate::app::task::TaskStore;
use crate::domain::statemachine::ModelDef;
use crate::domain::task::TaskStatus;

/// Parse a human-readable duration string into [`Duration`].
///
/// Supported units: `s` (seconds), `m` (minutes), `h` (hours), `d` (days).
/// Examples: `"30s"`, `"5m"`, `"2h"`, `"1d"`.
///
/// Returns `None` if the string is not recognised.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Find where the numeric part ends.
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num_str, unit) = s.split_at(split);
    let n: u64 = num_str.parse().ok()?;
    match unit.trim() {
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        "h" => Some(Duration::from_secs(n * 3600)),
        "d" => Some(Duration::from_secs(n * 86400)),
        _ => None,
    }
}

/// Run the timeout sweep loop forever with the given interval.
///
/// `models` is the list of SM model definitions known to this agent instance.
/// This is a long-running async function; call it inside `tokio::spawn`.
pub async fn run_timeout_sweep(models: Vec<ModelDef>, sweep_interval: Duration) {
    info!(
        interval_secs = sweep_interval.as_secs(),
        "timeout sweep loop started"
    );
    let mut ticker = tokio::time::interval(sweep_interval);
    ticker.tick().await; // skip first immediate tick

    loop {
        ticker.tick().await;
        if let Err(e) = sweep_once(&models) {
            warn!(error = %e, "timeout sweep encountered an error");
        }
    }
}

/// Execute a single sweep pass: check every active task with an SM instance.
fn sweep_once(models: &[ModelDef]) -> anyhow::Result<()> {
    let task_store = TaskStore::default_for_home();
    let sm_store = StateMachineStore::default_for_home();

    let active_tasks = task_store.list(Some(TaskStatus::Active))?;

    for task in active_tasks {
        // Only care about tasks linked to an SM instance.
        let sm_id = match &task.sm_instance_id {
            Some(id) => id.clone(),
            None => continue,
        };

        // Load the SM instance; skip on error (might have been deleted).
        let mut inst = match sm_store.load(&sm_id) {
            Ok(i) => i,
            Err(e) => {
                warn!(
                    task_id = %task.id,
                    sm_id = %sm_id,
                    error = %e,
                    "timeout sweep: failed to load SM instance, skipping"
                );
                continue;
            }
        };

        // Look up the model definition by name.
        let model = match models.iter().find(|m| m.name == inst.model) {
            Some(m) => m,
            None => continue, // model not loaded — can't determine timeout
        };

        // Find the transition out of the current state that carries a timeout.
        // We pick the first transition from the current state that has both
        // `timeout` and `timeout_goto` set.
        let timeout_def = model.transitions.iter().find(|t| {
            (t.from == inst.state || t.from == "*")
                && t.timeout.is_some()
                && t.timeout_goto.is_some()
        });

        let td = match timeout_def {
            Some(t) => t,
            None => continue, // no timeout configured for this state
        };

        let timeout_str = td.timeout.as_deref().unwrap_or("");
        let timeout_goto = match &td.timeout_goto {
            Some(g) => g.clone(),
            None => continue,
        };

        let timeout_dur = match parse_duration(timeout_str) {
            Some(d) => d,
            None => {
                warn!(
                    task_id = %task.id,
                    timeout = %timeout_str,
                    "timeout sweep: unparseable timeout string, skipping"
                );
                continue;
            }
        };

        // Check elapsed time since the task was last updated.
        let updated_at = match chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let elapsed = chrono::Utc::now()
            .signed_duration_since(updated_at)
            .to_std()
            .unwrap_or(Duration::ZERO);

        if elapsed < timeout_dur {
            continue; // not expired yet
        }

        warn!(
            task_id = %task.id,
            sm_id = %sm_id,
            state = %inst.state,
            timeout_goto = %timeout_goto,
            elapsed_secs = elapsed.as_secs(),
            timeout_secs = timeout_dur.as_secs(),
            "task timed out — forcing SM transition and failing task"
        );

        // Fail the task.
        if let Err(e) = task_store.timeout_fail(
            &task.id,
            &format!(
                "task timed out after {}s (limit: {}s)",
                elapsed.as_secs(),
                timeout_dur.as_secs()
            ),
        ) {
            warn!(task_id = %task.id, error = %e, "timeout sweep: failed to mark task as failed");
        }

        // Force-transition the SM instance to timeout_goto.
        if let Err(e) = sm_store.force_transition(
            &mut inst,
            &timeout_goto,
            "timeout",
            Some(&format!("timed out after {}s", elapsed.as_secs())),
        ) {
            warn!(
                sm_id = %sm_id,
                timeout_goto = %timeout_goto,
                error = %e,
                "timeout sweep: failed to force-transition SM instance"
            );
        }

        info!(
            task_id = %task.id,
            sm_id = %sm_id,
            timeout_goto = %timeout_goto,
            "timeout sweep: task expired and SM transitioned"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("1s"), Some(Duration::from_secs(1)));
    }

    #[test]
    fn test_parse_duration_minutes() {
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1m"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
    }

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("m"), None);
    }

    #[test]
    fn test_parse_duration_whitespace() {
        assert_eq!(parse_duration("  10s  "), Some(Duration::from_secs(10)));
    }
}
