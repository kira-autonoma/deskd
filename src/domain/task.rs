//! Task queue domain types.
//!
//! Pure data types — no I/O, no persistence logic.
//! Serde lives on infra DTOs (infra::dto), not here.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Active,
    Done,
    Failed,
    Cancelled,
    /// Task permanently failed after exhausting all retries.
    DeadLetter,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Active => write!(f, "active"),
            Self::Done => write!(f, "done"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::DeadLetter => write!(f, "dead_letter"),
        }
    }
}

/// Criteria for matching tasks to workers.
#[derive(Debug, Clone, Default)]
pub struct TaskCriteria {
    /// Required model (e.g. "claude-sonnet-4-6").
    pub model: Option<String>,
    /// Required labels.
    pub labels: Vec<String>,
}

/// A task in the queue.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub description: String,
    pub status: TaskStatus,
    pub criteria: TaskCriteria,
    /// Agent assigned to the task (set when status -> active).
    pub assignee: Option<String>,
    /// Result text (set when status -> done).
    pub result: Option<String>,
    /// Error message (set when status -> failed).
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Who created the task.
    pub created_by: String,
    /// Linked state machine instance ID (set when task is created by SM dispatch).
    pub sm_instance_id: Option<String>,
    /// Cost in USD for this task (set on completion).
    pub cost_usd: Option<f64>,
    /// Number of turns used (set on completion).
    pub turns: Option<u32>,
    /// Structured metadata (JSON object, e.g. worktree path, branch, repo URL).
    pub metadata: serde_json::Value,
    /// Timestamp when the task was timed out by the sweep loop (RFC 3339).
    pub timed_out_at: Option<String>,
    /// Current attempt number (0-based). Incremented on each retry.
    pub attempt: u32,
    /// Maximum number of retries before moving to DeadLetter. 0 means no retry.
    pub max_retries: u32,
    /// RFC 3339 timestamp before which this task should not be claimed (backoff).
    pub retry_after: Option<String>,
}

/// Summary of the queue for status display.
pub struct QueueSummary {
    pub pending: usize,
    pub active: usize,
    pub done: usize,
    pub failed: usize,
}

/// Compute the `retry_after` timestamp using exponential backoff.
/// Formula: `min(30s * 2^attempt, 5m)` from now.
pub fn compute_retry_after(attempt: u32) -> String {
    let base_secs: u64 = 30;
    let max_secs: u64 = 300; // 5 minutes
    let backoff_secs = std::cmp::min(
        base_secs.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1))),
        max_secs,
    );
    let retry_at = chrono::Utc::now() + chrono::Duration::seconds(backoff_secs as i64);
    retry_at.to_rfc3339()
}

/// Check if a task is pending retry (has a `retry_after` in the future).
pub fn is_retry_pending(task: &Task, now: &chrono::DateTime<chrono::Utc>) -> bool {
    if let Some(ref retry_after) = task.retry_after
        && let Ok(retry_time) = chrono::DateTime::parse_from_rfc3339(retry_after)
    {
        return retry_time > *now;
    }
    false
}

/// Check if a task's criteria match a worker's capabilities.
pub fn matches_criteria(
    criteria: &TaskCriteria,
    agent_model: &str,
    agent_labels: &[String],
) -> bool {
    if let Some(ref required_model) = criteria.model
        && required_model != agent_model
    {
        return false;
    }
    for label in &criteria.labels {
        if !agent_labels.contains(label) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_criteria() {
        assert!(matches_criteria(&TaskCriteria::default(), "any", &[]));
        assert!(matches_criteria(
            &TaskCriteria {
                model: Some("sonnet".into()),
                labels: vec![]
            },
            "sonnet",
            &[]
        ));
        assert!(!matches_criteria(
            &TaskCriteria {
                model: Some("sonnet".into()),
                labels: vec![]
            },
            "haiku",
            &[]
        ));
        assert!(matches_criteria(
            &TaskCriteria {
                model: None,
                labels: vec!["x".into()]
            },
            "any",
            &["x".into(), "y".into()]
        ));
        assert!(!matches_criteria(
            &TaskCriteria {
                model: None,
                labels: vec!["x".into()]
            },
            "any",
            &["y".into()]
        ));
    }

    #[test]
    fn test_compute_retry_after_backoff() {
        // attempt 1 -> 30s
        let before = chrono::Utc::now();
        let ts = compute_retry_after(1);
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        let diff = parsed.signed_duration_since(before);
        assert!(diff.num_seconds() >= 29 && diff.num_seconds() <= 31);

        // attempt 2 -> 60s
        let before = chrono::Utc::now();
        let ts = compute_retry_after(2);
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        let diff = parsed.signed_duration_since(before);
        assert!(diff.num_seconds() >= 59 && diff.num_seconds() <= 61);

        // attempt 4 -> 240s
        let before = chrono::Utc::now();
        let ts = compute_retry_after(4);
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        let diff = parsed.signed_duration_since(before);
        assert!(diff.num_seconds() >= 239 && diff.num_seconds() <= 241);

        // attempt 5 -> capped at 300s (5min)
        let before = chrono::Utc::now();
        let ts = compute_retry_after(5);
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts).unwrap();
        let diff = parsed.signed_duration_since(before);
        assert!(diff.num_seconds() >= 299 && diff.num_seconds() <= 301);
    }

    #[test]
    fn test_is_retry_pending() {
        let now = chrono::Utc::now();
        let future = (now + chrono::Duration::seconds(60)).to_rfc3339();
        let past = (now - chrono::Duration::seconds(60)).to_rfc3339();

        let mut task = Task {
            id: "t1".into(),
            description: "test".into(),
            status: TaskStatus::Pending,
            criteria: TaskCriteria::default(),
            assignee: None,
            result: None,
            error: None,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            created_by: "test".into(),
            sm_instance_id: None,
            cost_usd: None,
            turns: None,
            metadata: serde_json::Value::Null,
            timed_out_at: None,
            attempt: 1,
            max_retries: 3,
            retry_after: Some(future),
        };

        // Future retry_after -> pending
        assert!(is_retry_pending(&task, &now));

        // Past retry_after -> not pending
        task.retry_after = Some(past);
        assert!(!is_retry_pending(&task, &now));

        // No retry_after -> not pending
        task.retry_after = None;
        assert!(!is_retry_pending(&task, &now));
    }
}
