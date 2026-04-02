//! Store port traits — persistence abstraction for domain entities.
//!
//! Implementations: file-based (production) in task.rs/statemachine.rs,
//! in-memory (testing) in infra/memory_store.rs.

use anyhow::Result;

use std::path::Path;

use crate::domain::context::MainBranch;
use crate::domain::statemachine::{Instance, ModelDef};
use crate::domain::task::{QueueSummary, Task, TaskCriteria, TaskStatus};

/// Persistence operations for context branches (MainBranch YAML files).
pub trait ContextRepository: Send + Sync {
    fn load(&self, path: &Path) -> Result<MainBranch>;
    fn save(&self, branch: &MainBranch, path: &Path) -> Result<()>;
}

/// Persistence operations for the task queue.
pub trait TaskRepository: Send + Sync {
    fn load(&self, id: &str) -> Result<Task>;
    fn create(&self, description: &str, criteria: TaskCriteria, created_by: &str) -> Result<Task>;
    fn create_with_metadata(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        metadata: serde_json::Value,
    ) -> Result<Task>;
    fn create_for_sm(
        &self,
        description: &str,
        criteria: TaskCriteria,
        created_by: &str,
        sm_instance_id: &str,
    ) -> Result<Task>;
    fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>>;
    fn cancel(&self, id: &str) -> Result<Task>;
    fn claim_next(
        &self,
        agent_name: &str,
        agent_model: &str,
        agent_labels: &[String],
    ) -> Result<Option<Task>>;
    fn complete(
        &self,
        id: &str,
        result_text: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<Task>;
    fn fail(&self, id: &str, error_msg: &str) -> Result<Task>;
    fn queue_summary(&self) -> QueueSummary;
}

/// Persistence operations for state machine instances.
pub trait StateMachineRepository: Send + Sync {
    fn save(&self, inst: &Instance) -> Result<()>;
    fn load(&self, id: &str) -> Result<Instance>;
    fn list_all(&self) -> Result<Vec<Instance>>;
    fn delete(&self, id: &str) -> Result<()>;
    fn create(
        &self,
        model: &ModelDef,
        title: &str,
        body: &str,
        created_by: &str,
    ) -> Result<Instance>;
    #[allow(clippy::too_many_arguments)]
    fn move_to(
        &self,
        inst: &mut Instance,
        model: &ModelDef,
        target_state: &str,
        trigger: &str,
        note: Option<&str>,
        cost_usd: Option<f64>,
        turns: Option<u32>,
    ) -> Result<()>;
}
