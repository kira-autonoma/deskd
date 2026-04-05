//! Pull-based task queue backed by JSON files in `~/.deskd/tasks/`.
//!
//! Each task is a JSON file named `{task_id}.json`. Tasks have a lifecycle:
//! `pending` → `active` → `done` | `failed`. An optional `cancelled` status
//! is set by `task_cancel`.
//!
//! Workers poll for pending tasks matching their capabilities (model, labels).

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::path::PathBuf;

use crate::infra::dto::StoredTask;

// Re-export domain types for backward compatibility.
pub use crate::domain::task::*;

/// Persistent store for tasks, backed by a directory of JSON files.
pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    /// Default store location: `$HOME/.deskd/tasks`.
    pub fn default_for_home() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self::new(PathBuf::from(home).join(".deskd").join("tasks"))
    }

    fn task_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", id))
    }

    fn save(&self, task: &Task) -> Result<()> {
        let path = self.task_path(&task.id);
        let tmp = path.with_extension("tmp");
        let dto: StoredTask = task.into();
        let content = serde_json::to_string_pretty(&dto)?;
        std::fs::write(&tmp, &content)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Task> {
        let path = self.task_path(id);
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("Task '{}' not found", id))?;
        let dto: StoredTask = serde_json::from_str(&content)?;
        Ok(dto.into())
    }

    /// Create a new task in the queue.
    pub fn create(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
    ) -> Result<Task> {
        self.create_inner(
            description,
            criteria,
            created_by,
            None,
            serde_json::Value::Null,
        )
    }

    /// Create a new task with structured metadata.
    pub fn create_with_metadata(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        metadata: serde_json::Value,
    ) -> Result<Task> {
        self.create_inner(description, criteria, created_by, None, metadata)
    }

    /// Create a new task linked to a state machine instance.
    pub fn create_for_sm(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        sm_instance_id: &str,
    ) -> Result<Task> {
        self.create_inner(
            description,
            criteria,
            created_by,
            Some(sm_instance_id.to_string()),
            serde_json::Value::Null,
        )
    }

    fn create_inner(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        sm_instance_id: Option<String>,
        metadata: serde_json::Value,
    ) -> Result<Task> {
        let id = format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let now = Utc::now().to_rfc3339();

        let task = Task {
            id,
            description: description.to_string(),
            status: TaskStatus::Pending,
            criteria,
            assignee: None,
            result: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
            created_by: created_by.to_string(),
            sm_instance_id,
            cost_usd: None,
            turns: None,
            metadata,
            timed_out_at: None,
        };

        self.save(&task)?;
        Ok(task)
    }

    /// List all tasks, optionally filtered by status.
    pub fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if self.dir.exists() {
            for entry in std::fs::read_dir(&self.dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false)
                    && let Ok(content) = std::fs::read_to_string(&path)
                    && let Ok(dto) = serde_json::from_str::<StoredTask>(&content)
                {
                    let task: Task = dto.into();
                    if let Some(filter) = status_filter {
                        if task.status == filter {
                            tasks.push(task);
                        }
                    } else {
                        tasks.push(task);
                    }
                }
            }
        }

        tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(tasks)
    }

    /// Cancel a pending task.
    pub fn cancel(&self, id: &str) -> Result<Task> {
        let mut task = self.load(id)?;
        if task.status != TaskStatus::Pending {
            bail!(
                "Cannot cancel task '{}': status is '{}' (must be pending)",
                id,
                task.status
            );
        }
        task.status = TaskStatus::Cancelled;
        task.updated_at = Utc::now().to_rfc3339();
        self.save(&task)?;
        Ok(task)
    }

    /// Try to claim the next pending task matching the worker's capabilities.
    /// Returns None if no matching task is available.
    pub fn claim_next(
        &self,
        agent_name: &str,
        agent_model: &str,
        agent_labels: &[String],
    ) -> Result<Option<Task>> {
        if !self.dir.exists() {
            return Ok(None);
        }

        let mut pending: Vec<Task> = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false)
                && let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(dto) = serde_json::from_str::<StoredTask>(&content)
            {
                let task: Task = dto.into();
                if task.status == TaskStatus::Pending {
                    pending.push(task);
                }
            }
        }
        pending.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        for task in pending {
            if matches_criteria(&task.criteria, agent_model, agent_labels)
                && let Ok(claimed) = self.try_claim(&task.id, agent_name)
            {
                return Ok(Some(claimed));
            }
        }

        Ok(None)
    }

    fn try_claim(&self, task_id: &str, agent_name: &str) -> Result<Task> {
        let path = self.task_path(task_id);
        let content = std::fs::read_to_string(&path)?;
        let dto: StoredTask = serde_json::from_str(&content)?;
        let mut task: Task = dto.into();

        if task.status != TaskStatus::Pending {
            bail!("Task '{}' is no longer pending", task_id);
        }

        task.status = TaskStatus::Active;
        task.assignee = Some(agent_name.to_string());
        task.updated_at = Utc::now().to_rfc3339();
        self.save(&task)?;
        Ok(task)
    }

    /// Mark a task as completed, optionally recording cost and turn count.
    pub fn complete(
        &self,
        id: &str,
        result_text: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<Task> {
        let mut task = self.load(id)?;
        if task.status != TaskStatus::Active {
            bail!(
                "Cannot complete task '{}': status is '{}' (must be active)",
                id,
                task.status
            );
        }
        task.status = TaskStatus::Done;
        task.result = Some(result_text.to_string());
        task.cost_usd = cost_usd;
        task.turns = turns;
        task.updated_at = Utc::now().to_rfc3339();
        self.save(&task)?;
        Ok(task)
    }

    /// Mark a task as failed.
    pub fn fail(&self, id: &str, error_msg: &str) -> Result<Task> {
        let mut task = self.load(id)?;
        if task.status != TaskStatus::Active {
            bail!(
                "Cannot fail task '{}': status is '{}' (must be active)",
                id,
                task.status
            );
        }
        task.status = TaskStatus::Failed;
        task.error = Some(error_msg.to_string());
        task.updated_at = Utc::now().to_rfc3339();
        self.save(&task)?;
        Ok(task)
    }

    /// Mark an active or pending task as failed due to a timeout.
    ///
    /// Unlike `fail`, this method accepts tasks in either `Active` or `Pending`
    /// state (a pending task may time out before an agent even claims it).
    /// Sets `timed_out_at` in addition to the standard `Failed` fields.
    pub fn timeout_fail(&self, id: &str, error_msg: &str) -> Result<Task> {
        let mut task = self.load(id)?;
        if task.status != TaskStatus::Active && task.status != TaskStatus::Pending {
            bail!(
                "Cannot timeout task '{}': status is '{}' (must be active or pending)",
                id,
                task.status
            );
        }
        let now = Utc::now().to_rfc3339();
        task.status = TaskStatus::Failed;
        task.error = Some(error_msg.to_string());
        task.timed_out_at = Some(now.clone());
        task.updated_at = now;
        self.save(&task)?;
        Ok(task)
    }

    /// Summary of the queue for status display.
    pub fn queue_summary(&self) -> QueueSummary {
        let tasks = self.list(None).unwrap_or_default();
        let mut s = QueueSummary {
            pending: 0,
            active: 0,
            done: 0,
            failed: 0,
        };
        for t in &tasks {
            match t.status {
                TaskStatus::Pending => s.pending += 1,
                TaskStatus::Active => s.active += 1,
                TaskStatus::Done => s.done += 1,
                TaskStatus::Failed => s.failed += 1,
                TaskStatus::Cancelled | TaskStatus::DeadLetter => {}
            }
        }
        s
    }
}

