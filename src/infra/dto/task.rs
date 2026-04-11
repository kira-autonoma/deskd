//! Task storage DTOs and domain conversions.

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::domain::task::{Task, TaskCriteria, TaskStatus};

/// Storage format for tasks (JSON file on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTask {
    pub id: String,
    pub description: String,
    pub status: String,
    pub assignee: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub criteria: StoredTaskCriteria,
    #[serde(default)]
    pub sm_instance_id: Option<String>,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub turns: Option<u32>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub timed_out_at: Option<String>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_after: Option<String>,
}

/// Storage format for task matching criteria.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredTaskCriteria {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

impl From<StoredTask> for Task {
    fn from(dto: StoredTask) -> Self {
        Self {
            id: dto.id,
            description: dto.description,
            status: match dto.status.as_str() {
                "pending" => TaskStatus::Pending,
                "active" => TaskStatus::Active,
                "done" => TaskStatus::Done,
                "failed" => TaskStatus::Failed,
                "cancelled" => TaskStatus::Cancelled,
                "dead_letter" => TaskStatus::DeadLetter,
                other => {
                    warn!(status = other, "unknown task status, defaulting to Pending");
                    TaskStatus::Pending
                }
            },
            assignee: dto.assignee,
            result: dto.result,
            error: dto.error,
            created_by: dto.created_by,
            created_at: dto.created_at,
            updated_at: dto.updated_at,
            criteria: TaskCriteria {
                model: dto.criteria.model,
                labels: dto.criteria.labels,
            },
            sm_instance_id: dto.sm_instance_id,
            cost_usd: dto.cost_usd,
            turns: dto.turns,
            metadata: dto.metadata,
            timed_out_at: dto.timed_out_at,
            attempt: dto.attempt,
            max_retries: dto.max_retries,
            retry_after: dto.retry_after,
        }
    }
}

impl From<&Task> for StoredTask {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            description: task.description.clone(),
            status: match task.status {
                TaskStatus::Pending => "pending",
                TaskStatus::Active => "active",
                TaskStatus::Done => "done",
                TaskStatus::Failed => "failed",
                TaskStatus::Cancelled => "cancelled",
                TaskStatus::DeadLetter => "dead_letter",
            }
            .to_string(),
            assignee: task.assignee.clone(),
            result: task.result.clone(),
            error: task.error.clone(),
            created_by: task.created_by.clone(),
            created_at: task.created_at.clone(),
            updated_at: task.updated_at.clone(),
            criteria: StoredTaskCriteria {
                model: task.criteria.model.clone(),
                labels: task.criteria.labels.clone(),
            },
            sm_instance_id: task.sm_instance_id.clone(),
            cost_usd: task.cost_usd,
            turns: task.turns,
            metadata: task.metadata.clone(),
            timed_out_at: task.timed_out_at.clone(),
            attempt: task.attempt,
            max_retries: task.max_retries,
            retry_after: task.retry_after.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stored_task_roundtrip() {
        let task = Task {
            id: "task-1".into(),
            description: "test task".into(),
            status: TaskStatus::Active,
            assignee: Some("dev".into()),
            result: None,
            error: None,
            created_by: "cli".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            criteria: TaskCriteria {
                model: Some("claude-sonnet-4-6".into()),
                labels: vec!["rust".into()],
            },
            sm_instance_id: None,
            cost_usd: None,
            turns: None,
            metadata: serde_json::Value::Null,
            timed_out_at: None,
            attempt: 0,
            max_retries: 0,
            retry_after: None,
        };
        let stored: StoredTask = (&task).into();
        assert_eq!(stored.status, "active");
        let restored: Task = stored.into();
        assert_eq!(restored.status, TaskStatus::Active);
        assert_eq!(restored.id, "task-1");
        assert_eq!(
            restored.criteria.model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }
}
