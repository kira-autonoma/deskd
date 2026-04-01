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
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Active => write!(f, "active"),
            Self::Done => write!(f, "done"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
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
}

/// Summary of the queue for status display.
pub struct QueueSummary {
    pub pending: usize,
    pub active: usize,
    pub done: usize,
    pub failed: usize,
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
}
