//! In-memory implementations of store traits for testing.
//!
//! Stores data as infra DTOs internally, converting to/from domain types
//! at trait boundaries — same pattern as the file-backed stores.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::statemachine::{Instance, ModelDef, Transition};
use crate::domain::task::{QueueSummary, Task, TaskCriteria, TaskStatus, matches_criteria};
use crate::infra::dto::{StoredInstance, StoredTask};
use crate::ports::store::{StateMachineReader, StateMachineWriter, TaskReader, TaskWriter};

// ─── InMemoryTaskStore ───────────────────────────────────────────────────────

/// Test-only task store backed by a HashMap of StoredTask DTOs.
pub struct InMemoryTaskStore {
    tasks: Mutex<HashMap<String, StoredTask>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskReader for InMemoryTaskStore {
    fn load(&self, id: &str) -> Result<Task> {
        let tasks = self.tasks.lock().unwrap();
        tasks
            .get(id)
            .cloned()
            .map(Into::into)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))
    }

    fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        let tasks = self.tasks.lock().unwrap();
        let mut result: Vec<Task> = tasks
            .values()
            .cloned()
            .map(Into::into)
            .filter(|t: &Task| status_filter.is_none() || Some(t.status) == status_filter)
            .collect();
        result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(result)
    }

    fn queue_summary(&self) -> QueueSummary {
        let tasks = self.tasks.lock().unwrap();
        let mut s = QueueSummary {
            pending: 0,
            active: 0,
            done: 0,
            failed: 0,
        };
        for dto in tasks.values() {
            let task: Task = dto.clone().into();
            match task.status {
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

impl TaskWriter for InMemoryTaskStore {
    fn create(&self, description: &str, criteria: TaskCriteria, created_by: &str) -> Result<Task> {
        let id = format!("task-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let now = chrono::Utc::now().to_rfc3339();
        let task = Task {
            id: id.clone(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            criteria,
            assignee: None,
            result: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
            created_by: created_by.to_string(),
            sm_instance_id: None,
            cost_usd: None,
            turns: None,
            metadata: serde_json::Value::Null,
            timed_out_at: None,
            attempt: 0,
            max_retries: 0,
            retry_after: None,
        };
        let dto: StoredTask = (&task).into();
        self.tasks.lock().unwrap().insert(id, dto);
        Ok(task)
    }

    fn create_with_metadata(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        metadata: serde_json::Value,
    ) -> Result<Task> {
        let mut task = self.create(description, criteria, created_by)?;
        task.metadata = metadata;
        let dto: StoredTask = (&task).into();
        self.tasks.lock().unwrap().insert(task.id.clone(), dto);
        Ok(task)
    }

    fn create_for_sm(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        sm_instance_id: &str,
    ) -> Result<Task> {
        let mut task = self.create(description, criteria, created_by)?;
        task.sm_instance_id = Some(sm_instance_id.to_string());
        let dto: StoredTask = (&task).into();
        self.tasks.lock().unwrap().insert(task.id.clone(), dto);
        Ok(task)
    }

    fn cancel(&self, id: &str) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let dto = tasks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        let mut task: Task = dto.clone().into();
        if task.status != TaskStatus::Pending {
            bail!("Cannot cancel task '{}': status is '{}'", id, task.status);
        }
        task.status = TaskStatus::Cancelled;
        task.updated_at = chrono::Utc::now().to_rfc3339();
        tasks.insert(id.to_string(), (&task).into());
        Ok(task)
    }

    fn claim_next(
        &self,
        agent_name: &str,
        agent_model: &str,
        agent_labels: &[String],
    ) -> Result<Option<Task>> {
        let mut tasks = self.tasks.lock().unwrap();
        let now = chrono::Utc::now();
        let mut pending: Vec<String> = tasks
            .values()
            .cloned()
            .map(|dto| -> Task { dto.into() })
            .filter(|t| {
                t.status == TaskStatus::Pending
                    && matches_criteria(&t.criteria, agent_model, agent_labels)
                    && !crate::domain::task::is_retry_pending(t, &now)
            })
            .map(|t| t.id.clone())
            .collect();
        pending.sort();

        if let Some(id) = pending.first() {
            let dto = tasks.get(id).unwrap();
            let mut task: Task = dto.clone().into();
            task.status = TaskStatus::Active;
            task.assignee = Some(agent_name.to_string());
            task.updated_at = chrono::Utc::now().to_rfc3339();
            tasks.insert(id.clone(), (&task).into());
            Ok(Some(task))
        } else {
            Ok(None)
        }
    }

    fn complete(
        &self,
        id: &str,
        result_text: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let dto = tasks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        let mut task: Task = dto.clone().into();
        if task.status != TaskStatus::Active {
            bail!("Cannot complete task '{}': status is '{}'", id, task.status);
        }
        task.status = TaskStatus::Done;
        task.result = Some(result_text.to_string());
        task.cost_usd = cost_usd;
        task.turns = turns;
        task.updated_at = chrono::Utc::now().to_rfc3339();
        tasks.insert(id.to_string(), (&task).into());
        Ok(task)
    }

    fn fail(&self, id: &str, error_msg: &str) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let dto = tasks
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        let mut task: Task = dto.clone().into();
        if task.status != TaskStatus::Active {
            bail!("Cannot fail task '{}': status is '{}'", id, task.status);
        }

        let next_attempt = task.attempt + 1;
        if next_attempt <= task.max_retries {
            // Retry: reset to Pending with backoff.
            task.status = TaskStatus::Pending;
            task.attempt = next_attempt;
            task.assignee = None;
            task.error = Some(error_msg.to_string());
            task.retry_after = Some(crate::domain::task::compute_retry_after(next_attempt));
            task.updated_at = chrono::Utc::now().to_rfc3339();
        } else if task.max_retries > 0 {
            // Exhausted all retries — dead letter.
            task.status = TaskStatus::DeadLetter;
            task.error = Some(error_msg.to_string());
            task.updated_at = chrono::Utc::now().to_rfc3339();
        } else {
            // No retries configured — plain failure.
            task.status = TaskStatus::Failed;
            task.error = Some(error_msg.to_string());
            task.updated_at = chrono::Utc::now().to_rfc3339();
        }

        tasks.insert(id.to_string(), (&task).into());
        Ok(task)
    }
}

// ─── InMemoryStateMachineStore ───────────────────────────────────────────────

/// Test-only state machine store backed by a HashMap of StoredInstance DTOs.
pub struct InMemoryStateMachineStore {
    instances: Mutex<HashMap<String, StoredInstance>>,
}

impl InMemoryStateMachineStore {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryStateMachineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachineReader for InMemoryStateMachineStore {
    fn load(&self, id: &str) -> Result<Instance> {
        self.instances
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .map(Into::into)
            .ok_or_else(|| anyhow::anyhow!("Instance '{}' not found", id))
    }

    fn list_all(&self) -> Result<Vec<Instance>> {
        let instances = self.instances.lock().unwrap();
        let mut result: Vec<Instance> = instances.values().cloned().map(Into::into).collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(result)
    }
}

impl StateMachineWriter for InMemoryStateMachineStore {
    fn save(&self, inst: &Instance) -> Result<()> {
        let dto: StoredInstance = inst.into();
        self.instances.lock().unwrap().insert(inst.id.clone(), dto);
        Ok(())
    }

    fn delete(&self, id: &str) -> Result<()> {
        let mut instances = self.instances.lock().unwrap();
        if instances.remove(id).is_none() {
            bail!("Instance '{}' not found", id);
        }
        Ok(())
    }

    fn create(
        &self,
        model: &ModelDef,
        title: &str,
        body: &str,
        created_by: &str,
    ) -> Result<Instance> {
        let id = format!("sm-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let now = chrono::Utc::now().to_rfc3339();
        let initial_state = if model.initial.is_empty() {
            model
                .states
                .first()
                .cloned()
                .unwrap_or_else(|| "initial".to_string())
        } else {
            model.initial.clone()
        };

        let assignee = model
            .transitions
            .iter()
            .find(|t| t.from == model.initial || t.from == "*")
            .and_then(|t| t.assignee.clone())
            .unwrap_or_default();

        let inst = Instance {
            id: id.clone(),
            model: model.name.clone(),
            state: initial_state,
            title: title.to_string(),
            body: body.to_string(),
            assignee,
            result: None,
            error: None,
            created_by: created_by.to_string(),
            created_at: now.clone(),
            updated_at: now,
            history: Vec::new(),
            metadata: serde_json::Value::Null,
            total_cost: 0.0,
            total_turns: 0,
        };
        let dto: StoredInstance = (&inst).into();
        self.instances.lock().unwrap().insert(id, dto);
        Ok(inst)
    }

    fn move_to(
        &self,
        inst: &mut Instance,
        model: &ModelDef,
        target_state: &str,
        trigger: &str,
        note: Option<&str>,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<()> {
        if !model.states.contains(&target_state.to_string()) {
            bail!(
                "State '{}' not defined in model '{}'",
                target_state,
                model.name
            );
        }

        let from_state = inst.state.clone();
        let transition_def = model
            .transitions
            .iter()
            .find(|t| (t.from == from_state || t.from == "*") && t.to == target_state);

        if transition_def.is_none() {
            bail!(
                "No transition from '{}' to '{}' in model '{}'",
                from_state,
                target_state,
                model.name
            );
        }

        let now = chrono::Utc::now().to_rfc3339();
        let transition = Transition {
            from: from_state,
            to: target_state.to_string(),
            trigger: trigger.to_string(),
            timestamp: now.clone(),
            note: note.map(|s| s.to_string()),
            cost_usd,
            turns,
        };

        if let Some(c) = cost_usd {
            inst.total_cost += c;
        }
        if let Some(t) = turns {
            inst.total_turns += t;
        }

        inst.history.push(transition);
        inst.state = target_state.to_string();
        inst.updated_at = now.clone();

        if let Some(td) = transition_def
            && let Some(ref a) = td.assignee
        {
            inst.assignee = a.clone();
        }

        let dto: StoredInstance = (&*inst).into();
        self.instances.lock().unwrap().insert(inst.id.clone(), dto);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::statemachine::{ModelDef, TransitionDef};
    use crate::domain::task::TaskCriteria;

    #[test]
    fn test_memory_task_lifecycle() {
        let store = InMemoryTaskStore::new();
        let task = store
            .create("Fix bug", TaskCriteria::default(), "kira")
            .unwrap();
        assert_eq!(task.status, TaskStatus::Pending);

        let loaded = store.load(&task.id).unwrap();
        assert_eq!(loaded.description, "Fix bug");

        let claimed = store
            .claim_next("worker-1", "claude-sonnet-4-6", &[])
            .unwrap()
            .unwrap();
        assert_eq!(claimed.status, TaskStatus::Active);

        let done = store.complete(&claimed.id, "Fixed it", None, None).unwrap();
        assert_eq!(done.status, TaskStatus::Done);

        let summary = store.queue_summary();
        assert_eq!(summary.done, 1);
    }

    #[test]
    fn test_memory_sm_lifecycle() {
        let model = ModelDef {
            name: "review".to_string(),
            description: String::new(),
            states: vec![
                "open".to_string(),
                "in_review".to_string(),
                "done".to_string(),
            ],
            initial: "open".to_string(),
            terminal: vec!["done".to_string()],
            transitions: vec![
                TransitionDef {
                    from: "open".to_string(),
                    to: "in_review".to_string(),
                    trigger: Some("start".to_string()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
                TransitionDef {
                    from: "in_review".to_string(),
                    to: "done".to_string(),
                    trigger: Some("approve".to_string()),
                    on: None,
                    assignee: None,
                    prompt: None,
                    step_type: None,
                    notify: None,
                    timeout: None,
                    timeout_goto: None,
                    criteria: None,
                    max_retries: 0,
                },
            ],
        };

        let store = InMemoryStateMachineStore::new();
        let inst = store
            .create(&model, "PR Review", "Review this PR", "kira")
            .unwrap();
        assert_eq!(inst.state, "open");

        let mut inst = store.load(&inst.id).unwrap();
        store
            .move_to(&mut inst, &model, "in_review", "start", None, None, None)
            .unwrap();
        assert_eq!(inst.state, "in_review");

        store
            .move_to(
                &mut inst,
                &model,
                "done",
                "approve",
                Some("LGTM"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(inst.state, "done");
        assert_eq!(inst.history.len(), 2);

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_retry_on_fail_resets_to_pending() {
        let store = InMemoryTaskStore::new();
        let mut task = store
            .create("Retryable task", TaskCriteria::default(), "kira")
            .unwrap();

        // Set max_retries on the task (simulating SM dispatch setting it).
        task.max_retries = 2;
        let dto: crate::infra::dto::StoredTask = (&task).into();
        store.tasks.lock().unwrap().insert(task.id.clone(), dto);

        // Claim and fail the task — should retry (attempt 0 -> 1, max_retries=2).
        let claimed = store.claim_next("w1", "any", &[]).unwrap().unwrap();
        assert_eq!(claimed.id, task.id);

        let failed = store.fail(&claimed.id, "transient error").unwrap();
        assert_eq!(failed.status, TaskStatus::Pending);
        assert_eq!(failed.attempt, 1);
        assert!(failed.retry_after.is_some());
        assert!(failed.assignee.is_none());
        assert_eq!(failed.error.as_deref(), Some("transient error"));
    }

    #[test]
    fn test_retry_exhausted_goes_to_dead_letter() {
        let store = InMemoryTaskStore::new();
        let mut task = store
            .create("Retryable task", TaskCriteria::default(), "kira")
            .unwrap();

        // Set max_retries=1, attempt=1 (already retried once).
        task.max_retries = 1;
        task.attempt = 1;
        let dto: crate::infra::dto::StoredTask = (&task).into();
        store.tasks.lock().unwrap().insert(task.id.clone(), dto);

        // Claim and fail — should go to DeadLetter (attempt 1+1=2 > max_retries=1).
        let claimed = store.claim_next("w1", "any", &[]).unwrap().unwrap();
        let result = store.fail(&claimed.id, "permanent error").unwrap();
        assert_eq!(result.status, TaskStatus::DeadLetter);
    }

    #[test]
    fn test_no_retry_configured_goes_to_failed() {
        let store = InMemoryTaskStore::new();
        let task = store
            .create("No-retry task", TaskCriteria::default(), "kira")
            .unwrap();
        assert_eq!(task.max_retries, 0);

        let claimed = store.claim_next("w1", "any", &[]).unwrap().unwrap();
        let result = store.fail(&claimed.id, "error").unwrap();
        assert_eq!(result.status, TaskStatus::Failed);
    }

    #[test]
    fn test_claim_next_skips_retry_pending_tasks() {
        let store = InMemoryTaskStore::new();
        let mut task = store
            .create("Retry-pending task", TaskCriteria::default(), "kira")
            .unwrap();

        // Set retry_after to 1 hour in the future.
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        task.retry_after = Some(future);
        task.attempt = 1;
        task.max_retries = 3;
        let dto: crate::infra::dto::StoredTask = (&task).into();
        store.tasks.lock().unwrap().insert(task.id.clone(), dto);

        // claim_next should return None (task is pending but retry_after is in future).
        let claimed = store.claim_next("w1", "any", &[]).unwrap();
        assert!(claimed.is_none());
    }
}
