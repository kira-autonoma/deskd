//! In-memory implementations of store traits for testing.
//!
//! No filesystem, no I/O — pure in-memory state.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::domain::statemachine::{Instance, ModelDef, Transition};
use crate::domain::task::{QueueSummary, Task, TaskCriteria, TaskStatus, matches_criteria};
use crate::ports::store::{StateMachineRepository, TaskRepository};

// ─── InMemoryTaskStore ───────────────────────────────────────────────────────

/// Test-only task store backed by a HashMap.
pub struct InMemoryTaskStore {
    tasks: Mutex<HashMap<String, Task>>,
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

impl TaskRepository for InMemoryTaskStore {
    fn load(&self, id: &str) -> Result<Task> {
        let tasks = self.tasks.lock().unwrap();
        tasks
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))
    }

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
        };
        self.tasks.lock().unwrap().insert(id, task.clone());
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
        self.tasks
            .lock()
            .unwrap()
            .insert(task.id.clone(), task.clone());
        Ok(task)
    }

    fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        let tasks = self.tasks.lock().unwrap();
        let mut result: Vec<Task> = tasks
            .values()
            .filter(|t| status_filter.is_none() || Some(t.status) == status_filter)
            .cloned()
            .collect();
        result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(result)
    }

    fn cancel(&self, id: &str) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        if task.status != TaskStatus::Pending {
            bail!("Cannot cancel task '{}': status is '{}'", id, task.status);
        }
        task.status = TaskStatus::Cancelled;
        task.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(task.clone())
    }

    fn claim_next(
        &self,
        agent_name: &str,
        agent_model: &str,
        agent_labels: &[String],
    ) -> Result<Option<Task>> {
        let mut tasks = self.tasks.lock().unwrap();
        let mut pending: Vec<String> = tasks
            .values()
            .filter(|t| {
                t.status == TaskStatus::Pending
                    && matches_criteria(&t.criteria, agent_model, agent_labels)
            })
            .map(|t| t.id.clone())
            .collect();
        pending.sort();

        if let Some(id) = pending.first() {
            let task = tasks.get_mut(id).unwrap();
            task.status = TaskStatus::Active;
            task.assignee = Some(agent_name.to_string());
            task.updated_at = chrono::Utc::now().to_rfc3339();
            Ok(Some(task.clone()))
        } else {
            Ok(None)
        }
    }

    fn complete(&self, id: &str, result_text: &str) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        if task.status != TaskStatus::Active {
            bail!("Cannot complete task '{}': status is '{}'", id, task.status);
        }
        task.status = TaskStatus::Done;
        task.result = Some(result_text.to_string());
        task.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(task.clone())
    }

    fn fail(&self, id: &str, error_msg: &str) -> Result<Task> {
        let mut tasks = self.tasks.lock().unwrap();
        let task = tasks
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;
        if task.status != TaskStatus::Active {
            bail!("Cannot fail task '{}': status is '{}'", id, task.status);
        }
        task.status = TaskStatus::Failed;
        task.error = Some(error_msg.to_string());
        task.updated_at = chrono::Utc::now().to_rfc3339();
        Ok(task.clone())
    }

    fn queue_summary(&self) -> QueueSummary {
        let tasks = self.tasks.lock().unwrap();
        let mut s = QueueSummary {
            pending: 0,
            active: 0,
            done: 0,
            failed: 0,
        };
        for t in tasks.values() {
            match t.status {
                TaskStatus::Pending => s.pending += 1,
                TaskStatus::Active => s.active += 1,
                TaskStatus::Done => s.done += 1,
                TaskStatus::Failed => s.failed += 1,
                TaskStatus::Cancelled => {}
            }
        }
        s
    }
}

// ─── InMemoryStateMachineStore ───────────────────────────────────────────────

/// Test-only state machine store backed by a HashMap.
pub struct InMemoryStateMachineStore {
    instances: Mutex<HashMap<String, Instance>>,
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

impl StateMachineRepository for InMemoryStateMachineStore {
    fn save(&self, inst: &Instance) -> Result<()> {
        self.instances
            .lock()
            .unwrap()
            .insert(inst.id.clone(), inst.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Instance> {
        self.instances
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Instance '{}' not found", id))
    }

    fn list_all(&self) -> Result<Vec<Instance>> {
        let instances = self.instances.lock().unwrap();
        let mut result: Vec<Instance> = instances.values().cloned().collect();
        result.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(result)
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
        };
        self.instances.lock().unwrap().insert(id, inst.clone());
        Ok(inst)
    }

    fn move_to(
        &self,
        inst: &mut Instance,
        model: &ModelDef,
        target_state: &str,
        trigger: &str,
        note: Option<&str>,
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
        };

        inst.history.push(transition);
        inst.state = target_state.to_string();
        inst.updated_at = now.clone();

        if let Some(td) = transition_def
            && let Some(ref a) = td.assignee
        {
            inst.assignee = a.clone();
        }

        self.instances
            .lock()
            .unwrap()
            .insert(inst.id.clone(), inst.clone());
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

        let done = store.complete(&claimed.id, "Fixed it").unwrap();
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
            .move_to(&mut inst, &model, "in_review", "start", None)
            .unwrap();
        assert_eq!(inst.state, "in_review");

        store
            .move_to(&mut inst, &model, "done", "approve", Some("LGTM"))
            .unwrap();
        assert_eq!(inst.state, "done");
        assert_eq!(inst.history.len(), 2);

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 1);
    }
}
