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

/// Read-only persistence operations for the task queue.
pub trait TaskReader: Send + Sync {
    fn load(&self, id: &str) -> Result<Task>;
    fn list(&self, status_filter: Option<TaskStatus>) -> Result<Vec<Task>>;
    fn queue_summary(&self) -> QueueSummary;
}

/// Write/mutate persistence operations for the task queue.
pub trait TaskWriter: Send + Sync {
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
    fn save(&self, task: &Task) -> Result<()>;
}

/// Combined task persistence trait for code that needs both read and write access.
///
/// Supertrait of [`TaskReader`] + [`TaskWriter`]; implementors only need to satisfy
/// the two constituent traits — the blanket impl below covers the rest.
pub trait TaskRepository: TaskReader + TaskWriter {}

/// Blanket impl: any type that is both a `TaskReader` and a `TaskWriter` automatically
/// satisfies `TaskRepository`.
impl<T: TaskReader + TaskWriter> TaskRepository for T {}

/// Read-only persistence operations for state machine instances.
pub trait StateMachineReader: Send + Sync {
    fn load(&self, id: &str) -> Result<Instance>;
    fn list_all(&self) -> Result<Vec<Instance>>;
}

/// Write/mutate persistence operations for state machine instances.
pub trait StateMachineWriter: Send + Sync {
    fn save(&self, inst: &Instance) -> Result<()>;
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

/// Combined supertrait alias for backward compatibility — blanket-implemented
/// for any type that implements both `StateMachineReader` and `StateMachineWriter`.
pub trait StateMachineRepository: StateMachineReader + StateMachineWriter {}

impl<T: StateMachineReader + StateMachineWriter> StateMachineRepository for T {}