impl crate::ports::store::TaskReader for TaskStore {
    fn load(&self, id: &str) -> Result<Task> {
        self.load(id)
    }
    fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        self.list(status_filter)
    }
    fn queue_summary(&self) -> QueueSummary {
        self.queue_summary()
    }
}

impl crate::ports::store::TaskWriter for TaskStore {
    fn create(&self, description: &str, criteria: TaskCriteria, created_by: &str) -> Result<Task> {
        self.create(description, criteria, created_by)
    }
    fn create_with_metadata(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        metadata: serde_json::Value,
    ) -> Result<Task> {
        self.create_with_metadata(description, criteria, created_by, metadata)
    }
    fn create_for_sm(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        sm_instance_id: &str,
    ) -> Result<Task> {
        self.create_for_sm(description, criteria, created_by, sm_instance_id)
    }
    fn cancel(&self, id: &str) -> Result<Task> {
        self.cancel(id)
    }
    fn claim_next(
        &self,
        agent_name: &str,
        agent_model: &str,
        agent_labels: &[String],
    ) -> Result<Option<Task>> {
        self.claim_next(agent_name, agent_model, agent_labels)
    }
    fn complete(
        &self,
        id: &str,
        result_text: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<Task> {
        self.complete(id, result_text, cost_usd, turns)
    }
    fn fail(&self, id: &str, error_msg: &str) -> Result<Task> {
        self.fail(id, error_msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> TaskStore {
        let dir = std::env::temp_dir().join(format!("deskd-task-test-{}", uuid::Uuid::new_v4()));
        TaskStore::new(dir)
    }

    #[test]
    fn test_create_and_load() {
        let store = temp_store();
        let task = store
            .create("Fix the bug", TaskCriteria::default(), "kira")
            .unwrap();
        assert!(task.id.starts_with("task-"));
        assert_eq!(task.status, TaskStatus::Pending);

        let loaded = store.load(&task.id).unwrap();
        assert_eq!(loaded.id, task.id);
        assert_eq!(loaded.description, "Fix the bug");
    }

    #[test]
    fn test_list_with_filter() {
        let store = temp_store();
        store
            .create("Task 1", TaskCriteria::default(), "kira")
            .unwrap();
        let t2 = store
            .create("Task 2", TaskCriteria::default(), "kira")
            .unwrap();
        store.cancel(&t2.id).unwrap();

        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 2);

        let pending = store.list(Some(TaskStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].description, "Task 1");
    }

    #[test]
    fn test_cancel_pending() {
        let store = temp_store();
        let task = store
            .create("To cancel", TaskCriteria::default(), "kira")
            .unwrap();
        let cancelled = store.cancel(&task.id).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
    }

    #[test]
    fn test_cancel_active_fails() {
        let store = temp_store();
        let task = store
            .create("Active task", TaskCriteria::default(), "kira")
            .unwrap();
        let claimed = store.claim_next("w1", "any", &[]).unwrap().unwrap();
        assert_eq!(claimed.id, task.id);
        assert!(store.cancel(&task.id).is_err());
    }

    #[test]
    fn test_claim_next() {
        let store = temp_store();
        store
            .create(
                "Sonnet task",
                TaskCriteria {
                    model: Some("claude-sonnet-4-6".into()),
                    labels: vec![],
                },
                "kira",
            )
            .unwrap();
        store
            .create("Any task", TaskCriteria::default(), "kira")
            .unwrap();

        let claimed = store
            .claim_next("haiku-worker", "claude-haiku-4-5", &[])
            .unwrap();
        assert!(claimed.is_some());
        assert_eq!(claimed.unwrap().description, "Any task");
    }

    #[test]
    fn test_claim_with_labels() {
        let store = temp_store();
        store
            .create(
                "Labeled task",
                TaskCriteria {
                    model: None,
                    labels: vec!["uagent".into()],
                },
                "kira",
            )
            .unwrap();

        let claimed = store.claim_next("w1", "claude-sonnet-4-6", &[]).unwrap();
        assert!(claimed.is_none());

        let claimed = store
            .claim_next("w2", "claude-sonnet-4-6", &["uagent".into()])
            .unwrap();
        assert!(claimed.is_some());
    }

    #[test]
    fn test_complete_and_fail() {
        let store = temp_store();
        let t1 = store
            .create("Task OK", TaskCriteria::default(), "kira")
            .unwrap();
        let t2 = store
            .create("Task ERR", TaskCriteria::default(), "kira")
            .unwrap();

        store.claim_next("w1", "any", &[]).unwrap(); // claims t1
        store.claim_next("w2", "any", &[]).unwrap(); // claims t2

        let done = store.complete(&t1.id, "All good", None, None).unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.result.as_deref(), Some("All good"));

        let failed = store.fail(&t2.id, "Crashed").unwrap();
        assert_eq!(failed.status, TaskStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("Crashed"));
    }

    #[test]
    fn test_queue_summary() {
        let store = temp_store();
        store.create("A", TaskCriteria::default(), "kira").unwrap();
        store.create("B", TaskCriteria::default(), "kira").unwrap();
        let t3 = store.create("C", TaskCriteria::default(), "kira").unwrap();
        store.cancel(&t3.id).unwrap();

        let s = store.queue_summary();
        assert_eq!(s.pending, 2);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn test_timeout_fail_active_task() {
        let store = temp_store();
        store
            .create("Slow task", TaskCriteria::default(), "kira")
            .unwrap();

        let claimed = store.claim_next("agent-1", "any", &[]).unwrap().unwrap();
        assert_eq!(claimed.status, TaskStatus::Active);

        let timed = store
            .timeout_fail(&claimed.id, "timed out after 60s")
            .unwrap();
        assert_eq!(timed.status, TaskStatus::Failed);
        assert!(timed.timed_out_at.is_some());
        assert_eq!(timed.error.as_deref(), Some("timed out after 60s"));
    }

    #[test]
    fn test_timeout_fail_pending_task() {
        let store = temp_store();
        let task = store
            .create("Unclaimed task", TaskCriteria::default(), "kira")
            .unwrap();

        let timed = store.timeout_fail(&task.id, "never claimed").unwrap();
        assert_eq!(timed.status, TaskStatus::Failed);
        assert!(timed.timed_out_at.is_some());
    }

    #[test]
    fn test_timeout_fail_rejects_done_task() {
        let store = temp_store();
        store
            .create("Done task", TaskCriteria::default(), "kira")
            .unwrap();

        let claimed = store.claim_next("agent-1", "any", &[]).unwrap().unwrap();
        store.complete(&claimed.id, "all good", None, None).unwrap();

        let result = store.timeout_fail(&claimed.id, "too late");
        assert!(result.is_err());
    }
}
