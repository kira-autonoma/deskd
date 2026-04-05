//! WorkItem — intermediate representation between finding a transition and dispatching work.
//!
//! All state machine transitions create a WorkItem regardless of dispatch method.
//! The WorkItem captures step type, prompt, command, criteria, and retry policy,
//! then a single dispatcher routes it to the appropriate execution path.

use crate::domain::statemachine::StepType;
use crate::domain::task::TaskCriteria;

/// A unit of work derived from a state machine transition.
///
/// Created for every non-terminal dispatch. The dispatcher routes based on
/// `step_type` and `criteria` to the correct execution path (bus, queue,
/// shell, LLM-validate, or human notification).
#[derive(Debug, Clone)]
pub struct WorkItem {
    /// The SM instance this work item belongs to.
    pub instance_id: String,
    /// Step type determines the execution path.
    pub step_type: StepType,
    /// The target agent/bus address (from instance.assignee).
    pub assignee: String,
    /// Full task text (built from prompt + instance context).
    pub task_text: String,
    /// Raw prompt from the transition definition.
    pub prompt: String,
    /// Shell command for Check steps; model override for Validate steps.
    pub command: Option<String>,
    /// Notification target for Human steps.
    pub notify: Option<String>,
    /// Task queue criteria — when Some, Agent dispatch goes through the pull-based queue.
    pub criteria: Option<TaskCriteria>,
    /// Timeout duration string (e.g. "5m", "1h").
    pub timeout: Option<String>,
    /// State to go to on timeout.
    pub timeout_goto: Option<String>,
    /// Maximum number of retries for queue-based tasks (default 0).
    pub max_retries: u32,
}
